//! 上游刷新：定期任务、in-flight 合并、写前校验（AGENTS.md §4.2）。

use crate::cache::{self as cache, CacheEnvelope, CacheStatus, ErrorKind};
use crate::fetch::{Fetcher, validate_and_parse};
use crate::store::StoreHandle;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell, RwLock, Semaphore};

mod force;
#[cfg(test)]
mod manual_tests;

#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub upstream_id: String,
    pub status: CacheStatus,
    pub node_count: usize,
    pub message: Option<String>,
    /// 因上游已被修改/删除而丢弃本次结果。
    pub discarded: bool,
}

pub struct Refresher {
    store: StoreHandle,
    fetcher: RwLock<Fetcher>,
    /// 同一上游的重复刷新合并到同一个 OnceCell，不重复发起请求。
    inflight: Mutex<HashMap<String, Arc<OnceCell<Arc<RefreshOutcome>>>>>,
    /// 串行化缓存正文/状态提交，避免手动保存与在途刷新交错写入。
    cache_commits: Mutex<()>,
    concurrency: usize,
}

impl Refresher {
    #[cfg(test)]
    pub fn new(store: StoreHandle, timeout: Duration, concurrency: usize) -> anyhow::Result<Self> {
        Self::with_proxy(store, timeout, concurrency, None)
    }

    /// `proxy` 为规范化后的出口代理地址；`None` 表示直连。
    pub fn with_proxy(
        store: StoreHandle,
        timeout: Duration,
        concurrency: usize,
        proxy: Option<&str>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            store,
            fetcher: RwLock::new(Fetcher::with_proxy(timeout, proxy)?),
            inflight: Mutex::new(HashMap::new()),
            cache_commits: Mutex::new(()),
            concurrency: concurrency.max(1),
        })
    }

    /// 替换后续拉取使用的客户端；不持锁执行网络请求。
    pub async fn set_proxy(&self, proxy: Option<String>) -> anyhow::Result<()> {
        let current = self.fetcher.read().await.clone();
        let replacement = current.with_proxy_replacement(proxy.as_deref())?;
        *self.fetcher.write().await = replacement;
        Ok(())
    }

    /// 刷新全部上游，并发受 `concurrency` 限制。
    pub async fn refresh_all(self: &Arc<Self>) -> Vec<RefreshOutcome> {
        let snapshot = self.store.snapshot().await;
        let ids: Vec<String> = snapshot.upstreams.iter().map(|u| u.id.clone()).collect();
        drop(snapshot);
        self.refresh_ids(ids).await
    }

    /// 只刷新已经到期的上游（启动与定期任务用）。
    ///
    /// 到期判定：没有成功缓存、缓存正文为空、`last_success_at + interval` 已过，
    /// 或订阅本身已过期（`expire` 已过）。未到期时直接跳过，不发起网络请求。
    pub async fn refresh_due(self: &Arc<Self>, interval: Duration) -> Vec<RefreshOutcome> {
        let snapshot = self.store.snapshot().await;
        let upstreams: Vec<(String, String)> = snapshot
            .upstreams
            .iter()
            .map(|u| (u.id.clone(), u.name.clone()))
            .collect();
        let storage = self.store.storage();
        drop(snapshot);

        let now = Utc::now();
        let total = upstreams.len();
        let mut ids = Vec::new();
        for (id, name) in upstreams {
            let due = match cache::load(&storage, &id).await {
                Ok(Some(env)) => is_due(&env, now, interval),
                Ok(None) => true,
                Err(e) => {
                    tracing::warn!("读取上游 {id} 的缓存失败，按到期处理：{e}");
                    true
                }
            };
            if due {
                ids.push(id);
            } else {
                tracing::debug!("上游 {name} 的缓存未到期，跳过启动/定期刷新");
            }
        }

        if ids.is_empty() {
            tracing::info!("{total} 个上游均未到期，跳过本轮刷新");
            return Vec::new();
        }
        tracing::info!("{total} 个上游中 {} 个到期，开始刷新", ids.len());
        self.refresh_ids(ids).await
    }

    async fn refresh_ids(self: &Arc<Self>, ids: Vec<String>) -> Vec<RefreshOutcome> {
        let sem = Arc::new(Semaphore::new(self.concurrency));
        let mut handles = Vec::new();
        for id in ids {
            let me = self.clone();
            let sem = sem.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.ok()?;
                Some(me.refresh_one(&id).await)
            }));
        }

        let mut out = Vec::new();
        for h in handles {
            if let Ok(Some(o)) = h.await {
                out.push((*o).clone());
            }
        }
        out
    }

    /// 刷新单个上游；重复调用会复用同一在途请求。
    pub async fn refresh_one(self: &Arc<Self>, id: &str) -> Arc<RefreshOutcome> {
        let cell = {
            let mut map = self.inflight.lock().await;
            map.entry(id.to_string())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        let me = self.clone();
        let id_owned = id.to_string();
        let outcome = cell
            .get_or_init(|| async move { Arc::new(me.do_refresh(&id_owned).await) })
            .await
            .clone();

        // 清理在途登记（仅当仍是自己那个 cell）
        {
            let mut map = self.inflight.lock().await;
            if let Some(existing) = map.get(id)
                && Arc::ptr_eq(existing, &cell)
            {
                map.remove(id);
            }
        }
        outcome
    }

    async fn do_refresh(&self, id: &str) -> RefreshOutcome {
        let snapshot = self.store.snapshot().await;
        let Some(upstream) = snapshot.upstream(id) else {
            return RefreshOutcome {
                upstream_id: id.to_string(),
                status: CacheStatus::Failed,
                node_count: 0,
                message: Some("上游不存在".to_string()),
                discarded: true,
            };
        };
        let url = upstream.url.clone();
        let filter_keywords = snapshot.settings.filter_keywords.clone();
        let filter_enabled = snapshot.settings.filter_enabled;
        drop(snapshot);

        let loaded_env = match cache::load(&self.store.storage(), id).await {
            Ok(Some(e)) => Some(e),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("读取上游 {id} 的缓存失败，将重建：{e}");
                None
            }
        };
        let mut env = loaded_env.clone().unwrap_or_default();
        let initial_fingerprint = cache::fingerprint(loaded_env.as_ref()).ok();
        let fetcher = self.fetcher.read().await.clone();
        let result = fetcher
            .fetch(
                &url,
                env.meta.etag.as_deref(),
                env.meta.last_modified.as_deref(),
            )
            .await;

        let now = Utc::now();

        // 304：远端内容与本地一致，可安全提交（正文未变，元数据同属这一次刷新）
        if result.not_modified && result.error.is_none() && env.has_content() {
            let mut info = result.userinfo.clone();
            info.fill_from(&crate::subinfo::SubInfo {
                expire: env.meta.expire,
                total: env.meta.total,
                upload: env.meta.upload,
                download: env.meta.download,
            });

            // 正文沿用，但**派生结果必须用当前解析器重算**。
            // 否则解析逻辑升级（例如新增分享链接支持）后，304 会让
            // node_count / source_format 永远停在旧值——上游内容没变就永远不刷新。
            let (count, fmt, skipped, filtered_count, parse_note) =
                match validate_and_parse(&env.body, &filter_keywords, filter_enabled) {
                    Ok(p) => (
                        p.proxies.len(),
                        Some(p.source_format.clone()),
                        p.skipped_links.len(),
                        p.filtered.len(),
                        None,
                    ),
                    Err((_, msg)) => (
                        env.meta.node_count,
                        env.meta.source_format.clone(),
                        env.meta.skipped_links,
                        0,
                        Some(msg),
                    ),
                };

            let body = env.body.clone();
            env.commit_success(
                body,
                &info,
                result.etag.or(env.meta.etag.clone()),
                result.last_modified.or(env.meta.last_modified.clone()),
                count,
                now,
            );
            env.meta.source_format = fmt;
            env.meta.skipped_links = skipped;

            let mut notes = vec!["内容未变更（304）".to_string()];
            if filtered_count > 0 {
                notes.push(format!("已过滤 {filtered_count} 个信息节点"));
            }
            if let Some(m) = parse_note {
                tracing::warn!("上游 {id} 的缓存正文在 304 后重新解析失败，沿用旧统计：{m}");
            }
            return self
                .commit(
                    id,
                    &url,
                    env,
                    count,
                    Some(notes.join("，")),
                    initial_fingerprint.as_deref(),
                )
                .await;
        }
        // 无本地内容却收到 304 时，下面按普通响应继续处理

        if let Some((kind, msg)) = &result.error {
            env.commit_failure(*kind, msg.clone(), now);
            return self
                .commit_failure(
                    id,
                    &url,
                    env,
                    *kind,
                    msg.clone(),
                    initial_fingerprint.as_deref(),
                )
                .await;
        }

        let Some(body) = result.body else {
            let msg = "响应体缺失".to_string();
            env.commit_failure(ErrorKind::Network, msg.clone(), now);
            return self
                .commit_failure(
                    id,
                    &url,
                    env,
                    ErrorKind::Network,
                    msg,
                    initial_fingerprint.as_deref(),
                )
                .await;
        };

        match validate_and_parse(&body, &filter_keywords, filter_enabled) {
            Ok(parsed) => {
                let mut info = result.userinfo.clone();
                info.fill_from(&parsed.fallback_info);
                let count = parsed.proxies.len();
                env.commit_success(body, &info, result.etag, result.last_modified, count, now);
                env.meta.source_format = Some(parsed.source_format.clone());
                env.meta.skipped_links = parsed.skipped_links.len();

                let mut notes: Vec<String> = Vec::new();
                if parsed.source_format != "Clash YAML" {
                    notes.push(format!("识别为{}", parsed.source_format));
                }
                if !parsed.filtered.is_empty() {
                    notes.push(format!("已过滤 {} 个信息节点", parsed.filtered.len()));
                }
                if !parsed.skipped_links.is_empty() {
                    notes.push(format!(
                        "跳过 {} 条无法识别的链接",
                        parsed.skipped_links.len()
                    ));
                    for s in parsed.skipped_links.iter().take(5) {
                        tracing::warn!("上游 {id} 跳过链接：{s}");
                    }
                }
                let msg = (!notes.is_empty()).then(|| notes.join("，"));
                self.commit(id, &url, env, count, msg, initial_fingerprint.as_deref())
                    .await
            }
            Err((kind, msg)) => {
                // 零节点响应：旧缓存非空时不替换（§4.4）
                env.commit_failure(kind, msg.clone(), now);
                self.commit_failure(id, &url, env, kind, msg, initial_fingerprint.as_deref())
                    .await
            }
        }
    }

    async fn commit(
        &self,
        id: &str,
        url_used: &str,
        env: CacheEnvelope,
        node_count: usize,
        message: Option<String>,
        initial_fingerprint: Option<&str>,
    ) -> RefreshOutcome {
        let _commit_guard = self.cache_commits.lock().await;
        let current_store = self.store.cache_snapshot().await;
        if current_store.upstream(id).is_none_or(|u| u.url != url_used) {
            return RefreshOutcome {
                upstream_id: id.to_string(),
                status: CacheStatus::Stale,
                node_count,
                message: Some("上游已被修改或删除，本次结果已丢弃".to_string()),
                discarded: true,
            };
        }
        let current_fingerprint = cache::load(&self.store.storage(), id)
            .await
            .ok()
            .and_then(|env| cache::fingerprint(env.as_ref()).ok());
        if current_fingerprint.as_deref() != initial_fingerprint {
            return RefreshOutcome {
                upstream_id: id.to_string(),
                status: CacheStatus::Stale,
                node_count,
                message: Some("缓存已被其他操作修改，本次刷新结果已丢弃".to_string()),
                discarded: true,
            };
        }
        if let Err(e) = cache::save(&self.store.storage(), id, &env).await {
            return RefreshOutcome {
                upstream_id: id.to_string(),
                status: CacheStatus::Failed,
                node_count,
                message: Some(format!("写入缓存失败：{e}")),
                discarded: false,
            };
        }
        RefreshOutcome {
            upstream_id: id.to_string(),
            status: CacheStatus::Ok,
            node_count,
            message,
            discarded: false,
        }
    }

    async fn commit_failure(
        &self,
        id: &str,
        url_used: &str,
        env: CacheEnvelope,
        kind: ErrorKind,
        msg: String,
        initial_fingerprint: Option<&str>,
    ) -> RefreshOutcome {
        let _commit_guard = self.cache_commits.lock().await;
        let current_store = self.store.cache_snapshot().await;
        if current_store.upstream(id).is_none_or(|u| u.url != url_used) {
            return RefreshOutcome {
                upstream_id: id.to_string(),
                status: CacheStatus::Stale,
                node_count: env.meta.node_count,
                message: Some("上游已被修改或删除，本次结果已丢弃".to_string()),
                discarded: true,
            };
        }
        let current_fingerprint = cache::load(&self.store.storage(), id)
            .await
            .ok()
            .and_then(|env| cache::fingerprint(env.as_ref()).ok());
        if current_fingerprint.as_deref() != initial_fingerprint {
            return RefreshOutcome {
                upstream_id: id.to_string(),
                status: CacheStatus::Stale,
                node_count: env.meta.node_count,
                message: Some("缓存已被其他操作修改，本次刷新结果已丢弃".to_string()),
                discarded: true,
            };
        }
        // 状态字段仍要落盘（内容字段由 commit_failure 保证未动）
        if let Err(e) = cache::save(&self.store.storage(), id, &env).await {
            tracing::warn!("写入上游 {id} 的缓存状态失败：{e}");
        }
        RefreshOutcome {
            upstream_id: id.to_string(),
            status: env.meta.status,
            node_count: env.meta.node_count,
            message: Some(format!("[{}] {msg}", kind.label())),
            discarded: false,
        }
    }

    /// 校验并手动替换上游缓存正文。
    ///
    /// 正文解析在提交互斥锁外完成；锁内重新读取 store revision、上游和缓存指纹，
    /// 因此旧表单或在途刷新不能覆盖较新的内容。
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

        let _commit_guard = self.cache_commits.lock().await;
        let current_snapshot = self.store.cache_snapshot().await;
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
        let current = cache::load(&self.store.storage(), id).await?;
        let actual = cache::fingerprint(current.as_ref())?;
        if actual != expected_fingerprint {
            anyhow::bail!("缓存已被其他操作修改，请刷新页面后重试");
        }
        let mut env = current.unwrap_or_default();
        env.commit_manual(body, &parsed, Utc::now());
        cache::save(&self.store.storage(), id, &env).await
    }

    /// 启动定期刷新后台任务。
    pub fn spawn_periodic(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // 启动时只刷新到期项，不因为进程重启就重拉全部上游。
            let outcomes = self.refresh_due(interval).await;
            log_outcomes(&outcomes);

            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // 跳过立即触发的第一次
            loop {
                ticker.tick().await;
                let outcomes = self.refresh_due(interval).await;
                log_outcomes(&outcomes);
            }
        })
    }
}

/// 缓存是否已到刷新时间。
///
/// - 没有成功缓存 / 正文为空 → 必须刷新；
/// - 订阅本身已过期（`expire` 已过）→ 不刷新（避免对已失效订阅反复请求）；
/// - `last_success_at + interval <= now` → 刷新；
/// - 其余情况 → 跳过。
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

fn log_outcomes(outcomes: &[RefreshOutcome]) {
    for o in outcomes {
        if o.discarded {
            tracing::warn!(
                "上游 {} 的刷新结果已丢弃：{}",
                o.upstream_id,
                o.message.as_deref().unwrap_or("上游已被修改或删除")
            );
            continue;
        }
        match o.status {
            CacheStatus::Ok => tracing::info!(
                "上游 {} 刷新成功：{} 个节点{}",
                o.upstream_id,
                o.node_count,
                o.message
                    .as_ref()
                    .map(|m| format!("（{m}）"))
                    .unwrap_or_default()
            ),
            other => tracing::warn!(
                "上游 {} 刷新未成功（{}）：{}",
                o.upstream_id,
                other.label(),
                o.message.as_deref().unwrap_or("未知原因")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheMeta;

    fn env(last_success_at: Option<DateTime<Utc>>, expire: Option<i64>) -> CacheEnvelope {
        CacheEnvelope {
            meta: CacheMeta {
                last_success_at,
                expire,
                ..Default::default()
            },
            body: "proxies: []".to_string(),
        }
    }

    #[test]
    fn cache_without_success_is_due() {
        let now = Utc::now();
        assert!(is_due(&env(None, None), now, Duration::from_secs(6 * 3600)));
    }

    #[test]
    fn fresh_cache_is_not_due() {
        let now = Utc::now();
        let env = env(Some(now - ChronoDuration::hours(1)), None);
        assert!(!is_due(&env, now, Duration::from_secs(6 * 3600)));
    }

    #[test]
    fn aged_cache_is_due() {
        let now = Utc::now();
        let env = env(Some(now - ChronoDuration::hours(7)), None);
        assert!(is_due(&env, now, Duration::from_secs(6 * 3600)));
    }

    #[test]
    fn expired_subscription_is_not_due() {
        let now = Utc::now();
        let env = env(
            Some(now - ChronoDuration::minutes(5)),
            Some((now - ChronoDuration::hours(1)).timestamp()),
        );
        assert!(!is_due(&env, now, Duration::from_secs(6 * 3600)));
    }
}
