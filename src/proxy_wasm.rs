//! Workers 侧代理设置：只做配置校验/展示；Fetch 不支持自定义出口代理。
//!
//! 与 native `proxy` 同名 API，供共享的 `web` 页面使用。字段仍保留，
//! 但线上拉取**不会**走这里配置的代理（迁移文档 §5 已说明）。

use anyhow::{Result, bail};

struct Parsed<'a> {
    scheme: &'a str,
    host: &'a str,
    port: Option<&'a str>,
    has_userinfo: bool,
}

fn parse(raw: &str) -> Option<Parsed<'_>> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((user, hostport)) => (Some(user), hostport),
        None => (None, rest),
    };
    // IPv6 形态这里不展开；配置里用域名或 IPv4 即可。
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        _ => (hostport, None),
    };
    if host.is_empty() {
        return None;
    }
    Some(Parsed {
        scheme,
        host,
        port,
        has_userinfo: userinfo.is_some(),
    })
}

pub fn normalize(raw: &str) -> Result<Option<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let Some(parsed) = parse(raw) else {
        bail!("proxy 不是合法 URL：{}", redact(raw));
    };
    match parsed.scheme {
        "http" | "https" | "socks5" | "socks5h" => {}
        other => bail!("proxy 只支持 http / https / socks5 / socks5h，收到：{other}"),
    }
    let Some(port) = parsed.port else {
        bail!("proxy 必须显式写端口，例如 socks5://127.0.0.1:7891");
    };
    if port.parse::<u16>().is_err() {
        bail!("proxy 端口不是合法数字：{}", redact(raw));
    }
    Ok(Some(format!(
        "{}://{}{}:{port}",
        parsed.scheme,
        if parsed.has_userinfo {
            // 保留凭据，日志会脱敏。
            let userinfo = raw
                .split_once("://")
                .and_then(|(_, rest)| rest.rsplit_once('@'))
                .map(|(user, _)| user)
                .unwrap_or("");
            format!("{userinfo}@")
        } else {
            String::new()
        },
        parsed.host
    )))
}

pub fn redact(raw: &str) -> String {
    match parse(raw) {
        Some(parsed) => match parsed.port {
            Some(port) => format!("{}://{}:{port}", parsed.scheme, parsed.host),
            None => format!("{}://{}", parsed.scheme, parsed.host),
        },
        None => "(无法解析的 proxy 配置)".to_string(),
    }
}

pub fn resolve(store: Option<(bool, &str, bool)>, config: Option<String>) -> Option<String> {
    match store {
        Some((true, url, true)) => normalize(url).ok().flatten(),
        Some((false, _, true)) => None,
        Some(_) => config,
        None => config,
    }
}
