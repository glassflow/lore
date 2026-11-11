mod common;

#[path = "repo_atomic/cache.rs"]
mod cache;

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use axum::Router;
use axum::http::StatusCode;
use common::{
    bundle, commit_all, create_repo, create_tenant_key, fixture_repo, head, put_bundle, send,
    test_router, test_state,
};
use lore::embed::{Embedder, HashEmbedder};
use lore::http::AppState;
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::sync::Notify;
use uuid::Uuid;

/// Stops an actual embedding call after earlier files have already been written. A timeout
/// prevents a failing assertion from leaving a blocking worker alive forever.
struct Gate {
    document: Option<&'static str>,
    query: bool,
    fail: bool,
    reached: Notify,
    released: Mutex<bool>,
    ready: Condvar,
}

impl Gate {
    fn new(document: Option<&'static str>, query: bool, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            document,
            query,
            fail,
            reached: Notify::new(),
            released: Mutex::new(false),
            ready: Condvar::new(),
        })
    }

    fn stop(&self) -> Result<()> {
        self.reached.notify_one();
        let (released, _) = self
            .ready
            .wait_timeout_while(
                self.released.lock().unwrap(),
                Duration::from_secs(30),
                |v| !*v,
            )
            .unwrap();
        if !*released {
            bail!("embedding gate timed out");
        }
        if self.fail {
            bail!("injected embed failure");
        }
        Ok(())
    }

    async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(15), self.reached.notified())
            .await
            .expect("embedding reached the gate");
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.ready.notify_all();
    }
}

impl Embedder for Gate {
    fn name(&self) -> &str {
        "hash"
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if self
            .document
            .is_some_and(|needle| texts.iter().any(|t| t.contains(needle)))
        {
            self.stop()?;
        }
        HashEmbedder.embed_documents(texts)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        if self.query {
            self.stop()?;
        }
        HashEmbedder.embed_query(text)
    }
}

struct Env {
    state: AppState,
    router: Router,
    key: String,
    tenant: Uuid,
    source: Uuid,
    repo: TempDir,
    _data: TempDir,
}

impl Env {
    async fn new(pool: PgPool, indexed: bool) -> Self {
        let data = tempfile::tempdir().unwrap();
        let state = test_state(pool, data.path());
        let router = test_router(state.clone());
        let (tenant, key) = create_tenant_key(&router, "atomic", &["ingest", "read"]).await;
        let (_, source) = create_repo(&router, &key, "sample").await;
        let source = source["id"].as_str().unwrap().parse().unwrap();
        let repo = fixture_repo();
        for (path, text) in [
            ("a.md", "repoatomic old alpha"),
            ("b.md", "repoatomic old beta"),
            ("c.md", "repoatomic old removed"),
            ("d.md", "repoatomic unchanged"),
        ] {
            std::fs::write(repo.path().join(path), text).unwrap();
        }
        commit_all(&repo, "atomic fixture");
        let env = Self {
            state,
            router,
            key,
            tenant,
            source,
            repo,
            _data: data,
        };
        if indexed {
            assert_eq!(env.push(true).await.0, StatusCode::OK);
        }
        env
    }

    fn change(&self) {
        std::fs::write(self.repo.path().join("a.md"), "repoatomic new alpha").unwrap();
        std::fs::write(self.repo.path().join("b.md"), "repoatomic new beta").unwrap();
        std::fs::remove_file(self.repo.path().join("c.md")).unwrap();
        std::fs::write(self.repo.path().join("e.md"), "repoatomic new added").unwrap();
        commit_all(&self.repo, "replace tree");
    }

    fn gated_router(&self, gate: Arc<Gate>) -> Router {
        let mut state = self.state.clone();
        state.embedder = gate;
        test_router(state)
    }

    async fn push(&self, full: bool) -> (StatusCode, Value) {
        put_bundle(
            &self.router,
            &self.key,
            self.source,
            bundle(&self.repo, &["HEAD"]),
            &head(&self.repo),
            full,
        )
        .await
    }

    fn start_push(
        &self,
        router: Router,
        full: bool,
    ) -> tokio::task::JoinHandle<(StatusCode, Value)> {
        let key = self.key.clone();
        let source = self.source;
        let bytes = bundle(&self.repo, &["HEAD"]);
        let sha = head(&self.repo);
        tokio::spawn(async move { put_bundle(&router, &key, source, bytes, &sha, full).await })
    }

    async fn search(&self) -> Value {
        let (status, body) = send(
            &self.router,
            "GET",
            "/v1/search?q=repoatomic&source=sample&k=50",
            &self.key,
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn commit(&self) -> Option<String> {
        sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
            .bind(self.source)
            .fetch_one(&self.state.pool)
            .await
            .unwrap()
    }
}

fn contents(body: &Value) -> BTreeMap<String, String> {
    body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["snippet"].as_str().unwrap().contains("repoatomic"))
        .map(|h| {
            (
                h["path"].as_str().unwrap().to_string(),
                h["snippet"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn new_contents() -> BTreeMap<String, String> {
    [
        ("a.md", "repoatomic new alpha"),
        ("b.md", "repoatomic new beta"),
        ("d.md", "repoatomic unchanged"),
        ("e.md", "repoatomic new added"),
    ]
    .into_iter()
    .map(|(p, t)| (p.to_string(), t.to_string()))
    .collect()
}

#[sqlx::test(migrations = "./migrations")]
async fn changed_new_deleted_and_unchanged_files_publish_together(pool: PgPool) {
    let env = Env::new(pool.clone(), true).await;
    let old_commit = env.commit().await;
    let old = env.search().await;
    let unchanged_id = old["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["path"] == "d.md")
        .unwrap()["chunk_id"]
        .clone();
    env.change();
    let gate = Gate::new(Some("repoatomic new beta"), false, false);
    let push = env.start_push(env.gated_router(gate.clone()), false);
    gate.wait().await;
    assert_eq!(contents(&env.search().await), contents(&old));
    assert_eq!(env.commit().await, old_commit);
    for path in [
        format!("/v1/admin/sources/{}", env.source),
        format!("/v1/admin/tenants/{}", env.tenant),
    ] {
        let (status, _) = tokio::time::timeout(
            Duration::from_secs(1),
            send(&env.router, "DELETE", &path, "t", Vec::new()),
        )
        .await
        .expect("busy delete must not wait for embedding");
        assert_eq!(status, StatusCode::CONFLICT);
    }

    // Stored document reads must not use the already-updated checkout.
    for hit in old["hits"].as_array().unwrap() {
        if hit["path"] == "a.md" || hit["path"] == "c.md" {
            let uri = format!("/v1/documents/{}", hit["document_id"].as_str().unwrap());
            let (status, doc) = send(&env.router, "GET", &uri, &env.key, Vec::new()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(doc["text"], hit["snippet"]);
            assert_eq!(
                doc["metadata"]["commit"],
                old_commit.as_ref().unwrap().as_str()
            );
        }
    }
    // A standalone documents write on the same tenant stays usable during a repo transaction.
    let (status, note) = send(
        &env.router,
        "POST",
        "/v1/documents",
        &env.key,
        json!({"source":"notes","documents":[{"external_id":"n","title":"n","text":"independent"}]})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{note}");
    gate.release();
    let (status, report) = push.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{report}");
    let new = env.search().await;
    assert_eq!(contents(&new), new_contents());
    assert_eq!(env.commit().await, Some(head(&env.repo)));
    assert!(
        new["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["chunk_id"] == unchanged_id)
    );
    assert_eq!(report["files_deleted"], 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn first_push_has_no_partial_search_results(pool: PgPool) {
    let env = Env::new(pool, false).await;
    let gate = Gate::new(Some("repoatomic old beta"), false, false);
    let push = env.start_push(env.gated_router(gate.clone()), true);
    gate.wait().await;
    assert!(env.search().await["hits"].as_array().unwrap().is_empty());
    assert_eq!(env.commit().await, None);
    gate.release();
    assert_eq!(push.await.unwrap().0, StatusCode::OK);
    assert_eq!(contents(&env.search().await).len(), 4);
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_and_cancelled_pushes_roll_back_and_retry_cleanly(pool: PgPool) {
    let env = Env::new(pool, true).await;
    let old = contents(&env.search().await);
    let old_commit = env.commit().await;
    env.change();
    for cancel in [false, true] {
        let gate = Gate::new(Some("repoatomic new beta"), false, !cancel);
        let push = env.start_push(env.gated_router(gate.clone()), true);
        gate.wait().await;
        assert_eq!(contents(&env.search().await), old);
        if cancel {
            push.abort();
        }
        gate.release();
        if cancel {
            assert!(push.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(push.await.unwrap().0, StatusCode::INTERNAL_SERVER_ERROR);
        }
        assert_eq!(contents(&env.search().await), old);
        assert_eq!(env.commit().await, old_commit);
    }
    assert_eq!(env.push(false).await.0, StatusCode::OK);
    assert_eq!(contents(&env.search().await), new_contents());
}

#[sqlx::test(migrations = "./migrations")]
async fn publication_failure_rolls_back_deleted_files_and_new_documents(pool: PgPool) {
    let env = Env::new(pool.clone(), true).await;
    let old = env.search().await;
    let old_commit = env.commit().await;
    env.change();
    sqlx::raw_sql(
        "CREATE FUNCTION reject_publication() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN RAISE EXCEPTION 'injected publication failure'; END
         $$;
         CREATE TRIGGER reject_publication BEFORE UPDATE OF indexed_commit ON sources
         FOR EACH ROW EXECUTE FUNCTION reject_publication();",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(env.push(false).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(env.search().await, old);
    assert_eq!(env.commit().await, old_commit);
    sqlx::query("DROP TRIGGER reject_publication ON sources")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(env.push(false).await.0, StatusCode::OK);
    assert_eq!(contents(&env.search().await), new_contents());
}

#[sqlx::test(migrations = "./migrations")]
async fn search_snapshot_survives_publication_between_ranking_branches_and_hydration(pool: PgPool) {
    let env = Env::new(pool, true).await;
    let old = env.search().await;
    let gate = Gate::new(None, true, false);
    let router = env.gated_router(gate.clone());
    let key = env.key.clone();
    let search = tokio::spawn(async move {
        send(
            &router,
            "GET",
            "/v1/search?q=repoatomic&source=sample&k=50",
            &key,
            Vec::new(),
        )
        .await
    });
    // search has run the lexical branch and is now waiting to embed its vector query.
    gate.wait().await;
    env.change();
    assert_eq!(env.push(false).await.0, StatusCode::OK);
    assert_eq!(contents(&env.search().await), new_contents());
    gate.release();
    let (status, result) = search.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        result, old,
        "all branches and hydration must retain their original snapshot"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn overlapping_pushes_rebase_on_the_last_published_commit(pool: PgPool) {
    let env = Env::new(pool, true).await;
    env.change();
    let gate = Gate::new(Some("repoatomic new beta"), false, false);
    let first = env.start_push(env.gated_router(gate.clone()), false);
    gate.wait().await;
    std::fs::write(env.repo.path().join("a.md"), "repoatomic final alpha").unwrap();
    commit_all(&env.repo, "third version");
    let second = env.start_push(env.router.clone(), false);
    gate.release();
    assert_eq!(first.await.unwrap().0, StatusCode::OK);
    let (status, report) = second.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["files_indexed"], 1);
    let mut expected = new_contents();
    expected.insert("a.md".to_string(), "repoatomic final alpha".to_string());
    assert_eq!(contents(&env.search().await), expected);
    assert_eq!(env.commit().await, Some(head(&env.repo)));
}

async fn delete_during_push(pool: PgPool, tenant: bool) {
    let env = Env::new(pool.clone(), true).await;
    env.change();
    let gate = Gate::new(Some("repoatomic new beta"), false, false);
    let push = env.start_push(env.gated_router(gate.clone()), false);
    gate.wait().await;
    let id = if tenant { env.tenant } else { env.source };
    let mut deletion = tokio::spawn(async move {
        sqlx::query(if tenant {
            "DELETE FROM tenants WHERE id = $1"
        } else {
            "DELETE FROM sources WHERE id = $1"
        })
        .bind(id)
        .execute(&pool)
        .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut deletion)
            .await
            .is_err()
    );
    gate.release();
    assert_eq!(push.await.unwrap().0, StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(5), deletion)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    if tenant {
        let (status, _) = send(
            &env.router,
            "GET",
            "/v1/search?q=repoatomic",
            &env.key,
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    } else {
        assert!(env.search().await["hits"].as_array().unwrap().is_empty());
    }
    let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE tenant_id = $1")
        .bind(env.tenant)
        .fetch_one(&env.state.pool)
        .await
        .unwrap();
    assert_eq!(chunks, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn source_delete_cannot_interleave_with_repo_publication(pool: PgPool) {
    delete_during_push(pool, false).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn tenant_cascade_cannot_deadlock_with_repo_publication(pool: PgPool) {
    delete_during_push(pool, true).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn stored_document_metadata_and_chunks_share_a_snapshot(pool: PgPool) {
    let env = Env::new(pool.clone(), true).await;
    let id: Uuid = sqlx::query_scalar(
        "SELECT id FROM documents WHERE source_id = $1 AND external_id = 'a.md'",
    )
    .bind(env.source)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE chunks IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let reader_pool = pool.clone();
    let tenant = env.tenant;
    let reader =
        tokio::spawn(async move { lore::store::document_text(&reader_pool, tenant, id).await });
    // Wait until metadata has been read and the chunk SELECT is blocked on our table lock.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND query LIKE 'SELECT text, start_line, end_line FROM chunks%')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    sqlx::query("UPDATE documents SET metadata = '{\"version\":\"new\"}' WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE chunks SET text = 'new content' WHERE document_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (row, text) = reader.await.unwrap().unwrap();
    assert_eq!(row.metadata["commit"], head(&env.repo));
    assert_eq!(text, "repoatomic old alpha");
    let (row, text) = lore::store::document_text(&pool, tenant, id).await.unwrap();
    assert_eq!(row.metadata, json!({"version":"new"}));
    assert_eq!(text, "new content");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_one_connection_pool_finishes_without_nested_acquisition(pool: PgPool) {
    let small = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let env = Env::new(small, false).await;
    tokio::time::timeout(Duration::from_secs(15), env.push(true))
        .await
        .unwrap();
    env.change();
    let (status, report) = tokio::time::timeout(Duration::from_secs(15), env.push(false))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(contents(&env.search().await), new_contents());
}

/// The default is a quick pool regression. Set LORE_ATOMIC_BENCH_FILES=1930 and run only this
/// test from a fresh process for a roughly 24k-chunk push, with the same real HTTP paths.
#[sqlx::test(migrations = "./migrations")]
async fn large_push_leaves_the_other_pool_connection_available_for_search(pool: PgPool) {
    let files: usize = std::env::var("LORE_ATOMIC_BENCH_FILES")
        .ok()
        .map(|n| n.parse().unwrap())
        .unwrap_or(16);
    let small = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let env = Env::new(small, false).await;
    for n in 0..files {
        let text = (0..12)
            .map(|section| format!("# Subject {section}\natomicbulk file {n} section {section}\n"))
            .collect::<String>();
        std::fs::write(env.repo.path().join(format!("bulk{n:05}.md")), text).unwrap();
    }
    std::fs::write(env.repo.path().join("zzz.md"), "atomicbulk gate").unwrap();
    commit_all(&env.repo, "large fixture");
    let gate = Gate::new(Some("atomicbulk gate"), false, false);
    let started = std::time::Instant::now();
    let push = env.start_push(env.gated_router(gate.clone()), true);
    // Larger manual runs may take over the gate helper's normal 15-second deadline.
    tokio::time::timeout(Duration::from_secs(180), gate.reached.notified())
        .await
        .unwrap();
    assert_eq!(env.state.pool.size(), 2);
    let search_started = std::time::Instant::now();
    assert!(env.search().await["hits"].as_array().unwrap().is_empty());
    let search_time = search_started.elapsed();
    gate.release();
    let (status, report) = push.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(report["chunks"].as_u64().unwrap() >= files as u64 * 12);
    println!(
        "atomic push: files={files}, chunks={}, wall={:?}, search_during_push={search_time:?}, pool_connections={}",
        report["chunks"],
        started.elapsed(),
        env.state.pool.size(),
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn remote_sync_uses_atomic_publication_and_busy_delete_admission(pool: PgPool) {
    let env = Env::new(pool, true).await;
    let old = env.search().await;
    let old_commit = env.commit().await;
    let mut state = env.state.clone();
    Arc::make_mut(&mut state.config).sync_allow_file_remotes = true;
    let router = test_router(state.clone());
    let (status, _) = send(
        &router,
        "POST",
        "/v1/repos",
        &env.key,
        json!({"name":"sample","remote_url":format!("file://{}", env.repo.path().display())})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    env.change();
    for fail in [true, false] {
        let gate = Gate::new(Some("repoatomic new beta"), false, fail);
        let mut sync_state = state.clone();
        sync_state.embedder = gate.clone();
        let sync_router = test_router(sync_state);
        let key = env.key.clone();
        let uri = format!("/v1/repos/{}/sync", env.source);
        let sync =
            tokio::spawn(async move { send(&sync_router, "POST", &uri, &key, Vec::new()).await });
        gate.wait().await;
        assert_eq!(env.search().await, old);
        assert_eq!(env.commit().await, old_commit);
        for path in [
            format!("/v1/admin/sources/{}", env.source),
            format!("/v1/admin/tenants/{}", env.tenant),
        ] {
            let (status, _) = tokio::time::timeout(
                Duration::from_secs(1),
                send(&router, "DELETE", &path, "t", Vec::new()),
            )
            .await
            .expect("sync busy delete must be immediate");
            assert_eq!(status, StatusCode::CONFLICT);
        }
        gate.release();
        let (status, result) = sync.await.unwrap();
        if fail {
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{result}");
            assert_eq!(env.search().await, old);
            assert_eq!(env.commit().await, old_commit);
        } else {
            assert_eq!(status, StatusCode::OK, "{result}");
            assert_eq!(contents(&env.search().await), new_contents());
            assert_eq!(env.commit().await, Some(head(&env.repo)));
        }
    }
}
