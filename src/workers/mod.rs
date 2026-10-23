//! Cloudflare Workers 入口（MVP）。
//!
//! 路由：
//! - `GET /healthz`：健康检查（公开）；
//! - `GET /sub/{slug}`：导出 Clash YAML（Basic Auth 或方案令牌）；
//! - `GET /admin/store`：导出当前 store YAML（Basic Auth，用于从 native 迁移/备份）；
//! - `POST /admin/store`：导入 store YAML（Basic Auth，DO 单写者提交）；
//! - `GET /`：只读概览页。
//!
//! 存储：store 在 Durable Object「Core」里（单写者 + revision），上游缓存在 R2。
//! 迁移设计见 `docs/workers-migration.md`。

pub mod auth;
pub mod cache;
pub mod export;
pub mod store;
pub mod upstream;

use std::collections::HashMap;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Response as HttpResponse, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use tower::ServiceExt;
use worker::send::SendFuture;
use worker::{Context, Env, HttpRequest};

#[derive(Clone)]
pub struct AppState {
    pub env: Env,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/sub/{slug}", get(subscribe))
        .route("/admin/store", get(admin_store_get).post(admin_store_post))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

// ---------------------------------------------------------------- 页面

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    SendFuture::new(async move {
        if !crate::workers::auth::basic_ok(&state.env, &headers) {
            return unauthorized();
        }
        match index_body(&state.env).await {
            Ok(html) => text(StatusCode::OK, "text/html; charset=utf-8", html),
            Err(e) => text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain; charset=utf-8",
                format!("读取数据失败：{e}"),
            ),
        }
    })
    .await
}

async fn index_body(env: &Env) -> worker::Result<String> {
    let store = crate::workers::store::load(env).await?;
    let mut rows = String::new();
    for profile in &store.profiles {
        rows.push_str(&format!(
            "<li><a href=\"/sub/{}\">{}</a> <code>{}</code></li>",
            esc(&profile.slug),
            esc(&profile.name),
            esc(&profile.slug)
        ));
    }
    if rows.is_empty() {
        rows.push_str(
            "<li>还没有方案：先通过 <code>POST /admin/store</code> 导入 native 的 store.yaml</li>",
        );
    }
    Ok(format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>SubForge.rs · Workers</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:44rem;margin:3rem auto;padding:0 1rem;line-height:1.6}}\
         code{{background:#f3f3f3;padding:.1rem .3rem;border-radius:.2rem}}</style></head><body>\
         <h1>SubForge.rs · Cloudflare Workers</h1>\
         <p>数据版本 <code>rev {revision}</code> · 上游 {upstreams} · 分组 {groups} · 规则 {rules} · 方案 {profiles}</p>\
         <p>GeoIP 暂未启用（仅按节点名判定地区）。</p>\
         <h2>方案</h2><ul>{rows}</ul>\
         <p><a href=\"/admin/store\">导出 store.yaml</a> · <a href=\"/healthz\">/healthz</a></p>\
         </body></html>",
        revision = store.revision,
        upstreams = store.upstreams.len(),
        groups = store.groups.len(),
        rules = store.rules.len(),
        profiles = store.profiles.len(),
    ))
}

async fn subscribe(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    SendFuture::new(async move {
        let env = state.env;
        let store = match crate::workers::store::load(&env).await {
            Ok(store) => store,
            Err(e) => {
                return text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "text/plain; charset=utf-8",
                    format!("读取数据失败：{e}"),
                );
            }
        };
        let Some(profile) = store.profile(&slug) else {
            return text(
                StatusCode::NOT_FOUND,
                "text/plain; charset=utf-8",
                format!("方案「{slug}」不存在"),
            );
        };
        let token = query.get("token").map(String::as_str);
        let authorized = crate::workers::auth::basic_ok(&env, &headers)
            || crate::workers::auth::token_ok(profile.token.as_deref(), token);
        if !authorized {
            return unauthorized();
        }
        match crate::workers::export::export(&env, &slug).await {
            Ok(outcome) => {
                let mut resp = text(StatusCode::OK, "text/yaml; charset=utf-8", outcome.yaml);
                if !outcome.warnings.is_empty() {
                    let joined = outcome.warnings.join(" | ");
                    if let Ok(value) = HeaderValue::from_str(&joined) {
                        resp.headers_mut().insert(
                            header::HeaderName::from_static("x-subforge-warnings"),
                            value,
                        );
                    }
                }
                resp
            }
            Err(e) => text(
                StatusCode::BAD_GATEWAY,
                "text/plain; charset=utf-8",
                format!("导出失败：{e}"),
            ),
        }
    })
    .await
}

// ---------------------------------------------------------------- 管理接口

async fn admin_store_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    SendFuture::new(async move {
        if !crate::workers::auth::basic_ok(&state.env, &headers) {
            return unauthorized();
        }
        match crate::workers::store::load_yaml(&state.env).await {
            Ok(yaml) => text(StatusCode::OK, "text/yaml; charset=utf-8", yaml),
            Err(e) => text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain; charset=utf-8",
                format!("读取失败：{e}"),
            ),
        }
    })
    .await
}

async fn admin_store_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    SendFuture::new(async move {
        if !crate::workers::auth::basic_ok(&state.env, &headers) {
            return unauthorized();
        }
        match crate::workers::store::save_yaml(&state.env, &body).await {
            Ok(revision) => text(
                StatusCode::OK,
                "text/plain; charset=utf-8",
                format!("已保存，revision={revision}"),
            ),
            Err(e) => text(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                format!("保存失败：{e}"),
            ),
        }
    })
    .await
}

// ---------------------------------------------------------------- 响应辅助

fn text(status: StatusCode, content_type: &'static str, body: impl Into<String>) -> Response {
    let mut resp = Response::new(Body::from(body.into()));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

fn unauthorized() -> Response {
    let mut resp = text(
        StatusCode::UNAUTHORIZED,
        "text/plain; charset=utf-8",
        "需要认证：请提供 HTTP Basic Auth 凭据",
    );
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"SubForge.rs\", charset=\"UTF-8\""),
    );
    resp
}

fn esc(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------- Worker 入口

#[worker::event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> worker::Result<HttpResponse<Body>> {
    console_error_panic_hook::set_once();
    let response = match app(AppState { env }).oneshot(req).await {
        Ok(response) => response,
        Err(infallible) => match infallible {},
    };
    Ok(response)
}
