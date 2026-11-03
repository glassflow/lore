mod common;

use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    body_json, commit_all, create_tenant_key, fixture_repo, git, head, send, test_router,
    test_state_with,
};
use hmac::{Hmac, Mac};
use lore::http::AppState;
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

const SECRET: &str = "s3cret";

struct Env {
    router: Router,
    state: AppState,
    data: TempDir,
    key: String,
}

/// File remotes allowed and the webhook on, with the secret `SECRET`.
async fn env(pool: PgPool) -> Env {
    let data = tempfile::tempdir().unwrap();
    let state = test_state_with(pool, data.path(), |c| {
        c.sync_allow_file_remotes = true;
        c.github_webhook_secret = Some(SECRET.to_string());
    });
    let router = test_router(state.clone());
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    Env {
        router,
        state,
        data,
        key,
    }
}

fn file_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

async fn create(env: &Env, name: &str, remote_url: Option<&str>) -> (StatusCode, Value) {
    let mut body = json!({ "name": name });
    if let Some(url) = remote_url {
        body["remote_url"] = json!(url);
    }
    send(
        &env.router,
        "POST",
        "/v1/repos",
        &env.key,
        body.to_string().into_bytes(),
    )
    .await
}

async fn create_ok(env: &Env, name: &str, remote_url: &str) -> Uuid {
    let (status, repo) = create(env, name, Some(remote_url)).await;
    assert_eq!(status, StatusCode::OK, "{repo}");
    assert_eq!(repo["remote_url"], remote_url);
    repo["id"].as_str().unwrap().parse().unwrap()
}

async fn sync(env: &Env, id: Uuid) -> (StatusCode, Value) {
    let uri = format!("/v1/repos/{id}/sync");
    send(&env.router, "POST", &uri, &env.key, Vec::new()).await
}

async fn sync_ok(env: &Env, id: Uuid) -> Value {
    let (status, report) = sync(env, id).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    report
}

async fn doc_paths(pool: &PgPool, source_id: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT external_id FROM documents WHERE source_id = $1 ORDER BY 1")
        .bind(source_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn indexed_commit(pool: &PgPool, source_id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
        .bind(source_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The source as `GET /v1/admin/overview` shows it.
async fn admin_source(env: &Env, id: Uuid) -> Value {
    let req = Request::get("/v1/admin/overview")
        .header("authorization", "Bearer t")
        .body(Body::empty())
        .unwrap();
    let res = env.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let overview = body_json(res).await;
    overview["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["sources"].as_array().unwrap().iter())
        .find(|s| s["id"] == id.to_string())
        .cloned()
        .expect("source in the overview")
}

fn sign(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn deliver(
    router: &Router,
    event: &str,
    body: &[u8],
    signature: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::post("/v1/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", event);
    if let Some(sig) = signature {
        req = req.header("x-hub-signature-256", sig);
    }
    let res = router
        .clone()
        .oneshot(req.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    (status, body_json(res).await)
}

fn push_event(git_ref: &str, clone_url: &str) -> Vec<u8> {
    json!({
        "ref": git_ref,
        "deleted": false,
        "repository": {
            "default_branch": "main",
            "clone_url": clone_url,
            "html_url": "https://github.example/acme/other",
        },
    })
    .to_string()
    .into_bytes()
}

#[sqlx::test(migrations = "./migrations")]
async fn sync_indexes_the_remote_then_only_what_changed(pool: PgPool) {
    let env = env(pool.clone()).await;
    let remote = fixture_repo();
    let id = create_ok(&env, "sample", &file_url(remote.path())).await;

    let first = sync_ok(&env, id).await;
    assert_eq!(first["indexed_commit"], head(&remote));
    assert_eq!(first["up_to_date"], false);
    assert_eq!(first["full"], true);
    assert_eq!(first["files_indexed"], 6);
    assert_eq!(doc_paths(&pool, id).await.len(), 6);

    let again = sync_ok(&env, id).await;
    assert_eq!(again["up_to_date"], true);
    assert_eq!(again["files_indexed"], 0);

    // A lost data volume: the index is intact, the next sync brings the checkout back.
    let repo_dir = env.data.path().join("repos").join(id.to_string());
    std::fs::remove_dir_all(&repo_dir).unwrap();
    assert_eq!(sync_ok(&env, id).await["up_to_date"], true);
    assert!(repo_dir.join("checkout/README.md").is_file());

    std::fs::write(remote.path().join("NOTES.md"), "# Notes\n\nSync works.\n").unwrap();
    std::fs::remove_file(remote.path().join("scripts/seed.py")).unwrap();
    commit_all(&remote, "notes");
    let second = sync_ok(&env, id).await;
    assert_eq!(second["indexed_commit"], head(&remote));
    assert_eq!(second["full"], false);
    assert_eq!(second["files_indexed"], 1);
    assert_eq!(second["files_deleted"], 1);
    let docs = doc_paths(&pool, id).await;
    assert!(docs.contains(&"NOTES.md".to_string()));
    assert!(!docs.contains(&"scripts/seed.py".to_string()));

    let admin = admin_source(&env, id).await;
    assert_eq!(admin["remote_url"], file_url(remote.path()));
    assert_eq!(admin["last_sync_commit"], head(&remote));
    assert!(admin["last_sync_at"].is_string());
    assert_eq!(admin["last_sync_error"], Value::Null);

    // The checkout follows, for deep mode.
    let checkout = env
        .data
        .path()
        .join("repos")
        .join(id.to_string())
        .join("checkout");
    assert!(checkout.join("NOTES.md").is_file());
    assert!(!checkout.join("scripts/seed.py").exists());
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unreachable_remote_is_a_502_and_shows_on_the_admin_page(pool: PgPool) {
    let env = env(pool.clone()).await;
    let remote = fixture_repo();
    let id = create_ok(&env, "sample", &file_url(remote.path())).await;
    let indexed = sync_ok(&env, id).await["indexed_commit"].clone();

    // A port nothing listens on: refused at once, and no call leaves the machine.
    let (status, _) = create(&env, "sample", Some("https://127.0.0.1:9/acme/app.git")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = sync(&env, id).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"], "could not fetch the remote");

    let admin = admin_source(&env, id).await;
    let error = admin["last_sync_error"].as_str().expect("an error");
    assert!(error.starts_with("fetch failed:"), "{error}");
    assert_eq!(admin["last_sync_commit"], indexed);
    assert_eq!(admin["indexed_commit"], indexed);
    assert_eq!(doc_paths(&pool, id).await.len(), 6);

    // A path that is not a repository fails the same way.
    let gone = tempfile::tempdir().unwrap();
    let (status, _) = create(&env, "sample", Some(&file_url(gone.path()))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sync(&env, id).await.0, StatusCode::BAD_GATEWAY);

    // The next sync that works clears the error.
    create_ok(&env, "sample", &file_url(remote.path())).await;
    assert_eq!(sync_ok(&env, id).await["up_to_date"], true);
    assert_eq!(admin_source(&env, id).await["last_sync_error"], Value::Null);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_rewritten_history_is_indexed_correctly(pool: PgPool) {
    let env = env(pool.clone()).await;
    let remote = fixture_repo();
    let id = create_ok(&env, "sample", &file_url(remote.path())).await;
    sync_ok(&env, id).await;

    std::fs::write(remote.path().join("extra.md"), "# Extra\n").unwrap();
    commit_all(&remote, "extra");
    sync_ok(&env, id).await;
    assert!(doc_paths(&pool, id).await.contains(&"extra.md".to_string()));

    // Force-push style rewrite: drop the last commit and commit something else instead.
    git(&remote, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(remote.path().join("other.md"), "# Other\n").unwrap();
    commit_all(&remote, "other");
    let report = sync_ok(&env, id).await;
    assert_eq!(report["indexed_commit"], head(&remote));
    let docs = doc_paths(&pool, id).await;
    assert!(docs.contains(&"other.md".to_string()));
    assert!(!docs.contains(&"extra.md".to_string()), "{docs:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_lost_mirror_and_rewritten_history_fall_back_to_a_full_index(pool: PgPool) {
    let env = env(pool.clone()).await;
    let remote = fixture_repo();
    let id = create_ok(&env, "sample", &file_url(remote.path())).await;
    sync_ok(&env, id).await;

    // A new root commit: the old indexed commit is in no history the remote has.
    git(&remote, &["checkout", "-q", "--orphan", "rewritten"]);
    std::fs::remove_file(remote.path().join("docs/pricing.md")).unwrap();
    commit_all(&remote, "rewritten");
    git(&remote, &["branch", "-q", "-D", "main"]);
    git(&remote, &["branch", "-q", "-m", "main"]);
    std::fs::remove_dir_all(env.data.path().join("repos").join(id.to_string())).unwrap();

    let report = sync_ok(&env, id).await;
    assert_eq!(report["indexed_commit"], head(&remote));
    assert_eq!(report["full"], true);
    assert_eq!(report["files_deleted"], 1);
    let docs = doc_paths(&pool, id).await;
    assert_eq!(docs.len(), 5);
    assert!(!docs.contains(&"docs/pricing.md".to_string()));
    assert_eq!(indexed_commit(&pool, id).await, Some(head(&remote)));
}

#[sqlx::test(migrations = "./migrations")]
async fn remote_urls_are_checked_kept_and_cleared(pool: PgPool) {
    let env = env(pool.clone()).await;
    for bad in [
        "http://github.com/acme/app.git",
        "git@github.com:acme/app.git",
        "https://user:pw@github.com/acme/app.git",
        "/srv/repo",
    ] {
        let (status, body) = create(&env, "sample", Some(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    let id = create_ok(&env, "sample", "https://github.com/acme/app.git").await;
    // Created again without a remote: the stored one stays.
    let (_, repo) = create(&env, "sample", None).await;
    assert_eq!(repo["remote_url"], "https://github.com/acme/app.git");
    let (_, list) = send(&env.router, "GET", "/v1/repos", &env.key, Vec::new()).await;
    assert_eq!(
        list["repos"][0]["remote_url"],
        "https://github.com/acme/app.git"
    );
    // An empty string removes it, and a sync is then a 409.
    let (_, repo) = create(&env, "sample", Some("")).await;
    assert_eq!(repo["remote_url"], Value::Null);
    let (status, body) = sync(&env, id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "the repo has no remote_url");

    // Without LORE_SYNC_ALLOW_FILE_REMOTES a file remote is refused.
    let data = tempfile::tempdir().unwrap();
    let strict = test_router(test_state_with(pool.clone(), data.path(), |_| {}));
    let (_, key) = create_tenant_key(&strict, "strict", &["ingest", "read"]).await;
    let body = json!({ "name": "x", "remote_url": "file:///srv/repo" }).to_string();
    let (status, _) = send(&strict, "POST", "/v1/repos", &key, body.into_bytes()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn sync_needs_an_ingest_key_of_the_owning_tenant(pool: PgPool) {
    let env = env(pool.clone()).await;
    let remote = fixture_repo();
    let id = create_ok(&env, "sample", &file_url(remote.path())).await;
    let uri = format!("/v1/repos/{id}/sync");
    let (_, read_key) = create_tenant_key(&env.router, "reader", &["read"]).await;
    let (status, _) = send(&env.router, "POST", &uri, &read_key, Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, other) = create_tenant_key(&env.router, "other", &["ingest", "read"]).await;
    let (status, _) = send(&env.router, "POST", &uri, &other, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(indexed_commit(&pool, id).await, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_poller_syncs_every_repo_with_a_remote(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (a, b) = (fixture_repo(), fixture_repo());
    let id_a = create_ok(&env, "a", &file_url(a.path())).await;
    let id_b = create_ok(&env, "b", &file_url(b.path())).await;
    let (status, _) = create(&env, "no-remote", None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(lore::repo::sync::sync_all(&env.state).await.unwrap(), 2);
    assert_eq!(indexed_commit(&pool, id_a).await, Some(head(&a)));
    assert_eq!(indexed_commit(&pool, id_b).await, Some(head(&b)));
    assert_eq!(lore::repo::sync::sync_all(&env.state).await.unwrap(), 0);

    std::fs::write(b.path().join("b.md"), "# B\n").unwrap();
    commit_all(&b, "b");
    assert_eq!(lore::repo::sync::sync_all(&env.state).await.unwrap(), 1);
    assert_eq!(indexed_commit(&pool, id_b).await, Some(head(&b)));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_signed_push_to_the_default_branch_syncs_the_repo(pool: PgPool) {
    let env = env(pool.clone()).await;
    let remote = fixture_repo();
    let url = file_url(remote.path());
    let id = create_ok(&env, "sample", &url).await;

    // Another branch, or a deleted branch, is ignored.
    let body = push_event("refs/heads/feature", &url);
    let (status, reply) = deliver(&env.router, "push", &body, Some(&sign(&body))).await;
    assert_eq!(
        (status, reply["queued"].as_u64()),
        (StatusCode::ACCEPTED, Some(0))
    );

    let (status, _) = deliver(&env.router, "ping", b"{}", Some(&sign(b"{}"))).await;
    assert_eq!(status, StatusCode::OK);

    // The URL matches whatever its case or a trailing .git; the fetch uses the stored URL.
    let shouted = url.to_uppercase().replacen("FILE://", "file://", 1);
    let body = push_event("refs/heads/main", &format!("{shouted}.git"));
    let (status, reply) = deliver(&env.router, "push", &body, Some(&sign(&body))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    assert_eq!(reply["queued"], 1);

    let want = head(&remote);
    let mut waited = Duration::ZERO;
    while indexed_commit(&pool, id).await.as_deref() != Some(want.as_str()) {
        assert!(
            waited < Duration::from_secs(60),
            "the webhook sync did not finish"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert_eq!(doc_paths(&pool, id).await.len(), 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unsigned_or_badly_signed_delivery_is_refused(pool: PgPool) {
    let env = env(pool.clone()).await;
    let remote = fixture_repo();
    let url = file_url(remote.path());
    let id = create_ok(&env, "sample", &url).await;
    let body = push_event("refs/heads/main", &url);

    let (status, _) = deliver(&env.router, "push", &body, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mut other = Hmac::<Sha256>::new_from_slice(b"wrong").unwrap();
    other.update(&body);
    let wrong = format!("sha256={}", hex::encode(other.finalize().into_bytes()));
    let (status, _) = deliver(&env.router, "push", &body, Some(&wrong)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A signature of another body.
    let (status, _) = deliver(&env.router, "push", &body, Some(&sign(b"{}"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = deliver(&env.router, "push", &body, Some("sha256=zz")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(indexed_commit(&pool, id).await, None);

    // Without a secret the endpoint does not exist.
    let data = tempfile::tempdir().unwrap();
    let off = test_router(test_state_with(pool.clone(), data.path(), |_| {}));
    let (status, _) = deliver(&off, "push", &body, Some(&sign(&body))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
