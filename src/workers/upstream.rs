//! 上游拉取（Worker Fetch）+ 缓存提交。
//!
//! 复刻 native `refresh.rs` 的降级模型（AGENTS.md §4.3/§4.4）：
//! - 校验通过才更新内容字段；
//! - 失败/304 只更新状态字段或沿用正文；
//! - 零节点响应保留旧缓存。

use crate::cache::{CacheEnvelope, ErrorKind};
use crate::fetch::{USER_AGENT, validate_and_parse};
use crate::store::model::Store;
use crate::subinfo::{SubInfo, parse_userinfo};
use chrono::Utc;
use worker::{Env, Error, Fetch, Headers, Method, Request, RequestInit, Result};

/// HTTP 拉取结果（与 native `fetch::FetchResult` 同构，但不依赖 reqwest）。
#[derive(Debug, Clone, Default)]
pub struct FetchResult {
    pub not_modified: bool,
    pub body: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub userinfo: SubInfo,
    pub error: Option<(ErrorKind, String)>,
}

fn header(resp: &worker::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 拉取上游；`etag`/`last_modified` 用于条件请求。
pub async fn fetch(url: &str, etag: Option<&str>, last_modified: Option<&str>) -> FetchResult {
    let url = url.to_string();
    let etag = etag.map(|s| s.to_string());
    let last_modified = last_modified.map(|s| s.to_string());
    worker::send::SendFuture::new(async move {
        fetch_inner(&url, etag.as_deref(), last_modified.as_deref()).await
    })
    .await
}

async fn fetch_inner(url: &str, etag: Option<&str>, last_modified: Option<&str>) -> FetchResult {
    let headers = Headers::new();
    if let Err(e) = headers.set("User-Agent", USER_AGENT) {
        return FetchResult {
            error: Some((ErrorKind::Network, format!("构造请求头失败：{e}"))),
            ..Default::default()
        };
    }
    if let Some(value) = etag
        && let Err(e) = headers.set("If-None-Match", value)
    {
        tracing::warn!("设置 If-None-Match 失败：{e}");
    }
    if let Some(value) = last_modified
        && let Err(e) = headers.set("If-Modified-Since", value)
    {
        tracing::warn!("设置 If-Modified-Since 失败：{e}");
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let request = match Request::new_with_init(url, &init) {
        Ok(request) => request,
        Err(e) => {
            return FetchResult {
                error: Some((ErrorKind::Network, format!("构造请求失败：{e}"))),
                ..Default::default()
            };
        }
    };

    let mut resp = match Fetch::Request(request).send().await {
        Ok(resp) => resp,
        Err(e) => {
            return FetchResult {
                error: Some((ErrorKind::Network, format!("请求失败：{e}"))),
                ..Default::default()
            };
        }
    };

    let status = resp.status_code();
    let etag = header(&resp, "etag");
    let last_modified = header(&resp, "last-modified");
    let userinfo = header(&resp, "subscription-userinfo")
        .map(|v| parse_userinfo(&v))
        .unwrap_or_default();

    if status == 304 {
        return FetchResult {
            not_modified: true,
            etag,
            last_modified,
            userinfo,
            ..Default::default()
        };
    }
    if !(200..300).contains(&status) {
        return FetchResult {
            error: Some((ErrorKind::HttpStatus, format!("HTTP {status}"))),
            etag,
            last_modified,
            userinfo,
            ..Default::default()
        };
    }

    match resp.text().await {
        Ok(body) => FetchResult {
            body: Some(body),
            etag,
            last_modified,
            userinfo,
            ..Default::default()
        },
        Err(e) => FetchResult {
            error: Some((ErrorKind::Network, format!("读取响应失败：{e}"))),
            etag,
            last_modified,
            userinfo,
            ..Default::default()
        },
    }
}

/// 刷新单个上游并提交缓存。网络/校验失败按降级模型处理，返回 `Ok` 表示流程完成。
pub async fn refresh(env: &Env, store: &Store, id: &str) -> Result<()> {
    let Some(upstream) = store.upstream(id) else {
        return Err(Error::RustError(format!("上游「{id}」不存在")));
    };
    let mut envelope = crate::workers::cache::load(env, id)
        .await?
        .unwrap_or_default();
    let now = Utc::now();
    let outcome = fetch(
        &upstream.url,
        envelope.meta.etag.as_deref(),
        envelope.meta.last_modified.as_deref(),
    )
    .await;

    // 304：正文沿用，但派生统计必须用当前解析器重算（§4.2）。
    if outcome.not_modified && envelope.has_content() {
        let mut info = outcome.userinfo.clone();
        info.fill_from(&SubInfo {
            expire: envelope.meta.expire,
            total: envelope.meta.total,
            upload: envelope.meta.upload,
            download: envelope.meta.download,
        });
        let (count, source_format, skipped) = match validate_and_parse(
            &envelope.body,
            &store.settings.filter_keywords,
            store.settings.filter_enabled,
        ) {
            Ok(parsed) => (
                parsed.proxies.len(),
                Some(parsed.source_format),
                parsed.skipped_links.len(),
            ),
            Err(_) => (
                envelope.meta.node_count,
                envelope.meta.source_format.clone(),
                envelope.meta.skipped_links,
            ),
        };
        let body = envelope.body.clone();
        envelope.commit_success(
            body,
            &info,
            outcome.etag.or(envelope.meta.etag.clone()),
            outcome
                .last_modified
                .or(envelope.meta.last_modified.clone()),
            count,
            now,
        );
        envelope.meta.source_format = source_format;
        envelope.meta.skipped_links = skipped;
        crate::workers::cache::save(env, id, &envelope).await?;
        return Ok(());
    }

    if let Some((kind, message)) = outcome.error {
        envelope.commit_failure(kind, message, now);
        crate::workers::cache::save(env, id, &envelope).await?;
        return Ok(());
    }

    let Some(body) = outcome.body else {
        envelope.commit_failure(ErrorKind::Network, "响应体缺失", now);
        crate::workers::cache::save(env, id, &envelope).await?;
        return Ok(());
    };

    match validate_and_parse(
        &body,
        &store.settings.filter_keywords,
        store.settings.filter_enabled,
    ) {
        Ok(parsed) => {
            let mut info = outcome.userinfo.clone();
            info.fill_from(&parsed.fallback_info);
            let count = parsed.proxies.len();
            envelope.commit_success(body, &info, outcome.etag, outcome.last_modified, count, now);
            envelope.meta.source_format = Some(parsed.source_format);
            envelope.meta.skipped_links = parsed.skipped_links.len();
        }
        Err((kind, message)) => envelope.commit_failure(kind, message, now),
    }
    crate::workers::cache::save(env, id, &envelope).await
}

/// 供导出层使用的告警文本：把缓存状态转成一句可读提示。
pub fn status_warning(upstream_name: &str, envelope: &CacheEnvelope) -> Option<String> {
    use crate::cache::CacheStatus;
    match envelope.meta.status {
        CacheStatus::Ok => None,
        CacheStatus::Stale => Some(format!(
            "上游「{upstream_name}」最近一次刷新失败（{}），正在使用旧数据",
            envelope
                .meta
                .last_error_kind
                .map(|k| k.label())
                .unwrap_or("未知")
        )),
        CacheStatus::Empty => Some(format!(
            "上游「{upstream_name}」返回了零节点响应，正在使用旧数据"
        )),
        CacheStatus::Failed => Some(format!("上游「{upstream_name}」没有可用数据")),
    }
}
