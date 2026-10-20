//! Web 层：前后端不分离的服务端渲染（AGENTS.md §2）。
//!
//! - 页面由 axum handler 渲染 HTML 返回，模板引擎 askama；
//! - 交互模型为「表单 POST → 303 重定向」（PRG），不为页面另写 JSON API；
//! - 不引入任何前端框架或打包器。

pub mod pages;

use crate::auth::{self, SID_COOKIE};
use crate::pipeline::AppState;
use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use std::sync::Arc;
use tower_http::services::ServeDir;

// ---------------------------------------------------------------- 错误

/// 统一错误响应。
pub struct AppError(pub anyhow::Error);

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        tracing::warn!("请求处理失败：{msg}");
        // 错误页也套用页面骨架，用户不会掉出导航上下文，并能直接返回上一页重试。
        let body = match (ErrorTpl {
            message: msg.clone(),
            nav: "",
        })
        .render()
        {
            Ok(html) => html,
            Err(e) => {
                tracing::error!("错误页渲染失败：{e}");
                format!(
                    "<!doctype html><meta charset=\"utf-8\"><title>出错了</title>\
                     <h1>出错了</h1><pre>{}</pre><p><a href=\"/\">返回首页</a></p>",
                    html_escape(&msg)
                )
            }
        };
        (StatusCode::BAD_REQUEST, axum::response::Html(body)).into_response()
    }
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------- 模板

/// 统一错误页。套用页面骨架，避免用户掉出导航上下文。
#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTpl {
    message: String,
    nav: &'static str,
}

/// askama 模板 → HTTP 响应。
pub struct HtmlTemplate<T>(pub T);

impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(body) => axum::response::Html(body).into_response(),
            Err(e) => {
                tracing::error!("模板渲染失败：{e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("模板渲染失败：{e}"),
                )
                    .into_response()
            }
        }
    }
}

// ---------------------------------------------------------------- 请求上下文

/// 由认证中间件注入，供 handler 取用 CSRF token。
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub sid: String,
    pub csrf: String,
}

impl RequestContext {
    pub fn new(secret: &str, sid: &str) -> Self {
        Self {
            sid: sid.to_string(),
            csrf: auth::csrf_token(secret, sid),
        }
    }
}

// ---------------------------------------------------------------- 表单回显

/// 字段级校验错误。`field` 与控件的 `name` 一致，`label` 用于错误摘要。
#[derive(Debug, Clone)]
pub struct FieldError {
    pub field: String,
    pub label: String,
    pub message: String,
}

/// 表单提交失败时的回显。
///
/// 校验失败时**不重定向、也不丢弃输入**：把用户填的内容原样带回去，
/// 重新渲染同一个页面，并把错误挂到对应字段（规范 §6「表单」、
/// §7.2「请求失败：保留可恢复内容」）。
///
/// `scope` 区分同一页上的多个表单：空串是页面的主（新建）表单，
/// 其余取值标识被编辑的对象（上游 id / 分组名 / 地区名 / 规则 id）。
#[derive(Debug, Default, Clone)]
pub struct FormEcho {
    pub scope: String,
    /// 保序、**不去重**的原始键值对。
    ///
    /// 表单里的复选框组（`upstreams` / `groups` / `rules`）靠重复键表达多选，
    /// 用 map 存会折叠成最后一个值，回填时就只剩一项了。
    values: Vec<(String, String)>,
    errors: Vec<FieldError>,
}

impl FormEcho {
    /// 记录一次字段级校验失败。作用域为空串表示页面的主表单。
    pub fn fail(
        form: &FormData,
        scope: &str,
        field: &str,
        label: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            scope: scope.to_string(),
            values: form.pairs.clone(),
            errors: vec![FieldError {
                field: field.to_string(),
                label: label.to_string(),
                message: message.into(),
            }],
        }
    }

    /// 回填字段值：作用域不匹配时返回 `fallback`（通常是已存储的值）。
    pub fn value(&self, scope: &str, key: &str, fallback: &str) -> String {
        if self.scope != scope {
            return fallback.to_string();
        }
        // 取首个匹配项，与 `FormData::get` 的语义保持一致。
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| fallback.to_string())
    }

    /// 回填多值字段（复选框组，如表单里重复出现的 `upstreams`）。
    ///
    /// 作用域不匹配时返回空集合，调用方据此退回「按已存储状态渲染」。
    /// 作用域匹配但集合为空，表示用户把所有选项都取消了——这是有效输入，不能当成"没有回填"。
    pub fn values(&self, scope: &str, key: &str) -> Vec<String> {
        if self.scope != scope {
            return Vec::new();
        }
        self.values
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// 该作用域下的某字段是否校验失败。
    pub fn invalid(&self, scope: &str, field: &str) -> bool {
        self.scope == scope && self.errors.iter().any(|e| e.field == field)
    }

    /// 该作用域下某字段的错误文案。
    pub fn error(&self, scope: &str, field: &str) -> String {
        if self.scope != scope {
            return String::new();
        }
        self.errors
            .iter()
            .find(|e| e.field == field)
            .map(|e| e.message.clone())
            .unwrap_or_default()
    }

    /// 下拉框回填：作用域不匹配时用 `fallback`，匹配时以提交值为准。
    pub fn selected(&self, scope: &str, key: &str, fallback: &str, option: &str) -> bool {
        self.value(scope, key, fallback) == option
    }

    /// 复选框回填：作用域不匹配时用 `fallback`；匹配但键缺失表示用户没勾。
    ///
    /// 用 `Borrow<bool>` 是为了同时接受模板里按值取出的 `bool` 与按引用取出的 `&bool`。
    pub fn checked(
        &self,
        scope: &str,
        key: &str,
        fallback: impl std::borrow::Borrow<bool>,
    ) -> bool {
        if self.scope != scope {
            return *fallback.borrow();
        }
        match self.values.iter().find(|(k, _)| k == key) {
            Some((_, v)) => matches!(v.as_str(), "on" | "true" | "1" | "yes"),
            None => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn errors(&self) -> &[FieldError] {
        &self.errors
    }
}

// ---------------------------------------------------------------- 表单解析

/// 极简表单解析：支持重复键（多选）与百分号编码。
#[derive(Debug, Default, Clone)]
pub struct FormData {
    pairs: Vec<(String, String)>,
}

impl FormData {
    pub fn parse(body: &str) -> Self {
        let mut pairs = Vec::new();
        for chunk in body.split('&') {
            if chunk.is_empty() {
                continue;
            }
            let (k, v) = match chunk.split_once('=') {
                Some((k, v)) => (k, v),
                None => (chunk, ""),
            };
            pairs.push((percent_decode(k), percent_decode(v)));
        }
        Self { pairs }
    }

    pub fn from_bytes(bytes: &Bytes) -> Self {
        Self::parse(&String::from_utf8_lossy(bytes))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn get_owned(&self, key: &str) -> String {
        self.get(key).unwrap_or("").trim().to_string()
    }

    pub fn get_all(&self, key: &str) -> Vec<String> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect()
    }

    /// 复选框组：同一 key 的多个值，或单个逗号分隔值。
    pub fn get_list(&self, key: &str) -> Vec<String> {
        let mut out = self.get_all(key);
        if out.len() == 1 && out[0].contains(',') {
            out = out[0]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        out
    }

    pub fn get_bool(&self, key: &str) -> bool {
        matches!(
            self.get(key),
            Some("on") | Some("true") | Some("1") | Some("yes")
        )
    }

    pub fn get_usize(&self, key: &str) -> Option<usize> {
        self.get(key).and_then(|v| v.trim().parse().ok())
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.trim().parse().ok())
    }

    /// 校验 CSRF token。所有修改类操作都必须先调用。
    pub fn verify_csrf(&self, state: &AppState, ctx: &RequestContext) -> Result<(), AppError> {
        let submitted = self.get("csrf").unwrap_or("");
        if !auth::verify_csrf(&state.csrf_secret, &ctx.sid, submitted) {
            return Err(AppError(anyhow::anyhow!("CSRF 校验失败：请刷新页面后重试")));
        }
        Ok(())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------- 认证中间件

async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers().clone();

    if !auth::basic_auth_ok(&headers, &state.cfg.password) {
        return unauthorized();
    }

    // 修改类操作必须校验来源（§7.1）
    if req.method() == axum::http::Method::POST && !auth::origin_ok(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "来源检查失败：Origin/Referer 与 Host 不一致",
        )
            .into_response();
    }

    // 确保存在会话 cookie（CSRF token 由它派生）
    let existing = auth::cookie(&headers, SID_COOKIE);
    let sid = existing.clone().unwrap_or_else(|| auth::random_hex(16));
    req.extensions_mut()
        .insert(RequestContext::new(&state.csrf_secret, &sid));

    let mut resp = next.run(req).await;
    if existing.is_none() {
        if let Ok(value) = HeaderValue::from_str(&format!(
            "{SID_COOKIE}={sid}; Path=/; SameSite=Strict; HttpOnly"
        )) {
            resp.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    resp
}

fn unauthorized() -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        "需要认证：请提供 HTTP Basic Auth 凭据",
    )
        .into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"SubForge.rs\", charset=\"UTF-8\""),
    );
    resp
}

/// 供 `/sub/{slug}` 使用：Basic Auth 或订阅令牌任一通过。
pub fn subscribe_authorized(
    headers: &HeaderMap,
    state: &AppState,
    profile_token: Option<&str>,
    query_token: Option<&str>,
) -> bool {
    if auth::basic_auth_ok(headers, &state.cfg.password) {
        return true;
    }
    let Some(expected) = profile_token else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }
    let Some(got) = query_token else {
        return false;
    };
    // 恒定时间比较
    got.len() == expected.len()
        && got
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

// ---------------------------------------------------------------- 路由

pub fn router(state: Arc<AppState>) -> Router {
    let management = Router::new()
        .route("/", get(pages::index))
        .route(
            "/upstreams",
            get(pages::upstreams_page).post(pages::upstreams_save),
        )
        .route("/upstreams/refresh-all", post(pages::upstreams_refresh_all))
        .route("/upstreams/{id}/refresh", post(pages::upstream_refresh))
        .route("/upstreams/{id}/accept", post(pages::upstream_accept))
        .route("/upstreams/{id}/delete", post(pages::upstream_delete))
        .route("/groups", get(pages::groups_page).post(pages::groups_save))
        .route(
            "/groups/preset-catch-all",
            post(pages::groups_preset_catch_all),
        )
        .route("/groups/rename", post(pages::groups_rename))
        .route("/groups/move", post(pages::groups_move))
        .route("/groups/reorder", post(pages::groups_reorder))
        .route("/groups/delete", post(pages::groups_delete))
        .route("/rules", get(pages::rules_page).post(pages::rules_save))
        .route("/rules/import", post(pages::rules_import))
        .route("/rules/move", post(pages::rules_move))
        .route("/rules/{id}/delete", post(pages::rules_delete))
        .route(
            "/nodes",
            get(pages::manual_nodes_page).post(pages::manual_nodes_save),
        )
        .route("/nodes/delete", post(pages::manual_nodes_delete))
        .route(
            "/aliases",
            get(pages::aliases_page).post(pages::aliases_save),
        )
        .route("/aliases/delete", post(pages::aliases_delete))
        .route(
            "/general",
            get(pages::general_page).post(pages::general_save),
        )
        .route("/general/geoip/settings", post(pages::geoip_settings_save))
        .route("/general/geoip/download", post(pages::geoip_download))
        .route(
            "/profiles/new",
            get(pages::profile_new).post(pages::profile_create),
        )
        .route(
            "/profiles/{slug}",
            get(pages::profile_page).post(pages::profile_save),
        )
        .route("/profiles/{slug}/preview", post(pages::profile_preview))
        .route("/profiles/{slug}/delete", post(pages::profile_delete))
        .route("/backups", get(pages::backups_page))
        .route("/backups/restore", post(pages::backups_restore))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    let static_dir = state.static_dir.clone();
    Router::new()
        .merge(management)
        .route("/sub/{slug}", get(pages::subscribe))
        .route("/healthz", get(pages::healthz))
        .nest_service("/static", static_assets(static_dir))
        .with_state(state)
}

/// 静态资源：每次请求都要求浏览器重新验证（`Cache-Control: no-cache`）。
///
/// 页面脚本/样式改动后，旧的 `app.js` 缓存会让「复制链接」这类新按钮**可见但无行为**。
/// 这里不做 `no-store`，保留 304 复用，只强制重新验证。
fn static_assets(dir: std::path::PathBuf) -> Router {
    Router::new()
        .fallback_service(ServeDir::new(dir))
        .layer(axum::middleware::from_fn(static_no_cache))
}

async fn static_no_cache(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

/// 便捷：303 重定向（PRG 模式）。
pub fn see_other(location: &str) -> Response {
    Redirect::to(location).into_response()
}

// ---------------------------------------------------------------- 订阅链接

/// 规范化 BASEURL：空串允许（页面展示相对路径）；否则必须是 http(s) URL。
///
/// 末尾斜杠会被去掉，避免拼出 `//sub/...`。
pub fn normalize_base_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|e| format!("不是合法的 URL：{e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(format!(
                "必须以 http:// 或 https:// 开头，当前是 {scheme}://"
            ));
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("不要包含用户名或密码".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("不要包含查询串或锚点".to_string());
    }
    Ok(trimmed.to_string())
}

/// 拼订阅链接：`[BASEURL]/sub/<slug>?token=...&download=1`。
///
/// - `base_url` 为空 → 返回相对路径（同源浏览器可用，复制给客户端前应配置 BASEURL）。
/// - `token` 做百分号编码，允许用户设置含特殊字符的令牌。
pub fn sub_url(base_url: &str, slug: &str, token: Option<&str>, download: bool) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let mut url = format!("{base}/sub/{slug}");
    let mut params: Vec<String> = Vec::new();
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        params.push(format!("token={}", url_encode_component(token)));
    }
    if download {
        params.push("download=1".to_string());
    }
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }
    url
}

/// 查询串组件编码（unreserved 字符保持原样）。
fn url_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 下载文件名：用**方案名称**生成，保留中文/emoji，去掉路径分隔符等非法字符，
/// 并确保以 `.yaml` 结尾（名称本身已是 `.yaml` / `.yml` 时不重复追加）。
///
/// 名称为空或清理后为空时退回 `fallback`（通常是 slug），最终兜底 `config.yaml`。
pub fn safe_yaml_filename(name: &str, fallback: &str) -> String {
    let mut stem: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    stem = stem.trim().trim_matches('.').trim().to_string();

    // 去掉名称里已有的 YAML 扩展名，避免出现 `home.yaml.yaml`。
    let lower = stem.to_ascii_lowercase();
    for ext in [".yaml", ".yml"] {
        if lower.ends_with(ext) {
            stem.truncate(stem.len() - ext.len());
            break;
        }
    }
    stem = stem.trim().trim_matches('.').trim().to_string();

    if stem.is_empty() {
        stem = fallback.trim().to_string();
    }
    if stem.is_empty() {
        stem = "config".to_string();
    }
    // 限制长度（按字符计数，避免截断 UTF-8）。
    if stem.chars().count() > 80 {
        stem = stem.chars().take(80).collect::<String>().trim().to_string();
    }
    format!("{stem}.yaml")
}

/// 生成 RFC 6266 / 5987 的 `Content-Disposition`：
/// `filename` 用安全 ASCII 回退（slug），`filename*` 携带 UTF-8 方案名。
pub fn content_disposition(kind: &str, filename: &str, fallback_stem: &str) -> String {
    let fallback = ascii_file_stem(fallback_stem);
    format!(
        "{kind}; filename=\"{fallback}.yaml\"; filename*=UTF-8''{}",
        url_encode_component(filename)
    )
}

fn ascii_file_stem(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    out = out.trim_matches('.').to_string();
    if out.is_empty() {
        out = "config".to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_parsing_handles_repeats_and_encoding() {
        let f = FormData::parse("a=1&b=x+y&b=z&c=%E4%B8%AD%E6%96%87");
        assert_eq!(f.get("a"), Some("1"));
        assert_eq!(f.get_all("b"), vec!["x y".to_string(), "z".to_string()]);
        assert_eq!(f.get("c"), Some("中文"));
    }

    #[test]
    fn comma_list_fallback() {
        let f = FormData::parse("groups=a%2Cb%2Cc");
        assert_eq!(
            f.get_list("groups"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn bool_and_number_parsing() {
        let f = FormData::parse("flag=on&n=42&empty=");
        assert!(f.get_bool("flag"));
        assert_eq!(f.get_usize("n"), Some(42));
        assert_eq!(f.get_owned("empty"), "");
    }

    #[test]
    fn form_echo_keeps_every_value_of_repeated_keys() {
        // 复选框组靠**重复键**表达多选。回显若用 map 存会折叠成最后一个值，
        // 方案页「勾选了哪些上游/分组」就会只剩一项。
        let f = FormData::parse("upstreams=a&upstreams=b&upstreams=c&name=x");
        let echo = FormEcho::fail(&f, "", "name", "名称", "不能为空");
        assert_eq!(echo.values("", "upstreams"), vec!["a", "b", "c"]);
        assert_eq!(echo.value("", "name", ""), "x");
        // 作用域不匹配 → 不回填，调用方继续用已存储的值。
        assert!(echo.values("import", "upstreams").is_empty());
        assert_eq!(echo.value("import", "name", "stored"), "stored");
        // 「一个都没勾」是有效输入：键不存在得到空集合，
        // 不能与「本次没有回显」混为一谈（后者由 is_empty() 判定）。
        assert!(echo.values("", "groups").is_empty());
        assert!(!echo.is_empty());
    }

    #[test]
    fn html_escaping() {
        assert_eq!(
            html_escape("<a href=\"x\">&"),
            "&lt;a href=&quot;x&quot;&gt;&amp;"
        );
    }

    #[test]
    fn base_url_is_normalized_and_validated() {
        assert_eq!(normalize_base_url("  ").unwrap(), "");
        assert_eq!(
            normalize_base_url("https://clash.example.com///").unwrap(),
            "https://clash.example.com"
        );
        assert_eq!(
            normalize_base_url("http://192.168.1.5:8080/clash").unwrap(),
            "http://192.168.1.5:8080/clash"
        );
        assert!(normalize_base_url("clash.example.com").is_err());
        assert!(normalize_base_url("ftp://example.com").is_err());
        assert!(normalize_base_url("https://example.com/?a=1").is_err());
        assert!(normalize_base_url("https://user:pass@example.com").is_err());
    }

    #[test]
    fn subscription_urls_include_token_and_download_flag() {
        // 未配置 BASEURL：相对路径，仍可直接访问。
        assert_eq!(sub_url("", "b64", None, false), "/sub/b64");
        assert_eq!(sub_url("", "b64", Some("abc"), false), "/sub/b64?token=abc");
        assert_eq!(
            sub_url("", "b64", Some("abc"), true),
            "/sub/b64?token=abc&download=1"
        );
        // 配了 BASEURL：完整链接，末尾斜杠不会产生 //。
        assert_eq!(
            sub_url("https://clash.example.com/", "b64", None, true),
            "https://clash.example.com/sub/b64?download=1"
        );
        // 令牌里的特殊字符必须编码，否则查询串会被截断。
        assert_eq!(
            sub_url("https://x.test", "b64", Some("a+b/c&d"), false),
            "https://x.test/sub/b64?token=a%2Bb%2Fc%26d"
        );
        // 空令牌视为未设置。
        assert_eq!(
            sub_url("https://x.test", "b64", Some(""), false),
            "https://x.test/sub/b64"
        );
    }

    #[test]
    fn yaml_filename_uses_profile_name_and_sanitizes() {
        assert_eq!(safe_yaml_filename("base64方案", "b64"), "base64方案.yaml");
        assert_eq!(safe_yaml_filename("  home.yaml  ", "b64"), "home.yaml");
        assert_eq!(safe_yaml_filename("a/b:c*?", "b64"), "a_b_c__.yaml");
        assert_eq!(safe_yaml_filename("", "b64"), "b64.yaml");
        assert_eq!(safe_yaml_filename("...", "b64"), "b64.yaml");
        // 超长名称按字符截断，不切断 UTF-8。
        let long = "方".repeat(100);
        let name = safe_yaml_filename(&long, "b64");
        assert!(name.ends_with(".yaml"));
        assert_eq!(name.chars().count(), 85); // 80 个字符 + ".yaml"
    }

    #[test]
    fn content_disposition_has_ascii_fallback_and_utf8_filename() {
        let value = content_disposition("attachment", "base64方案.yaml", "b64");
        assert!(value.is_ascii(), "header 值必须是 ASCII：{value}");
        assert_eq!(
            value,
            "attachment; filename=\"b64.yaml\"; filename*=UTF-8''base64%E6%96%B9%E6%A1%88.yaml"
        );
        assert_eq!(
            content_disposition("inline", "home.yaml", "home-slug"),
            "inline; filename=\"home-slug.yaml\"; filename*=UTF-8''home.yaml"
        );
    }
}
