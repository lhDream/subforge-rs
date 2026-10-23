//! 上游缓存信封（AGENTS.md §4.1）。
//!
//! **单文件**：正文与元数据在同一文件、同一次原子替换中提交，从根本上杜绝
//! 「正文写成功、元数据写失败」导致的内容与到期信息错配。
//!
//! 字段分两类，由 [`CacheEnvelope`] 的方法在类型层面强制区分：
//! - **内容字段**（`body` / `last_success_at` / `expire` / 流量 / `etag` / `node_count`）
//!   全有或全无，仅在响应通过全部校验时一起更新；
//! - **状态字段**（`last_attempt_at` / `last_error` / `last_error_kind` / `status`）
//!   每次刷新都更新。

use crate::store::atomic_write;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CacheStatus {
    /// 最近一次刷新成功且内容有效。
    #[default]
    Ok,
    /// 刷新失败/内容无效，正在使用旧缓存。
    Stale,
    /// 响应合法但零节点。
    Empty,
    /// 无任何可用缓存。
    Failed,
}

impl CacheStatus {
    pub fn label(&self) -> &'static str {
        match self {
            CacheStatus::Ok => "正常",
            CacheStatus::Stale => "陈旧",
            CacheStatus::Empty => "空响应",
            CacheStatus::Failed => "失败",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Network,
    HttpStatus,
    Parse,
    Structure,
    Empty,
}

impl ErrorKind {
    pub fn label(&self) -> &'static str {
        match self {
            ErrorKind::Network => "网络",
            ErrorKind::HttpStatus => "HTTP 状态",
            ErrorKind::Parse => "解析",
            ErrorKind::Structure => "结构",
            ErrorKind::Empty => "空响应",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheMeta {
    // ---- 状态字段：每次刷新都更新
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub status: CacheStatus,
    pub last_error: Option<String>,
    pub last_error_kind: Option<ErrorKind>,

    // ---- 内容字段：仅校验通过时一起更新
    pub last_success_at: Option<DateTime<Utc>>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub expire: Option<i64>,
    pub total: Option<u64>,
    pub upload: Option<u64>,
    pub download: Option<u64>,
    pub node_count: usize,

    /// 响应实际采用的格式（`Clash YAML` / `分享链接（base64）` 等）。
    #[serde(default)]
    pub source_format: Option<String>,
    /// 分享链接模式下因协议不支持等原因跳过的条目数。
    #[serde(default)]
    pub skipped_links: usize,

    /// 用户强制接受零节点响应的时刻（§4.4）。
    pub forced_at: Option<DateTime<Utc>>,

    /// 最近一次手动保存正文的时刻；网络同步成功后清除。
    #[serde(default)]
    pub manual_saved_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheEnvelope {
    #[serde(default)]
    pub meta: CacheMeta,
    /// 上游原始响应文本，原样保存。
    #[serde(default)]
    pub body: String,
}

impl CacheEnvelope {
    /// 是否有可用内容（有正文即视为可用，无论新鲜度——§4.7）。
    pub fn has_content(&self) -> bool {
        self.meta.last_success_at.is_some() && !self.body.trim().is_empty()
    }

    /// 内容字段与状态字段一起提交（校验通过）。
    #[allow(clippy::too_many_arguments)]
    pub fn commit_success(
        &mut self,
        body: String,
        info: &crate::subinfo::SubInfo,
        etag: Option<String>,
        last_modified: Option<String>,
        node_count: usize,
        now: DateTime<Utc>,
    ) {
        // 内容字段：全有或全无
        self.body = body;
        self.meta.last_success_at = Some(now);
        self.meta.etag = etag;
        self.meta.last_modified = last_modified;
        self.meta.expire = info.expire;
        self.meta.total = info.total;
        self.meta.upload = info.upload;
        self.meta.download = info.download;
        self.meta.node_count = node_count;

        // 状态字段
        self.meta.last_attempt_at = Some(now);
        self.meta.status = CacheStatus::Ok;
        self.meta.last_error = None;
        self.meta.last_error_kind = None;
        self.meta.manual_saved_at = None;
    }

    /// 用已校验的正文替换缓存，并清除远端条件请求与旧流量状态。
    pub fn commit_manual(
        &mut self,
        body: String,
        parsed: &crate::fetch::ParsedBody,
        now: DateTime<Utc>,
    ) {
        self.body = body;
        self.meta.last_success_at = Some(now);
        self.meta.etag = None;
        self.meta.last_modified = None;
        self.meta.expire = parsed.fallback_info.expire;
        self.meta.total = parsed.fallback_info.total;
        self.meta.upload = parsed.fallback_info.upload;
        self.meta.download = parsed.fallback_info.download;
        self.meta.node_count = parsed.proxies.len();
        self.meta.source_format = Some(parsed.source_format.clone());
        self.meta.skipped_links = parsed.skipped_links.len();
        self.meta.forced_at = None;
        self.meta.manual_saved_at = Some(now);
        self.meta.last_attempt_at = Some(now);
        self.meta.status = CacheStatus::Ok;
        self.meta.last_error = None;
        self.meta.last_error_kind = None;
    }

    /// 仅更新状态字段，**保留全部内容字段**。
    pub fn commit_failure(
        &mut self,
        kind: ErrorKind,
        message: impl Into<String>,
        now: DateTime<Utc>,
    ) {
        self.meta.last_attempt_at = Some(now);
        self.meta.last_error = Some(message.into());
        self.meta.last_error_kind = Some(kind);
        self.meta.status = match kind {
            ErrorKind::Empty => CacheStatus::Empty,
            _ => {
                if self.has_content() {
                    CacheStatus::Stale
                } else {
                    CacheStatus::Failed
                }
            }
        };
        // 注意：绝不触碰 body / last_success_at / expire / 流量 / etag / node_count
    }
}

/// 计算缓存信封指纹；`None` 表示不存在缓存。
/// 状态变化也使旧编辑页失效：宁可要求重新载入，不将旧正文绑定到新状态。
pub fn fingerprint(env: Option<&CacheEnvelope>) -> Result<String> {
    use sha2::{Digest, Sha256};
    let serialized = serde_yaml_ng::to_string(&env)?;
    Ok(format!("{:x}", Sha256::digest(serialized.as_bytes())))
}

pub fn cache_path(dir: &Path, id: &str) -> Result<PathBuf> {
    if !crate::store::valid_slug(id) {
        bail!("非法的上游 id：{id}");
    }
    Ok(dir.join(format!("{id}.yaml")))
}

pub fn load(dir: &Path, id: &str) -> Result<Option<CacheEnvelope>> {
    let path = cache_path(dir, id)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取缓存失败：{}", path.display()))?;
    let env: CacheEnvelope = serde_yaml_ng::from_str(&text)
        .with_context(|| format!("解析缓存失败：{}", path.display()))?;
    Ok(Some(env))
}

pub fn save(dir: &Path, id: &str, env: &CacheEnvelope) -> Result<()> {
    let path = cache_path(dir, id)?;
    let text = serde_yaml_ng::to_string(env)?;
    atomic_write(&path, &text)
}

pub fn remove(dir: &Path, id: &str) -> Result<()> {
    let path = cache_path(dir, id)?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("删除缓存失败：{}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subinfo::SubInfo;

    fn info() -> SubInfo {
        SubInfo {
            upload: Some(1),
            download: Some(2),
            total: Some(100),
            expire: Some(999),
        }
    }

    #[test]
    fn success_commits_all_content_fields() {
        let mut env = CacheEnvelope::default();
        let now = Utc::now();
        env.commit_success(
            "proxies: []".into(),
            &info(),
            Some("etag1".into()),
            None,
            5,
            now,
        );

        assert!(env.has_content());
        assert_eq!(env.meta.status, CacheStatus::Ok);
        assert_eq!(env.meta.node_count, 5);
        assert_eq!(env.meta.expire, Some(999));
        assert_eq!(env.meta.etag.as_deref(), Some("etag1"));
        assert_eq!(env.meta.last_success_at, Some(now));
    }

    #[test]
    fn failure_never_touches_content_fields() {
        let mut env = CacheEnvelope::default();
        let success_at = Utc::now();
        env.commit_success(
            "原始正文".into(),
            &info(),
            Some("e".into()),
            None,
            7,
            success_at,
        );

        let later = success_at + chrono::Duration::hours(6);
        env.commit_failure(ErrorKind::Parse, "返回了 HTML 登录页", later);

        // 内容字段原封不动
        assert_eq!(env.body, "原始正文");
        assert_eq!(env.meta.last_success_at, Some(success_at));
        assert_eq!(env.meta.expire, Some(999));
        assert_eq!(env.meta.etag.as_deref(), Some("e"));
        assert_eq!(env.meta.node_count, 7);

        // 状态字段已更新
        assert_eq!(env.meta.status, CacheStatus::Stale);
        assert_eq!(env.meta.last_error_kind, Some(ErrorKind::Parse));
        assert_eq!(env.meta.last_attempt_at, Some(later));
        assert!(env.has_content(), "旧数据必须仍然可用");
    }

    #[test]
    fn failure_without_content_is_failed_status() {
        let mut env = CacheEnvelope::default();
        env.commit_failure(ErrorKind::Network, "连接超时", Utc::now());
        assert_eq!(env.meta.status, CacheStatus::Failed);
        assert!(!env.has_content());
    }

    #[test]
    fn empty_response_gets_its_own_status() {
        let mut env = CacheEnvelope::default();
        env.commit_success("old".into(), &info(), None, None, 3, Utc::now());
        env.commit_failure(ErrorKind::Empty, "零节点", Utc::now());
        assert_eq!(env.meta.status, CacheStatus::Empty);
        assert_eq!(env.body, "old");
    }

    #[test]
    fn envelope_roundtrips_through_yaml() {
        let mut env = CacheEnvelope::default();
        env.commit_success(
            "proxies:\n  - name: a\n".into(),
            &info(),
            None,
            None,
            1,
            Utc::now(),
        );
        let text = serde_yaml_ng::to_string(&env).unwrap();
        let back: CacheEnvelope = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(back.body, env.body);
        assert_eq!(back.meta.node_count, 1);
    }

    #[test]
    fn manual_body_keeps_text_and_clears_remote_validators() {
        let mut env = CacheEnvelope::default();
        env.commit_success(
            "old".into(),
            &info(),
            Some("remote".into()),
            Some("date".into()),
            2,
            Utc::now(),
        );
        env.meta.forced_at = Some(Utc::now());
        let body = "# 保留注释\nproxies:\n  - name: HK\n    type: socks5\n    server: 127.0.0.1\n    port: 1080\n";
        let parsed = crate::fetch::validate_and_parse(body, &[], false).unwrap();
        let now = Utc::now();
        env.commit_manual(body.into(), &parsed, now);
        assert_eq!(env.body, body);
        assert_eq!(env.meta.node_count, 1);
        assert!(env.meta.etag.is_none());
        assert!(env.meta.last_modified.is_none());
        assert!(env.meta.expire.is_none());
        assert!(env.meta.forced_at.is_none());
        assert_eq!(env.meta.manual_saved_at, Some(now));
        assert!(env.has_content());
    }

    #[test]
    fn cache_fingerprint_changes_for_equal_length_body_edits() {
        let mut env = CacheEnvelope {
            body: "one".into(),
            ..Default::default()
        };
        let before = fingerprint(Some(&env)).unwrap();
        env.body = "two".into();
        assert_ne!(before, fingerprint(Some(&env)).unwrap());
        assert_ne!(before, fingerprint(None).unwrap());
    }

    #[test]
    fn cache_path_rejects_traversal() {
        assert!(cache_path(Path::new("."), "../evil").is_err());
    }
}
