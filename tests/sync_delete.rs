//! Remote sync and admin deletion share repository lifecycle locks.
mod common;

use axum::http::StatusCode;
use common::{create_tenant_key, send, test_router, test_state};
use lore::{repo::sync::sync_repo, store};
use sqlx::PgPool;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn fetching_sync_without_push_registration_blocks_both_deletes(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let state = test_state(pool.clone(), data.path());
    let router = test_router(state.clone());
    let (tenant, _) = create_tenant_key(&router, "integration", &["ingest", "read"]).await;
    let source = store::ensure_source(&pool, tenant, "repo", "remote", None)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "https://127.0.0.1:{}/remote.git",
        listener.local_addr().unwrap().port()
    );
    lore::repo::sync::set_remote_url(&pool, source.id, Some(&url))
        .await
        .unwrap();
    let syncing = {
        let (state, source) = (state.clone(), source.clone());
        tokio::spawn(async move { sync_repo(&state, &source, None).await })
    };
    // Hold the TLS connection before responding, so real git fetch is in flight.
    let (mut connection, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut hello = [0; 1024];
    assert!(
        tokio::time::timeout(Duration::from_secs(5), connection.read(&mut hello))
            .await
            .unwrap()
            .unwrap()
            > 0
    );
    assert!(
        !lore::repo::progress::source_has_live_run(&pool, source.id)
            .await
            .unwrap(),
        "fetch has not recorded a run"
    );
    for path in [
        format!("/v1/admin/sources/{}", source.id),
        format!("/v1/admin/tenants/{tenant}"),
    ] {
        let (status, body) = tokio::time::timeout(
            Duration::from_secs(5),
            send(&router, "DELETE", &path, "t", Vec::new()),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
    assert!(
        store::get_source(&pool, tenant, source.id)
            .await
            .unwrap()
            .is_some()
    );
    drop(connection);
    drop(listener);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), syncing)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let (status, _) = send(
        &router,
        "DELETE",
        &format!("/v1/admin/tenants/{tenant}"),
        "t",
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        !data
            .path()
            .join("repos")
            .join(source.id.to_string())
            .exists()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_before_queued_sync_does_not_recreate_repo_directory(pool: PgPool) {
    for delete_tenant in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let state = test_state(pool.clone(), data.path());
        let router = test_router(state.clone());
        let name = format!("integration-{}", Uuid::new_v4());
        let (tenant, _) = create_tenant_key(&router, &name, &["ingest", "read"]).await;
        let source = store::ensure_source(&pool, tenant, "repo", "remote", None)
            .await
            .unwrap();
        let path = data.path().join("repos").join(source.id.to_string());
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("marker"), "repo files").unwrap();
        let mut blocking = pool.begin().await.unwrap();
        if delete_tenant {
            // A no-key-update lock blocks source DELETE but not tenant's FOR UPDATE.
            sqlx::query("SELECT id FROM sources WHERE id = $1 FOR NO KEY UPDATE")
                .bind(source.id)
                .execute(&mut *blocking)
                .await
                .unwrap();
        } else {
            sqlx::query("SELECT id FROM sources WHERE id = $1 FOR UPDATE")
                .bind(source.id)
                .execute(&mut *blocking)
                .await
                .unwrap();
        }
        let deleting = {
            let router = router.clone();
            let uri = if delete_tenant {
                format!("/v1/admin/tenants/{tenant}")
            } else {
                format!("/v1/admin/sources/{}", source.id)
            };
            tokio::spawn(async move { send(&router, "DELETE", &uri, "t", Vec::new()).await })
        };
        let lock = state.source_locks.get(source.id);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if lock.try_lock().is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // Poll the real sync future once: it must queue on deletion's source lock.
        let mut syncing = Box::pin(sync_repo(&state, &source, None));
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(syncing.as_mut(), cx).is_pending())
            })
            .await
        );
        blocking.commit().await.unwrap();
        let (status, body) = tokio::time::timeout(Duration::from_secs(5), deleting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        assert!(!path.exists(), "delete removed repo files");
        let error = tokio::time::timeout(Duration::from_secs(5), syncing)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("vanished"), "{error}");
        assert!(!path.exists(), "queued sync must not recreate files");
        assert!(
            store::get_source(&pool, tenant, source.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
