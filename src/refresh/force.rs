//! 强制接受响应也必须遵循手动缓存的提交保护。
use super::*;

impl Refresher {
    pub async fn force_accept(&self, id: &str) -> anyhow::Result<()> {
        let snapshot = self.store.snapshot().await;
        let upstream = snapshot
            .upstream(id)
            .ok_or_else(|| anyhow::anyhow!("上游不存在"))?;
        let url = upstream.url.clone();
        let initial = cache::load(&self.store.cache_dir(), id)?;
        let expected = cache::fingerprint(initial.as_ref())?;
        let mut env = initial.ok_or_else(|| anyhow::anyhow!("上游没有缓存"))?;
        let fetcher = self.fetcher.read().await.clone();
        let result = fetcher
            .fetch(
                &url,
                env.meta.etag.as_deref(),
                env.meta.last_modified.as_deref(),
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
                env.meta.source_format = Some(parsed.source_format);
                env.meta.skipped_links = parsed.skipped_links.len();
                parsed.proxies.len()
            }
            Err(_) => {
                env.meta.source_format = None;
                env.meta.skipped_links = 0;
                0
            }
        };
        env.commit_success(
            body,
            &info,
            result.etag,
            result.last_modified,
            count,
            Utc::now(),
        );
        env.meta.forced_at = Some(Utc::now());
        let _guard = self.cache_commits.lock().await;
        self.store
            .with_current(|current| {
                if current.revision != snapshot.revision
                    || current.upstream(id).is_none_or(|u| u.url != url)
                {
                    anyhow::bail!("上游已被修改或删除，本次结果已丢弃");
                }
                let current_env = cache::load(&self.store.cache_dir(), id)?;
                if cache::fingerprint(current_env.as_ref())? != expected {
                    anyhow::bail!("缓存已被其他操作修改，本次结果已丢弃");
                }
                cache::save(&self.store.cache_dir(), id, &env)
            })
            .await
    }
}
