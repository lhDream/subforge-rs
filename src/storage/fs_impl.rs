//! native 实现：`data/` 下的文件系统。

use super::{ObjectMeta, valid_key};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
}

impl Storage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 打开（或首次初始化）数据目录。等价于原 `StoreHandle::new` 的前半段。
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.root.join("cache"),
            self.root.join("cache").join("rendered"),
            self.root.join("geoip"),
            self.root.join("backups"),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("创建目录失败：{}", dir.display()))?;
        }
        Ok(())
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        if !valid_key(key) {
            bail!("非法的存储 key：{key}");
        }
        Ok(self.root.join(key))
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path(key)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("读取失败：{}", path.display())),
        }
    }

    pub async fn get_string(&self, key: &str) -> Result<Option<String>> {
        match self.get(key).await? {
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .with_context(|| format!("内容不是合法 UTF-8：{key}")),
            None => Ok(None),
        }
    }

    pub async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path(key)?;
        crate::store::atomic_write_bytes(&path, bytes)
    }

    pub async fn put_string(&self, key: &str, text: &str) -> Result<()> {
        self.put(key, text.as_bytes()).await
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("删除失败：{}", path.display())),
        }
    }

    /// 列出 `prefix` 开头的 key。`prefix` 为空表示全部。
    pub async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let mut out = Vec::new();
        let base = self.root.clone();
        if !base.exists() {
            return Ok(out);
        }
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };
                let path = entry.path();
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(rel) = path.strip_prefix(&base) else {
                    continue;
                };
                let key = rel.to_string_lossy().replace('\\', "/");
                if !key.starts_with(prefix) {
                    continue;
                }
                let modified: Option<DateTime<Utc>> =
                    meta.modified().ok().map(DateTime::<Utc>::from);
                out.push(ObjectMeta {
                    key,
                    size: meta.len(),
                    modified,
                });
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// 当前时间（native 用系统时钟；wasm 见对应实现）。
    pub fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
