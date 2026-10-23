//! 可选的出口代理（`config.toml` 的 `proxy`）。
//!
//! 有些上游机场只在特定网络/地区可访问，直连会超时。这里只支持
//! **HTTP(S) / SOCKS5 代理**（可以是本机 Mihomo 等客户端监听的端口），
//! 不实现 Clash 节点协议本身——那属于内核职责（AGENTS.md §1「非目标」）。
//!
//! 默认不配置 → 直连，行为与之前完全一致。

use anyhow::{Context, Result, bail};

/// 校验并返回规范化后的代理地址。空串表示不使用代理。
///
/// 接受 `http://`、`https://`、`socks5://`、`socks5h://`；
/// 允许带 `user:password@`，但**日志中必须脱敏**。
pub fn normalize(raw: &str) -> Result<Option<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let url =
        reqwest::Url::parse(raw).with_context(|| format!("proxy 不是合法 URL：{}", redact(raw)))?;
    match url.scheme() {
        "http" | "https" | "socks5" | "socks5h" => {}
        other => bail!("proxy 只支持 http / https / socks5 / socks5h，收到：{other}"),
    }
    if url.host_str().is_none_or(str::is_empty) {
        bail!("proxy 缺少主机名：{}", redact(raw));
    }
    if url.port().is_none() {
        bail!("proxy 必须显式写端口，例如 socks5://127.0.0.1:7891");
    }
    Ok(Some(url.to_string()))
}

/// 带凭据的代理地址在日志里只显示 scheme 与主机端口。
pub fn redact(raw: &str) -> String {
    match reqwest::Url::parse(raw.trim()) {
        Ok(url) => {
            let host = url.host_str().unwrap_or("?");
            match url.port() {
                Some(port) => format!("{}://{host}:{port}", url.scheme()),
                None => format!("{}://{host}", url.scheme()),
            }
        }
        Err(_) => "(无法解析的 proxy 配置)".to_string(),
    }
}

/// 给 reqwest 客户端加上代理配置。
pub fn apply(
    builder: reqwest::ClientBuilder,
    proxy: Option<&str>,
) -> Result<reqwest::ClientBuilder> {
    let Some(raw) = proxy else {
        return Ok(builder);
    };
    let proxy =
        reqwest::Proxy::all(raw).with_context(|| format!("代理配置无效：{}", redact(raw)))?;
    Ok(builder.proxy(proxy))
}

/// 启动时决定实际使用的代理：网页设置在「设置过」时优先，否则用 `config.toml`。
///
/// `store` 为 `(enabled, url, configured)`。用户主动关闭代理也算「设置过」，
/// 此时保持直连，而不是回退到配置文件里的旧值。
pub fn resolve(store: Option<(bool, &str, bool)>, config: Option<String>) -> Option<String> {
    match store {
        // 用户已在网页里做过选择（启用或主动关闭），以网页为准。
        Some((true, url, true)) => normalize(url).ok().flatten(),
        Some((false, _, true)) => None,
        // 旧数据或首次初始化：还没有在网页里设置过，回退到 config.toml。
        Some(_) => config,
        None => config,
    }
}

/// 测试用：解析出的协议是否被接受。
#[cfg(test)]
pub fn supported_scheme(raw: &str) -> bool {
    reqwest::Url::parse(raw.trim())
        .map(|u| matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_means_direct() {
        assert_eq!(normalize("").unwrap(), None);
        assert_eq!(normalize("   ").unwrap(), None);
    }

    #[test]
    fn accepts_common_proxy_urls() {
        for raw in [
            "http://127.0.0.1:7890",
            "https://127.0.0.1:7890",
            "socks5://127.0.0.1:7891",
            "socks5h://127.0.0.1:7891",
            "socks5://user:pass@127.0.0.1:1080",
        ] {
            assert!(normalize(raw).unwrap().is_some(), "{raw} 应被接受");
        }
    }

    #[test]
    fn rejects_unknown_scheme_and_missing_port() {
        let err = normalize("vmess://abc@host:443").unwrap_err().to_string();
        assert!(err.contains("只支持"), "{err}");
        let err = normalize("socks5://127.0.0.1").unwrap_err().to_string();
        assert!(err.contains("端口"), "{err}");
        assert!(normalize("not a url").is_err());
    }

    #[test]
    fn credentials_are_redacted_in_messages() {
        let raw = "socks5://user:secret-pass@127.0.0.1:1080";
        let shown = redact(raw);
        assert!(!shown.contains("secret-pass"), "{shown}");
        assert!(!shown.contains("user"), "{shown}");
        assert_eq!(shown, "socks5://127.0.0.1:1080");
        // 字段本身保留凭据，供真正的请求使用。
        assert!(normalize(raw).unwrap().unwrap().contains("secret-pass"));
    }

    #[test]
    fn unsupported_node_protocols_are_not_proxies() {
        assert!(!supported_scheme("ss://YWVz@127.0.0.1:8388"));
        assert!(supported_scheme("socks5://127.0.0.1:7891"));
    }

    #[test]
    fn startup_proxy_prefers_web_settings_then_config() {
        let cfg = Some("http://127.0.0.1:7890".to_string());
        // 从未在网页设置过 → 用 config.toml。
        assert_eq!(resolve(Some((false, "", false)), cfg.clone()), cfg);
        assert_eq!(resolve(None, cfg.clone()), cfg);
        // 网页启用 → 用网页地址，忽略 config.toml。
        assert_eq!(
            resolve(Some((true, "socks5://127.0.0.1:7891", true)), cfg.clone()),
            Some("socks5://127.0.0.1:7891".to_string())
        );
        // 网页主动关闭 → 直连，不回退到 config.toml 的旧值。
        assert_eq!(
            resolve(Some((false, "socks5://127.0.0.1:7891", true)), cfg),
            None
        );
        // 网页存了非法地址（被手改）→ 直连，不回退旧值。
        assert_eq!(
            resolve(
                Some((true, "vmess://x@h:443", true)),
                Some("http://a:1".into())
            ),
            None
        );
    }
}
