//! Workers 侧认证。
//!
//! 复用 `crate::auth` 的 Basic Auth / CSRF 纯逻辑；这里只做 worker↔axum 的类型转换，
//! 以及从 Worker secrets/vars 读取管理密码。

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use worker::Env;

/// 管理密码：优先 secret，其次普通 var。未配置时返回 `None`（拒绝所有管理请求）。
pub fn password(env: &Env) -> Option<String> {
    env.secret("SUBFORGE_PASSWORD")
        .ok()
        .map(|s| s.to_string())
        .or_else(|| env.var("SUBFORGE_PASSWORD").ok().map(|v| v.to_string()))
        .filter(|s| !s.is_empty())
}

/// 把 `worker::Headers` 转成 axum 的 `HeaderMap`，以便复用 native 的认证实现。
pub fn to_header_map(headers: &worker::Headers) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.entries() {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) else {
            continue;
        };
        out.append(name, value);
    }
    out
}

/// Basic Auth 是否通过。
pub fn basic_ok(env: &Env, headers: &HeaderMap) -> bool {
    match password(env) {
        Some(password) => crate::auth::basic_auth_ok(headers, &password),
        None => false,
    }
}

/// 订阅令牌比较（恒定时间）。
pub fn token_ok(expected: Option<&str>, got: Option<&str>) -> bool {
    let (Some(expected), Some(got)) = (expected, got) else {
        return false;
    };
    if expected.is_empty() || got.len() != expected.len() {
        return false;
    }
    got.bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}
