//! last_good 渲染结果缓存（AGENTS.md §6.3）。
//!
//! 采用 `last_good` 策略时，导出失败会返回最近一次成功生成的**完整**配置。
//! 必须在响应头与页面显示生成时间与回退原因，避免用户误以为最新修改已生效。

use crate::storage::Storage;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastGood {
    pub generated_at: DateTime<Utc>,
    /// 回退原因（本次导出为何失败）。
    pub reason: String,
    pub yaml: String,
}

/// 渲染结果 key：`cache/rendered/<slug>.yaml`。
pub fn key(slug: &str) -> Result<String> {
    if !crate::store::valid_slug(slug) {
        bail!("非法的方案 slug：{slug}");
    }
    Ok(format!("cache/rendered/{slug}.yaml"))
}

pub async fn load(storage: &Storage, slug: &str) -> Result<Option<LastGood>> {
    let key = key(slug)?;
    let Some(text) = storage
        .get_string(&key)
        .await
        .with_context(|| format!("读取 last_good 失败：{key}"))?
    else {
        return Ok(None);
    };
    Ok(Some(serde_yaml_ng::from_str(&text)?))
}

pub async fn save(storage: &Storage, slug: &str, value: &LastGood) -> Result<()> {
    let key = key(slug)?;
    let text = serde_yaml_ng::to_string(value)?;
    storage
        .put_string(&key, &text)
        .await
        .with_context(|| format!("写入 last_good 失败：{key}"))
}

pub async fn remove(storage: &Storage, slug: &str) -> Result<()> {
    let key = key(slug)?;
    storage.delete(&key).await
}
