//! Cloudflare Workers 入口。
//!
//! 与 native 共用同一份 `web` / `pipeline`：Worker 只负责构造 `AppState`
//! （Storage=DO+R2、Refresher=Fetch、GeoIP=R2）并把请求交给 axum Router。
//!
//! 迁移设计见 `docs/workers-migration.md`。

pub mod store;
pub mod upstream;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Response as HttpResponse, StatusCode, header};
use tower::ServiceExt;
use worker::{Context, Env, HttpRequest};

async fn app_state(env: Env) -> Result<Arc<crate::pipeline::AppState>, String> {
    // 每次请求从 DO 读取快照并构造状态：单人自用、数据量小，
    // 换来的是没有跨 isolate 的过期缓存问题。CSRF 密钥由密码派生，天然稳定。
    build_state(env).await
}

async fn build_state(env: Env) -> Result<Arc<crate::pipeline::AppState>, String> {
    let cfg = crate::config::Config::from_env(&env);
    if cfg.password.is_empty() {
        return Err(
            "未配置 SUBFORGE_PASSWORD（wrangler secret put SUBFORGE_PASSWORD）".to_string(),
        );
    }

    let storage = Arc::new(crate::storage::Storage::new(env.clone()));
    let store = crate::store::StoreHandle::open(storage.clone())
        .await
        .map_err(|e| format!("加载业务数据失败：{e:#}"))?;
    let refresher = Arc::new(
        crate::refresh::Refresher::with_proxy(
            store.clone(),
            cfg.fetch_timeout,
            cfg.fetch_concurrency,
            None,
        )
        .map_err(|e| format!("初始化刷新器失败：{e:#}"))?,
    );
    let geo = Arc::new(
        crate::geoip::GeoResolver::load(&storage)
            .await
            .map_err(|e| format!("加载 GeoIP 库失败：{e:#}"))?,
    );
    let geoip = Arc::new(
        crate::geoip_update::GeoipUpdater::with_proxy(store.clone(), geo.clone(), None)
            .map_err(|e| format!("初始化 GeoIP 更新器失败：{e:#}"))?,
    );
    geoip.reload_status().await;

    // 多 isolate 下 CSRF 密钥必须稳定：优先 secret，其次由密码派生。
    let csrf_secret = env
        .secret("SUBFORGE_CSRF_SECRET")
        .ok()
        .map(|s| s.to_string())
        .or_else(|| env.var("SUBFORGE_CSRF_SECRET").ok().map(|v| v.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            use sha2::Digest;
            format!("{:x}", sha2::Sha256::digest(cfg.password.as_bytes()))
        });

    Ok(Arc::new(crate::pipeline::AppState {
        cfg,
        store,
        refresher,
        geo,
        geoip,
        csrf_secret,
        static_dir: std::path::PathBuf::new(),
        render_cache: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        node_names_cache: tokio::sync::RwLock::new(None),
    }))
}

#[worker::event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> worker::Result<HttpResponse<Body>> {
    console_error_panic_hook::set_once();
    let state = app_state(env).await.map_err(worker::Error::RustError)?;
    if req.uri().path() == "/admin/store" {
        return handle_store_admin(state, req).await;
    }
    let response = match crate::web::router(state).oneshot(req).await {
        Ok(response) => response,
        Err(infallible) => match infallible {},
    };
    Ok(response)
}

/// Workers 专用的整库导入/导出：native 的 `data/store.yaml` 靠它迁移上来。
///
/// 共用 web 路由没有这个能力（native 也从不通过网页整库覆盖），因此单独处理，
/// 鉴权与共享层一致（Basic Auth，用户名 `admin`）。
async fn handle_store_admin(
    state: Arc<crate::pipeline::AppState>,
    req: HttpRequest,
) -> worker::Result<HttpResponse<Body>> {
    if !crate::auth::basic_auth_ok(req.headers(), &state.cfg.password) {
        return Ok(text_response(
            StatusCode::UNAUTHORIZED,
            "需要认证：请提供 HTTP Basic Auth 凭据",
        ));
    }
    match *req.method() {
        axum::http::Method::GET => match state.store.storage().get_string("store.yaml").await {
            Ok(Some(yaml)) => Ok(text_response_with_type(
                StatusCode::OK,
                "text/yaml; charset=utf-8",
                yaml,
            )),
            Ok(None) => Ok(text_response(StatusCode::NOT_FOUND, "没有 store.yaml")),
            Err(e) => Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("读取失败：{e:#}"),
            )),
        },
        axum::http::Method::POST => {
            let (_parts, body) = req.into_parts();
            let bytes = match axum::body::to_bytes(Body::new(body), 10 * 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    return Ok(text_response(
                        StatusCode::BAD_REQUEST,
                        &format!("读取请求体失败：{e}"),
                    ));
                }
            };
            let text = match String::from_utf8(bytes.to_vec()) {
                Ok(text) => text,
                Err(_) => {
                    return Ok(text_response(
                        StatusCode::BAD_REQUEST,
                        "请求体不是合法 UTF-8",
                    ));
                }
            };
            let parsed: crate::store::Store = match serde_yaml_ng::from_str(&text) {
                Ok(store) => store,
                Err(e) => {
                    return Ok(text_response(
                        StatusCode::BAD_REQUEST,
                        &format!("不是合法 YAML：{e}"),
                    ));
                }
            };
            let parsed = match crate::store::migrate::migrate(parsed) {
                Ok(store) => store,
                Err(e) => {
                    return Ok(text_response(
                        StatusCode::BAD_REQUEST,
                        &format!("数据迁移失败：{e:#}"),
                    ));
                }
            };
            if let Err(e) = crate::validate::validate_structure(&parsed) {
                return Ok(text_response(
                    StatusCode::BAD_REQUEST,
                    &format!("数据校验失败：{e:#}"),
                ));
            }
            match state
                .store
                .update(None, |draft| {
                    *draft = parsed.clone();
                    Ok(())
                })
                .await
            {
                Ok((_, revision)) => Ok(text_response(
                    StatusCode::OK,
                    &format!("已导入，revision={revision}"),
                )),
                Err(e) => Ok(text_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("写入失败：{e:#}"),
                )),
            }
        }
        _ => Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "只支持 GET / POST",
        )),
    }
}

fn text_response(status: StatusCode, body: &str) -> HttpResponse<Body> {
    text_response_with_type(status, "text/plain; charset=utf-8", body.to_string())
}

fn text_response_with_type(
    status: StatusCode,
    content_type: &'static str,
    body: String,
) -> HttpResponse<Body> {
    let mut resp = HttpResponse::new(Body::from(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    resp
}

/// Cron 触发：刷新到期上游，并按设置更新 GeoIP 库。
///
/// 与 native 的 `spawn_periodic` 语义一致（只处理到期项）；Workers 上没有常驻进程，
/// 由 `wrangler.jsonc` 的 `triggers.crons` 驱动。
#[worker::event(scheduled)]
async fn scheduled(event: worker::ScheduledEvent, env: Env, _ctx: worker::ScheduleContext) {
    console_error_panic_hook::set_once();
    if let Err(e) = run_scheduled(event, env).await {
        tracing::error!("cron 任务失败：{e}");
    }
}

async fn run_scheduled(event: worker::ScheduledEvent, env: Env) -> worker::Result<()> {
    let state = app_state(env).await.map_err(worker::Error::RustError)?;
    tracing::info!("cron 触发：{}", event.cron());

    let interval = state.cfg.refresh_interval;
    let outcomes = state.refresher.refresh_due(interval).await;
    for outcome in &outcomes {
        tracing::info!(
            "cron 刷新上游 {}：{:?}，{} 个节点",
            outcome.upstream_id,
            outcome.status,
            outcome.node_count
        );
    }

    let snapshot = state.store.snapshot().await;
    let settings = snapshot.settings.geoip.clone();
    drop(snapshot);
    let status = state.geoip.status();
    if geoip_due(&status, &settings, chrono::Utc::now()) {
        match state.geoip.update("cron", false).await {
            Ok(result) => tracing::info!("cron 更新 GeoIP：{}", result.message),
            Err(e) => tracing::warn!("cron 更新 GeoIP 失败：{e:#}"),
        }
    }
    Ok(())
}

fn geoip_due(
    status: &crate::geoip_update::GeoipStatus,
    settings: &crate::store::model::GeoipSettings,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !settings.auto_update {
        return false;
    }
    let Some(last) = status.last_success_at else {
        return true;
    };
    let interval = chrono::Duration::days(i64::from(settings.interval_days_clamped()));
    now.signed_duration_since(last) >= interval
}
