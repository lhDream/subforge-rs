//! 上游拉取（Worker Fetch）+ 缓存提交。
//!
//! 复刻 native `refresh.rs` 的降级模型（AGENTS.md §4.3/§4.4）：
//! - 校验通过才更新内容字段；
//! - 失败/304 只更新状态字段或沿用正文；
//! - 零节点响应保留旧缓存。

use crate::cache::ErrorKind;
use crate::fetch::USER_AGENT;

use crate::subinfo::{SubInfo, parse_userinfo};

use worker::{Fetch, Headers, Method, Request, RequestInit};

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
