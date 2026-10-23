//! 数据层：单一 `store.yaml` + 内存快照 + 乐观版本 + 原子写 + 备份恢复。
//!
//! 为什么是单文件（AGENTS.md §8.1）：分组重命名会同时改动分组、规则和多个
//! 方案。若分散在多个文件，即使各自原子写，写到一半崩溃仍会留下新旧混杂的数据。
//! 单文件后一次 `rename` 就是全部业务数据的一致提交，备份也天然是一致快照。

pub mod migrate;
pub mod model;

pub use model::*;

use crate::storage::Storage;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
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
    /// 备份对象名（`store.<ts>.yaml`），同时用作存储 key 的最后一段。
    pub name: String,
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
    storage: Arc<Storage>,
    state: RwLock<Arc<Store>>,
}

impl StoreHandle {
    /// 打开（或首次初始化）业务数据。native 传 `data/` 目录的 Storage，
    /// Workers 传 DO/R2 后端的 Storage；两者语义一致。
    pub async fn open(storage: Arc<Storage>) -> Result<Self> {
        storage.ensure_dirs()?;
        let store = match storage.get_string("store.yaml").await? {
            Some(text) => {
                warn_removed_apps(&text, "store.yaml");
                let parsed: Store =
                    serde_yaml_ng::from_str(&text).context("解析 store.yaml 失败")?;
                let parsed = migrate::migrate(parsed)?;
                crate::validate::validate_structure(&parsed).context("数据校验失败：store.yaml")?;
                parsed
            }
            None => {
                let fresh = Store::default();
                storage
                    .put_string("store.yaml", &serde_yaml_ng::to_string(&fresh)?)
                    .await?;
                tracing::info!("已初始化数据文件 store.yaml");
                fresh
            }
        };

        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                state: RwLock::new(Arc::new(store)),
            }),
        })
    }

    /// 使用已有快照构造（Workers 侧从 DO 读 YAML 后直接给）。
    pub fn from_store(storage: Arc<Storage>, store: Store) -> Self {
        Self {
            inner: Arc::new(Inner {
                storage,
                state: RwLock::new(Arc::new(store)),
            }),
        }
    }

    // ---------------------------------------------------------- 存储

    pub fn storage(&self) -> Arc<Storage> {
        self.inner.storage.clone()
    }

    /// native 专用：数据目录根（GeoIP 等仍按目录访问的组件使用）。
    #[cfg(not(target_arch = "wasm32"))]
    pub fn data_dir(&self) -> &std::path::Path {
        self.inner.storage.root()
    }

    // ---------------------------------------------------------- 读

    /// 取当前快照。导出全程持有同一份快照，保证一致性。
    pub async fn snapshot(&self) -> Arc<Store> {
        self.inner.state.read().await.clone()
    }

    /// 缓存提交期间固定业务版本；调用者不得持有此锁进行网络请求。
    pub async fn cache_snapshot(&self) -> tokio::sync::RwLockReadGuard<'_, Arc<Store>> {
        self.inner.state.read().await
    }

    /// 在短暂读锁内检查当前业务数据并执行本地提交；回调不得进行网络请求。
    pub async fn with_current<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let guard = self.inner.state.read().await;
        f(&guard)
    }

    // ---------------------------------------------------------- 写

    /// 在写锁内完成「读快照 → 计算新状态 → 校验 → 原子提交 → 替换快照」。
    ///
    /// `expected_revision` 非空时做乐观并发校验，防止丢失更新（§8.2）。
    ///
    /// 注：native 是本地文件写入；Workers 上 `Storage::put` 会经 DO 串行化，
    /// 两者都保证「一次 put = 一次提交」，因此锁内不做其他网络请求。
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

        self.backup_current().await?;
        self.inner
            .storage
            .put_string("store.yaml", &text)
            .await
            .context("写入 store.yaml 失败")?;

        let rev = draft.revision;
        *guard = Arc::new(draft);
        Ok((out, rev))
    }

    /// 备份当前 store.yaml（若存在），并裁剪历史。
    async fn backup_current(&self) -> Result<()> {
        let Some(current) = self.inner.storage.get_string("store.yaml").await? else {
            return Ok(());
        };
        let ts = Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let key = format!("backups/store.{ts}.yaml");
        self.inner
            .storage
            .put_string(&key, &current)
            .await
            .with_context(|| format!("备份失败：{key}"))?;
        self.prune_backups().await?;
        Ok(())
    }

    async fn prune_backups(&self) -> Result<()> {
        let mut backups = self.list_backups().await?;
        if backups.len() <= MAX_BACKUPS {
            return Ok(());
        }
        // 文件名含可排序时间戳，升序即最旧在前。
        backups.sort_by(|a, b| a.name.cmp(&b.name));
        let remove_count = backups.len() - MAX_BACKUPS;
        for b in backups.into_iter().take(remove_count) {
            let _ = self
                .inner
                .storage
                .delete(&format!("backups/{}", b.name))
                .await;
        }
        Ok(())
    }

    pub async fn list_backups(&self) -> Result<Vec<BackupInfo>> {
        let objects = self.inner.storage.list("backups/").await?;
        let mut out = Vec::new();
        for object in objects {
            let name = object.key.rsplit('/').next().unwrap_or("").to_string();
            if !name.starts_with("store.") || !name.ends_with(".yaml") {
                continue;
            }
            let created_at = name
                .trim_start_matches("store.")
                .trim_end_matches(".yaml")
                .to_string();
            out.push(BackupInfo {
                name,
                created_at,
                size: object.size,
            });
        }
        out.sort_by(|a, b| b.name.cmp(&a.name)); // 最新在前
        Ok(out)
    }

    /// `name` 是 [`BackupInfo::name`]（`store.<ts>.yaml`），不接受任意路径。
    pub async fn read_backup(&self, name: &str) -> Result<Store> {
        if name.contains('/') || name.contains('\\') || !name.starts_with("store.") {
            bail!("非法的备份名");
        }
        let key = format!("backups/{name}");
        let text = self
            .inner
            .storage
            .get_string(&key)
            .await?
            .with_context(|| format!("备份不存在：{name}"))?;
        let store: Store = serde_yaml_ng::from_str(&text)?;
        migrate::migrate(store)
    }

    /// 从备份恢复。恢复前会先备份当前状态（`update` 内部完成）。
    pub async fn restore(&self, name: &str, expected_revision: Option<u64>) -> Result<u64> {
        let restored = self.read_backup(name).await?;
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
fn warn_removed_apps(text: &str, name: &str) {
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
            "{name} 里还有 {count} 个「应用」定义，但应用模块已被移除，这些定义会被忽略；请把它们的域名规则改到「路由规则」页维护"
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
        let mut f = std::fs::File::create(&tmp).map_err(|e| {
            // 附带系统错误码：仅报路径会把 ACCESS_DENIED / SHARING_VIOLATION 等真实原因藏起来。
            anyhow::anyhow!("创建临时文件失败：{}：{e}", tmp.display())
        })?;
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
