//! Workers 侧 GeoIP：MMDB 从对象存储加载；无 DNS API，域名节点只按名字判定。

use crate::region::RegionInputs;
use crate::storage::Storage;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use maxminddb::Reader;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::RwLock;

/// 自动下载库的固定 key。
pub const AUTO_DB_KEY: &str = "geoip/geoip-auto.mmdb";

#[derive(Debug, Clone)]
pub struct GeoDbInfo {
    pub path: PathBuf,
    pub database_type: String,
    pub build_epoch: u64,
    pub ip_version: u16,
    pub node_count: u32,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmdbMeta {
    pub database_type: String,
    pub build_epoch: u64,
    pub ip_version: u16,
    pub node_count: u32,
}

struct Loaded {
    reader: Reader<Vec<u8>>,
    size: u64,
}

pub struct GeoResolver {
    loaded: RwLock<Option<Loaded>>,
}

impl GeoResolver {
    /// 从对象存储加载；库缺失时**不报错**，只禁用 GeoIP（§3.5 降级）。
    pub async fn load(storage: &Storage) -> Result<Self> {
        let loaded = match storage.get(AUTO_DB_KEY).await? {
            Some(bytes) => {
                let size = bytes.len() as u64;
                let reader = Reader::from_source(bytes).context("解析 GeoIP 库失败")?;
                tracing::info!(
                    "已加载 GeoIP 库：{}（{} 字节）",
                    reader.metadata.database_type,
                    size
                );
                Some(Loaded { reader, size })
            }
            None => {
                tracing::warn!("对象存储中没有 GeoIP 库，地区判定仅使用节点名");
                None
            }
        };
        Ok(Self {
            loaded: RwLock::new(loaded),
        })
    }

    pub fn is_available(&self) -> bool {
        self.loaded.read().map(|g| g.is_some()).unwrap_or(false)
    }

    pub fn db_info(&self) -> Option<GeoDbInfo> {
        let guard = self.loaded.read().ok()?;
        let loaded = guard.as_ref()?;
        let meta = &loaded.reader.metadata;
        Some(GeoDbInfo {
            path: PathBuf::from(AUTO_DB_KEY),
            database_type: meta.database_type.to_string(),
            build_epoch: meta.build_epoch,
            ip_version: meta.ip_version,
            node_count: meta.node_count,
            size: loaded.size,
            modified: None,
        })
    }

    pub fn db_path(&self) -> Option<PathBuf> {
        Some(PathBuf::from(AUTO_DB_KEY))
    }

    /// 安装新库（下载校验通过后调用）。
    pub fn install_bytes(&self, bytes: Vec<u8>) -> Result<GeoDbInfo> {
        inspect_mmdb(&bytes)?;
        let size = bytes.len() as u64;
        let reader = Reader::from_source(bytes).context("加载 GeoIP 库失败")?;
        let info = {
            let meta = &reader.metadata;
            GeoDbInfo {
                path: PathBuf::from(AUTO_DB_KEY),
                database_type: meta.database_type.to_string(),
                build_epoch: meta.build_epoch,
                ip_version: meta.ip_version,
                node_count: meta.node_count,
                size,
                modified: None,
            }
        };
        let mut guard = self
            .loaded
            .write()
            .map_err(|_| anyhow::anyhow!("GeoIP 锁被污染"))?;
        *guard = Some(Loaded { reader, size });
        Ok(info)
    }

    pub async fn countries_for(&self, server: &str) -> Vec<String> {
        let Ok(ip) = server.parse::<IpAddr>() else {
            // Workers 没有 DNS API；域名节点只能靠名字判定。
            return Vec::new();
        };
        let guard = match self.loaded.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let Some(loaded) = guard.as_ref() else {
            return Vec::new();
        };
        match loaded.reader.lookup::<maxminddb::geoip2::Country>(ip) {
            Ok(Some(country)) => country
                .country
                .and_then(|c| c.iso_code)
                .map(|code| vec![code.to_string()])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    pub async fn region_inputs(&self, name: &str, server: &str) -> RegionInputs {
        RegionInputs {
            node_name: name.to_string(),
            server: server.to_string(),
            geo_countries: self.countries_for(server).await,
        }
    }
}

/// 校验一段字节是否是合法 MMDB，并返回元数据。
pub fn inspect_mmdb(bytes: &[u8]) -> Result<MmdbMeta> {
    let reader = Reader::from_source(bytes.to_vec()).context("不是有效的 MMDB 文件")?;
    let meta = &reader.metadata;
    Ok(MmdbMeta {
        database_type: meta.database_type.to_string(),
        build_epoch: meta.build_epoch,
        ip_version: meta.ip_version,
        node_count: meta.node_count,
    })
}

pub fn format_epoch(epoch: u64) -> String {
    DateTime::<Utc>::from_timestamp(epoch as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}
