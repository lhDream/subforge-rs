use super::*;

async fn setup() -> (StoreHandle, Refresher) {
    let dir = std::env::temp_dir().join(format!("subforge-manual-{}", crate::auth::random_hex(8)));
    let storage = std::sync::Arc::new(crate::storage::Storage::new(dir));
    let store = StoreHandle::open(storage).await.unwrap();
    store
        .update(None, |s| {
            s.upstreams.push(crate::store::model::Upstream {
                id: "a".into(),
                name: "A".into(),
                url: "http://127.0.0.1:1/sub".into(),
                note: None,
            });
            Ok(())
        })
        .await
        .unwrap();
    let refresher = Refresher::new(store.clone(), Duration::from_secs(1), 1).unwrap();
    (store, refresher)
}
const BODY: &str =
    "# comment\nproxies:\n  - {name: HK-manual, type: socks5, server: 127.0.0.1, port: 1080}\n";

#[tokio::test]
async fn manual_content_is_raw_and_stale_success_or_failure_cannot_replace_it() {
    let (store, refresher) = setup().await;
    let fp = cache::fingerprint(None).unwrap();
    refresher
        .save_manual("a", store.snapshot().await.revision, &fp, BODY.into())
        .await
        .unwrap();
    let saved = cache::load(&store.storage(), "a").await.unwrap().unwrap();
    assert_eq!(saved.body, BODY);
    assert!(saved.meta.manual_saved_at.is_some());
    let old = CacheEnvelope::default();
    let success = refresher
        .commit(
            "a",
            "http://127.0.0.1:1/sub",
            old.clone(),
            0,
            None,
            Some(&fp),
        )
        .await;
    assert!(success.discarded);
    let failure = refresher
        .commit_failure(
            "a",
            "http://127.0.0.1:1/sub",
            old,
            ErrorKind::Network,
            "network".into(),
            Some(&fp),
        )
        .await;
    assert!(failure.discarded);
    assert_eq!(
        cache::load(&store.storage(), "a")
            .await
            .unwrap()
            .unwrap()
            .body,
        BODY
    );
    assert!(
        refresher
            .save_manual("a", store.snapshot().await.revision, &fp, BODY.into())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_manual_input_never_replaces_current_content() {
    let (store, refresher) = setup().await;
    let fp = cache::fingerprint(None).unwrap();
    for invalid in ["", "secret-not-yaml", "proxies: []"] {
        let error = refresher
            .save_manual("a", store.snapshot().await.revision, &fp, invalid.into())
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("secret-not-yaml"));
        assert!(cache::load(&store.storage(), "a").await.unwrap().is_none());
    }
}

#[test]
fn manual_commit_clears_old_remote_metadata_and_network_success_clears_marker() {
    let mut env = CacheEnvelope::default();
    env.meta.etag = Some("old".into());
    env.meta.last_modified = Some("old".into());
    env.meta.total = Some(999);
    env.meta.expire = Some(999);
    env.meta.upload = Some(9);
    env.meta.download = Some(9);
    env.meta.forced_at = Some(Utc::now());
    let parsed = validate_and_parse(BODY, &[], true).unwrap();
    env.commit_manual(BODY.into(), &parsed, Utc::now());
    assert!(
        env.meta.etag.is_none() && env.meta.last_modified.is_none() && env.meta.forced_at.is_none()
    );
    assert!(
        env.meta.total.is_none()
            && env.meta.expire.is_none()
            && env.meta.upload.is_none()
            && env.meta.download.is_none()
    );
    env.commit_failure(ErrorKind::Network, "failed", Utc::now());
    assert!(env.meta.manual_saved_at.is_some());
    env.commit_success(BODY.into(), &Default::default(), None, None, 1, Utc::now());
    assert!(env.meta.manual_saved_at.is_none());
}
