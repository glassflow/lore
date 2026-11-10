//! The admin overview and push progress endpoints behind `LORE_ADMIN_TOKEN`.

mod common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    body_json, bundle, commit_all, create_repo, create_tenant_key, head, push_fixture, put_bundle,
    send, test_router, test_state,
};
use lore::http::AppState;
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

struct Env {
    router: Router,
    state: AppState,
    data: TempDir,
}

fn env(pool: PgPool) -> Env {
    let data = tempfile::tempdir().unwrap();
    let state = test_state(pool, data.path());
    Env {
        router: test_router(state.clone()),
        state,
        data,
    }
}

/// `GET path` with the admin token `t`.
async fn admin_get(router: &Router, path: &str) -> Value {
    let (status, body) = get(router, path, Some("t")).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body
}

async fn get(router: &Router, path: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::get(path);
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    let res = router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    (status, body_json(res).await)
}

async fn chunks_of(pool: &PgPool, source_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.source_id = $1",
    )
    .bind(source_id.parse::<Uuid>().unwrap())
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The source called `name` in a tenant of the overview.
fn source<'a>(tenant: &'a Value, name: &str) -> &'a Value {
    tenant["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no source {name} in {tenant}"))
}

#[sqlx::test(migrations = "./migrations")]
async fn the_new_endpoints_need_the_admin_token(pool: PgPool) {
    let env = env(pool.clone());
    let (_, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    for path in ["/v1/admin/overview", "/v1/admin/pushes"] {
        for bearer in [None, Some("wrong"), Some(key.as_str())] {
            let (status, body) = get(&env.router, path, bearer).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} with {bearer:?}");
            assert_eq!(body["error"], "unauthorized");
        }
    }

    let mut state = test_state(pool, env.data.path());
    let mut config = (*state.config).clone();
    config.admin_token = None;
    state.config = std::sync::Arc::new(config);
    let off = test_router(state);
    for path in ["/v1/admin/overview", "/v1/admin/pushes"] {
        let (status, body) = get(&off, path, Some("t")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(body["error"], "admin API disabled");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn the_overview_of_an_empty_server_has_its_facts(pool: PgPool) {
    let env = env(pool);
    let overview = admin_get(&env.router, "/v1/admin/overview").await;
    assert_eq!(overview["tenants"], json!([]));
    assert!(overview["now"].is_string());
    let mut server = overview["server"].clone();
    // Memory is read from /proc, so it is known on Linux only.
    let memory = server.as_object_mut().unwrap().remove("memory").unwrap();
    for field in ["rss_bytes", "peak_rss_bytes"] {
        let v = &memory[field];
        if cfg!(target_os = "linux") {
            assert!(v.as_u64().unwrap() > 0, "{field}: {v}");
        } else {
            assert_eq!(*v, Value::Null, "{field}");
        }
    }
    assert_eq!(
        server,
        json!({
            "version": env.state.config.version,
            "llm": "fake",
            "embedder": "hash",
            "modes": ["fast", "sources", "deep"],
            "models": {
                "fast": {"model": "fake", "effort": null},
                "deep": {"model": "fake", "effort": null}
            },
            "tenants": 0,
            "sources": 0,
            "documents": 0,
            "chunks": 0
        })
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_overview_says_how_much_room_the_index_takes(pool: PgPool) {
    let env = env(pool);
    let (_, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    push_fixture(&env.router, &key).await;
    let overview = admin_get(&env.router, "/v1/admin/overview").await;
    let storage = &overview["storage"];

    let database = storage["database_bytes"].as_i64().unwrap();
    assert!(database > 0);
    let tables = storage["tables"].as_array().unwrap();
    let table = |name: &str| {
        tables
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("no table {name} in {tables:?}"))
    };
    for name in [
        "chunks",
        "documents",
        "sources",
        "tenants",
        "api_keys",
        "spans",
    ] {
        let t = table(name);
        let (data, index, total) = (
            t["data_bytes"].as_i64().unwrap(),
            t["index_bytes"].as_i64().unwrap(),
            t["total_bytes"].as_i64().unwrap(),
        );
        assert_eq!(data + index, total, "{t}");
        assert!(total <= database, "{t}");
    }
    assert!(table("chunks")["data_bytes"].as_i64().unwrap() > 0);
    let totals: Vec<i64> = tables
        .iter()
        .map(|t| t["total_bytes"].as_i64().unwrap())
        .collect();
    assert!(totals.windows(2).all(|w| w[0] >= w[1]), "largest first");

    let indexes = storage["indexes"].as_array().unwrap();
    let index = |name: &str| {
        indexes
            .iter()
            .find(|i| i["name"] == name)
            .unwrap_or_else(|| panic!("no index {name} in {indexes:?}"))
    };
    assert_eq!(index("chunks_embedding")["method"], "hnsw");
    assert_eq!(index("chunks_embedding")["table"], "chunks");
    assert_eq!(index("chunks_tsv")["method"], "gin");
    assert!(index("chunks_embedding")["bytes"].as_i64().unwrap() > 0);

    // The repo's mirror and checkout are on disk, and are all of the repos directory.
    let sample = source(&overview["tenants"][0], "sample");
    let disk = sample["disk_bytes"].as_u64().unwrap();
    assert!(disk > 0);
    assert_eq!(storage["repos_bytes"], disk);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_overview_lists_tenants_with_their_sources_counts_and_keys(pool: PgPool) {
    let env = env(pool.clone());
    let (acme_id, acme_key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (_, sample_repo) = push_fixture(&env.router, &acme_key).await;
    let sha = head(&sample_repo);
    let body = json!({"source": "notes", "documents": [
        {"external_id": "n1", "title": "Review", "text": "The quarterly review moved to Thursdays."},
        {"external_id": "n2", "title": "Plans", "text": "# Plans\n\nFree and pro.\n\n# Limits\n\nTen seats."}
    ]});
    let (status, _) = send(
        &env.router,
        "POST",
        "/v1/documents",
        &acme_key,
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // A repo that was created and never pushed.
    let (status, _) = create_repo(&env.router, &acme_key, "empty").await;
    assert_eq!(status, StatusCode::OK);

    let (other_id, other_key) = create_tenant_key(&env.router, "other", &["read"]).await;
    let keys = admin_get(&env.router, &format!("/v1/admin/tenants/{other_id}/keys")).await;
    let other_key_id = keys["keys"][0]["id"].as_str().unwrap().to_string();
    let (status, _) = send(
        &env.router,
        "DELETE",
        &format!("/v1/admin/keys/{other_key_id}"),
        "t",
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let overview = admin_get(&env.router, "/v1/admin/overview").await;
    let tenants = overview["tenants"].as_array().unwrap();
    let names: Vec<&str> = tenants
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["acme", "other"]);
    let (acme, other) = (&tenants[0], &tenants[1]);
    assert_eq!(acme["id"], acme_id.to_string());
    assert!(acme["created_at"].is_string());

    // Sources by kind, then name: documents before repos.
    let kinds: Vec<(&str, &str)> = acme["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["kind"].as_str().unwrap(), s["name"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("documents", "notes"),
            ("repo", "empty"),
            ("repo", "sample")
        ]
    );

    let sample = source(acme, "sample");
    assert_eq!(sample["indexed_commit"], sha.as_str());
    assert_eq!(sample["documents"], 6);
    let sample_chunks = chunks_of(&pool, sample["id"].as_str().unwrap()).await;
    assert!(sample_chunks >= 6);
    assert_eq!(sample["chunks"], sample_chunks);
    assert!(sample["last_indexed_at"].is_string());
    assert!(sample.get("tenant_id").is_none());

    let notes = source(acme, "notes");
    assert_eq!(notes["indexed_commit"], Value::Null);
    assert_eq!(notes["documents"], 2);
    let notes_chunks = chunks_of(&pool, notes["id"].as_str().unwrap()).await;
    assert_eq!(notes["chunks"], notes_chunks);
    assert!(notes["last_indexed_at"].is_string());

    let empty = source(acme, "empty");
    assert_eq!(empty["indexed_commit"], Value::Null);
    assert_eq!(empty["documents"], 0);
    assert_eq!(empty["chunks"], 0);
    assert_eq!(empty["last_indexed_at"], Value::Null);

    // Only repos keep files on disk.
    assert!(sample["disk_bytes"].as_u64().unwrap() > 0);
    assert_eq!(empty["disk_bytes"], 0);
    assert_eq!(notes["disk_bytes"], Value::Null);

    // Keys: prefix, scopes and dates, never the key or its hash.
    let acme_keys = acme["keys"].as_array().unwrap();
    assert_eq!(acme_keys.len(), 1);
    assert_eq!(acme_keys[0]["prefix"], acme_key[..11]);
    assert_eq!(acme_keys[0]["scopes"], json!(["ingest", "read"]));
    assert_eq!(acme_keys[0]["revoked_at"], Value::Null);
    assert_eq!(other["sources"], json!([]));
    assert_eq!(other["keys"][0]["id"], other_key_id.as_str());
    assert!(other["keys"][0]["revoked_at"].is_string());
    let text = overview.to_string();
    for secret in [&acme_key, &other_key] {
        assert!(!text.contains(secret.as_str()), "a key leaked: {text}");
    }
    for k in [&acme_keys[0], &other["keys"][0]] {
        let mut fields: Vec<&str> = k.as_object().unwrap().keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            ["created_at", "id", "prefix", "revoked_at", "scopes"]
        );
    }

    // The totals add up.
    let server = &overview["server"];
    assert_eq!(server["tenants"], 2);
    assert_eq!(server["sources"], 3);
    assert_eq!(server["documents"], 8);
    assert_eq!(server["chunks"], sample_chunks + notes_chunks);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_push_that_ended_is_listed_with_its_counts(pool: PgPool) {
    let env = env(pool);
    let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
    assert_eq!(pushes["running"], json!([]));
    assert_eq!(pushes["recent"], json!([]));
    assert!(pushes["since"].is_string() && pushes["now"].is_string());

    let (tenant_id, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (source_id, repo) = push_fixture(&env.router, &key).await;
    let first_sha = head(&repo);

    let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
    assert_eq!(pushes["running"], json!([]));
    let recent = pushes["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 1);
    let p = &recent[0];
    assert_eq!(p["id"], 1);
    assert_eq!(p["tenant"], "acme");
    assert_eq!(p["tenant_id"], tenant_id.to_string());
    assert_eq!(p["source"], "sample");
    assert_eq!(p["source_id"], source_id.to_string());
    assert_eq!(p["commit"], first_sha.as_str());
    assert_eq!(p["full"], true);
    assert_eq!(p["state"], "done");
    assert_eq!(p["error"], Value::Null);
    // Six files indexed and the lock file skipped: seven files went through.
    assert_eq!(p["files_total"], 7);
    assert_eq!(p["files_done"], 7);
    assert!(p["chunks"].as_u64().unwrap() >= 6);
    assert!(p["finished_at"].as_str().unwrap() >= p["started_at"].as_str().unwrap());
    // A first push embeds every chunk.
    assert_eq!(p["chunks_embedded"], p["chunks"]);
    assert!(p["embed_ms"].is_u64());

    // An incremental push of a new file and a changed one comes first and is not full. The
    // changed file keeps the vectors of the chunks that did not change.
    std::fs::write(repo.path().join("NOTES.md"), "# Notes\n\nfresh\n").unwrap();
    let plans = repo.path().join("src/billing/plans.rs");
    let mut text = std::fs::read_to_string(&plans).unwrap();
    text.push_str("\npub fn extra_seats() -> u32 {\n    5\n}\n");
    std::fs::write(&plans, text).unwrap();
    commit_all(&repo, "notes");
    let sha = head(&repo);
    let bytes = bundle(&repo, &["HEAD", &format!("^{first_sha}")]);
    let (status, report) = put_bundle(&env.router, &key, source_id, bytes, &sha, false).await;
    assert_eq!(status, StatusCode::OK, "{report}");

    let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
    let recent = pushes["recent"].as_array().unwrap();
    let ids: Vec<u64> = recent.iter().map(|p| p["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, [2, 1]);
    let p = &recent[0];
    assert_eq!(p["commit"], sha.as_str());
    assert_eq!(p["full"], false);
    assert_eq!(p["state"], "done");
    assert_eq!(
        (&p["files_total"], &p["files_done"]),
        (&json!(2), &json!(2))
    );
    assert_eq!(p["chunks"], report["chunks"]);
    let (chunks, embedded) = (
        p["chunks"].as_u64().unwrap(),
        p["chunks_embedded"].as_u64().unwrap(),
    );
    assert!(embedded >= 2, "the note and the new function: {p}");
    assert!(
        embedded < chunks,
        "unchanged chunks keep their vectors: {p}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_same_repo_pushed_into_another_tenant_embeds_nothing(pool: PgPool) {
    let env = env(pool);
    let (_, acme) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (_, globex) = create_tenant_key(&env.router, "globex", &["ingest", "read"]).await;
    push_fixture(&env.router, &acme).await;
    push_fixture(&env.router, &globex).await;

    let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
    let recent = pushes["recent"].as_array().unwrap();
    let (second, first) = (&recent[0], &recent[1]);
    assert_eq!(
        (&first["tenant"], &second["tenant"]),
        (&json!("acme"), &json!("globex"))
    );
    assert_eq!(first["chunks"], second["chunks"]);
    assert_eq!(first["chunks_embedded"], first["chunks"]);
    assert_eq!(
        second["chunks_embedded"], 0,
        "every vector came from the cache: {second}"
    );
}
#[sqlx::test(migrations = "./migrations")]
async fn a_failed_push_is_listed_with_why(pool: PgPool) {
    let env = env(pool);
    let (_, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (source_id, repo) = push_fixture(&env.router, &key).await;
    let prev = head(&repo);
    std::fs::write(repo.path().join("NOTES.md"), "# Notes\n").unwrap();
    commit_all(&repo, "notes");
    let sha = head(&repo);

    // An upload that is not a bundle is refused before it becomes a push.
    let (status, _) = put_bundle(&env.router, &key, source_id, b"nope".to_vec(), &sha, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
    assert_eq!(pushes["recent"].as_array().unwrap().len(), 1);

    // The server lost its mirror, so the incremental bundle cannot be applied.
    std::fs::remove_dir_all(env.data.path().join("repos")).unwrap();
    let bytes = bundle(&repo, &["HEAD", &format!("^{prev}")]);
    let (status, _) = put_bundle(&env.router, &key, source_id, bytes, &sha, false).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
    let p = &pushes["recent"][0];
    assert_eq!(p["id"], 2);
    assert_eq!(p["state"], "failed");
    assert_eq!(p["error"], "need a full bundle (the CLI sends one next)");
    assert_eq!(p["files_total"], Value::Null);
    assert!(p["finished_at"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_push_waiting_for_its_source_is_listed_as_running(pool: PgPool) {
    let env = env(pool);
    let (_, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    let (status, created) = create_repo(&env.router, &key, "sample").await;
    assert_eq!(status, StatusCode::OK);
    let source_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let repo = common::fixture_repo();
    let sha = head(&repo);
    let bytes = bundle(&repo, &["HEAD"]);

    // Hold the source's lock, as a push in progress would.
    let lock = env.state.source_locks.get(source_id);
    let guard = lock.lock().await;
    let pushing = {
        let (router, key, sha) = (env.router.clone(), key.clone(), sha.clone());
        tokio::spawn(async move { put_bundle(&router, &key, source_id, bytes, &sha, true).await })
    };

    let mut running = Value::Null;
    for _ in 0..200 {
        let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
        if !pushes["running"].as_array().unwrap().is_empty() {
            running = pushes["running"][0].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(running["state"], "waiting", "{running}");
    assert_eq!(running["tenant"], "acme");
    assert_eq!(running["source"], "sample");
    assert_eq!(running["commit"], sha.as_str());
    assert_eq!(running["files_total"], Value::Null);
    assert_eq!(running["finished_at"], Value::Null);

    drop(guard);
    let (status, report) = pushing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{report}");
    let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
    assert_eq!(pushes["running"], json!([]));
    assert_eq!(pushes["recent"][0]["state"], "done");
    assert_eq!(pushes["recent"][0]["id"], running["id"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_push_of_a_deleted_tenant_has_no_tenant_name(pool: PgPool) {
    let env = env(pool.clone());
    let (tenant_id, key) = create_tenant_key(&env.router, "acme", &["ingest", "read"]).await;
    push_fixture(&env.router, &key).await;
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .execute(&pool)
        .await
        .unwrap();
    let pushes = admin_get(&env.router, "/v1/admin/pushes").await;
    assert_eq!(pushes["recent"][0]["tenant"], Value::Null);
    assert_eq!(pushes["recent"][0]["source"], "sample");
    let overview = admin_get(&env.router, "/v1/admin/overview").await;
    assert_eq!(overview["tenants"], json!([]));
    assert_eq!(overview["server"]["documents"], 0);
}
