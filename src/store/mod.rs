//! 数据层：单一 `store.yaml` + 内存快照 + 乐观版本 + 原子写 + 备份恢复。
//!
//! 为什么是单文件（AGENTS.md §8.1）：分组重命名会同时改动分组、规则和多个
//! 方案。若分散在多个文件，即使各自原子写，写到一半崩溃仍会留下新旧混杂的数据。
//! 单文件后一次 `rename` 就是全部业务数据的一致提交，备份也天然是一致快照。

pub mod migrate;
pub mod model;

pub use model::*;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// 备份保留份数。
pub const MAX_BACKUPS: usize = 20;

/// slug / id 校验：`^[a-z0-9-]+$`，同时用作 URL 片段与文件名，防止 `../` 穿越。
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Debug, Clone)]
pub struct BackupInfo {
    pub path: PathBuf,
    pub created_at: String,
    pub size: u64,
}

/// 某一类资产在「当前 → 备份」之间的差异。
#[derive(Debug, Clone, Default)]
pub struct DiffSet {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl DiffSet {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// 当前数据与某份备份的差异摘要（恢复前预览用）。
#[derive(Debug, Clone, Default)]
pub struct StoreDiff {
    pub upstreams: DiffSet,
    pub groups: DiffSet,
    pub rules: DiffSet,
    pub aliases: DiffSet,
    pub manual_nodes: DiffSet,
    pub profiles: DiffSet,
    pub settings_changed: bool,
    pub general_changed: bool,
}

impl StoreDiff {
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty()
            && self.groups.is_empty()
            && self.rules.is_empty()
            && self.aliases.is_empty()
            && self.manual_nodes.is_empty()
            && self.profiles.is_empty()
            && !self.settings_changed
            && !self.general_changed
    }
}

fn diff_set<T: Serialize>(
    current: &[T],
    backup: &[T],
    key: impl Fn(&T) -> String,
    value: impl Fn(&T) -> String,
) -> DiffSet {
    let current: BTreeMap<String, String> = current
        .iter()
        .map(|item| (key(item), value(item)))
        .collect();
    let backup: BTreeMap<String, String> =
        backup.iter().map(|item| (key(item), value(item))).collect();

    let mut out = DiffSet::default();
    for (name, v) in &current {
        match backup.get(name) {
            None => out.added.push(name.clone()),
            Some(old) if old != v => out.changed.push(name.clone()),
            Some(_) => {}
        }
    }
    for name in backup.keys() {
        if !current.contains_key(name) {
            out.removed.push(name.clone());
        }
    }
    out
}

fn yaml_value<T: Serialize>(value: &T) -> String {
    serde_yaml_ng::to_string(value).unwrap_or_default()
}

impl Store {
    /// 计算「当前快照 → 备份」的差异。备份里新增的对象是恢复后会出现的。
    pub fn diff(&self, backup: &Store) -> StoreDiff {
        StoreDiff {
            upstreams: diff_set(
                &backup.upstreams,
                &self.upstreams,
                |u| u.id.clone(),
                yaml_value,
            ),
            groups: diff_set(&backup.groups, &self.groups, |g| g.name.clone(), yaml_value),
            rules: diff_set(&backup.rules, &self.rules, |r| r.id.clone(), yaml_value),
            aliases: diff_set(
                &backup.aliases,
                &self.aliases,
                |a| a.region.clone(),
                |a| a.aliases.join("|"),
            ),
            manual_nodes: diff_set(
                &backup.manual_nodes,
                &self.manual_nodes,
                |n| n.id.clone(),
                |n| n.link.clone(),
            ),
            profiles: diff_set(
                &backup.profiles,
                &self.profiles,
                |p| p.slug.clone(),
                yaml_value,
            ),
            settings_changed: yaml_value(&self.settings) != yaml_value(&backup.settings),
            general_changed: yaml_value(&self.general) != yaml_value(&backup.general),
        }
    }
}

#[derive(Clone)]
pub struct StoreHandle {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    state: RwLock<Arc<Store>>,
}

impl StoreHandle {
    /// 打开（或首次初始化）数据目录。
    pub fn new(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.into();
        for d in [
            data_dir.clone(),
            data_dir.join("cache"),
            data_dir.join("cache").join("rendered"),
            data_dir.join("geoip"),
            data_dir.join("backups"),
        ] {
            std::fs::create_dir_all(&d)
                .with_context(|| format!("创建目录失败：{}", d.display()))?;
        }

        let store_path = data_dir.join("store.yaml");
        let store = if store_path.exists() {
            let text = std::fs::read_to_string(&store_path)
                .with_context(|| format!("读取失败：{}", store_path.display()))?;
            warn_removed_apps(&text, &store_path);
            let parsed: Store = serde_yaml_ng::from_str(&text)
                .with_context(|| format!("解析失败：{}", store_path.display()))?;
            let parsed = migrate::migrate(parsed)?;
            crate::validate::validate_structure(&parsed)
                .with_context(|| format!("数据校验失败：{}", store_path.display()))?;
            parsed
        } else {
            let fresh = Store::default();
            atomic_write(&store_path, &serde_yaml_ng::to_string(&fresh)?)?;
            tracing::info!("已初始化数据文件 {}", store_path.display());
            fresh
        };

        Ok(Self {
            inner: Arc::new(Inner {
                data_dir,
                state: RwLock::new(Arc::new(store)),
            }),
        })
    }

    // ---------------------------------------------------------- 路径

    pub fn store_path(&self) -> PathBuf {
        self.inner.data_dir.join("store.yaml")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.inner.data_dir.join("cache")
    }
    pub fn rendered_dir(&self) -> PathBuf {
        self.inner.data_dir.join("cache").join("rendered")
    }
    pub fn geoip_dir(&self) -> PathBuf {
        self.inner.data_dir.join("geoip")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.inner.data_dir.join("backups")
    }

    // ---------------------------------------------------------- 读

    /// 取当前快照。导出全程持有同一份快照，保证一致性。
    pub async fn snapshot(&self) -> Arc<Store> {
        self.inner.state.read().await.clone()
    }

    // ---------------------------------------------------------- 写

    /// 在写锁内完成「读快照 → 计算新状态 → 校验 → 原子写盘 → 替换快照」。
    ///
    /// `expected_revision` 非空时做乐观并发校验，防止丢失更新（§8.2）。
    ///
    /// 注：文件很小（单人自用），此处使用同步文件 IO；锁内不做任何网络请求。
    pub async fn update<T>(
        &self,
        expected_revision: Option<u64>,
        f: impl FnOnce(&mut Store) -> Result<T>,
    ) -> Result<(T, u64)> {
        let mut guard = self.inner.state.write().await;
        let current = guard.clone();

        if let Some(exp) = expected_revision
            && exp != current.revision
        {
            bail!(
                "数据已被其他操作修改（当前版本 {}，提交版本 {}），请刷新页面后重试",
                current.revision,
                exp
            );
        }

        let mut draft = (*current).clone();
        let out = f(&mut draft)?;
        draft.format_version = FORMAT_VERSION;
        crate::validate::validate_structure(&draft)?;

        draft.revision = current.revision.saturating_add(1);
        let text = serde_yaml_ng::to_string(&draft).context("序列化数据失败")?;

        self.backup_current()?;
        atomic_write(&self.store_path(), &text)?;

        let rev = draft.revision;
        *guard = Arc::new(draft);
        Ok((out, rev))
    }

    /// 备份当前 store.yaml（若存在），并裁剪历史。
    fn backup_current(&self) -> Result<()> {
        let store_path = self.store_path();
        if !store_path.exists() {
            return Ok(());
        }
        let ts = Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let dest = self.backups_dir().join(format!("store.{ts}.yaml"));
        std::fs::copy(&store_path, &dest)
            .with_context(|| format!("备份失败：{}", dest.display()))?;
        self.prune_backups()?;
        Ok(())
    }

    fn prune_backups(&self) -> Result<()> {
        let mut backups = self.list_backups()?;
        if backups.len() <= MAX_BACKUPS {
            return Ok(());
        }
        // 文件名含可排序时间戳，升序即最旧在前。
        backups.sort_by(|a, b| a.path.cmp(&b.path));
        let remove_count = backups.len() - MAX_BACKUPS;
        for b in backups.into_iter().take(remove_count) {
            let _ = std::fs::remove_file(&b.path);
        }
        Ok(())
    }

    pub fn list_backups(&self) -> Result<Vec<BackupInfo>> {
        let dir = self.backups_dir();
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !name.starts_with("store.") || !name.ends_with(".yaml") {
                continue;
            }
            let meta = entry.metadata()?;
            let created_at = name
                .trim_start_matches("store.")
                .trim_end_matches(".yaml")
                .to_string();
            out.push(BackupInfo {
                path,
                created_at,
                size: meta.len(),
            });
        }
        out.sort_by(|a, b| b.path.cmp(&a.path)); // 最新在前
        Ok(out)
    }

    pub fn read_backup(&self, path: &Path) -> Result<Store> {
        // 只允许读取 backups 目录下的文件，避免路径穿越。
        let canonical_dir = self.backups_dir().canonicalize()?;
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(&canonical_dir) {
            bail!("非法的备份路径");
        }
        let text = std::fs::read_to_string(&canonical)?;
        let store: Store = serde_yaml_ng::from_str(&text)?;
        migrate::migrate(store)
    }

    /// 从备份恢复。恢复前会先备份当前状态（`update` 内部完成）。
    pub async fn restore(&self, path: &Path, expected_revision: Option<u64>) -> Result<u64> {
        let restored = self.read_backup(path)?;
        let (_, rev) = self
            .update(expected_revision, |draft| {
                let rev = draft.revision;
                *draft = restored.clone();
                draft.revision = rev; // revision 由 update 统一递增
                Ok(())
            })
            .await?;
        Ok(rev)
    }
}

/// 旧数据文件里残留的「应用」定义（§12 v15 移除了应用模块）。
///
/// serde 默认忽略未知字段，应用定义会**悄无声息**地消失。这里显式告警，
/// 让用户知道要去「路由规则」页把它们的域名规则补回来。
fn warn_removed_apps(text: &str, path: &Path) {
    let count = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text)
        .ok()
        .and_then(|v| {
            v.as_mapping()
                .and_then(|m| mget(m, "apps"))
                .and_then(|a| a.as_sequence())
                .map(|s| s.len())
        })
        .unwrap_or(0);
    if count > 0 {
        tracing::warn!(
            "{} 里还有 {count} 个「应用」定义，但应用模块已被移除，这些定义会被忽略；请把它们的域名规则改到「路由规则」页维护",
            path.display()
        );
    }
}

/// 原子写：同目录临时文件 → fsync → rename。
///
/// Rust 的 `std::fs::rename` 在 Windows 上使用 `MOVEFILE_REPLACE_EXISTING`，
/// 可以覆盖已存在的目标文件。
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    atomic_write_bytes(path, contents.as_bytes())
}

/// 与 [`atomic_write`] 相同，但接受任意字节（GeoIP 的 `.mmdb` 是二进制）。
pub fn atomic_write_bytes(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;

    let dir = path.parent().context("路径没有父目录")?;
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("路径没有文件名")?;
    let tmp = dir.join(format!(".{file_name}.tmp"));

    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("创建临时文件失败：{}", tmp.display()))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("替换文件失败：{}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_diff_reports_added_removed_changed() {
        let backup = Store::default();
        let mut current = backup.clone();
        current.manual_nodes.push(ManualNode {
            id: "n-test".into(),
            link: "socks5://127.0.0.1:7891#测试".into(),
        });
        current
            .groups
            .iter_mut()
            .find(|g| g.name == "Google服务")
            .unwrap()
            .manual_nodes
            .push("DIRECT".into());
        current.rules.retain(|r| r.id != "r-final");

        let diff = current.diff(&backup);
        assert!(!diff.is_empty());
        assert!(diff.manual_nodes.removed.contains(&"n-test".to_string()));
        assert!(diff.groups.changed.contains(&"Google服务".to_string()));
        assert!(diff.rules.added.contains(&"r-final".to_string()));
    }
}
