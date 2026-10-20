//! GeoIP 查询（IO 层，AGENTS.md §3.5）。
//!
//! 本模块负责**准备数据**：解析域名、查 MMDB。真正的地区判定在 `region` 模块，
//! 那部分是纯函数，便于单测。
//!
//! ⚠️ **中转与 CDN 失真**：GeoIP 判的是 `server` 入口 IP 的归属地，不是节点实际
//! 落地地区。机场用香港入口中转、实际落地日本时，GeoIP 会确定地判成香港。因此默认
//! `region_source = name_first`，GeoIP 只作兜底。
//!
//! 库文件可以由通用设置页自动下载安装（`geoip_update`），安装后**立即热加载**，
//! 不需要重启进程。

use crate::region::RegionInputs;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

/// 一个已加载的 GeoIP 库的描述（供页面与日志展示）。
#[derive(Debug, Clone)]
pub struct GeoDbInfo {
    pub path: PathBuf,
    /// MMDB 元数据里的数据库类型，如 `DBIP-Country-Lite` / `GeoLite2-Country`。
    pub database_type: String,
    /// 库的构建时间（Unix 秒）。
    pub build_epoch: u64,
    pub ip_version: u16,
    /// 搜索树节点数（不是国家数）。
    pub node_count: u32,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}

/// MMDB 元数据（校验阶段使用，不含文件信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmdbMeta {
    pub database_type: String,
    pub build_epoch: u64,
    pub ip_version: u16,
    pub node_count: u32,
}

#[derive(Clone)]
struct Loaded {
    reader: Arc<maxminddb::Reader<Vec<u8>>>,
    path: PathBuf,
}

pub struct GeoResolver {
    loaded: RwLock<Option<Loaded>>,
    dns_cache: Mutex<HashMap<String, Vec<IpAddr>>>,
}

impl GeoResolver {
    /// 从目录中加载 `.mmdb`。库缺失时**不报错**，只是整体禁用 GeoIP。
    ///
    /// 目录中有多个库时选 **mtime 最新**的那个：自动下载写入的
    /// `geoip-auto.mmdb` 因此会覆盖用户早先手工放入的旧库，重启后行为与热加载一致。
    pub fn load(dir: &Path) -> Result<Self> {
        let loaded = match newest_db_file(dir)? {
            Some(path) => {
                let loaded = Loaded::open(&path)?;
                tracing::info!(
                    "已加载 GeoIP 库：{}（{}，构建于 {}）",
                    path.display(),
                    loaded.reader.metadata.database_type,
                    format_epoch(loaded.reader.metadata.build_epoch)
                );
                Some(loaded)
            }
            None => {
                tracing::warn!(
                    "未找到 GeoIP 库（{} 下无 .mmdb），地区判定将仅使用节点名",
                    dir.display()
                );
                None
            }
        };

        Ok(Self {
            loaded: RwLock::new(loaded),
            dns_cache: Mutex::new(HashMap::new()),
        })
    }

    /// 原子安装一段 MMDB 字节并热加载。
    ///
    /// **先校验再落盘**：不是合法 MMDB 时不会替换现有库，也不会留下半截文件。
    pub fn install_bytes(&self, bytes: Vec<u8>, path: &Path) -> Result<GeoDbInfo> {
        let meta = inspect_mmdb(&bytes)?;
        crate::store::atomic_write_bytes(path, &bytes)?;
        let reader = maxminddb::Reader::from_source(bytes)
            .with_context(|| format!("加载 GeoIP 库失败：{}", path.display()))?;
        let loaded = Loaded {
            reader: Arc::new(reader),
            path: path.to_path_buf(),
        };
        let info = self.info_of(&loaded);
        self.set_loaded(Some(loaded));
        debug_assert_eq!(info.database_type, meta.database_type);
        Ok(info)
    }

    pub fn is_available(&self) -> bool {
        self.reader().is_some()
    }

    /// 当前库文件路径。
    pub fn db_path(&self) -> Option<PathBuf> {
        self.loaded().map(|l| l.path)
    }

    /// 当前库的文件信息（大小、mtime、元数据）。
    pub fn db_info(&self) -> Option<GeoDbInfo> {
        self.loaded().map(|l| self.info_of(&l))
    }

    fn info_of(&self, loaded: &Loaded) -> GeoDbInfo {
        let fs = std::fs::metadata(&loaded.path).ok();
        let meta = &loaded.reader.metadata;
        GeoDbInfo {
            path: loaded.path.clone(),
            database_type: meta.database_type.clone(),
            build_epoch: meta.build_epoch,
            ip_version: meta.ip_version,
            node_count: meta.node_count,
            size: fs.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: fs
                .and_then(|m| m.modified().ok())
                .map(DateTime::<Utc>::from),
        }
    }

    fn loaded(&self) -> Option<Loaded> {
        match self.loaded.read() {
            Ok(g) => g.clone(),
            Err(e) => e.into_inner().clone(),
        }
    }

    fn set_loaded(&self, value: Option<Loaded>) {
        match self.loaded.write() {
            Ok(mut g) => *g = value,
            Err(e) => *e.into_inner() = value,
        }
    }

    fn reader(&self) -> Option<Arc<maxminddb::Reader<Vec<u8>>>> {
        self.loaded().map(|l| l.reader)
    }

    fn lookup_country(&self, ip: IpAddr) -> Option<String> {
        let reader = self.reader()?;
        let rec: maxminddb::geoip2::Country = reader.lookup(ip).ok()??;
        rec.country?.iso_code.map(|s| s.to_string())
    }

    /// 解析 `server` 为 IP 列表（域名走 DNS，结果带缓存）。
    async fn resolve(&self, server: &str) -> Vec<IpAddr> {
        let server = server.trim();
        if server.is_empty() {
            return Vec::new();
        }
        if let Ok(ip) = server.parse::<IpAddr>() {
            return vec![ip];
        }

        {
            let cache = self.dns_cache.lock().await;
            if let Some(v) = cache.get(server) {
                return v.clone();
            }
        }

        let ips: Vec<IpAddr> = match tokio::net::lookup_host((server, 0)).await {
            Ok(iter) => iter.map(|s| s.ip()).collect(),
            Err(e) => {
                tracing::debug!("解析 {server} 失败：{e}");
                Vec::new()
            }
        };

        self.dns_cache
            .lock()
            .await
            .insert(server.to_string(), ips.clone());
        ips
    }

    /// 取 `server` 对应的国家码。
    ///
    /// 一个域名可能解析到多个国家的 IP：按出现次数取最多者；并列时按 IP 排序取首个，
    /// 并由调用方记录歧义（AGENTS.md §3.5）。
    pub async fn countries_for(&self, server: &str) -> Vec<String> {
        if !self.is_available() {
            return Vec::new();
        }
        let ips = self.resolve(server).await;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for ip in ips {
            if is_private(&ip) {
                continue;
            }
            if let Some(c) = self.lookup_country(ip) {
                *counts.entry(c).or_insert(0) += 1;
            }
        }
        if counts.is_empty() {
            return Vec::new();
        }
        let max = counts.values().copied().max().unwrap_or(0);
        let mut winners: Vec<String> = counts
            .into_iter()
            .filter(|(_, c)| *c == max)
            .map(|(k, _)| k)
            .collect();
        winners.sort();
        winners
    }

    /// 为一个节点准备 [`RegionInputs`]。
    pub async fn region_inputs(&self, name: &str, server: &str) -> RegionInputs {
        RegionInputs {
            node_name: name.to_string(),
            server: server.to_string(),
            geo_countries: self.countries_for(server).await,
        }
    }
}

impl Loaded {
    fn open(path: &Path) -> Result<Self> {
        let reader = maxminddb::Reader::open_readfile(path)
            .with_context(|| format!("加载 GeoIP 库失败：{}", path.display()))?;
        Ok(Self {
            reader: Arc::new(reader),
            path: path.to_path_buf(),
        })
    }
}

/// 校验一段字节是否是可用的 MMDB，并返回元数据。
///
/// 下载流程在写盘前调用它，避免把错误页、HTML 或截断文件装成「数据库」。
pub fn inspect_mmdb(bytes: &[u8]) -> Result<MmdbMeta> {
    if bytes.is_empty() {
        bail!("GeoIP 库内容为空");
    }
    let reader = maxminddb::Reader::from_source(bytes).context("不是有效的 MMDB 文件")?;
    let meta = &reader.metadata;
    if meta.node_count == 0 {
        bail!("MMDB 元数据异常：node_count 为 0");
    }
    if meta.ip_version != 4 && meta.ip_version != 6 {
        bail!("MMDB 元数据异常：不支持的 IP 版本 {}", meta.ip_version);
    }
    if meta.database_type.trim().is_empty() {
        bail!("MMDB 元数据异常：database_type 为空");
    }
    Ok(MmdbMeta {
        database_type: meta.database_type.clone(),
        build_epoch: meta.build_epoch,
        ip_version: meta.ip_version,
        node_count: meta.node_count,
    })
}

/// 目录中 mtime 最新的 `.mmdb` 文件；忽略原子写临时文件（`.` 开头）。
pub fn newest_db_file(dir: &Path) -> Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    let mut candidates: Vec<(Option<std::time::SystemTime>, PathBuf)> = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("读取 GeoIP 目录失败：{}", dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("mmdb") {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified()).ok();
        candidates.push((modified, path));
    }
    // 新的在前；同一时刻按路径排序，保证结果稳定。
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(candidates.into_iter().next().map(|(_, p)| p))
}

/// 把 Unix 秒格式化为 UTC 日期（页面与日志用）。
pub fn format_epoch(epoch: u64) -> String {
    let secs = i64::try_from(epoch).unwrap_or(i64::MAX);
    DateTime::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "未知".to_string())
}

fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rw-demo-geoip-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn private_addresses_are_skipped() {
        assert!(is_private(&"127.0.0.1".parse().unwrap()));
        assert!(is_private(&"10.0.0.1".parse().unwrap()));
        assert!(is_private(&"192.168.1.1".parse().unwrap()));
        assert!(is_private(&"172.16.0.1".parse().unwrap()));
        assert!(!is_private(&"1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn missing_library_degrades_gracefully() {
        let tmp = temp_dir("empty");
        let r = GeoResolver::load(&tmp).unwrap();
        assert!(!r.is_available());
        assert!(r.db_path().is_none());
    }

    #[tokio::test]
    async fn no_library_returns_no_countries() {
        let tmp = temp_dir("empty2");
        let r = GeoResolver::load(&tmp).unwrap();
        assert!(r.countries_for("1.2.3.4").await.is_empty());
    }

    #[test]
    fn newest_mmdb_wins_and_temp_files_are_ignored() {
        let tmp = temp_dir("newest");
        std::fs::write(tmp.join("old.mmdb"), b"old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(tmp.join(".new.mmdb.tmp"), b"tmp").unwrap();
        std::fs::write(tmp.join("newest.mmdb"), b"new").unwrap();
        std::fs::write(tmp.join("notes.txt"), b"x").unwrap();

        let found = newest_db_file(&tmp).unwrap().unwrap();
        assert_eq!(found.file_name().unwrap(), "newest.mmdb");
    }

    #[test]
    fn invalid_bytes_are_rejected_by_inspection() {
        assert!(inspect_mmdb(b"").is_err());
        assert!(inspect_mmdb(b"<html>not a database</html>").is_err());
    }

    #[test]
    fn invalid_install_does_not_replace_loaded_library() {
        let tmp = temp_dir("invalid-install");
        let r = GeoResolver::load(&tmp).unwrap();
        let err = r.install_bytes(b"not mmdb".to_vec(), &tmp.join("bad.mmdb"));
        assert!(err.is_err());
        assert!(!r.is_available());
        assert!(!tmp.join("bad.mmdb").exists(), "校验失败不得落盘");
    }

    #[test]
    fn epoch_formatting() {
        assert_eq!(format_epoch(0), "1970-01-01");
        assert_eq!(format_epoch(u64::MAX), format_epoch(u64::MAX));
    }
}
