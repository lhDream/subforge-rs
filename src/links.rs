//! 分享链接解析（AGENTS.md §4.3 的补充路径）。
//!
//! 机场常按 User-Agent 返回不同格式。即使带了 Clash 兼容 UA，仍有上游返回
//! **base64 编码的分享链接列表**（v2ray/ss 生态的通用格式）。本模块把这类响应
//! 转换成 Clash 的 `proxies` 条目，使其与 Clash YAML 走同一条后续管线。
//!
//! 支持的协议：`ss` / `vmess` / `vless` / `trojan` / `hysteria2`(`hy2`) / `tuic`。
//! 其余协议（`ssr` / `hysteria` v1 等）会被**跳过并计数**——宁可少几个节点并明确
//! 告知，也不要生成连不上的节点让用户在客户端里干瞪眼。

use crate::store::model::{Mapping, Yaml, ystr};
use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde_json::Value as Json;

fn b64_engine() -> GeneralPurpose {
    GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    )
}

/// 宽容的 base64 解码：忽略空白、同时接受标准与 URL-safe 字母表、容忍缺失填充。
pub fn decode_base64(input: &str) -> Option<String> {
    let cleaned: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() {
        return None;
    }
    if !cleaned
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_'))
    {
        return None;
    }
    let normalized: String = cleaned
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let bytes = b64_engine().decode(normalized.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// 形如分享链接列表？（多数非空行都含 `://`）
pub fn looks_like_link_list(text: &str) -> bool {
    let mut total = 0usize;
    let mut links = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        total += 1;
        if line.contains("://") {
            links += 1;
        }
    }
    total > 0 && links * 2 >= total
}

/// 若整体是 base64 且解出来像链接列表，则返回解码结果。
pub fn maybe_unwrap_base64(body: &str) -> Option<String> {
    let decoded = decode_base64(body)?;
    looks_like_link_list(&decoded).then_some(decoded)
}

#[derive(Debug, Default)]
pub struct LinkOutcome {
    pub proxies: Vec<Mapping>,
    /// 被跳过的链接及原因。
    pub skipped: Vec<String>,
    /// 参与解析的非空行数。
    pub total_lines: usize,
}

/// 解析整份订阅文本（自动处理整体 base64 与折行）。
pub fn parse_subscription(body: &str) -> LinkOutcome {
    let text = maybe_unwrap_base64(body).unwrap_or_else(|| body.to_string());
    let lines = join_folded_lines(&text);
    let mut out = LinkOutcome::default();

    for (idx, line) in lines.iter().enumerate() {
        out.total_lines += 1;
        match parse_link(line) {
            Ok(proxy) => out.proxies.push(proxy),
            Err(reason) => {
                out.skipped
                    .push(format!("第 {} 行（{}）：{reason}", idx + 1, short(line)))
            }
        }
    }
    out
}

/// 是否为「只含 base64 字母表」的行——折行的续接片段。
fn is_base64ish(line: &str) -> bool {
    !line.is_empty()
        && line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_'))
}

/// 合并被折行的链接。
///
/// 机场生成订阅时常把长链接按 76 字符折行（MIME 惯例），会出现两种形态：
///
/// ```text
/// vmess://                       ← 前缀单独一行
/// eyJ2IjoiMiIsInBzIjoi...
///
/// vmess://eyJ2IjoiMiIsInBzIjoi   ← 链接中途断开
/// 576O5Zu9MDEgR1BUIiwiYWRk...
/// ```
///
/// 逐行解析会把这两种都当成无效行。这里在解析前把它们拼回去。
fn join_folded_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }

        if line.contains("://") {
            // 完整链接，或 `scheme://` 前缀——都开启一条新记录
            out.push(line.to_string());
        } else if is_base64ish(line) {
            // 续接片段：并到上一条
            match out.last_mut() {
                Some(last) => last.push_str(line),
                None => out.push(line.to_string()),
            }
        } else {
            out.push(line.to_string());
        }
    }
    out
}

fn short(s: &str) -> String {
    let head: String = s.chars().take(48).collect();
    if s.chars().count() > 48 {
        format!("{head}…")
    } else {
        head
    }
}

/// 解析单条分享链接。
pub fn parse_link(raw: &str) -> Result<Mapping, String> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| "不是链接（缺少 ://）".to_string())?;
    let scheme = scheme.trim().to_ascii_lowercase();
    match scheme.as_str() {
        "ss" => parse_ss(rest),
        "vmess" => parse_vmess(rest),
        "vless" => parse_vless(rest),
        "trojan" => parse_trojan(rest),
        "hysteria2" | "hy2" => parse_hysteria2(rest),
        "tuic" => parse_tuic(rest),
        "socks5" | "socks5h" => parse_socks5(rest),
        "ssr" => Err("暂不支持 SSR（请改用支持该协议的订阅）".to_string()),
        "hysteria" => Err("暂不支持 hysteria v1，请使用 hysteria2".to_string()),
        other => Err(format!("不支持的协议 {other}")),
    }
}

// ---------------------------------------------------------------- 通用 URI 拆分

#[derive(Debug, Default)]
struct Uri {
    userinfo: Option<String>,
    host: String,
    port: u16,
    query: Vec<(String, String)>,
    name: String,
}

impl Uri {
    fn q(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
    fn q_bool(&self, key: &str) -> bool {
        matches!(
            self.q(key).map(|v| v.to_ascii_lowercase()),
            Some(ref v) if v == "1" || v == "true" || v == "yes"
        )
    }
    /// 名称缺失时用 host:port 兜底。
    fn display_name(&self, scheme: &str) -> String {
        if self.name.trim().is_empty() {
            format!("{scheme}-{}:{}", self.host, self.port)
        } else {
            self.name.trim().to_string()
        }
    }
}

fn split_fragment(s: &str) -> (&str, String) {
    match s.split_once('#') {
        Some((body, frag)) => (body, percent_decode(frag)),
        None => (s, String::new()),
    }
}

fn split_query(s: &str) -> (&str, Vec<(String, String)>) {
    match s.split_once('?') {
        Some((body, query)) => (body, parse_query(query)),
        None => (s, Vec::new()),
    }
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// 拆分 `[userinfo@]host:port`，支持 IPv6 方括号写法。
fn split_authority(s: &str, default_port: u16) -> Result<(Option<String>, String, u16), String> {
    let s = s.trim_end_matches('/');
    let (userinfo, hostport) = match s.rfind('@') {
        // userinfo 是 percent-encoded 文本（密码里可能有 @、: 等）。
        // ss 的 SIP002 用 base64 作 userinfo，其字母表不含 `%`，解码是恒等变换。
        Some(i) => (Some(percent_decode(&s[..i])), &s[i + 1..]),
        None => (None, s),
    };

    let hostport = hostport.trim();
    if hostport.is_empty() {
        return Err("缺少主机地址".to_string());
    }

    let (host, port_str) = if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6: [::1]:443
        let end = rest
            .find(']')
            .ok_or_else(|| "IPv6 地址缺少 ]".to_string())?;
        let host = &rest[..end];
        let after = &rest[end + 1..];
        let port = after.strip_prefix(':').unwrap_or("");
        (host.to_string(), port.to_string())
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.to_string()),
            None => (hostport.to_string(), String::new()),
        }
    };

    let port = if port_str.trim().is_empty() {
        default_port
    } else {
        port_str
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("端口非法：{port_str}"))?
    };
    if host.is_empty() {
        return Err("主机地址为空".to_string());
    }
    Ok((userinfo, host, port))
}

fn parse_uri(rest: &str, default_port: u16) -> Result<Uri, String> {
    let (body, name) = split_fragment(rest);
    let (body, query) = split_query(body);
    let (userinfo, host, port) = split_authority(body, default_port)?;
    Ok(Uri {
        userinfo,
        host,
        port,
        query,
        name,
    })
}

/// URI 百分号解码（`+` 按字面处理，分享链接里不应出现表单式空格编码）。
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------- YAML 构造

fn put_str(m: &mut Mapping, k: &str, v: impl Into<String>) {
    let v = v.into();
    if !v.is_empty() {
        m.insert(ystr(k), ystr(v));
    }
}
fn put_int(m: &mut Mapping, k: &str, v: i64) {
    m.insert(ystr(k), Yaml::Number(v.into()));
}
fn put_bool(m: &mut Mapping, k: &str, v: bool) {
    m.insert(ystr(k), Yaml::Bool(v));
}
fn put_seq(m: &mut Mapping, k: &str, items: Vec<String>) {
    if !items.is_empty() {
        m.insert(
            ystr(k),
            Yaml::Sequence(items.into_iter().map(ystr).collect()),
        );
    }
}
fn put_map(m: &mut Mapping, k: &str, v: Mapping) {
    if !v.is_empty() {
        m.insert(ystr(k), Yaml::Mapping(v));
    }
}
fn map_of(pairs: Vec<(&str, Yaml)>) -> Mapping {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(ystr(k), v);
    }
    m
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

// ---------------------------------------------------------------- ss

fn parse_ss(rest: &str) -> Result<Mapping, String> {
    let (body, name) = split_fragment(rest);
    let (body, query) = split_query(body);

    let (method, password, host, port) = if let Some(at) = body.rfind('@') {
        // SIP002：userinfo 是 base64url(method:password)。
        // 部分机场会把它百分号编码（`=` → `%3D`），先解码再解 base64。
        let creds_b64 = percent_decode(&body[..at]);
        let hostport = &body[at + 1..];
        let creds =
            decode_base64(&creds_b64).ok_or_else(|| "userinfo 不是合法 base64".to_string())?;
        let (m, p) = creds
            .split_once(':')
            .ok_or_else(|| "凭据缺少 `:`".to_string())?;
        let (_, h, pt) = split_authority(hostport, 0)?;
        (m.to_string(), p.to_string(), h, pt)
    } else {
        // 旧格式：整段 base64 编码 `method:password@host:port`
        let decoded =
            decode_base64(body).ok_or_else(|| "既不是 SIP002 也不是合法 base64".to_string())?;
        let (creds, hostport) = decoded
            .rsplit_once('@')
            .ok_or_else(|| "解码后缺少 @".to_string())?;
        let (m, p) = creds
            .split_once(':')
            .ok_or_else(|| "凭据缺少 `:`".to_string())?;
        let (_, h, pt) = split_authority(hostport, 0)?;
        (m.to_string(), p.to_string(), h, pt)
    };

    if port == 0 {
        return Err("缺少端口".to_string());
    }

    let mut m = Mapping::new();
    put_str(
        &mut m,
        "name",
        if name.trim().is_empty() {
            format!("ss-{host}:{port}")
        } else {
            name
        },
    );
    put_str(&mut m, "type", "ss");
    put_str(&mut m, "server", host);
    put_int(&mut m, "port", port as i64);
    put_str(&mut m, "cipher", method);
    put_str(&mut m, "password", password);
    put_bool(&mut m, "udp", true);

    // 插件
    if let Some(plugin) = query
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("plugin"))
        .map(|(_, v)| v.clone())
    {
        let (kind, opts) = parse_ss_plugin(&plugin)?;
        put_str(&mut m, "plugin", kind);
        put_map(&mut m, "plugin-opts", opts);
    }

    Ok(m)
}

/// `obfs-local;obfs=http;obfs-host=x` / `v2ray-plugin;mode=websocket;host=x;path=/y;tls`
fn parse_ss_plugin(spec: &str) -> Result<(String, Mapping), String> {
    let mut parts = spec.split(';');
    let kind = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let mut kv: Vec<(String, String)> = Vec::new();
    for p in parts {
        if p.trim().is_empty() {
            continue;
        }
        match p.split_once('=') {
            Some((k, v)) => kv.push((k.trim().to_ascii_lowercase(), v.trim().to_string())),
            None => kv.push((p.trim().to_ascii_lowercase(), String::new())),
        }
    }
    let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());

    match kind.as_str() {
        "obfs-local" | "obfs" | "simple-obfs" => {
            let mut opts = Mapping::new();
            if let Some(mode) = get("obfs").or_else(|| get("mode")) {
                put_str(&mut opts, "mode", mode);
            }
            if let Some(host) = get("obfs-host").or_else(|| get("host")) {
                put_str(&mut opts, "host", host);
            }
            Ok(("obfs".to_string(), opts))
        }
        "v2ray-plugin" => {
            let mut opts = Mapping::new();
            put_str(
                &mut opts,
                "mode",
                get("mode").unwrap_or_else(|| "websocket".to_string()),
            );
            if let Some(host) = get("host") {
                put_str(&mut opts, "host", host);
            }
            if let Some(path) = get("path") {
                put_str(&mut opts, "path", path);
            }
            if kv.iter().any(|(k, _)| k == "tls") {
                put_bool(&mut opts, "tls", true);
            }
            Ok(("v2ray-plugin".to_string(), opts))
        }
        other => Err(format!("不支持的 ss 插件「{other}」")),
    }
}

// ---------------------------------------------------------------- vmess

fn parse_vmess(rest: &str) -> Result<Mapping, String> {
    let decoded = decode_base64(rest).ok_or_else(|| "不是合法 base64".to_string())?;
    let json: Json = serde_json::from_str(&decoded).map_err(|e| format!("JSON 解析失败：{e}"))?;

    let s = |k: &str| -> String {
        match json.get(k) {
            Some(Json::String(v)) => v.clone(),
            Some(Json::Number(v)) => v.to_string(),
            _ => String::new(),
        }
    };

    let host = s("add");
    if host.is_empty() {
        return Err("缺少 add 字段".to_string());
    }
    let port: u16 = s("port")
        .parse()
        .map_err(|_| format!("port 非法：{}", s("port")))?;
    let uuid = s("id");
    if uuid.is_empty() {
        return Err("缺少 id 字段".to_string());
    }

    let mut m = Mapping::new();
    let name = s("ps");
    put_str(
        &mut m,
        "name",
        if name.trim().is_empty() {
            format!("vmess-{host}:{port}")
        } else {
            name
        },
    );
    put_str(&mut m, "type", "vmess");
    put_str(&mut m, "server", host);
    put_int(&mut m, "port", port as i64);
    put_str(&mut m, "uuid", uuid);
    put_int(&mut m, "alterId", s("aid").parse::<i64>().unwrap_or(0));
    let cipher = {
        let c = s("scy");
        if c.is_empty() { s("security") } else { c }
    };
    put_str(
        &mut m,
        "cipher",
        if cipher.is_empty() {
            "auto".to_string()
        } else {
            cipher
        },
    );
    put_bool(&mut m, "udp", true);

    let tls = s("tls");
    if tls.eq_ignore_ascii_case("tls") || tls == "true" {
        put_bool(&mut m, "tls", true);
        let sni = {
            let x = s("sni");
            if x.is_empty() { s("serverName") } else { x }
        };
        put_str(&mut m, "servername", sni);
        put_seq(&mut m, "alpn", split_csv(&s("alpn")));
    }
    if !s("fp").is_empty() {
        put_str(&mut m, "client-fingerprint", s("fp"));
    }

    let net = {
        let n = s("net");
        if n.is_empty() { "tcp".to_string() } else { n }
    };
    let host_header = s("host");
    let path = s("path");
    match net.as_str() {
        "ws" => {
            put_str(&mut m, "network", "ws");
            let mut opts = Mapping::new();
            if !path.is_empty() {
                put_str(&mut opts, "path", path);
            }
            if !host_header.is_empty() {
                put_map(
                    &mut opts,
                    "headers",
                    map_of(vec![("Host", ystr(host_header))]),
                );
            }
            put_map(&mut m, "ws-opts", opts);
        }
        "grpc" => {
            put_str(&mut m, "network", "grpc");
            let mut opts = Mapping::new();
            let svc = if path.is_empty() { host_header } else { path };
            if !svc.is_empty() {
                put_str(&mut opts, "grpc-service-name", svc);
            }
            put_map(&mut m, "grpc-opts", opts);
        }
        "h2" => {
            put_str(&mut m, "network", "h2");
            let mut opts = Mapping::new();
            put_seq(&mut opts, "host", split_csv(&host_header));
            if !path.is_empty() {
                put_str(&mut opts, "path", path);
            }
            put_map(&mut m, "h2-opts", opts);
        }
        "tcp" => {
            if s("type").eq_ignore_ascii_case("http") {
                put_str(&mut m, "network", "http");
                let mut opts = Mapping::new();
                put_seq(&mut opts, "method", vec!["GET".to_string()]);
                put_seq(
                    &mut opts,
                    "path",
                    if path.is_empty() {
                        vec!["/".to_string()]
                    } else {
                        vec![path]
                    },
                );
                if !host_header.is_empty() {
                    put_map(
                        &mut opts,
                        "headers",
                        map_of(vec![("Host", ystr(host_header))]),
                    );
                }
                put_map(&mut m, "http-opts", opts);
            }
        }
        other => return Err(format!("不支持的 vmess 传输方式「{other}」")),
    }

    Ok(m)
}

// ---------------------------------------------------------------- vless

fn parse_vless(rest: &str) -> Result<Mapping, String> {
    let uri = parse_uri(rest, 443)?;
    let uuid = uri
        .userinfo
        .clone()
        .ok_or_else(|| "缺少 uuid".to_string())?;

    let mut m = Mapping::new();
    put_str(&mut m, "name", uri.display_name("vless"));
    put_str(&mut m, "type", "vless");
    put_str(&mut m, "server", uri.host.clone());
    put_int(&mut m, "port", uri.port as i64);
    put_str(&mut m, "uuid", uuid);
    put_bool(&mut m, "udp", true);

    let security = uri.q("security").unwrap_or("").to_ascii_lowercase();
    if matches!(security.as_str(), "tls" | "reality" | "xtls") {
        put_bool(&mut m, "tls", true);
    }
    put_str(&mut m, "servername", uri.q("sni").unwrap_or(""));
    put_str(&mut m, "flow", uri.q("flow").unwrap_or(""));
    put_str(&mut m, "client-fingerprint", uri.q("fp").unwrap_or(""));
    if security == "reality" {
        let mut ro = Mapping::new();
        put_str(&mut ro, "public-key", uri.q("pbk").unwrap_or(""));
        put_str(&mut ro, "short-id", uri.q("sid").unwrap_or(""));
        if ro.is_empty() {
            return Err("security=reality 但缺少 pbk".to_string());
        }
        put_map(&mut m, "reality-opts", ro);
    }
    if uri.q_bool("allowInsecure") || uri.q_bool("insecure") {
        put_bool(&mut m, "skip-cert-verify", true);
    }
    put_seq(&mut m, "alpn", split_csv(uri.q("alpn").unwrap_or("")));
    apply_stream_opts(&mut m, &uri, "vless")?;
    Ok(m)
}

// ---------------------------------------------------------------- trojan

fn parse_trojan(rest: &str) -> Result<Mapping, String> {
    let uri = parse_uri(rest, 443)?;
    let password = uri.userinfo.clone().ok_or_else(|| "缺少密码".to_string())?;

    let mut m = Mapping::new();
    put_str(&mut m, "name", uri.display_name("trojan"));
    put_str(&mut m, "type", "trojan");
    put_str(&mut m, "server", uri.host.clone());
    put_int(&mut m, "port", uri.port as i64);
    put_str(&mut m, "password", password);
    put_bool(&mut m, "udp", true);

    put_str(
        &mut m,
        "sni",
        uri.q("sni").or_else(|| uri.q("peer")).unwrap_or(""),
    );
    if uri.q_bool("allowInsecure") || uri.q_bool("insecure") {
        put_bool(&mut m, "skip-cert-verify", true);
    }
    put_seq(&mut m, "alpn", split_csv(uri.q("alpn").unwrap_or("")));
    put_str(&mut m, "client-fingerprint", uri.q("fp").unwrap_or(""));
    apply_stream_opts(&mut m, &uri, "trojan")?;
    Ok(m)
}

/// ws / grpc 等传输层选项，vless 与 trojan 共用。
fn apply_stream_opts(m: &mut Mapping, uri: &Uri, proto: &str) -> Result<(), String> {
    let net = uri.q("type").unwrap_or("tcp").to_ascii_lowercase();
    let path = uri.q("path").unwrap_or("");
    let host = uri.q("host").unwrap_or("");
    match net.as_str() {
        "tcp" | "" => {}
        "ws" => {
            put_str(m, "network", "ws");
            let mut opts = Mapping::new();
            if !path.is_empty() {
                put_str(&mut opts, "path", path);
            }
            if !host.is_empty() {
                put_map(&mut opts, "headers", map_of(vec![("Host", ystr(host))]));
            }
            put_map(m, "ws-opts", opts);
        }
        "grpc" => {
            put_str(m, "network", "grpc");
            let mut opts = Mapping::new();
            let svc = uri
                .q("serviceName")
                .filter(|s| !s.is_empty())
                .unwrap_or(path);
            if !svc.is_empty() {
                put_str(&mut opts, "grpc-service-name", svc);
            }
            put_map(m, "grpc-opts", opts);
        }
        "h2" => {
            put_str(m, "network", "h2");
            let mut opts = Mapping::new();
            put_seq(&mut opts, "host", split_csv(host));
            if !path.is_empty() {
                put_str(&mut opts, "path", path);
            }
            put_map(m, "h2-opts", opts);
        }
        other => return Err(format!("不支持的 {proto} 传输方式「{other}」")),
    }
    Ok(())
}

// ---------------------------------------------------------------- socks5

/// `socks5://[user:pass@]host:port#name`
///
/// 也接受 `socks5h://`（映射为 Clash 的 `socks5` 类型）；默认端口 1080。
fn parse_socks5(rest: &str) -> Result<Mapping, String> {
    let uri = parse_uri(rest, 1080)?;
    let mut m = Mapping::new();
    put_str(&mut m, "name", uri.display_name("socks5"));
    put_str(&mut m, "type", "socks5");
    put_str(&mut m, "server", uri.host.clone());
    put_int(&mut m, "port", uri.port as i64);

    if let Some(info) = &uri.userinfo {
        let info = percent_decode(info);
        if let Some((user, pass)) = info.split_once(':') {
            put_str(&mut m, "username", user);
            put_str(&mut m, "password", pass);
        } else {
            put_str(&mut m, "username", info);
        }
    }
    put_bool(&mut m, "udp", true);
    Ok(m)
}

// ---------------------------------------------------------------- hysteria2
fn parse_hysteria2(rest: &str) -> Result<Mapping, String> {
    let uri = parse_uri(rest, 443)?;
    let password = uri.userinfo.clone().unwrap_or_default();

    let mut m = Mapping::new();
    put_str(&mut m, "name", uri.display_name("hysteria2"));
    put_str(&mut m, "type", "hysteria2");
    put_str(&mut m, "server", uri.host.clone());
    put_int(&mut m, "port", uri.port as i64);
    if password.is_empty() {
        // 部分写法把认证放在 query
        put_str(&mut m, "password", uri.q("auth").unwrap_or(""));
    } else {
        put_str(&mut m, "password", password);
    }
    if m.get(ystr("password")).is_none() {
        return Err("缺少密码".to_string());
    }
    put_str(&mut m, "sni", uri.q("sni").unwrap_or(""));
    if uri.q_bool("insecure") || uri.q_bool("allowInsecure") {
        put_bool(&mut m, "skip-cert-verify", true);
    }
    put_str(&mut m, "obfs", uri.q("obfs").unwrap_or(""));
    put_str(
        &mut m,
        "obfs-password",
        uri.q("obfs-password").unwrap_or(""),
    );
    Ok(m)
}

// ---------------------------------------------------------------- tuic

fn parse_tuic(rest: &str) -> Result<Mapping, String> {
    let uri = parse_uri(rest, 443)?;
    let creds = uri.userinfo.clone().unwrap_or_default();
    let (uuid, password) = creds
        .split_once(':')
        .ok_or_else(|| "凭据应为 uuid:password".to_string())?;

    let mut m = Mapping::new();
    put_str(&mut m, "name", uri.display_name("tuic"));
    put_str(&mut m, "type", "tuic");
    put_str(&mut m, "server", uri.host.clone());
    put_int(&mut m, "port", uri.port as i64);
    put_str(&mut m, "uuid", uuid);
    put_str(&mut m, "password", password);
    put_str(&mut m, "sni", uri.q("sni").unwrap_or(""));
    if uri.q_bool("allow_insecure") || uri.q_bool("insecure") {
        put_bool(&mut m, "skip-cert-verify", true);
    }
    put_seq(&mut m, "alpn", split_csv(uri.q("alpn").unwrap_or("")));
    put_str(
        &mut m,
        "congestion-controller",
        uri.q("congestion_control").unwrap_or(""),
    );
    put_str(
        &mut m,
        "udp-relay-mode",
        uri.q("udp_relay_mode").unwrap_or(""),
    );
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::mstr;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }

    fn b64url(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    #[test]
    fn decodes_with_and_without_padding() {
        assert_eq!(decode_base64("YWVz").as_deref(), Some("aes"));
        assert_eq!(decode_base64("YWVz\n").as_deref(), Some("aes"));
        assert_eq!(
            decode_base64(&b64url("aes:pass")).as_deref(),
            Some("aes:pass")
        );
        assert_eq!(decode_base64("!!!not base64!!!"), None);
    }

    #[test]
    fn detects_link_list() {
        assert!(looks_like_link_list("ss://x@1.2.3.4:443#a\nvmess://abc\n"));
        assert!(!looks_like_link_list("port: 7890\nmode: rule\n"));
        assert!(!looks_like_link_list(""));
    }

    #[test]
    fn unwraps_whole_body_base64() {
        let inner = "ss://YWVzLTI1Ni1nY206cGFzcw==@1.2.3.4:443#香港\n";
        let body = b64(inner);
        let decoded = maybe_unwrap_base64(&body).expect("应识别为 base64 链接列表");
        assert!(decoded.contains("ss://"));
    }

    #[test]
    fn parses_ss_sip002() {
        let link = format!(
            "ss://{}@1.2.3.4:8388#%F0%9F%87%AD%F0%9F%87%B0%20%E9%A6%99%E6%B8%AF01",
            b64url("aes-256-gcm:secret")
        );
        let m = parse_link(&link).unwrap();
        assert_eq!(mstr(&m, "type"), Some("ss"));
        assert_eq!(mstr(&m, "server"), Some("1.2.3.4"));
        assert_eq!(mstr(&m, "cipher"), Some("aes-256-gcm"));
        assert_eq!(mstr(&m, "password"), Some("secret"));
        assert_eq!(mstr(&m, "name"), Some("🇭🇰 香港01"));
        assert_eq!(m.get(ystr("port")).unwrap().as_i64(), Some(8388));
    }

    #[test]
    fn parses_ss_legacy_form() {
        let link = format!("ss://{}#legacy", b64("aes-128-gcm:pw@5.6.7.8:443"));
        let m = parse_link(&link).unwrap();
        assert_eq!(mstr(&m, "server"), Some("5.6.7.8"));
        assert_eq!(mstr(&m, "cipher"), Some("aes-128-gcm"));
        assert_eq!(mstr(&m, "name"), Some("legacy"));
    }

    #[test]
    fn parses_ss_plugin() {
        let link = format!(
            "ss://{}@1.2.3.4:443?plugin=obfs-local%3Bobfs%3Dhttp%3Bobfs-host%3Dbing.com#p",
            b64url("aes-256-gcm:secret")
        );
        let m = parse_link(&link).unwrap();
        assert_eq!(mstr(&m, "plugin"), Some("obfs"));
        let opts = m.get(ystr("plugin-opts")).unwrap().as_mapping().unwrap();
        assert_eq!(mstr(opts, "mode"), Some("http"));
        assert_eq!(mstr(opts, "host"), Some("bing.com"));
    }

    #[test]
    fn rejects_unknown_ss_plugin() {
        let link = format!(
            "ss://{}@1.2.3.4:443?plugin=shadow-tls%3Bhost%3Dx#p",
            b64url("aes-256-gcm:secret")
        );
        assert!(parse_link(&link).is_err());
    }

    #[test]
    fn parses_vmess_ws_tls() {
        let json = r#"{"v":"2","ps":"美国 GPT","add":"a.example.com","port":"443","id":"11111111-2222-3333-4444-555555555555","aid":"0","scy":"auto","net":"ws","type":"none","host":"cdn.example.com","path":"/ws","tls":"tls","sni":"cdn.example.com"}"#;
        let link = format!("vmess://{}", b64(json));
        let m = parse_link(&link).unwrap();
        assert_eq!(mstr(&m, "type"), Some("vmess"));
        assert_eq!(mstr(&m, "name"), Some("美国 GPT"));
        assert_eq!(
            mstr(&m, "uuid"),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(mstr(&m, "network"), Some("ws"));
        assert_eq!(m.get(ystr("tls")).unwrap().as_bool(), Some(true));
        let ws = m.get(ystr("ws-opts")).unwrap().as_mapping().unwrap();
        assert_eq!(mstr(ws, "path"), Some("/ws"));
        let headers = ws.get(ystr("headers")).unwrap().as_mapping().unwrap();
        assert_eq!(mstr(headers, "Host"), Some("cdn.example.com"));
    }

    #[test]
    fn parses_vmess_grpc() {
        let json = r#"{"ps":"g","add":"h.com","port":443,"id":"u","net":"grpc","path":"svc"}"#;
        let m = parse_link(&format!("vmess://{}", b64(json))).unwrap();
        assert_eq!(mstr(&m, "network"), Some("grpc"));
        let g = m.get(ystr("grpc-opts")).unwrap().as_mapping().unwrap();
        assert_eq!(mstr(g, "grpc-service-name"), Some("svc"));
    }

    #[test]
    fn parses_vless_reality() {
        let link = "vless://11111111-2222-3333-4444-555555555555@h.example.com:443?encryption=none&security=reality&sni=www.apple.com&fp=chrome&pbk=PUBKEY&sid=abcd&type=tcp&flow=xtls-rprx-vision#reality%E8%8A%82%E7%82%B9";
        let m = parse_link(link).unwrap();
        assert_eq!(mstr(&m, "type"), Some("vless"));
        assert_eq!(m.get(ystr("tls")).unwrap().as_bool(), Some(true));
        assert_eq!(mstr(&m, "flow"), Some("xtls-rprx-vision"));
        assert_eq!(mstr(&m, "client-fingerprint"), Some("chrome"));
        assert_eq!(mstr(&m, "name"), Some("reality节点"));
        let ro = m.get(ystr("reality-opts")).unwrap().as_mapping().unwrap();
        assert_eq!(mstr(ro, "public-key"), Some("PUBKEY"));
        assert_eq!(mstr(ro, "short-id"), Some("abcd"));
    }

    #[test]
    fn parses_vless_ws() {
        let link =
            "vless://uuid-1@h.com:8443?security=tls&sni=h.com&type=ws&path=%2Fpath&host=cdn.com#n";
        let m = parse_link(link).unwrap();
        assert_eq!(mstr(&m, "network"), Some("ws"));
        let ws = m.get(ystr("ws-opts")).unwrap().as_mapping().unwrap();
        assert_eq!(mstr(ws, "path"), Some("/path"));
    }

    #[test]
    fn parses_socks5_with_auth() {
        let link =
            "socks5://alice:p%40ss@192.168.1.20:7891#%E5%8F%A6%E4%B8%80%E5%8F%B0%E7%94%B5%E8%84%91";
        let m = parse_link(link).unwrap();
        assert_eq!(mstr(&m, "type"), Some("socks5"));
        assert_eq!(mstr(&m, "server"), Some("192.168.1.20"));
        assert_eq!(m.get(ystr("port")).unwrap().as_i64(), Some(7891));
        assert_eq!(mstr(&m, "username"), Some("alice"));
        assert_eq!(mstr(&m, "password"), Some("p@ss"));
        assert_eq!(mstr(&m, "name"), Some("另一台电脑"));
        assert_eq!(m.get(ystr("udp")).unwrap().as_bool(), Some(true));
    }

    #[test]
    fn parses_socks5_without_auth_and_socks5h() {
        let m = parse_link("socks5h://1.2.3.4:1080").unwrap();
        assert_eq!(mstr(&m, "type"), Some("socks5"));
        assert_eq!(mstr(&m, "server"), Some("1.2.3.4"));
        assert_eq!(mstr(&m, "name"), Some("socks5-1.2.3.4:1080"));
        assert!(mstr(&m, "username").is_none());
    }

    #[test]
    fn parses_trojan() {
        let link = "trojan://my%40pass@t.example.com:443?sni=t.example.com&allowInsecure=1&type=ws&path=%2Fws#trojan%E8%8A%82%E7%82%B9";
        let m = parse_link(link).unwrap();
        assert_eq!(mstr(&m, "type"), Some("trojan"));
        assert_eq!(mstr(&m, "password"), Some("my@pass"));
        assert_eq!(mstr(&m, "sni"), Some("t.example.com"));
        assert_eq!(
            m.get(ystr("skip-cert-verify")).unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(mstr(&m, "network"), Some("ws"));
    }

    #[test]
    fn parses_hysteria2() {
        let link = "hysteria2://pw@h2.example.com:8443?sni=h2.example.com&insecure=1&obfs=salamander&obfs-password=xyz#h2";
        let m = parse_link(link).unwrap();
        assert_eq!(mstr(&m, "type"), Some("hysteria2"));
        assert_eq!(mstr(&m, "password"), Some("pw"));
        assert_eq!(mstr(&m, "obfs"), Some("salamander"));
        assert_eq!(mstr(&m, "obfs-password"), Some("xyz"));
        assert_eq!(
            m.get(ystr("skip-cert-verify")).unwrap().as_bool(),
            Some(true)
        );
    }

    #[test]
    fn parses_hy2_alias() {
        let m = parse_link("hy2://pw@h.com:443#a").unwrap();
        assert_eq!(mstr(&m, "type"), Some("hysteria2"));
    }

    #[test]
    fn parses_tuic() {
        let link = "tuic://uuid-x:pw-x@t.example.com:443?sni=t.example.com&alpn=h3&congestion_control=bbr#tuic";
        let m = parse_link(link).unwrap();
        assert_eq!(mstr(&m, "type"), Some("tuic"));
        assert_eq!(mstr(&m, "uuid"), Some("uuid-x"));
        assert_eq!(mstr(&m, "password"), Some("pw-x"));
        assert_eq!(mstr(&m, "congestion-controller"), Some("bbr"));
        assert_eq!(m.get(ystr("alpn")).unwrap().as_sequence().unwrap().len(), 1);
    }

    #[test]
    fn unsupported_schemes_are_reported_not_fatal() {
        assert!(parse_link("ssr://abcdef").unwrap_err().contains("SSR"));
        assert!(parse_link("hysteria://x@h:443").unwrap_err().contains("v1"));
        assert!(parse_link("wireguard://x").is_err());
        assert!(parse_link("not-a-link").is_err());
    }

    #[test]
    fn ipv6_host_is_parsed() {
        let link = "trojan://pw@[2001:db8::1]:443#v6";
        let m = parse_link(link).unwrap();
        assert_eq!(mstr(&m, "server"), Some("2001:db8::1"));
        assert_eq!(m.get(ystr("port")).unwrap().as_i64(), Some(443));
    }

    #[test]
    fn full_subscription_conversion() {
        let ss = format!("ss://{}@1.2.3.4:443#HK", b64url("aes-256-gcm:pw"));
        let vmess = format!(
            "vmess://{}",
            b64(r#"{"ps":"US","add":"a.com","port":443,"id":"u","net":"tcp"}"#)
        );
        let inner = format!("{ss}\n{vmess}\ntrojan://pw@t.com:443#TJ\nssr://unsupported\n");
        let body = b64(&inner);

        let out = parse_subscription(&body);
        assert_eq!(out.proxies.len(), 3, "三条有效链接应全部转换");
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].contains("SSR"), "{:?}", out.skipped);
        assert_eq!(out.total_lines, 4);
    }

    #[test]
    fn scheme_only_prefix_line_is_joined() {
        // `vmess://` 单独一行，payload 在下一行——76 字符折行的常见形态。
        let vmess = b64(r#"{"ps":"US","add":"a.com","port":443,"id":"u","net":"tcp"}"#);
        let body = format!(
            "ss://YWVzLTI1Ni1nY206cHc=@1.2.3.4:443#HK\nvmess://\n{vmess}\ntrojan://pw@t.com:443#TJ\n"
        );
        let out = parse_subscription(&body);
        assert_eq!(
            out.proxies.len(),
            3,
            "折行不应导致链接丢失：{:?}",
            out.skipped
        );
    }

    #[test]
    fn mid_link_folding_is_joined() {
        let vmess = b64(r#"{"ps":"US","add":"a.com","port":443,"id":"u","net":"tcp"}"#);
        let (head, tail) = vmess.split_at(20);
        let out = parse_subscription(&format!("vmess://{head}\n{tail}\n"));
        assert_eq!(out.proxies.len(), 1, "{:?}", out.skipped);
        assert_eq!(mstr(&out.proxies[0], "server"), Some("a.com"));
    }

    #[test]
    fn non_base64_link_list_also_works() {
        let body = "ss://YWVzLTI1Ni1nY206cHc=@1.2.3.4:443#A\n";
        let out = parse_subscription(body);
        assert_eq!(out.proxies.len(), 1);
    }

    #[test]
    fn empty_and_garbage_yield_nothing() {
        assert!(parse_subscription("").proxies.is_empty());
        assert!(parse_subscription("hello world").proxies.is_empty());
        assert!(parse_subscription("proxies:\n- name: a").proxies.is_empty());
    }
}
