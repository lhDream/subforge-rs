//! 认证与 CSRF（AGENTS.md §7）。
//!
//! ⚠️ **为什么不能只靠 SameSite cookie**：在纯 Basic Auth 下，浏览器会自动附带
//! 凭据，服务端只认 `Authorization` 头，缺少 cookie 并不影响操作。因此修改类操作
//! 必须校验 **CSRF token**，并叠加 `Origin`/`Referer` 来源检查。

use axum::http::{HeaderMap, header};
use base64::Engine;
use rand::Rng;
use sha2::{Digest, Sha256};

pub const SID_COOKIE: &str = "rwd_sid";

/// 生成随机十六进制串（CSRF 密钥、订阅令牌）。
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill(&mut buf[..]);
    hex::encode(buf)
}

/// 恒定时间比较，避免时序侧信道。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 校验 HTTP Basic Auth。用户名不限制，密码必须匹配。
pub fn basic_auth_ok(headers: &HeaderMap, password: &str) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(text) = value.to_str() else {
        return false;
    };
    let Some(encoded) = text
        .strip_prefix("Basic ")
        .or_else(|| text.strip_prefix("basic "))
    else {
        return false;
    };
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(raw) else {
        return false;
    };
    let Some((_user, pass)) = decoded.split_once(':') else {
        return false;
    };
    ct_eq(pass.as_bytes(), password.as_bytes())
}

/// 读取指定 cookie。
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=')
            && k.trim() == name
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// 由密钥与 sid 派生 CSRF token。
pub fn csrf_token(secret: &str, sid: &str) -> String {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    h.update(b":csrf:");
    h.update(sid.as_bytes());
    hex::encode(h.finalize())
}

/// 校验 CSRF token。
pub fn verify_csrf(secret: &str, sid: &str, submitted: &str) -> bool {
    if sid.is_empty() || submitted.is_empty() {
        return false;
    }
    ct_eq(
        csrf_token(secret, sid).as_bytes(),
        submitted.trim().as_bytes(),
    )
}

/// 来源检查：`Origin`（或退化的 `Referer`）的 host 必须与 `Host` 一致。
///
/// 两个头都不存在时放行——非浏览器客户端（curl / 测试）不会带它们，而 Basic Auth
/// 已经是一道门槛。
pub fn origin_ok(headers: &HeaderMap) -> bool {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let candidate = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get(header::REFERER)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        });

    let Some(candidate) = candidate else {
        return true;
    };

    let Some(rest) = candidate
        .strip_prefix("http://")
        .or_else(|| candidate.strip_prefix("https://"))
    else {
        return false;
    };
    let candidate_host = rest.split('/').next().unwrap_or("");
    candidate_host.eq_ignore_ascii_case(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn accepts_correct_basic_auth() {
        // "admin:secret" 的 base64
        let h = headers_with(header::AUTHORIZATION, "Basic YWRtaW46c2VjcmV0");
        assert!(basic_auth_ok(&h, "secret"));
        assert!(!basic_auth_ok(&h, "wrong"));
    }

    #[test]
    fn rejects_malformed_authorization() {
        assert!(!basic_auth_ok(&HeaderMap::new(), "secret"));
        assert!(!basic_auth_ok(
            &headers_with(header::AUTHORIZATION, "Bearer abc"),
            "secret"
        ));
        assert!(!basic_auth_ok(
            &headers_with(header::AUTHORIZATION, "Basic !!!not-base64!!!"),
            "secret"
        ));
        assert!(!basic_auth_ok(
            &headers_with(header::AUTHORIZATION, "Basic bm9jb2xvbg=="),
            "secret"
        ));
    }

    #[test]
    fn csrf_roundtrip() {
        let secret = "s3cret";
        let sid = "abc123";
        let token = csrf_token(secret, sid);
        assert!(verify_csrf(secret, sid, &token));
        assert!(!verify_csrf(secret, sid, "deadbeef"));
        assert!(!verify_csrf(secret, "other", &token));
        assert!(!verify_csrf(secret, "", &token));
    }

    #[test]
    fn origin_check_matches_host() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8080"));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8080"),
        );
        assert!(origin_ok(&h));

        let mut bad = HeaderMap::new();
        bad.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8080"));
        bad.insert(header::ORIGIN, HeaderValue::from_static("http://evil.test"));
        assert!(!origin_ok(&bad));
    }

    #[test]
    fn origin_absent_is_allowed() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8080"));
        assert!(origin_ok(&h));
    }

    #[test]
    fn cookie_parsing() {
        let h = headers_with(header::COOKIE, "a=1; rwd_sid=xyz; b=2");
        assert_eq!(cookie(&h, SID_COOKIE).as_deref(), Some("xyz"));
        assert_eq!(cookie(&h, "a").as_deref(), Some("1"));
        assert_eq!(cookie(&h, "missing"), None);
    }
}
