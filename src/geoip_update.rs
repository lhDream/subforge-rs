//! GeoIP 库自动下载、安装与定期更新。
//!
//! 数据源默认使用 **DB-IP Lite Country**（免注册、按月发布 `.mmdb.gz`），
//! 也可以配置自定义 http(s) 地址。无论来源如何，字节必须先经 [`crate::geoip::inspect_mmdb`]
//! 校验，再原子替换 `data/geoip/geoip-auto.mmdb` 并热加载；**失败绝不动现有库**。
//!
//! 状态记录在 `data/geoip/status.yaml`（与业务数据 `store.yaml` 分开，避免每次
//! 更新都制造一次业务数据备份与 revision 变化）。设置本身保存在
//! `store.yaml` 的 `settings.geoip`，由通用设置页维护。

use crate::cache::ErrorKind;
use crate::geoip::{self, GeoDbInfo, GeoResolver};
use crate::store::model::GeoipSettings;
use crate::store::{StoreHandle, atomic_write};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Utc};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex;

/// 自动下载安装的文件名。固定名称确保同一时间只有一个「自动库」，
/// 也避免用户手工放入的库与它按名字混在一起。
pub const AUTO_DB_FILE: &str = "geoip-auto.mmdb";
/// 更新状态文件名（位于 `data/geoip/`）。
pub const STATUS_FILE: &str = "status.yaml";

/// 内置来源的说明文案。
pub const BUILTIN_SOURCE_LABEL: &str = "DB-IP Lite Country（免注册，按月发布）";

/// 压缩包大小上限：DB-IP Country Lite 约 4 MB，留足余量。
const MAX_COMPRESSED: usize = 64 * 1024 * 1024;
/// 解压后大小上限：当前约 8 MB。
const MAX_DECOMPRESSED: usize = 256 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// 后台检查间隔（不是下载间隔；下载间隔由设置决定）。
const CHECK_TICK: Duration = Duration::from_secs(600);
/// 启动后稍等再检查，避免与上游刷新同时抢占网络。
const STARTUP_DELAY: Duration = Duration::from_secs(10);
/// 失败后的重试冷却，避免每 10 分钟锤一次下载源。
const FAILURE_RETRY_SECS: i64 = 3600;

/// 持久化的更新状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeoipStatus {
    #[serde(default)]
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// 最近一次「确认内容可用」的时间（含 304）。
    #[serde(default)]
    pub last_success_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_kind: Option<ErrorKind>,
    /// 实际使用的下载地址（可能含自定义 token，页面展示前必须脱敏）。
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub database_type: Option<String>,
    #[serde(default)]
    pub build_epoch: Option<u64>,
    #[serde(default)]
    pub ip_version: Option<u16>,
    #[serde(default)]
    pub node_count: Option<u32>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    /// 本次是否真的替换了库文件。
    pub changed: bool,
    /// 已有更新在途，本次跳过。
    pub skipped_busy: bool,
    pub message: String,
    pub info: Option<GeoDbInfo>,
}

pub struct GeoipUpdater {
    store: StoreHandle,
    geo: Arc<GeoResolver>,
    client: tokio::sync::RwLock<reqwest::Client>,
    timeout: Duration,
    status: RwLock<GeoipStatus>,
    status_path: PathBuf,
    /// 同一时间只允许一个更新（手动与定期共用）。
    update_lock: Mutex<()>,
}

impl GeoipUpdater {
    /// `proxy` 为规范化后的出口代理地址；`None` 表示直连。
    pub fn with_proxy(
        store: StoreHandle,
        geo: Arc<GeoResolver>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let status_path = store.data_dir().join("geoip").join(STATUS_FILE);
        let status = load_status(&status_path);
        let builder = reqwest::Client::builder()
            .user_agent(format!(
                "{}/{} (+geoip-updater)",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ))
            .connect_timeout(Duration::from_secs(15))
            .timeout(REQUEST_TIMEOUT);
        let client = crate::proxy::apply(builder, proxy)
            .context("创建 GeoIP 下载客户端失败")?
            .build()
            .context("创建 GeoIP 下载客户端失败")?;
        Ok(Self {
            store,
            geo,
            client: tokio::sync::RwLock::new(client),
            timeout: REQUEST_TIMEOUT,
            status: RwLock::new(status),
            status_path,
            update_lock: Mutex::new(()),
        })
    }

    pub fn status(&self) -> GeoipStatus {
        match self.status.read() {
            Ok(g) => g.clone(),
            Err(e) => e.into_inner().clone(),
        }
    }

    /// 下载并安装一次。
    ///
    /// `force = true`（手动按钮）时忽略 ETag / Last-Modified 条件请求；
    /// 定期更新用 `force = false`，源站内容未变时走 304，不重复下载 4 MB。
    pub async fn update(&self, reason: &str, force: bool) -> Result<UpdateOutcome> {
        // 手动与定期任务共用一把锁：已有更新在途时直接跳过，不并发下载。
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
        let custom = settings.uses_custom_url();
        let candidates = if custom {
            let url = settings.url.trim().to_string();
            validate_source_url(&url)?;
            vec![url]
        } else {
            // 本月文件通常 1 号发布；月初头几天仍可能 404，回退到上一个月。
            let (year, month) = (now.year(), now.month());
            let (prev_year, prev_month) = previous_month(year, month);
            vec![
                builtin_source_url(year, month),
                builtin_source_url(prev_year, prev_month),
            ]
        };

        let mut last_error: Option<FetchError> = None;
        for (index, url) in candidates.iter().enumerate() {
            let status = self.status();
            let conditional = (!force
                && self.geo.is_available()
                && status.source_url.as_deref() == Some(url.as_str()))
            .then_some(DownloadCondition {
                etag: status.etag.clone(),
                last_modified: status.last_modified.clone(),
            })
            .filter(|c| c.etag.is_some() || c.last_modified.is_some());

            match self.fetch(url, conditional.as_ref()).await {
                Ok(download) => {
                    if download.not_modified {
                        self.record_not_modified(url, download.etag, download.last_modified, now);
                        let message = "GeoIP 库已是最新版本（304）".to_string();
                        tracing::info!("[{reason}] {message}");
                        return Ok(UpdateOutcome {
                            changed: false,
                            skipped_busy: false,
                            message,
                            info: self.geo.db_info(),
                        });
                    }

                    let bytes = download.bytes.context("下载响应缺少内容")?;
                    let processed = (|| -> Result<GeoDbInfo> {
                        let payload = decode_payload(&bytes)?;
                        geoip::inspect_mmdb(&payload)?;
                        let path = self.store.data_dir().join("geoip").join(AUTO_DB_FILE);
                        self.geo.install_bytes(payload, &path)
                    })();

                    match processed {
                        Ok(info) => {
                            self.record_success(
                                url,
                                download.etag,
                                download.last_modified,
                                &info,
                                now,
                            );
                            let message = format!(
                                "已下载并安装 {}（构建于 {}，{}）",
                                info.database_type,
                                geoip::format_epoch(info.build_epoch),
                                human_size(info.size)
                            );
                            tracing::info!("[{reason}] GeoIP 库更新成功：{message}");
                            return Ok(UpdateOutcome {
                                changed: true,
                                skipped_busy: false,
                                message,
                                info: Some(info),
                            });
                        }
                        Err(e) => {
                            let message = format!("{e:#}");
                            self.record_failure(ErrorKind::Parse, message.clone(), now);
                            tracing::warn!(
                                "[{reason}] 下载内容不可用（{}）：{message}",
                                ErrorKind::Parse.label()
                            );
                            return Err(anyhow::anyhow!(message));
                        }
                    }
                }
                Err(err) => {
                    // 内置源：本月尚未发布时静默尝试上一个月。
                    if !custom && index == 0 && err.http_status == Some(404) {
                        tracing::info!(
                            "本月 GeoIP 文件尚未发布（{}），尝试上一个月",
                            redact_url(url)
                        );
                        last_error = Some(err);
                        continue;
                    }
                    self.record_failure(err.kind, err.message.clone(), now);
                    tracing::warn!(
                        "[{reason}] GeoIP 库更新失败（{}）：{}",
                        err.kind.label(),
                        err.message
                    );
                    return Err(anyhow::anyhow!(err.message));
                }
            }
        }

        let err = last_error.unwrap_or_else(|| FetchError {
            kind: ErrorKind::Network,
            message: "没有可用的下载地址".to_string(),
            http_status: None,
        });
        self.record_failure(err.kind, err.message.clone(), now);
        Err(anyhow::anyhow!(err.message))
    }

    /// 启动后台定期检查任务。
    pub fn spawn_periodic(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tokio::time::sleep(STARTUP_DELAY).await;
            loop {
                let snapshot = self.store.snapshot().await;
                let settings = snapshot.settings.geoip.clone();
                drop(snapshot);

                if settings.auto_update && self.is_due(&settings, Utc::now()) {
                    match self.update("auto", false).await {
                        Ok(o) if o.skipped_busy => {}
                        Ok(o) => tracing::info!("GeoIP 自动更新：{}", o.message),
                        Err(e) => tracing::warn!("GeoIP 自动更新失败：{e:#}"),
                    }
                }
                tokio::time::sleep(CHECK_TICK).await;
            }
        })
    }

    /// 替换后续 GeoIP 下载使用的客户端。
    pub async fn set_proxy(&self, proxy: Option<String>) -> Result<()> {
        let builder = reqwest::Client::builder()
            .user_agent(format!(
                "{}/{} (+geoip-updater)",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ))
            .connect_timeout(Duration::from_secs(15))
            .timeout(self.timeout);
        let client = crate::proxy::apply(builder, proxy.as_deref())?
            .build()
            .context("创建 GeoIP 下载客户端失败")?;
        *self.client.write().await = client;
        Ok(())
    }

    /// 是否到了更新时机。`pub(crate)` 便于单测。
    pub(crate) fn is_due(&self, settings: &GeoipSettings, now: DateTime<Utc>) -> bool {
        let status = self.status();
        let db_modified = self.geo.db_info().and_then(|i| i.modified);
        is_update_due(settings, now, &status, self.geo.is_available(), db_modified)
    }

    async fn fetch(
        &self,
        url: &str,
        conditional: Option<&DownloadCondition>,
    ) -> std::result::Result<Download, FetchError> {
        let client = self.client.read().await.clone();
        let mut req = client.get(url);
        if let Some(c) = conditional {
            if let Some(etag) = &c.etag {
                req = req.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            if let Some(lm) = &c.last_modified {
                req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
            }
        }

        let mut resp = req.send().await.map_err(|e| FetchError {
            kind: ErrorKind::Network,
            message: format!("请求失败：{e}"),
            http_status: None,
        })?;

        let status_code = resp.status();
        if status_code == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Download {
                not_modified: true,
                bytes: None,
                etag: header_str(&resp, reqwest::header::ETAG),
                last_modified: header_str(&resp, reqwest::header::LAST_MODIFIED),
            });
        }
        if !status_code.is_success() {
            return Err(FetchError {
                kind: ErrorKind::HttpStatus,
                message: format!("HTTP {status_code}"),
                http_status: Some(status_code.as_u16()),
            });
        }

        if let Some(len) = resp.content_length()
            && len > MAX_COMPRESSED as u64
        {
            return Err(FetchError {
                kind: ErrorKind::Structure,
                message: format!(
                    "响应体过大（{}），超过上限 {}",
                    human_size(len),
                    human_size(MAX_COMPRESSED as u64)
                ),
                http_status: Some(status_code.as_u16()),
            });
        }

        let etag = header_str(&resp, reqwest::header::ETAG);
        let last_modified = header_str(&resp, reqwest::header::LAST_MODIFIED);
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_COMPRESSED {
                        return Err(FetchError {
                            kind: ErrorKind::Structure,
                            message: format!(
                                "响应体超过上限 {}",
                                human_size(MAX_COMPRESSED as u64)
                            ),
                            http_status: Some(status_code.as_u16()),
                        });
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(FetchError {
                        kind: ErrorKind::Network,
                        message: format!("读取响应失败：{e}"),
                        http_status: None,
                    });
                }
            }
        }
        if bytes.is_empty() {
            return Err(FetchError {
                kind: ErrorKind::Empty,
                message: "响应体为空".to_string(),
                http_status: Some(status_code.as_u16()),
            });
        }

        Ok(Download {
            not_modified: false,
            bytes: Some(bytes),
            etag,
            last_modified,
        })
    }

    fn record_success(
        &self,
        url: &str,
        etag: Option<String>,
        last_modified: Option<String>,
        info: &GeoDbInfo,
        now: DateTime<Utc>,
    ) {
        let snapshot = {
            let mut st = self.write_status();
            st.last_attempt_at = Some(now);
            st.last_success_at = Some(now);
            st.last_error = None;
            st.last_error_kind = None;
            st.source_url = Some(url.to_string());
            st.etag = etag;
            st.last_modified = last_modified;
            st.database_type = Some(info.database_type.clone());
            st.build_epoch = Some(info.build_epoch);
            st.ip_version = Some(info.ip_version);
            st.node_count = Some(info.node_count);
            st.size = Some(info.size);
            st.clone()
        };
        self.save_status(&snapshot);
    }

    fn record_not_modified(
        &self,
        url: &str,
        etag: Option<String>,
        last_modified: Option<String>,
        now: DateTime<Utc>,
    ) {
        let snapshot = {
            let mut st = self.write_status();
            st.last_attempt_at = Some(now);
            // 304 确认「远端内容就是当前版本」，因此刷新成功时间，
            // 否则到期判定会一直认为库是旧的，每个 tick 都重发请求。
            st.last_success_at = Some(now);
            st.last_error = None;
            st.last_error_kind = None;
            st.source_url = Some(url.to_string());
            if etag.is_some() {
                st.etag = etag;
            }
            if last_modified.is_some() {
                st.last_modified = last_modified;
            }
            st.clone()
        };
        self.save_status(&snapshot);
    }

    fn record_failure(&self, kind: ErrorKind, message: String, now: DateTime<Utc>) {
        let snapshot = {
            let mut st = self.write_status();
            st.last_attempt_at = Some(now);
            st.last_error = Some(message);
            st.last_error_kind = Some(kind);
            st.clone()
        };
        self.save_status(&snapshot);
    }

    fn write_status(&self) -> std::sync::RwLockWriteGuard<'_, GeoipStatus> {
        match self.status.write() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        }
    }

    fn save_status(&self, status: &GeoipStatus) {
        let text = match serde_yaml_ng::to_string(status) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("序列化 GeoIP 状态失败：{e}");
                return;
            }
        };
        if let Err(e) = atomic_write(&self.status_path, &text) {
            tracing::warn!("写入 GeoIP 状态失败：{e}");
        }
    }
}

/// 纯函数：是否应该发起一次更新检查。
///
/// `geo_available` 表示当前是否有可用库，`db_modified` 是库文件 mtime。
/// 没有库时应尽快下载，但**失败后同样要冷却**，否则每个后台 tick 都会锤一次下载源。
fn is_update_due(
    settings: &GeoipSettings,
    now: DateTime<Utc>,
    status: &GeoipStatus,
    geo_available: bool,
    db_modified: Option<DateTime<Utc>>,
) -> bool {
    if !geo_available {
        return if status.last_error.is_some() {
            failure_retry_due(status, now)
        } else {
            true
        };
    }

    // 自定义地址被改过：无论本地库多新，都应重新下载一次。
    if settings.uses_custom_url() && status.source_url.as_deref() != Some(settings.url.trim()) {
        return true;
    }

    let interval = chrono::Duration::days(i64::from(settings.interval_days_clamped()));
    let last = match (status.last_success_at, db_modified) {
        (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let fresh = last
        .map(|t| now.signed_duration_since(t) < interval)
        .unwrap_or(false);
    if !fresh {
        return true;
    }
    failure_retry_due(status, now)
}

/// 上次失败时，是否已经过了重试冷却。
fn failure_retry_due(status: &GeoipStatus, now: DateTime<Utc>) -> bool {
    if status.last_error.is_none() {
        return false;
    }
    status
        .last_attempt_at
        .map(|a| now.signed_duration_since(a) >= chrono::Duration::seconds(FAILURE_RETRY_SECS))
        .unwrap_or(true)
}

struct DownloadCondition {
    etag: Option<String>,
    last_modified: Option<String>,
}

struct Download {
    not_modified: bool,
    bytes: Option<Vec<u8>>,
    etag: Option<String>,
    last_modified: Option<String>,
}

struct FetchError {
    kind: ErrorKind,
    message: String,
    http_status: Option<u16>,
}

fn header_str(
    resp: &reqwest::Response,
    name: impl reqwest::header::AsHeaderName,
) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 解压下载内容：识别 gzip magic，其余按原始 MMDB 处理。
pub fn decode_payload(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_COMPRESSED {
        bail!(
            "下载内容过大（{}），超过上限 {}",
            human_size(bytes.len() as u64),
            human_size(MAX_COMPRESSED as u64)
        );
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let decoder = GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder
            .take(MAX_DECOMPRESSED as u64 + 1)
            .read_to_end(&mut out)
            .context("gzip 解压失败")?;
        if out.len() > MAX_DECOMPRESSED {
            bail!(
                "解压后的 GeoIP 库过大（超过 {}）",
                human_size(MAX_DECOMPRESSED as u64)
            );
        }
        if out.is_empty() {
            bail!("解压后的 GeoIP 库为空");
        }
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

/// 内置 DB-IP Lite Country 的下载地址（按 UTC 年月生成）。
pub fn builtin_source_url(year: i32, month: u32) -> String {
    format!("https://download.db-ip.com/free/dbip-country-lite-{year:04}-{month:02}.mmdb.gz")
}

/// 当前月份对应的内置地址。
pub fn current_builtin_source_url(now: DateTime<Utc>) -> String {
    builtin_source_url(now.year(), now.month())
}

fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month <= 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

/// 自定义下载地址必须是 http(s)。
pub fn validate_source_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("下载地址不是合法的 URL")?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        scheme => bail!("下载地址必须使用 http:// 或 https://，当前是 {scheme}://"),
    }
}

/// 日志/页面展示用：去掉凭据、查询串与片段，避免把自定义地址里的 token 打出来。
pub fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return "（无法解析的地址）".to_string();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn load_status(path: &Path) -> GeoipStatus {
    let Ok(text) = std::fs::read_to_string(path) else {
        return GeoipStatus::default();
    };
    match serde_yaml_ng::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("解析 GeoIP 状态文件失败，将重新开始记录：{e}");
            GeoipStatus::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn gzip_payload_is_detected_and_decoded() {
        let raw = b"fake mmdb bytes";
        assert_eq!(decode_payload(&gzip(raw)).unwrap(), raw);
    }

    #[test]
    fn plain_payload_passes_through() {
        assert_eq!(decode_payload(b"plain").unwrap(), b"plain");
    }

    #[test]
    fn broken_gzip_is_rejected() {
        let mut broken = vec![0x1f, 0x8b];
        broken.extend_from_slice(b"definitely not gzip");
        assert!(decode_payload(&broken).is_err());
    }

    #[test]
    fn builtin_url_uses_year_month() {
        assert_eq!(
            builtin_source_url(2026, 10),
            "https://download.db-ip.com/free/dbip-country-lite-2026-10.mmdb.gz"
        );
    }

    #[test]
    fn previous_month_handles_january() {
        assert_eq!(previous_month(2026, 1), (2025, 12));
        assert_eq!(previous_month(2026, 10), (2026, 9));
    }

    #[test]
    fn custom_source_url_must_be_http() {
        assert!(validate_source_url("https://example.com/db.mmdb.gz").is_ok());
        assert!(validate_source_url("http://example.com/db.mmdb.gz").is_ok());
        assert!(validate_source_url("ftp://example.com/db.mmdb.gz").is_err());
        assert!(validate_source_url("not a url").is_err());
    }

    #[test]
    fn url_is_redacted_for_display() {
        let redacted = redact_url("https://user:secret@example.com/db.mmdb.gz?license=abc");
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("license"));
        assert!(redacted.contains("example.com"));
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(8 * 1024 * 1024), "8.0 MB");
    }

    // ---------------------------------------------------------- is_update_due

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn settings(interval_days: u32, url: &str) -> GeoipSettings {
        GeoipSettings {
            auto_update: true,
            interval_days,
            url: url.to_string(),
        }
    }

    fn status(last_success: DateTime<Utc>, failure: Option<(DateTime<Utc>, &str)>) -> GeoipStatus {
        let mut s = GeoipStatus {
            last_success_at: Some(last_success),
            ..Default::default()
        };
        if let Some((attempt, msg)) = failure {
            s.last_attempt_at = Some(attempt);
            s.last_error = Some(msg.to_string());
            s.last_error_kind = Some(ErrorKind::Network);
        }
        s
    }

    #[test]
    fn missing_library_is_due_immediately() {
        let s = settings(7, "");
        assert!(is_update_due(
            &s,
            now(),
            &GeoipStatus::default(),
            false,
            None
        ));
    }

    #[test]
    fn missing_library_respects_failure_cooldown() {
        let s = settings(7, "");
        let recent = status(
            now() - chrono::Duration::days(30),
            Some((now() - chrono::Duration::minutes(5), "boom")),
        );
        assert!(!is_update_due(&s, now(), &recent, false, None));

        let old = status(
            now() - chrono::Duration::days(30),
            Some((now() - chrono::Duration::hours(2), "boom")),
        );
        assert!(is_update_due(&s, now(), &old, false, None));
    }

    #[test]
    fn fresh_library_is_not_due() {
        let s = settings(7, "");
        let st = status(now() - chrono::Duration::days(2), None);
        assert!(!is_update_due(&s, now(), &st, true, None));
    }

    #[test]
    fn stale_library_is_due() {
        let s = settings(7, "");
        let st = status(now() - chrono::Duration::days(8), None);
        assert!(is_update_due(&s, now(), &st, true, None));
    }

    #[test]
    fn fresh_library_retries_failed_check_after_cooldown() {
        let s = settings(7, "");
        let recent = status(
            now() - chrono::Duration::days(2),
            Some((now() - chrono::Duration::minutes(10), "boom")),
        );
        assert!(!is_update_due(&s, now(), &recent, true, None));
        let old = status(
            now() - chrono::Duration::days(2),
            Some((now() - chrono::Duration::hours(2), "boom")),
        );
        assert!(is_update_due(&s, now(), &old, true, None));
    }

    #[test]
    fn changed_custom_url_is_due_even_when_fresh() {
        let s = settings(7, "https://example.com/new.mmdb.gz");
        let mut st = status(now() - chrono::Duration::days(1), None);
        st.source_url = Some("https://example.com/old.mmdb.gz".to_string());
        assert!(is_update_due(&s, now(), &st, true, None));

        st.source_url = Some("https://example.com/new.mmdb.gz".to_string());
        assert!(!is_update_due(&s, now(), &st, true, None));
    }

    #[test]
    fn db_file_mtime_can_make_last_success_older() {
        // 状态文件的 success 较旧，但库文件是刚替换的（例如手工放入）→ 不应重复下载。
        let s = settings(7, "");
        let st = status(now() - chrono::Duration::days(30), None);
        assert!(!is_update_due(
            &s,
            now(),
            &st,
            true,
            Some(now() - chrono::Duration::days(1))
        ));
    }
}
