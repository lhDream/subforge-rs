//! 上游拉取与响应校验（AGENTS.md §4.2 / §4.3）。
//!
//! **HTTP 200 不代表内容可用。** 上游可能返回登录页、错误提示、损坏 YAML，或只剩
//! 流量信息的节点列表。只有通过全部校验的响应才能替换有效缓存。

use crate::cache::ErrorKind;
use crate::store::model::{Mapping, Yaml, mget, mstr};
#[cfg(not(target_arch = "wasm32"))]
use crate::subinfo::parse_userinfo;
use crate::subinfo::{SubInfo, extract_from_names, is_fake_node};
#[cfg(not(target_arch = "wasm32"))]
use anyhow::Context;
use anyhow::Result;

/// 机场常按 UA 返回不同格式；UA 不对会拿到 base64 分享链接列表而非 Clash YAML。
pub const USER_AGENT: &str = "mihomo/1.18.0";

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default)]
pub struct FetchResult {
    /// 上游返回 304，内容未变更。
    pub not_modified: bool,
    pub body: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub userinfo: SubInfo,
    pub error: Option<(ErrorKind, String)>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FetchResult {
    fn err(kind: ErrorKind, msg: impl Into<String>) -> Self {
        Self {
            error: Some((kind, msg.into())),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------- 网络部分（native）
//
// Workers 没有 reqwest，拉取改由 `worker::Fetch` 在后续阶段实现；这里只保留
// **响应校验与解析**（纯逻辑），保证 `cache`/`render` 等模块在 wasm 上可复用。

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    timeout: std::time::Duration,
}

#[cfg(not(target_arch = "wasm32"))]
impl Fetcher {
    /// `proxy` 为规范化后的代理地址；`None` 表示直连。
    pub fn with_proxy(timeout: std::time::Duration, proxy: Option<&str>) -> Result<Self> {
        let builder = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT);
        let client = crate::proxy::apply(builder, proxy)
            .context("初始化上游拉取客户端失败")?
            .build()?;
        Ok(Self { client, timeout })
    }

    pub fn with_proxy_replacement(&self, proxy: Option<&str>) -> Result<Self> {
        Self::with_proxy(self.timeout, proxy)
    }

    pub async fn fetch(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> FetchResult {
        let mut req = self.client.get(url);
        if let Some(e) = etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, e);
        }
        if let Some(lm) = last_modified {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return FetchResult::err(ErrorKind::Network, format!("请求失败：{e}")),
        };

        let status = resp.status();
        let etag = header_str(&resp, reqwest::header::ETAG);
        let last_modified = header_str(&resp, reqwest::header::LAST_MODIFIED);
        let userinfo = header_str(&resp, "subscription-userinfo")
            .map(|v| parse_userinfo(&v))
            .unwrap_or_default();

        if status == reqwest::StatusCode::NOT_MODIFIED {
            return FetchResult {
                not_modified: true,
                etag,
                last_modified,
                userinfo,
                ..Default::default()
            };
        }

        if !status.is_success() {
            return FetchResult {
                error: Some((ErrorKind::HttpStatus, format!("HTTP {status}"))),
                etag,
                last_modified,
                userinfo,
                ..Default::default()
            };
        }

        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return FetchResult::err(ErrorKind::Network, format!("读取响应失败：{e}")),
        };

        FetchResult {
            not_modified: false,
            body: Some(body),
            etag,
            last_modified,
            userinfo,
            error: None,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn header_str(
    resp: &reqwest::Response,
    name: impl reqwest::header::AsHeaderName,
) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 校验通过后的解析结果。
#[derive(Debug, Clone, Default)]
pub struct ParsedBody {
    /// **已过滤假节点**的有效节点。
    pub proxies: Vec<Mapping>,
    /// 被过滤掉的节点名（预览页必须展示，便于发现误杀）。
    pub filtered: Vec<String>,
    /// 从信息节点名称回退提取的订阅信息。
    pub fallback_info: SubInfo,
    /// 响应实际采用的格式（Clash YAML / 分享链接）。
    pub source_format: String,
    /// 分享链接模式下无法识别而跳过的条目（含原因）。
    pub skipped_links: Vec<String>,
}

/// 尝试把响应体当作 Clash YAML 解析，产出原始 `proxies`。
fn parse_clash_yaml(body: &str) -> Result<Vec<Mapping>, (ErrorKind, String)> {
    let value: Yaml = serde_yaml_ng::from_str(body).map_err(|e| {
        let head: String = body.chars().take(80).collect();
        let head = head.replace('\n', " ");
        (
            ErrorKind::Parse,
            format!("不是合法 YAML（{e}）；响应开头：{head}"),
        )
    })?;

    let mapping = match value {
        Yaml::Mapping(m) => m,
        other => {
            let kind = match other {
                Yaml::String(_) => "字符串（可能是 base64 分享链接列表）",
                Yaml::Sequence(_) => "序列",
                _ => "标量",
            };
            return Err((ErrorKind::Structure, format!("顶层不是映射，而是{kind}")));
        }
    };

    let proxies_value = mget(&mapping, "proxies").ok_or_else(|| {
        (
            ErrorKind::Structure,
            "缺少 proxies 键（可能返回的是登录页或错误提示）".to_string(),
        )
    })?;
    let seq = proxies_value
        .as_sequence()
        .ok_or_else(|| (ErrorKind::Structure, "proxies 不是序列".to_string()))?;

    let mut all: Vec<Mapping> = Vec::with_capacity(seq.len());
    for (i, item) in seq.iter().enumerate() {
        let Yaml::Mapping(m) = item else {
            return Err((ErrorKind::Structure, format!("第 {} 个节点不是映射", i + 1)));
        };
        match mstr(m, "name") {
            Some(n) if !n.trim().is_empty() => all.push(m.clone()),
            _ => {
                return Err((
                    ErrorKind::Structure,
                    format!("第 {} 个节点缺少字符串 name", i + 1),
                ));
            }
        }
    }
    Ok(all)
}

/// 按 §4.3 的检查校验响应体。
///
/// 先按 Clash YAML 解析；失败则回退到**分享链接列表**（含整体 base64 编码的形态），
/// 这是机场在 UA 不被识别时最常见的返回格式。
///
/// 失败返回 `(归类, 可读原因)`，调用方据此更新**状态字段**并保留旧缓存。
pub fn validate_and_parse(
    body: &str,
    filter_keywords: &[String],
    filter_enabled: bool,
) -> std::result::Result<ParsedBody, (ErrorKind, String)> {
    // 响应体非空
    if body.trim().is_empty() {
        return Err((ErrorKind::Parse, "响应体为空".to_string()));
    }

    let (all, source_format, skipped_links) = match parse_clash_yaml(body) {
        Ok(proxies) => (proxies, "Clash YAML".to_string(), Vec::new()),
        Err((_, yaml_err)) => {
            // 回退：分享链接列表（可能整体 base64）
            let linked = crate::links::parse_subscription(body);
            if linked.proxies.is_empty() {
                return Err((
                    ErrorKind::Structure,
                    format!(
                        "{yaml_err}；也无法解析为分享链接列表\
                         （共 {} 行有效内容，均无法识别。若是机场按 UA 返回不同格式，\
                         可尝试在通用设置中更换 User-Agent）",
                        linked.total_lines
                    ),
                ));
            }
            let fmt = if crate::links::maybe_unwrap_base64(body).is_some() {
                "分享链接（base64）"
            } else {
                "分享链接（明文）"
            };
            (linked.proxies, fmt.to_string(), linked.skipped)
        }
    };

    // 回退提取订阅信息（信息节点名称）
    let names: Vec<&str> = all.iter().filter_map(|m| mstr(m, "name")).collect();
    let fallback_info = extract_from_names(names);

    // 过滤假节点后有效节点数 > 0
    let mut kept: Vec<Mapping> = Vec::new();
    let mut filtered: Vec<String> = Vec::new();
    for m in all {
        let name = mstr(&m, "name").unwrap_or("").to_string();
        if filter_enabled && is_fake_node(&name, filter_keywords) {
            filtered.push(name);
        } else {
            kept.push(m);
        }
    }

    if kept.is_empty() {
        return Err((
            ErrorKind::Empty,
            if filtered.is_empty() {
                "订阅中没有任何节点".to_string()
            } else {
                format!(
                    "过滤信息节点后没有剩余节点（被过滤：{}）",
                    filtered.join("、")
                )
            },
        ));
    }

    Ok(ParsedBody {
        proxies: kept,
        filtered,
        fallback_info,
        source_format,
        skipped_links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::default_filter_keywords;

    fn kw() -> Vec<String> {
        default_filter_keywords()
    }

    #[test]
    fn accepts_valid_clash_yaml() {
        let body = r#"
proxies:
  - name: "🇭🇰 香港 01"
    type: ss
    server: 1.2.3.4
    port: 443
  - name: "🇯🇵 日本 01"
    type: vmess
    server: 5.6.7.8
    port: 443
"#;
        let p = validate_and_parse(body, &kw(), true).unwrap();
        assert_eq!(p.proxies.len(), 2);
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn rejects_html_login_page() {
        let body = "<!DOCTYPE html><html><body>请先登录</body></html>";
        let (kind, _) = validate_and_parse(body, &kw(), true).unwrap_err();
        // HTML 往往能当标量解析，归类为结构错误
        assert!(matches!(kind, ErrorKind::Parse | ErrorKind::Structure));
    }

    #[test]
    fn rejects_empty_body() {
        let (kind, _) = validate_and_parse("   \n", &kw(), true).unwrap_err();
        assert_eq!(kind, ErrorKind::Parse);
    }

    fn names(p: &ParsedBody) -> Vec<String> {
        p.proxies
            .iter()
            .map(|m| mstr(m, "name").unwrap_or("").to_string())
            .collect()
    }

    fn b64(s: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }

    #[test]
    fn accepts_base64_link_list() {
        // 机场在 UA 不被识别时最常见的返回格式。
        let inner = "ss://YWVzLTI1Ni1nY206cHc=@1.2.3.4:443#HK\n\
                     trojan://pw@t.example.com:443#TJ\n";
        let p = validate_and_parse(&b64(inner), &kw(), true).unwrap();
        assert_eq!(p.proxies.len(), 2);
        assert!(p.source_format.contains("base64"), "{}", p.source_format);
        assert_eq!(names(&p), vec!["HK", "TJ"]);
        assert_eq!(mstr(&p.proxies[0], "type"), Some("ss"));
        assert_eq!(mstr(&p.proxies[1], "type"), Some("trojan"));
    }

    #[test]
    fn accepts_plain_link_list_without_base64() {
        let body = "ss://YWVzLTI1Ni1nY206cHc=@1.2.3.4:443#HK\n";
        let p = validate_and_parse(body, &kw(), true).unwrap();
        assert_eq!(names(&p), vec!["HK"]);
        assert!(p.source_format.contains("明文"), "{}", p.source_format);
    }

    #[test]
    fn unsupported_links_are_skipped_and_reported() {
        let body = "ss://YWVzLTI1Ni1nY206cHc=@1.2.3.4:443#HK\n\
                    ssr://c29tZXRoaW5n\n\
                    wireguard://x@1.2.3.4:51820#WG\n";
        let p = validate_and_parse(body, &kw(), true).unwrap();
        assert_eq!(names(&p), vec!["HK"]);
        assert_eq!(p.skipped_links.len(), 2, "{:?}", p.skipped_links);
        assert!(p.skipped_links.iter().any(|s| s.contains("SSR")));
        assert!(p.skipped_links.iter().any(|s| s.contains("wireguard")));
    }

    #[test]
    fn info_nodes_in_link_list_are_filtered_too() {
        // 信息节点也可能以分享链接形式出现。
        let body = format!(
            "ss://{}@1.2.3.4:443#%E5%89%A9%E4%BD%99%E6%B5%81%E9%87%8F%EF%BC%9A100GB\n\
             ss://{}@5.6.7.8:443#HK\n",
            b64("aes-256-gcm:pw").replace('=', "%3D"),
            b64("aes-256-gcm:pw").replace('=', "%3D"),
        );
        let p = validate_and_parse(&body, &kw(), true).unwrap();
        assert_eq!(p.proxies.len(), 1);
        assert_eq!(p.filtered.len(), 1);
        assert!(p.filtered[0].contains("剩余流量"), "{:?}", p.filtered);
    }

    #[test]
    fn clash_yaml_takes_precedence_over_link_parsing() {
        let body =
            "proxies:\n  - name: from-yaml\n    type: ss\n    server: 1.2.3.4\n    port: 443\n";
        let p = validate_and_parse(body, &kw(), true).unwrap();
        assert_eq!(p.source_format, "Clash YAML");
        assert_eq!(names(&p), vec!["from-yaml"]);
    }

    #[test]
    fn error_mentions_both_attempts_when_nothing_parses() {
        let (kind, msg) = validate_and_parse("hello world", &kw(), true).unwrap_err();
        assert_eq!(kind, ErrorKind::Structure);
        assert!(msg.contains("分享链接"), "{msg}");
        assert!(msg.contains("User-Agent"), "{msg}");
    }

    #[test]
    fn rejects_top_level_sequence() {
        let (kind, msg) = validate_and_parse("- a\n- b\n", &kw(), true).unwrap_err();
        assert_eq!(kind, ErrorKind::Structure);
        assert!(msg.contains("序列"), "{msg}");
    }

    #[test]
    fn rejects_missing_proxies_key() {
        let (kind, msg) = validate_and_parse("port: 7890\nmode: rule\n", &kw(), true).unwrap_err();
        assert_eq!(kind, ErrorKind::Structure);
        assert!(msg.contains("缺少 proxies"), "{msg}");
    }

    #[test]
    fn rejects_proxies_not_sequence() {
        let (kind, _) = validate_and_parse("proxies: {}\n", &kw(), true).unwrap_err();
        assert_eq!(kind, ErrorKind::Structure);
    }

    #[test]
    fn rejects_node_without_name() {
        let (kind, msg) =
            validate_and_parse("proxies:\n  - type: ss\n    server: 1.2.3.4\n", &kw(), true)
                .unwrap_err();
        assert_eq!(kind, ErrorKind::Structure);
        assert!(msg.contains("name"), "{msg}");
    }

    #[test]
    fn zero_nodes_after_filtering_is_empty_kind() {
        let body = r#"
proxies:
  - name: "剩余流量：100 GB"
    type: ss
  - name: "套餐到期：2025-12-31"
    type: ss
"#;
        let (kind, msg) = validate_and_parse(body, &kw(), true).unwrap_err();
        assert_eq!(kind, ErrorKind::Empty);
        assert!(msg.contains("剩余流量"), "{msg}");
    }

    #[test]
    fn fake_nodes_are_filtered_but_reported() {
        let body = r#"
proxies:
  - name: "剩余流量：100 GB"
    type: ss
  - name: "🇭🇰 香港 01"
    type: ss
"#;
        let p = validate_and_parse(body, &kw(), true).unwrap();
        assert_eq!(p.proxies.len(), 1);
        assert_eq!(p.filtered, vec!["剩余流量：100 GB".to_string()]);
    }

    #[test]
    fn filtering_can_be_disabled() {
        let body = "proxies:\n  - name: \"剩余流量：100 GB\"\n    type: ss\n";
        let p = validate_and_parse(body, &kw(), false).unwrap();
        assert_eq!(p.proxies.len(), 1);
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn unknown_fields_are_preserved() {
        let body = r#"
proxies:
  - name: "n1"
    type: vless
    reality-opts:
      public-key: abc
    smux:
      enabled: true
"#;
        let p = validate_and_parse(body, &kw(), true).unwrap();
        let m = &p.proxies[0];
        assert!(mget(m, "reality-opts").is_some());
        assert!(mget(m, "smux").is_some());
    }

    #[test]
    fn fallback_info_extracted_from_names() {
        let body = r#"
proxies:
  - name: "剩余流量：100 GB"
    type: ss
  - name: "套餐到期：2026-01-01"
    type: ss
  - name: "🇭🇰 香港 01"
    type: ss
"#;
        let p = validate_and_parse(body, &kw(), true).unwrap();
        assert_eq!(p.fallback_info.total, Some(100 * 1024 * 1024 * 1024));
        assert!(p.fallback_info.expire.is_some());
    }
}
