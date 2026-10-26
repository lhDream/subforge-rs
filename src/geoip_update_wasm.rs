//! Workers 侧 GeoIP 库更新：Fetch 下载 → 校验 → 写对象存储 → 热加载。
//!
//! 与 native `geoip_update` 同名 API；没有 ETag 条件请求与定期任务（Cron 后续补）。

use crate::geoip::{GeoDbInfo, GeoResolver};
use crate::storage::Storage;
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

pub const AUTO_DB_FILE: &str = "geoip-auto.mmdb";
pub const STATUS_FILE: &str = "status.yaml";
pub const BUILTIN_SOURCE_LABEL: &str = "DB-IP Lite Country（免注册，按月发布）";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeoipStatus {
    #[serde(default)]
    pub last_attempt_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_success_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_kind: Option<crate::cache::ErrorKind>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub changed: bool,
    pub skipped_busy: bool,
    pub message: String,
    pub info: Option<GeoDbInfo>,
}

pub struct GeoipUpdater {
    store: crate::store::StoreHandle,
    geo: Arc<GeoResolver>,
    status: RwLock<GeoipStatus>,
    update_lock: Mutex<()>,
}

impl GeoipUpdater {
    pub fn with_proxy(
        store: crate::store::StoreHandle,
        geo: Arc<GeoResolver>,
        _proxy: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            store,
            geo,
            status: RwLock::new(GeoipStatus::default()),
            update_lock: Mutex::new(()),
        })
    }

    pub fn status(&self) -> GeoipStatus {
        self.status.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// Workers Fetch 不支持出口代理；保留接口以兼容共享代码。
    pub async fn set_proxy(&self, _proxy: Option<String>) -> Result<()> {
        Ok(())
    }

    /// 从对象存储读取上次更新状态（启动时调用一次）。
    pub async fn reload_status(&self) {
        let storage = self.store.storage();
        if let Ok(Some(text)) = storage.get_string(&format!("geoip/{STATUS_FILE}")).await
            && let Ok(status) = serde_yaml_ng::from_str::<GeoipStatus>(&text)
            && let Ok(mut guard) = self.status.write()
        {
            *guard = status;
        }
    }

    pub async fn update(&self, reason: &str, force: bool) -> Result<UpdateOutcome> {
        let Ok(_guard) = self.update_lock.try_lock() else {
            return Ok(UpdateOutcome {
                changed: false,
                skipped_busy: true,
                message: "已有一次 GeoIP 更新正在进行".to_string(),
                info: self.geo.db_info(),
            });
        };

        let snapshot = self.store.snapshot().await;
        let settings = snapshot.settings.geoip.clone();
        drop(snapshot);
        let now = Utc::now();
        let url = if settings.uses_custom_url() {
            let url = settings.url.trim().to_string();
            validate_source_url(&url)?;
            url
        } else {
            current_builtin_source_url(now)
        };

        let mut status = self.status();
        status.last_attempt_at = Some(now);
        status.source_url = Some(redact_url(&url));

        // 手动下载忽略条件请求；定期更新仍带 ETag，但本地没有可用库时也强制重下。
        let has_local_db = self.geo.is_available();
        let conditional = !force && has_local_db;
        let result = self.download(&url, &mut status, conditional).await;
        let storage = self.store.storage();
        match result {
            Ok(bytes) => {
                let payload = decode_payload(&bytes)?;
                // 先在内存里校验并装载；只有成功后才落盘，避免把坏库写进 R2，
                // 让下次启动的 GeoResolver 读到损坏数据。
                let info = self.geo.install_bytes(payload.clone())?;
                storage.put(crate::geoip::AUTO_DB_KEY, &payload).await?;
                status.last_success_at = Some(now);
                status.last_error = None;
                status.last_error_kind = None;
                let _ = storage
                    .put_string(
                        &format!("geoip/{STATUS_FILE}"),
                        &serde_yaml_ng::to_string(&status).unwrap_or_default(),
                    )
                    .await;
                if let Ok(mut guard) = self.status.write() {
                    *guard = status;
                }
                Ok(UpdateOutcome {
                    changed: true,
                    skipped_busy: false,
                    message: format!("GeoIP 库已更新（{reason}）"),
                    info: Some(info),
                })
            }
            Err(e) => {
                status.last_error = Some(format!("{e:#}"));
                status.last_error_kind = Some(crate::cache::ErrorKind::Network);
                let _ = storage
                    .put_string(
                        &format!("geoip/{STATUS_FILE}"),
                        &serde_yaml_ng::to_string(&status).unwrap_or_default(),
                    )
                    .await;
                if let Ok(mut guard) = self.status.write() {
                    *guard = status;
                }
                Err(e)
            }
        }
    }

    async fn download(
        &self,
        url: &str,
        status: &mut GeoipStatus,
        conditional: bool,
    ) -> Result<Vec<u8>> {
        let url = url.to_string();
        let etag = if conditional {
            status.etag.clone()
        } else {
            None
        };
        worker::send::SendFuture::new(async move {
            self.download_inner(&url, status, etag.as_deref()).await
        })
        .await
    }

    async fn download_inner(
        &self,
        url: &str,
        status: &mut GeoipStatus,
        etag: Option<&str>,
    ) -> Result<Vec<u8>> {
        let storage = self.store.storage();
        let env: &Env = storage.env();
        let _ = env;
        let headers = Headers::new();
        headers.set(
            "User-Agent",
            &format!(
                "{}/{} (+geoip-updater)",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ),
        )?;
        if let Some(etag) = etag {
            let _ = headers.set("If-None-Match", etag);
        }
        let mut init = RequestInit::new();
        init.with_method(Method::Get).with_headers(headers);
        let request = Request::new_with_init(url, &init)?;
        let mut resp = Fetch::Request(request)
            .send()
            .await
            .context("下载 GeoIP 库失败")?;
        if resp.status_code() == 304 {
            anyhow::bail!("源站返回 304（未变更）");
        }
        if !(200..300).contains(&resp.status_code()) {
            anyhow::bail!("源站返回 HTTP {}", resp.status_code());
        }
        status.etag = resp.headers().get("etag").ok().flatten();
        status.last_modified = resp.headers().get("last-modified").ok().flatten();
        resp.bytes()
            .await
            .map_err(|e| anyhow::anyhow!("读取下载内容失败：{e}"))
    }
}

/// 解压 `.mmdb.gz`；非 gzip 内容原样返回。
pub fn decode_payload(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .context("解压 .mmdb.gz 失败")?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

pub fn builtin_source_url(year: i32, month: u32) -> String {
    format!("https://download.db-ip.com/free/dbip-country-lite-{year:04}-{month:02}.mmdb.gz")
}

pub fn current_builtin_source_url(now: DateTime<Utc>) -> String {
    builtin_source_url(now.year(), now.month())
}

pub fn validate_source_url(url: &str) -> Result<()> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        anyhow::bail!("下载地址必须以 http:// 或 https:// 开头");
    }
    Ok(())
}

/// 展示前脱敏：隐藏 query 中的 token。
pub fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?***"),
        None => url.to_string(),
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// 供启动时加载状态（避免在同步构造函数里 await）。
pub async fn load_status(storage: &Storage) -> GeoipStatus {
    match storage.get_string(&format!("geoip/{STATUS_FILE}")).await {
        Ok(Some(text)) => serde_yaml_ng::from_str(&text).unwrap_or_default(),
        _ => GeoipStatus::default(),
    }
}
