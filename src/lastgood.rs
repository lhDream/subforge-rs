//! last_good 渲染结果缓存（AGENTS.md §6.3）。
//!
//! 采用 `last_good` 策略时，导出失败会返回最近一次成功生成的**完整**配置。
//! 必须在响应头与页面显示生成时间与回退原因，避免用户误以为最新修改已生效。

use crate::store::atomic_write;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastGood {
    pub generated_at: DateTime<Utc>,
    /// 回退原因（本次导出为何失败）。
    pub reason: String,
    pub yaml: String,
}

pub fn path(dir: &Path, slug: &str) -> Result<PathBuf> {
    if !crate::store::valid_slug(slug) {
        bail!("非法的方案 slug：{slug}");
    }
    Ok(dir.join(format!("{slug}.yaml")))
}

pub fn load(dir: &Path, slug: &str) -> Result<Option<LastGood>> {
    let p = path(dir, slug)?;
    if !p.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("读取 last_good 失败：{}", p.display()))?;
    Ok(Some(serde_yaml_ng::from_str(&text)?))
}

pub fn save(dir: &Path, slug: &str, value: &LastGood) -> Result<()> {
    let p = path(dir, slug)?;
    atomic_write(&p, &serde_yaml_ng::to_string(value)?)
}

pub fn remove(dir: &Path, slug: &str) -> Result<()> {
    let p = path(dir, slug)?;
    if p.exists() {
        std::fs::remove_file(&p)?;
    }
    Ok(())
}
