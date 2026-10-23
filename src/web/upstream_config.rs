//! 上游缓存原文预览与手动编辑页。

use super::{AppError, FormData, FormEcho, HtmlTemplate, RequestContext, see_other};
use crate::cache::{self, CacheEnvelope};
use crate::pipeline::AppState;
use crate::store::valid_slug;
use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Default, Deserialize)]
pub struct ConfigQuery {
    pub saved: Option<String>,
}

#[derive(Template)]
#[template(path = "upstream_config.html")]
pub struct UpstreamConfigTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub id: String,
    pub name: String,
    pub revision: u64,
    pub fingerprint: String,
    pub body: String,
    pub has_cache: bool,
    pub cache_error: String,
    pub source_format: String,
    pub node_count: usize,
    pub skipped_links: usize,
    pub filtered_count: usize,
    pub last_success: String,
    pub manual_saved_at: String,
    pub saved: bool,
    pub form: FormEcho,
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn fmt_ts(value: Option<chrono::DateTime<Utc>>) -> String {
    value
        .map(|v| v.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "—".to_string())
}

#[allow(clippy::too_many_arguments)]
fn template(
    ctx: &RequestContext,
    id: String,
    name: String,
    revision: u64,
    fingerprint: String,
    body: String,
    env: Option<&CacheEnvelope>,
    cache_error: String,
    saved: bool,
    form: FormEcho,
) -> UpstreamConfigTpl {
    let (source_format, node_count, skipped_links, last_success, manual_saved_at) = env
        .map(|e| {
            (
                e.meta.source_format.clone().unwrap_or_default(),
                e.meta.node_count,
                e.meta.skipped_links,
                fmt_ts(e.meta.last_success_at),
                fmt_ts(e.meta.manual_saved_at),
            )
        })
        .unwrap_or_else(|| (String::new(), 0, 0, "—".to_string(), "—".to_string()));
    UpstreamConfigTpl {
        nav: "upstreams",
        csrf: ctx.csrf.clone(),
        id,
        name,
        revision,
        fingerprint,
        body,
        has_cache: env.is_some(),
        cache_error,
        source_format,
        node_count,
        skipped_links,
        filtered_count: 0,
        last_success,
        manual_saved_at,
        saved,
        form,
    }
}

#[allow(clippy::too_many_arguments)]
async fn page_inner(
    state: Arc<AppState>,
    ctx: RequestContext,
    id: String,
    query: ConfigQuery,
    body_override: Option<String>,
    fingerprint_override: Option<String>,
    revision_override: Option<u64>,
    form: FormEcho,
) -> Result<Response, AppError> {
    if !valid_slug(&id) {
        return Err(anyhow::anyhow!("非法的上游 id：{id}").into());
    }
    let snapshot = state.store.snapshot().await;
    let Some(upstream) = snapshot.upstream(&id) else {
        return Err(anyhow::anyhow!("上游「{id}」不存在").into());
    };
    let revision = revision_override.unwrap_or(snapshot.revision);
    let loaded = cache::load(&state.store.storage(), &id).await;
    let (env, cache_error) = match loaded {
        Ok(env) => (env, String::new()),
        Err(_) => (
            None,
            "缓存文件损坏或无法读取，请先修复缓存文件后重试".to_string(),
        ),
    };
    let stored_body = env.as_ref().map(|e| e.body.clone()).unwrap_or_default();
    let stored_fingerprint = crate::cache::fingerprint(env.as_ref()).unwrap_or_default();
    let body = body_override.unwrap_or(stored_body);
    let fingerprint = fingerprint_override.unwrap_or(stored_fingerprint);
    let mut tpl = template(
        &ctx,
        id,
        upstream.name.clone(),
        revision,
        fingerprint,
        body,
        env.as_ref(),
        cache_error,
        query.saved.is_some(),
        form,
    );
    if let Some(env) = env.as_ref()
        && let Ok(parsed) = crate::fetch::validate_and_parse(
            &env.body,
            &snapshot.settings.filter_keywords,
            snapshot.settings.filter_enabled,
        )
    {
        tpl.filtered_count = parsed.filtered.len();
    }
    Ok(no_store(HtmlTemplate(tpl).into_response()))
}

pub async fn page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(id): Path<String>,
    Query(query): Query<ConfigQuery>,
) -> Response {
    no_store(
        match page_inner(state, ctx, id, query, None, None, None, FormEcho::default()).await {
            Ok(response) => response,
            Err(error) => error.into_response(),
        },
    )
}

pub async fn save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    no_store(
        match save_inner(State(state), axum::Extension(ctx), Path(id), body).await {
            Ok(response) => response,
            Err(error) => error.into_response(),
        },
    )
}

async fn save_inner(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    if !valid_slug(&id) {
        return Err(anyhow::anyhow!("非法的上游 id：{id}").into());
    }
    let snapshot = state.store.snapshot().await;
    if snapshot.upstream(&id).is_none() {
        return Err(anyhow::anyhow!("上游「{id}」不存在").into());
    }
    let revision = snapshot.revision;
    let submitted_revision = form.get_u64("revision");
    let expected_fingerprint = form.get("fingerprint").unwrap_or("").to_string();
    let entered_body = form.get("body").unwrap_or("").to_string();
    let current_env = cache::load(&state.store.storage(), &id)
        .await
        .map_err(|_| anyhow::anyhow!("缓存文件损坏或无法读取，请先修复缓存文件后重试"))?;
    let current_fingerprint = crate::cache::fingerprint(current_env.as_ref())?;
    let mut field = "body";
    let message = if submitted_revision != Some(revision) {
        field = "revision";
        "页面版本已过期，请刷新后重试；为避免覆盖新缓存，本次没有保存".to_string()
    } else if expected_fingerprint != current_fingerprint {
        field = "fingerprint";
        "缓存已被其他操作修改，请刷新页面后重试；为避免覆盖新缓存，本次没有保存".to_string()
    } else if entered_body.trim().is_empty() {
        "配置正文不能为空".to_string()
    } else {
        String::new()
    };
    if !message.is_empty() {
        let echo = FormEcho::fail(&form, &id, field, "配置正文", message);
        return page_inner(
            state,
            ctx,
            id,
            ConfigQuery::default(),
            Some(entered_body),
            Some(expected_fingerprint),
            Some(submitted_revision.unwrap_or(0)),
            echo,
        )
        .await;
    }
    if let Err(error) = state
        .refresher
        .save_manual(&id, revision, &expected_fingerprint, entered_body.clone())
        .await
    {
        // 解析错误可能带正文片段，只回填到已认证页面，不经过会记录错误的 AppError。
        let echo = FormEcho::fail(&form, &id, "body", "配置正文", error.to_string());
        return page_inner(
            state,
            ctx,
            id,
            ConfigQuery::default(),
            Some(entered_body),
            Some(expected_fingerprint),
            Some(revision),
            echo,
        )
        .await;
    }
    state.render_cache.write().await.clear();
    *state.node_names_cache.write().await = None;
    Ok(no_store(see_other(&format!(
        "/upstreams/{id}/config?saved=1"
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use askama::Template;

    async fn test_state() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("subforge-web-{}", crate::auth::random_hex(8)));
        let storage = std::sync::Arc::new(crate::storage::Storage::new(dir.clone()));
        let store = crate::store::StoreHandle::open(storage).await.unwrap();
        store
            .update(None, |s| {
                s.upstreams.push(crate::store::model::Upstream {
                    id: "test".into(),
                    name: "测试上游".into(),
                    url: "http://127.0.0.1:1/unused".into(),
                    note: None,
                });
                Ok(())
            })
            .await
            .unwrap();
        let geo = Arc::new(crate::geoip::GeoResolver::load(&dir.join("geoip")).unwrap());
        Arc::new(AppState {
            cfg: crate::config::Config::default(),
            refresher: Arc::new(
                crate::refresh::Refresher::new(store.clone(), std::time::Duration::from_secs(1), 1)
                    .unwrap(),
            ),
            geoip: Arc::new(
                crate::geoip_update::GeoipUpdater::with_proxy(store.clone(), geo.clone(), None)
                    .unwrap(),
            ),
            geo,
            store,
            csrf_secret: crate::auth::random_hex(16),
            static_dir: "static".into(),
            render_cache: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            node_names_cache: tokio::sync::RwLock::new(None),
        })
    }

    async fn response_text(response: Response) -> String {
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn post_body(ctx: &RequestContext, revision: u64, fingerprint: &str, body: &str) -> Bytes {
        Bytes::from(format!(
            "csrf={}&revision={revision}&fingerprint={fingerprint}&body={}",
            ctx.csrf,
            super::super::url_encode_component(body)
        ))
    }

    #[tokio::test]
    async fn upstream_config_no_cache_and_successful_manual_save() {
        let state = test_state().await;
        let ctx = RequestContext::new(&state.csrf_secret, "session");
        let response = page(
            State(state.clone()),
            axum::Extension(ctx.clone()),
            Path("test".into()),
            Query(ConfigQuery::default()),
        )
        .await;
        let html = response_text(response).await;
        assert!(html.contains("可以直接粘贴 Clash YAML"));
        assert!(
            cache::load(&state.store.storage(), "test")
                .await
                .unwrap()
                .is_none()
        );
        let revision = state.store.snapshot().await.revision;
        let fp = cache::fingerprint(None).unwrap();
        let body = "\n# 保留原文\nproxies:\n  - {name: HK, type: socks5, server: 127.0.0.1, port: 1080}\n\n";
        *state.node_names_cache.write().await = Some(("old".into(), vec!["old".into()]));
        let response = save(
            State(state.clone()),
            axum::Extension(ctx.clone()),
            Path("test".into()),
            post_body(&ctx, revision, &fp, body),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/upstreams/test/config?saved=1"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let env = cache::load(&state.store.storage(), "test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(env.body, body);
        assert!(state.node_names_cache.read().await.is_none());
        let response = page(
            State(state.clone()),
            axum::Extension(ctx),
            Path("test".into()),
            Query(ConfigQuery {
                saved: Some("1".into()),
            }),
        )
        .await;
        let html = response_text(response).await;
        assert!(html.contains("手动保存可用，并非网络同步成功"));
        assert!(html.contains("下一次网络同步成功后可以覆盖"));
        assert!(html.contains("Clash YAML"));
        assert!(html.contains("# 保留原文"));
    }

    #[tokio::test]
    async fn upstream_config_invalid_body_and_stale_form_keep_input_and_old_fingerprint() {
        let state = test_state().await;
        let ctx = RequestContext::new(&state.csrf_secret, "session");
        let revision = state.store.snapshot().await.revision;
        let fp = cache::fingerprint(None).unwrap();
        let body = "\n  </textarea><script>secret</script>  \n\n";
        let response = save(
            State(state.clone()),
            axum::Extension(ctx.clone()),
            Path("test".into()),
            post_body(&ctx, revision, &fp, body),
        )
        .await;
        let html = response_text(response).await;
        assert!(html.contains("提交未生效"));
        assert!(
            html.contains("\n  &#60;/textarea&#62;&#60;script&#62;secret&#60;/script&#62;  \n\n")
        );
        assert!(html.contains(&format!("name=\"fingerprint\" value=\"{fp}\"")));
        assert!(
            cache::load(&state.store.storage(), "test")
                .await
                .unwrap()
                .is_none()
        );
        let valid = "proxies:\n  - {name: HK, type: socks5, server: 127.0.0.1, port: 1080}\n";
        state
            .refresher
            .save_manual("test", revision, &fp, valid.into())
            .await
            .unwrap();
        let response = save(
            State(state.clone()),
            axum::Extension(ctx.clone()),
            Path("test".into()),
            post_body(&ctx, revision, &fp, body),
        )
        .await;
        let html = response_text(response).await;
        assert!(html.contains("缓存已被其他操作修改"));
        assert!(html.contains(&format!("name=\"fingerprint\" value=\"{fp}\"")));
        assert_eq!(
            cache::load(&state.store.storage(), "test")
                .await
                .unwrap()
                .unwrap()
                .body,
            valid
        );
    }

    #[test]
    fn failed_form_preserves_raw_body_and_old_version_tokens() {
        let raw = "  invalid configuration\n\n";
        let data = FormData::parse(&format!(
            "revision=7&fingerprint=old-fingerprint&body={}",
            raw.replace(' ', "+").replace('\n', "%0A")
        ));
        let echo = FormEcho::fail(&data, "a", "fingerprint", "配置正文", "缓存已变更");
        assert_eq!(echo.value("a", "body", "new body"), raw);
        assert_eq!(
            echo.value("a", "fingerprint", "new-fingerprint"),
            "old-fingerprint"
        );
        assert_eq!(echo.value("a", "revision", "8"), "7");
    }

    #[test]
    fn template_escapes_sensitive_textarea_body() {
        let t = UpstreamConfigTpl {
            nav: "upstreams",
            csrf: "csrf".into(),
            id: "a".into(),
            name: "A".into(),
            revision: 1,
            fingerprint: "fp".into(),
            body: "</textarea><script>alert(1)</script>".into(),
            has_cache: true,
            cache_error: String::new(),
            source_format: "Clash YAML".into(),
            node_count: 1,
            skipped_links: 0,
            filtered_count: 0,
            last_success: "—".into(),
            manual_saved_at: "—".into(),
            saved: false,
            form: FormEcho::default(),
        };
        let html = t.render().unwrap();
        assert!(!html.contains("</textarea><script>"));
        assert!(html.contains("&#60;/textarea&#62;"));
    }

    #[test]
    fn no_cache_page_offers_paste_without_javascript() {
        let ctx = RequestContext {
            sid: String::new(),
            csrf: "csrf".into(),
        };
        let t = template(
            &ctx,
            "a".into(),
            "A".into(),
            7,
            "old-fingerprint".into(),
            String::new(),
            None,
            String::new(),
            false,
            FormEcho::default(),
        );
        let html = t.render().unwrap();
        assert!(html.contains("可以直接粘贴"));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("name=\"fingerprint\" value=\"old-fingerprint\""));
        assert!(html.contains("校验并保存到缓存"));
    }
}
