//! Cloudflare Workers 入口（P0 骨架）。
//!
//! 设计见 `docs/workers-migration.md`。当前阶段只验证：
//! - crate 能以 wasm32 编译；
//! - worker `http` 特性 + axum Router 能直接对接 `#[event(fetch)]`；
//! - askama 模板在 wasm 上可用。
//!
//! 数据层（DO/R2）、认证与导出链路在后续阶段接入。

use askama::Template;
use axum::{Router, body::Body, http::Response as HttpResponse, response::Html, routing::get};
use tower::ServiceExt;
use worker::*;

/// P0 首页：说明当前迁移阶段。
#[derive(Template)]
#[template(
    source = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>SubForge.rs · Workers</title>
  <style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;line-height:1.6;color:#222}h1{font-size:1.5rem}code{background:#f3f3f3;padding:.1rem .3rem;border-radius:.2rem}</style>
</head>
<body>
  <h1>SubForge.rs · Cloudflare Workers</h1>
  <p>迁移骨架（P0）已就位。当前版本 <code>{{ version }}</code>。</p>
  <p>数据层（Durable Object / R2）尚未接入，管理页与订阅导出将在后续阶段提供。</p>
  <p><a href="/healthz">/healthz</a></p>
</body>
</html>"#,
    ext = "html"
)]
struct IndexTemplate {
    version: &'static str,
}

/// 应用路由。后续阶段会在这里合并 native `web::router` 的等价路由。
pub fn app() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .fallback(not_found)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn index() -> Html<String> {
    let version = env!("CARGO_PKG_VERSION");
    match (IndexTemplate { version }).render() {
        Ok(html) => Html(html),
        Err(e) => Html(format!("模板渲染失败：{e}")),
    }
}

async fn not_found() -> (axum::http::StatusCode, &'static str) {
    (axum::http::StatusCode::NOT_FOUND, "not found")
}

#[event(fetch)]
async fn fetch(req: HttpRequest, _env: Env, _ctx: Context) -> Result<HttpResponse<Body>> {
    console_error_panic_hook::set_once();
    // `Router` 的 `Service::call` 只会返回 `Infallible`，这里是穷尽的空匹配。
    let response = match app().oneshot(req).await {
        Ok(response) => response,
        Err(infallible) => match infallible {},
    };
    Ok(response)
}
