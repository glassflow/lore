//! Deleting tenants and sources through the admin API.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use common::{
    bundle, create_repo, create_tenant_key, head, push_fixture, put_bundle, send, serve_router,
    test_router, test_state,
};
use lore::http::AppState;
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;

const GENAI: &str = include_str!("fixtures/otlp/otel_genai_session.json");

struct Env {
    router: Router,
    state: AppState,
    pool: PgPool,
    data: TempDir,
}

fn env(pool: PgPool) -> Env {
    let data = tempfile::tempdir().unwrap();
    let state = test_state(pool.clone(), data.path());
    Env {
        router: test_router(state.clone()),
        state,
        pool,
        data,
    }
}

impl Env {
    async fn delete(&self, path: &str) -> (StatusCode, Value) {
        send(&self.router, "DELETE", path, "t", Vec::new()).await
    }

    fn repo_dir(&self, source_id: Uuid) -> PathBuf {
        self.data.path().join("repos").join(source_id.to_string())
    }

    /// A tenant with a pushed repo, a note, trace spans with their queued session and a key.
    async fn full_tenant(&self, name: &str) -> (Uuid, String, Uuid) {
        let (tenant, key) = create_tenant_key(&self.router, name, &["ingest", "read"]).await;
        let (source, _repo) = push_fixture(&self.router, &key).await;
        let note = json!({
            "source": "notes",
            "documents": [{ "external_id": "n1", "title": "Review", "text": "zorblax review" }]
        });
        let (status, body) = send(
            &self.router,
            "POST",
            "/v1/documents",
            &key,
            note.to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = send(
            &self.router,
            "POST",
            "/v1/traces",
            &key,
            GENAI.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (tenant, key, source)
    }
}

/// Rows of the tenant in each table that has a `tenant_id` column, plus its `tenants` row.
async fn rows_of(pool: &PgPool, tenant: Uuid) -> Vec<(String, i64)> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.columns
         WHERE table_schema = current_schema() AND column_name = 'tenant_id'
         ORDER BY table_name",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(tables.len() >= 6, "{tables:?}");
    let mut out = Vec::new();
    for t in tables {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {t} WHERE tenant_id = $1"))
            .bind(tenant)
            .fetch_one(pool)
            .await
            .unwrap();
        out.push((t, n));
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(pool)
        .await
        .unwrap();
    out.push(("tenants".to_string(), n));
    out
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_needs_the_admin_token(pool: PgPool) {
    let env = env(pool);
    let (tenant, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let path = format!("/v1/admin/tenants/{tenant}");
    for bearer in ["wrong", key.as_str()] {
        let (status, _) = send(&env.router, "DELETE", &path, bearer, Vec::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer}");
    }
    let path = format!("/v1/admin/sources/{}", Uuid::new_v4());
    let (status, _) = send(&env.router, "DELETE", &path, &key, Vec::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(rows_of(&env.pool, tenant).await.last().unwrap().1, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_a_tenant_removes_all_its_rows_and_repo_dirs_and_nothing_else(pool: PgPool) {
    let env = env(pool);
    let (acme, _acme_key, acme_repo) = env.full_tenant("acme").await;
    let (other, other_key, other_repo) = env.full_tenant("other").await;
    let other_before = rows_of(&env.pool, other).await;
    let acme_before = rows_of(&env.pool, acme).await;
    for (table, n) in &acme_before {
        assert!(
            *n > 0,
            "acme has no {table} rows to delete: {acme_before:?}"
        );
    }
    assert!(env.repo_dir(acme_repo).exists());

    let (status, body) = env.delete(&format!("/v1/admin/tenants/{acme}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    for (table, n) in rows_of(&env.pool, acme).await {
        assert_eq!(n, 0, "{table} still holds rows of the deleted tenant");
    }
    assert!(!env.repo_dir(acme_repo).exists());

    assert_eq!(rows_of(&env.pool, other).await, other_before);
    assert!(env.repo_dir(other_repo).exists());
    let (status, body) = send(
        &env.router,
        "GET",
        "/v1/search?q=zorblax",
        &other_key,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["hits"].as_array().unwrap().is_empty());

    let (status, body) = env.delete(&format!("/v1/admin/tenants/{acme}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_a_source_removes_its_documents_and_repo_dir_only(pool: PgPool) {
    let env = env(pool);
    let (acme, acme_key, acme_repo) = env.full_tenant("acme").await;
    let (other, _, other_repo) = env.full_tenant("other").await;
    let other_before = rows_of(&env.pool, other).await;

    let (status, body) = env.delete(&format!("/v1/admin/sources/{acme_repo}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!env.repo_dir(acme_repo).exists());

    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM sources WHERE id = $1)
              + (SELECT count(*) FROM documents WHERE source_id = $1)
              + (SELECT count(*) FROM chunks c JOIN documents d ON d.id = c.document_id
                 WHERE d.source_id = $1)",
    )
    .bind(acme_repo)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(left, 0);

    // The tenant keeps its other sources and its keys.
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sources WHERE tenant_id = $1 ORDER BY name")
            .bind(acme)
            .fetch_all(&env.pool)
            .await
            .unwrap();
    assert!(names.contains(&"notes".to_string()), "{names:?}");
    assert!(!names.contains(&"sample".to_string()), "{names:?}");
    let (status, _) = send(&env.router, "GET", "/v1/me", &acme_key, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(rows_of(&env.pool, other).await, other_before);
    assert!(env.repo_dir(other_repo).exists());

    let (status, _) = env.delete(&format!("/v1/admin/sources/{acme_repo}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn live_run(pool: &PgPool, source: Uuid) -> bool {
    lore::repo::progress::source_has_live_run(pool, source)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_during_a_push_is_a_409(pool: PgPool) {
    let env = env(pool);
    let (tenant, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (status, created) = create_repo(&env.router, &key, "sample").await;
    assert_eq!(status, StatusCode::OK);
    let source: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let repo = common::fixture_repo();
    let sha = head(&repo);
    let bytes = bundle(&repo, &["HEAD"]);

    // Hold the source's lock so the push waits, recorded as a live run.
    let lock = env.state.source_locks.get(source);
    let guard = lock.lock().await;
    let pushing = {
        let (router, key, sha) = (env.router.clone(), key.clone(), sha.clone());
        tokio::spawn(async move { put_bundle(&router, &key, source, bytes, &sha, true).await })
    };
    for _ in 0..200 {
        if live_run(&env.pool, source).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(live_run(&env.pool, source).await);

    let (status, body) = env.delete(&format!("/v1/admin/sources/{source}")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("push"), "{body}");
    let (status, body) = env.delete(&format!("/v1/admin/tenants/{tenant}")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(rows_of(&env.pool, tenant).await.last().unwrap().1, 1);

    drop(guard);
    let (status, report) = pushing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(env.repo_dir(source).exists());

    let (status, body) = env.delete(&format!("/v1/admin/tenants/{tenant}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!env.repo_dir(source).exists());
}

/// A run of `source` that is not ended, last written `age_secs` ago, by a process that holds
/// no lock here (another instance, or one that was killed).
async fn insert_run(pool: &PgPool, tenant: Uuid, source: Uuid, age_secs: i32) {
    sqlx::query(
        "INSERT INTO index_runs (source_id, tenant_id, commit, stage, updated_at)
         VALUES ($1, $2, $3, 'indexing', now() - make_interval(secs => $4))",
    )
    .bind(source)
    .bind(tenant)
    .bind("a".repeat(40))
    .bind(f64::from(age_secs))
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn a_live_run_of_another_process_is_a_409_with_the_lock_free(pool: PgPool) {
    let env = env(pool);
    let (tenant, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (_, created) = create_repo(&env.router, &key, "sample").await;
    let source: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    insert_run(&env.pool, tenant, source, 5).await;

    let (status, body) = env.delete(&format!("/v1/admin/sources/{source}")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = env.delete(&format!("/v1/admin/tenants/{tenant}")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(rows_of(&env.pool, tenant).await.last().unwrap().1, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_stale_run_does_not_block_deleting_a_source_or_a_tenant(pool: PgPool) {
    let env = env(pool);
    let (tenant, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (_, first) = create_repo(&env.router, &key, "one").await;
    let (_, second) = create_repo(&env.router, &key, "two").await;
    let first: Uuid = first["id"].as_str().unwrap().parse().unwrap();
    let second: Uuid = second["id"].as_str().unwrap().parse().unwrap();
    insert_run(&env.pool, tenant, first, 120).await;
    insert_run(&env.pool, tenant, second, 120).await;

    let (status, body) = env.delete(&format!("/v1/admin/sources/{first}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = env.delete(&format!("/v1/admin/tenants/{tenant}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_held_source_lock_without_a_registered_push_is_also_a_409(pool: PgPool) {
    let env = env(pool);
    let (tenant, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (_, created) = create_repo(&env.router, &key, "sample").await;
    let source: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let lock = env.state.source_locks.get(source);
    let _guard = lock.lock().await;
    let (status, _) = env.delete(&format!("/v1/admin/sources/{source}")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = env.delete(&format!("/v1/admin/tenants/{tenant}")).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn tenant_delete_checks_a_source_created_concurrently(pool: PgPool) {
    let env = env(pool);
    let (tenant, _) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let source = Uuid::new_v4();
    let mut creating = env.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO sources (id, tenant_id, kind, name) VALUES ($1, $2, 'repo', 'new')")
        .bind(source)
        .bind(tenant)
        .execute(&mut *creating)
        .await
        .unwrap();
    let lock = env.state.source_locks.get(source);
    let _guard = lock.lock().await;
    // The uncommitted source's FK holds a key-share lock on the tenant. Deletion
    // must reject it without waiting, then also check the source lock on retry.
    let path = format!("/v1/admin/tenants/{tenant}");
    let (status, body) = tokio::time::timeout(Duration::from_secs(1), env.delete(&path))
        .await
        .expect("tenant deletion must not wait for source creation");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    creating.commit().await.unwrap();
    let (status, body) = env.delete(&path).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let sources: i64 = sqlx::query_scalar("SELECT count(*) FROM sources WHERE id = $1")
        .bind(source)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(sources, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_held_tenant_key_share_returns_409_without_waiting(pool: PgPool) {
    let env = env(pool);
    let (tenant, _key, source) = env.full_tenant("acme").await;
    let before = rows_of(&env.pool, tenant).await;
    let mut pushing = env.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM tenants WHERE id = $1 FOR KEY SHARE")
        .bind(tenant)
        .execute(&mut *pushing)
        .await
        .unwrap();
    let path = format!("/v1/admin/tenants/{tenant}");
    let (status, body) = tokio::time::timeout(Duration::from_secs(1), env.delete(&path))
        .await
        .expect("tenant deletion must not wait for an active push transaction");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(rows_of(&env.pool, tenant).await, before);
    assert!(env.repo_dir(source).exists());

    pushing.rollback().await.unwrap();
    let (status, body) = env.delete(&path).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!env.repo_dir(source).exists());
}

#[sqlx::test(migrations = "./migrations")]
async fn unknown_ids_are_404(pool: PgPool) {
    let env = env(pool);
    for path in ["tenants", "sources"] {
        let (status, body) = env
            .delete(&format!("/v1/admin/{path}/{}", Uuid::new_v4()))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn the_cli_deletes_sources_and_tenants(pool: PgPool) {
    let env = env(pool);
    let (tenant, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (_, created) = create_repo(&env.router, &key, "sample").await;
    let source = created["id"].as_str().unwrap().to_string();
    let base = serve_router(env.router.clone()).await;
    let lore = |args: Vec<String>| {
        let base = base.clone();
        async move {
            let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_lore"))
                .args(["--url", &base, "--admin-token", "t"])
                .args(&args)
                .env_remove("LORE_URL")
                .env_remove("LORE_KEY")
                .env_remove("LORE_ADMIN_TOKEN")
                .output()
                .await
                .expect("the lore binary runs");
            let stdout: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
            (
                out.status.success(),
                stdout,
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }
    };

    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (ok, out, err) = lore(args(&["admin", "source", "delete", &source])).await;
    assert!(ok, "{err}");
    assert_eq!(out, json!({ "id": source, "deleted": true }));

    let (ok, _, err) = lore(args(&["admin", "source", "delete", &source])).await;
    assert!(!ok);
    assert!(err.contains("404"), "{err}");

    let id = tenant.to_string();
    let (ok, out, err) = lore(args(&["admin", "tenant", "delete", &id])).await;
    assert!(ok, "{err}");
    assert_eq!(out, json!({ "id": id, "deleted": true }));
    assert_eq!(rows_of(&env.pool, tenant).await.last().unwrap().1, 0);
}
