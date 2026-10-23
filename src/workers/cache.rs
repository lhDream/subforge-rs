//! 上游缓存（R2 对象）。
//!
//! 与 native 的 `data/cache/<id>.yaml` 一一对应：每个上游一个对象，内容仍是
//! `CacheEnvelope` 序列化后的 YAML——正文与元数据一起写入，保持 AGENTS.md §4.1
//! 的「单对象原子提交」语义。

use crate::cache::CacheEnvelope;
use worker::{Env, Error, Result};

/// R2 绑定名（见 `wrangler.jsonc`）。
pub const BINDING: &str = "CACHE";

fn key(id: &str) -> Result<String> {
    if !crate::store::valid_slug(id) {
        return Err(Error::RustError(format!("非法的上游 id：{id}")));
    }
    Ok(format!("cache/{id}.yaml"))
}

fn werr(context: &str, e: impl std::fmt::Display) -> Error {
    Error::RustError(format!("{context}：{e}"))
}

pub async fn load(env: &Env, id: &str) -> Result<Option<CacheEnvelope>> {
    let bucket = env.bucket(BINDING)?;
    let object = bucket.get(key(id)?).execute().await?;
    let Some(object) = object else {
        return Ok(None);
    };
    let body = object
        .body()
        .ok_or_else(|| Error::RustError("缓存对象没有正文".to_string()))?;
    let text = body.text().await?;
    let envelope = serde_yaml_ng::from_str(&text).map_err(|e| werr("解析缓存失败", e))?;
    Ok(Some(envelope))
}

pub async fn save(env: &Env, id: &str, envelope: &CacheEnvelope) -> Result<()> {
    let bucket = env.bucket(BINDING)?;
    let text = serde_yaml_ng::to_string(envelope).map_err(|e| werr("序列化缓存失败", e))?;
    bucket.put(key(id)?, text).execute().await?;
    Ok(())
}

pub async fn remove(env: &Env, id: &str) -> Result<()> {
    let bucket = env.bucket(BINDING)?;
    bucket.delete(key(id)?).await?;
    Ok(())
}
