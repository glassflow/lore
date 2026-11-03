mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use common::{create_tenant_key, send, test_router, test_state_with};
use lore::embed::{Embedder, HashEmbedder};
use lore::http::AppState;
use lore::search::{SearchQuery, search};
use lore::sessions::{dequeue_if_unchanged, process_due_sessions};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const CLAUDE_CODE: &str = include_str!("fixtures/otlp/claude_code_session.json");
const SESSION_ID: &str = "sess-fixture-1";

struct Env {
    state: AppState,
    router: Router,
    tenant: Uuid,
    key: String,
    _data: tempfile::TempDir,
}

impl Env {
    async fn new(pool: PgPool, quiet_secs: u64) -> Env {
        let data = tempfile::tempdir().unwrap();
        let state = test_state_with(pool, data.path(), |cfg| cfg.session_quiet_secs = quiet_secs);
        let router = test_router(state.clone());
        let (tenant, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        Env {
            state,
            router,
            tenant,
            key,
            _data: data,
        }
    }

    async fn post(&self, body: &str) {
        let req = Request::post("/v1/traces")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.key))
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    async fn queue_len(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM session_queue WHERE tenant_id = $1")
            .bind(self.tenant)
            .fetch_one(&self.state.pool)
            .await
            .unwrap()
    }

    async fn last_span_at(&self) -> DateTime<Utc> {
        sqlx::query_scalar(
            "SELECT last_span_at FROM session_queue WHERE tenant_id = $1 AND session_id = $2",
        )
        .bind(self.tenant)
        .bind(SESSION_ID)
        .fetch_one(&self.state.pool)
        .await
        .unwrap()
    }

    /// `(updated_at, content_hash)` of the session document.
    async fn document_stamp(&self) -> (DateTime<Utc>, String) {
        sqlx::query_as(
            "SELECT updated_at, content_hash FROM documents
             WHERE tenant_id = $1 AND kind = 'session' AND external_id = $2",
        )
        .bind(self.tenant)
        .bind(SESSION_ID)
        .fetch_one(&self.state.pool)
        .await
        .unwrap()
    }

    async fn sessions_found(&self, q: &str) -> Vec<lore::search::Hit> {
        search(
            &self.state.pool,
            self.state.embedder.clone(),
            SearchQuery {
                tenant_id: self.tenant,
                q: q.to_string(),
                k: 5,
                kinds: Some(vec!["session".to_string()]),
                sources: None,
            },
        )
        .await
        .unwrap()
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_posted_session_becomes_a_searchable_document(pool: PgPool) {
    let env = Env::new(pool, 0).await;
    env.post(CLAUDE_CODE).await;
    assert_eq!(env.queue_len().await, 1);

    assert_eq!(process_due_sessions(&env.state, 20).await.unwrap(), 1);
    assert_eq!(env.queue_len().await, 0);

    let hits = env.sessions_found("how many plans").await;
    let first = hits.first().expect("a session hit");
    assert_eq!(first.kind, "session");
    assert!(first.title.starts_with("How many plans"), "{}", first.title);
    assert_eq!(first.path, None);
    assert_eq!(first.uri, None);
    assert_eq!(first.external_id.as_deref(), Some(SESSION_ID));

    // The HTTP reply carries the session id too.
    let (status, body) = send(
        &env.router,
        "GET",
        "/v1/search?q=how%20many%20plans&kind=session",
        &env.key,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hits"][0]["kind"], "session");
    assert_eq!(body["hits"][0]["external_id"], SESSION_ID);
    assert!(body["hits"][0]["path"].is_null());
    assert!(body["hits"][0]["uri"].is_null());

    let (_, hash) = env.document_stamp().await;
    assert_eq!(hash.len(), 64);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unchanged_session_is_not_indexed_again(pool: PgPool) {
    let env = Env::new(pool, 0).await;
    env.post(CLAUDE_CODE).await;
    process_due_sessions(&env.state, 20).await.unwrap();
    let before = env.document_stamp().await;

    // The same spans again queue the session, but the rendered text is the same.
    env.post(CLAUDE_CODE).await;
    assert_eq!(env.queue_len().await, 1);
    assert_eq!(process_due_sessions(&env.state, 20).await.unwrap(), 1);
    assert_eq!(env.queue_len().await, 0);
    assert_eq!(env.document_stamp().await, before);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_changed_session_is_indexed_again(pool: PgPool) {
    let env = Env::new(pool, 0).await;
    env.post(CLAUDE_CODE).await;
    process_due_sessions(&env.state, 20).await.unwrap();
    let before = env.document_stamp().await;

    env.post(&CLAUDE_CODE.replace("There are three plans", "There are four plans"))
        .await;
    process_due_sessions(&env.state, 20).await.unwrap();
    let after = env.document_stamp().await;
    assert_ne!(after.1, before.1);
    assert!(after.0 > before.0);

    let hits = env.sessions_found("there are four plans").await;
    assert!(hits[0].text.contains("four plans"), "{}", hits[0].text);
    let documents: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents WHERE tenant_id = $1 AND kind = 'session'",
    )
    .bind(env.tenant)
    .fetch_one(&env.state.pool)
    .await
    .unwrap();
    assert_eq!(documents, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_session_that_is_still_active_waits(pool: PgPool) {
    let env = Env::new(pool, 3600).await;
    env.post(CLAUDE_CODE).await;
    assert_eq!(process_due_sessions(&env.state, 20).await.unwrap(), 0);
    assert_eq!(env.queue_len().await, 1);
    assert!(env.sessions_found("how many plans").await.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn new_spans_keep_the_queue_row(pool: PgPool) {
    let env = Env::new(pool, 0).await;
    env.post(CLAUDE_CODE).await;
    let read = env.last_span_at().await;

    // A span arrives after the worker read the row: its delete must not remove the row.
    env.post(CLAUDE_CODE).await;
    let removed = dequeue_if_unchanged(&env.state.pool, env.tenant, SESSION_ID, read)
        .await
        .unwrap();
    assert!(!removed);
    assert_eq!(env.queue_len().await, 1);

    let current = env.last_span_at().await;
    assert!(current > read);
    let removed = dequeue_if_unchanged(&env.state.pool, env.tenant, SESSION_ID, current)
        .await
        .unwrap();
    assert!(removed);
    assert_eq!(env.queue_len().await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn another_tenants_session_is_not_found(pool: PgPool) {
    let env = Env::new(pool, 0).await;
    let (other, _) = create_tenant_key(&env.router, "other", &["read"]).await;
    env.post(CLAUDE_CODE).await;
    process_due_sessions(&env.state, 20).await.unwrap();
    let hits = search(
        &env.state.pool,
        env.state.embedder.clone(),
        SearchQuery {
            tenant_id: other,
            q: "how many plans".to_string(),
            k: 5,
            kinds: None,
            sources: None,
        },
    )
    .await
    .unwrap();
    assert!(hits.is_empty());
}

/// The fixture with every span marked as an unfinished snapshot.
fn all_pending() -> String {
    let mut v: serde_json::Value = serde_json::from_str(CLAUDE_CODE).unwrap();
    for rs in v["resourceSpans"].as_array_mut().unwrap() {
        for ss in rs["scopeSpans"].as_array_mut().unwrap() {
            for span in ss["spans"].as_array_mut().unwrap() {
                span["attributes"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({
                        "key": "glassflow.span.pending",
                        "value": { "boolValue": true }
                    }));
            }
        }
    }
    v.to_string()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_session_of_pending_spans_only_is_not_indexed(pool: PgPool) {
    let env = Env::new(pool, 0).await;
    env.post(&all_pending()).await;
    assert_eq!(env.queue_len().await, 1);

    process_due_sessions(&env.state, 20).await.unwrap();
    assert_eq!(env.queue_len().await, 0);
    let documents: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents WHERE tenant_id = $1 AND kind = 'session'",
    )
    .bind(env.tenant)
    .fetch_one(&env.state.pool)
    .await
    .unwrap();
    assert_eq!(documents, 0);
}

/// A `HashEmbedder` that counts the texts it is asked to embed.
struct Counting {
    texts: AtomicUsize,
}

impl Embedder for Counting {
    fn name(&self) -> &str {
        "hash"
    }

    fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        HashEmbedder.embed_documents(texts)
    }

    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        HashEmbedder.embed_query(text)
    }
}

/// A session of `turns` chat spans, each with about 4000 characters of text, so that every few
/// turns make a chunk of their own.
fn long_session(turns: usize) -> String {
    let spans: Vec<serde_json::Value> = (0..turns)
        .map(|n| {
            let filler = |w: &str| format!("{w} ").repeat(300);
            serde_json::json!({
                "traceId": "5b8efff798038103d269b633813fc60c",
                "spanId": format!("b1b2c3d4e5f6{n:04x}"),
                "name": "chat claude-opus-5-5",
                "kind": 1,
                "startTimeUnixNano": format!("{}", 1_791_450_000_000_000_000_u64 + n as u64 * 1_000_000_000),
                "endTimeUnixNano": format!("{}", 1_791_450_000_500_000_000_u64 + n as u64 * 1_000_000_000),
                "attributes": [
                    { "key": "session.id", "value": { "stringValue": SESSION_ID } },
                    { "key": "gen_ai.operation.name", "value": { "stringValue": "chat" } },
                    { "key": "input.value", "value": { "stringValue": format!("sentinel{n}q {}", filler("ask")) } },
                    { "key": "output.value", "value": { "stringValue": format!("sentinel{n}a {}", filler("reply")) } },
                ],
            })
        })
        .collect();
    serde_json::json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                { "key": "service.name", "value": { "stringValue": "claude-code" } }
            ] },
            "scopeSpans": [{ "scope": { "name": "t" }, "spans": spans }],
        }]
    })
    .to_string()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_grown_session_embeds_only_new_or_changed_chunks(pool: PgPool) {
    let mut env = Env::new(pool, 0).await;
    let counter = Arc::new(Counting {
        texts: AtomicUsize::new(0),
    });
    env.state.embedder = counter.clone();
    env.router = test_router(env.state.clone());
    let embedded = || counter.texts.load(Ordering::SeqCst);
    let stored = |env: &Env| {
        let pool = env.state.pool.clone();
        let tenant = env.tenant;
        async move {
            sqlx::query_as::<_, (String, String)>(
                "SELECT c.text, c.embedding::text FROM chunks c JOIN documents d ON d.id = c.document_id
                 WHERE d.tenant_id = $1 AND d.kind = 'session' ORDER BY c.text",
            )
            .bind(tenant)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };

    env.post(&long_session(8)).await;
    process_due_sessions(&env.state, 20).await.unwrap();
    let first_chunks = stored(&env).await.len();
    assert!(first_chunks >= 4, "{first_chunks} chunks");
    assert_eq!(
        embedded(),
        first_chunks,
        "the first index embeds every chunk"
    );

    // One more turn: the header and the last chunk change, the chunks between them do not.
    env.post(&long_session(9)).await;
    process_due_sessions(&env.state, 20).await.unwrap();
    let after = stored(&env).await;
    let second_run = embedded() - first_chunks;
    assert!(after.len() > first_chunks, "{} chunks", after.len());
    assert!(second_run >= 1, "something changed");
    assert!(
        second_run <= 3 && second_run < after.len(),
        "{second_run} of {} chunks were embedded again",
        after.len()
    );

    // Every turn is still found, and the stored vectors are the ones a full index would make.
    for n in 0..9 {
        let hits = env.sessions_found(&format!("sentinel{n}q")).await;
        assert!(
            hits.iter()
                .any(|h| h.text.contains(&format!("sentinel{n}q"))),
            "turn {n} not found"
        );
    }
    sqlx::query("DELETE FROM documents WHERE tenant_id = $1 AND kind = 'session'")
        .bind(env.tenant)
        .execute(&env.state.pool)
        .await
        .unwrap();
    env.post(&long_session(9)).await;
    process_due_sessions(&env.state, 20).await.unwrap();
    assert_eq!(stored(&env).await, after);

    // The same text again embeds nothing.
    let before_noop = embedded();
    env.post(&long_session(9)).await;
    process_due_sessions(&env.state, 20).await.unwrap();
    assert_eq!(embedded(), before_noop);
}
