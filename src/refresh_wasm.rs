//! Workers 侧刷新：用 `worker::Fetch` 拉取上游，复用共享的缓存信封与降级语义。
//!
//! 与 native `refresh.rs` 同名 API，供共享的 `pipeline` / `web` 直接调用。

use crate::cache::{self, CacheEnvelope, CacheStatus, ErrorKind};
use crate::fetch::validate_and_parse;
use crate::store::StoreHandle;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub upstream_id: String,
    pub status: CacheStatus,
    pub node_count: usize,
    pub message: Option<String>,
    pub discarded: bool,
}

pub struct Refresher {
    store: StoreHandle,
    concurrency: usize,
    cache_commits: Mutex<()>,
}

impl Refresher {
    pub fn with_proxy(
        store: StoreHandle,
        _timeout: Duration,
        concurrency: usize,
        _proxy: Option<&str>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            store,
            concurrency: concurrency.max(1),
            cache_commits: Mutex::new(()),
        })
    }

    /// Workers Fetch 不支持自定义出口代理；保留接口以兼容共享代码。
    pub async fn set_proxy(&self, _proxy: Option<String>) -> anyhow::Result<()> {
        Ok(())
    }

    pub async fn refresh_all(self: &Arc<Self>) -> Vec<RefreshOutcome> {
        let snapshot = self.store.snapshot().await;
        let ids: Vec<String> = snapshot.upstreams.iter().map(|u| u.id.clone()).collect();
        drop(snapshot);
        let mut out = Vec::new();
        for id in ids {
            out.push((*self.refresh_one(&id).await).clone());
        }
        out
    }

    pub async fn refresh_due(self: &Arc<Self>, interval: Duration) -> Vec<RefreshOutcome> {
        let snapshot = self.store.snapshot().await;
        let storage = self.store.storage();
        let now = Utc::now();
        let mut ids = Vec::new();
        for upstream in &snapshot.upstreams {
            let due = match cache::load(&storage, &upstream.id).await {
                Ok(Some(env)) => is_due(&env, now, interval),
                Ok(None) => true,
                Err(_) => true,
            };
            if due {
                ids.push(upstream.id.clone());
            }
        }
        drop(snapshot);
        let mut out = Vec::new();
        for id in ids {
            out.push((*self.refresh_one(&id).await).clone());
        }
        out
    }

    pub async fn refresh_one(self: &Arc<Self>, id: &str) -> Arc<RefreshOutcome> {
        let storage = self.store.storage();
        let snapshot = self.store.snapshot().await;
        let Some(upstream) = snapshot.upstream(id).cloned() else {
            return Arc::new(RefreshOutcome {
                upstream_id: id.to_string(),
                status: CacheStatus::Failed,
                node_count: 0,
                message: Some("上游不存在".to_string()),
                discarded: true,
            });
        };
        let mut envelope = cache::load(&storage, id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let now = Utc::now();
        let outcome = crate::workers::upstream::fetch(
            &upstream.url,
            envelope.meta.etag.as_deref(),
            envelope.meta.last_modified.as_deref(),
        )
        .await;

        // 304：正文沿用，派生统计用当前解析器重算（§4.2）。
        if outcome.not_modified && envelope.has_content() {
            let mut info = outcome.userinfo.clone();
            info.fill_from(&crate::subinfo::SubInfo {
                expire: envelope.meta.expire,
                total: envelope.meta.total,
                upload: envelope.meta.upload,
                download: envelope.meta.download,
            });
            let (count, source_format, skipped) = match validate_and_parse(
                &envelope.body,
                &snapshot.settings.filter_keywords,
                snapshot.settings.filter_enabled,
            ) {
                Ok(parsed) => (
                    parsed.proxies.len(),
                    Some(parsed.source_format),
                    parsed.skipped_links.len(),
                ),
                Err(_) => (
                    envelope.meta.node_count,
                    envelope.meta.source_format.clone(),
                    envelope.meta.skipped_links,
                ),
            };
            let body = envelope.body.clone();
            envelope.commit_success(
                body,
                &info,
                outcome.etag.or(envelope.meta.etag.clone()),
                outcome
                    .last_modified
                    .or(envelope.meta.last_modified.clone()),
                count,
                now,
            );
            envelope.meta.source_format = source_format;
            envelope.meta.skipped_links = skipped;
            let _ = cache::save(&storage, id, &envelope).await;
            return Arc::new(outcome_of(id, &envelope, None, false));
        }

        if let Some((kind, message)) = outcome.error {
            envelope.commit_failure(kind, message, now);
            let _ = cache::save(&storage, id, &envelope).await;
            return Arc::new(outcome_of(id, &envelope, None, false));
        }

        let Some(body) = outcome.body else {
            envelope.commit_failure(ErrorKind::Network, "响应体缺失", now);
            let _ = cache::save(&storage, id, &envelope).await;
            return Arc::new(outcome_of(id, &envelope, None, false));
        };

        let mut message = None;
        match validate_and_parse(
            &body,
            &snapshot.settings.filter_keywords,
            snapshot.settings.filter_enabled,
        ) {
            Ok(parsed) => {
                let mut info = outcome.userinfo.clone();
                info.fill_from(&parsed.fallback_info);
                let count = parsed.proxies.len();
                envelope.commit_success(
                    body,
                    &info,
                    outcome.etag,
                    outcome.last_modified,
                    count,
                    now,
                );
                envelope.meta.source_format = Some(parsed.source_format);
                envelope.meta.skipped_links = parsed.skipped_links.len();
            }
            Err((kind, msg)) => {
                message = Some(msg.clone());
                envelope.commit_failure(kind, msg, now);
            }
        }
        let _ = cache::save(&storage, id, &envelope).await;
        Arc::new(outcome_of(id, &envelope, message, false))
    }

    /// 手动替换缓存（§4.1）：校验 → 版本/指纹复核 → 提交。
    pub async fn save_manual(
        &self,
        id: &str,
        expected_revision: u64,
        expected_fingerprint: &str,
        body: String,
    ) -> anyhow::Result<()> {
        let snapshot = self.store.snapshot().await;
        let upstream_url = snapshot
            .upstream(id)
            .ok_or_else(|| anyhow::anyhow!("上游不存在"))?
            .url
            .clone();
        let parsed = validate_and_parse(
            &body,
            &snapshot.settings.filter_keywords,
            snapshot.settings.filter_enabled,
        )
        .map_err(|(kind, _)| {
            anyhow::anyhow!(
                "配置校验失败（{}）：请检查 YAML 或分享链接，且过滤后至少保留一个有效节点",
                kind.label()
            )
        })?;
        drop(snapshot);

        let _guard = self.cache_commits.lock().await;
        let storage = self.store.storage();
        let current_snapshot = self.store.snapshot().await;
        let current_upstream = current_snapshot
            .upstream(id)
            .ok_or_else(|| anyhow::anyhow!("上游不存在"))?;
        if current_snapshot.revision != expected_revision {
            anyhow::bail!(
                "数据已被其他操作修改（当前版本 {}，提交版本 {}），请刷新页面后重试",
                current_snapshot.revision,
                expected_revision
            );
        }
        if current_upstream.url != upstream_url {
            anyhow::bail!("上游地址已被修改，请刷新页面后重试");
        }
        let current = cache::load(&storage, id).await?;
        if cache::fingerprint(current.as_ref())? != expected_fingerprint {
            anyhow::bail!("缓存已被其他操作修改，请刷新页面后重试");
        }
        let mut envelope = current.unwrap_or_default();
        envelope.commit_manual(body, &parsed, Utc::now());
        cache::save(&storage, id, &envelope).await
    }

    /// 强制接受当前响应（§4.4 零节点策略的手动覆盖）。
    pub async fn force_accept(&self, id: &str) -> anyhow::Result<()> {
        let snapshot = self.store.snapshot().await;
        let upstream = snapshot
            .upstream(id)
            .ok_or_else(|| anyhow::anyhow!("上游不存在"))?;
        let url = upstream.url.clone();
        let storage = self.store.storage();
        let initial = cache::load(&storage, id).await?;
        let expected = cache::fingerprint(initial.as_ref())?;
        let mut envelope = initial.ok_or_else(|| anyhow::anyhow!("上游没有缓存"))?;
        let result = crate::workers::upstream::fetch(
            &url,
            envelope.meta.etag.as_deref(),
            envelope.meta.last_modified.as_deref(),
        )
        .await;
        let Some(body) = result.body else {
            return Ok(());
        };
        let mut info = result.userinfo;
        let count = match validate_and_parse(
            &body,
            &snapshot.settings.filter_keywords,
            snapshot.settings.filter_enabled,
        ) {
            Ok(parsed) => {
                info.fill_from(&parsed.fallback_info);
                envelope.meta.source_format = Some(parsed.source_format);
                envelope.meta.skipped_links = parsed.skipped_links.len();
                parsed.proxies.len()
            }
            Err(_) => {
                envelope.meta.source_format = None;
                envelope.meta.skipped_links = 0;
                0
            }
        };
        envelope.commit_success(
            body,
            &info,
            result.etag,
            result.last_modified,
            count,
            Utc::now(),
        );
        envelope.meta.forced_at = Some(Utc::now());

        let _guard = self.cache_commits.lock().await;
        let current = self.store.snapshot().await;
        if current.revision != snapshot.revision
            || current.upstream(id).is_none_or(|u| u.url != url)
        {
            anyhow::bail!("上游已被修改或删除，本次结果已丢弃");
        }
        if cache::fingerprint(cache::load(&storage, id).await?.as_ref())? != expected {
            anyhow::bail!("缓存已被其他操作修改，本次结果已丢弃");
        }
        cache::save(&storage, id, &envelope).await
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }
}

fn outcome_of(
    id: &str,
    envelope: &CacheEnvelope,
    message: Option<String>,
    discarded: bool,
) -> RefreshOutcome {
    RefreshOutcome {
        upstream_id: id.to_string(),
        status: envelope.meta.status,
        node_count: envelope.meta.node_count,
        message: message.or_else(|| envelope.meta.last_error.clone()),
        discarded,
    }
}

fn is_due(env: &CacheEnvelope, now: DateTime<Utc>, interval: Duration) -> bool {
    if !env.has_content() {
        return true;
    }
    let Some(last_success) = env.meta.last_success_at else {
        return true;
    };
    if let Some(expire) = env.meta.expire
        && let Some(expire_at) = DateTime::<Utc>::from_timestamp(expire, 0)
        && expire_at <= now
    {
        return false;
    }
    let interval = ChronoDuration::from_std(interval).unwrap_or_else(|_| ChronoDuration::zero());
    now.signed_duration_since(last_success) >= interval
}
