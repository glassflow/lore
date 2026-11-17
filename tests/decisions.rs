//! Decision documents: the metadata on `POST /v1/documents`, in the prompt, on citations and on
//! `GET /v1/documents/{id}`; the `kind` filter on search and ask; and `GET /v1/documents`.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::http::StatusCode;
use common::{create_tenant_key, send, test_router, test_state};
use lore::llm::{Block, FakeLlm, Llm, LlmRequest, LlmResponse, Role, Usage};
use serde_json::{Value, json};
use sqlx::PgPool;

const QUEUE_QUESTION: &str = "why did we choose postgres over kafka for the event queue?";
const QUEUE_TEXT: &str =
    "We chose postgres over kafka for the event queue: one less system to run.";

/// A model double that records every request and plays its script before falling back to the
/// fake model.
struct Probe {
    inner: FakeLlm,
    script: Mutex<VecDeque<LlmResponse>>,
    calls: Mutex<Vec<LlmRequest>>,
}

impl Probe {
    fn new(script: Vec<LlmResponse>) -> Arc<Probe> {
        Arc::new(Probe {
            inner: FakeLlm::new(),
            script: Mutex::new(script.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<LlmRequest> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl Llm for Probe {
    fn name(&self) -> &str {
        "probe"
    }

    fn supports_tools(&self) -> bool {
        true
    }

    async fn complete(&self, req: LlmRequest) -> anyhow::Result<LlmResponse> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(req.clone());
        let next = self
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match next {
            Some(res) => Ok(res),
            None => self.inner.complete(req).await,
        }
    }
}

fn reply(blocks: Vec<Block>, stop_reason: &str) -> LlmResponse {
    LlmResponse {
        content: blocks,
        stop_reason: stop_reason.to_string(),
        usage: Usage::default(),
        model: "probe".to_string(),
    }
}

/// Every text the request carries: the system prompt, the messages and the tool results.
fn request_text(req: &LlmRequest) -> String {
    let mut out = req.system.clone();
    for m in &req.messages {
        for b in &m.content {
            match b {
                Block::Text { text } => out.push_str(text),
                Block::ToolResult { content, .. } => out.push_str(content),
                _ => {}
            }
            out.push('\n');
        }
    }
    out
}

struct Env {
    router: Router,
    key: String,
    pool: PgPool,
    probe: Arc<Probe>,
    _data: tempfile::TempDir,
}

impl Env {
    async fn new(pool: PgPool) -> Env {
        Env::scripted(pool, Vec::new()).await
    }

    async fn scripted(pool: PgPool, script: Vec<LlmResponse>) -> Env {
        let data = tempfile::tempdir().unwrap();
        let probe = Probe::new(script);
        let mut state = test_state(pool.clone(), data.path());
        state.llm = probe.clone();
        let router = test_router(state);
        let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        Env {
            router,
            key,
            pool,
            probe,
            _data: data,
        }
    }

    async fn call(&self, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        send(&self.router, method, uri, &self.key, bytes).await
    }

    async fn put(&self, doc: Value) -> (StatusCode, Value) {
        self.call(
            "POST",
            "/v1/documents",
            json!({ "source": "decisions", "documents": [doc] }),
        )
        .await
    }

    async fn put_ok(&self, doc: Value) {
        let (status, body) = self.put(doc).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    async fn id_of(&self, external_id: &str) -> String {
        let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM documents WHERE external_id = $1")
            .bind(external_id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        id.to_string()
    }

    async fn ask(&self, body: Value) -> Value {
        let (status, body) = self.call("POST", "/v1/ask", body).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }
}

fn decision(external_id: &str, title: &str, text: &str, metadata: Value) -> Value {
    json!({
        "external_id": external_id,
        "title": title,
        "text": text,
        "kind": "decision",
        "metadata": metadata,
    })
}

fn queue_decision() -> Value {
    decision("d-queue", "Queue storage", QUEUE_TEXT, full_metadata())
}

fn full_metadata() -> Value {
    json!({
        "decided_by": ["Ashish Bagri"],
        "decided_at": "2026-08-03",
        "changed_by": ["Ana Cole"],
        "changed_at": "2026-09-01T09:30:00Z",
        "pr": "https://github.com/glassflow/lore/pull/42",
        "where": "repo_folder",
        "repo": "glassflow/lore",
        "project": "software-second-brain",
        "added_by": "factory"
    })
}

fn note(external_id: &str, text: &str) -> Value {
    json!({ "external_id": external_id, "title": "Note", "text": text, "kind": "note" })
}

// ---------------------------------------------------------------------------------------------
// the prompt and the citations

#[sqlx::test(migrations = "./migrations")]
async fn decision_citation_has_who_and_when_in_the_prompt_and_on_the_citation(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put_ok(queue_decision()).await;

    let body = env.ask(json!({ "question": QUEUE_QUESTION })).await;

    // The model saw who decided and when, in front of the passage.
    let calls = env.probe.calls();
    assert_eq!(calls.len(), 1);
    let prompt = request_text(&calls[0]);
    assert!(
        prompt.contains(
            "[1] Queue storage (decision, decided by Ashish Bagri on 3 August 2026)\n```\n"
        ),
        "{prompt}"
    );
    assert!(prompt.contains(QUEUE_TEXT), "{prompt}");
    // The system prompt tells the model to say so.
    assert!(
        calls[0].system.contains("who decided and"),
        "{}",
        calls[0].system
    );

    // The citation carries all of the decision's metadata.
    let citation = &body["citations"][0];
    assert_eq!(citation["kind"], "decision");
    assert_eq!(citation["metadata"], full_metadata(), "{body}");
    assert_eq!(citation["metadata"]["decided_by"], json!(["Ashish Bagri"]));
    assert_eq!(citation["metadata"]["decided_at"], "2026-08-03");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_note_citation_has_no_metadata_and_the_prompt_says_note(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put_ok(note("n1", QUEUE_TEXT)).await;

    let body = env.ask(json!({ "question": QUEUE_QUESTION })).await;
    assert!(body["citations"][0].get("metadata").is_none(), "{body}");
    let prompt = request_text(&env.probe.calls()[0]);
    assert!(prompt.contains("[1] Note (note)\n"), "{prompt}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_decision_with_only_some_metadata_still_asks_and_cites(pool: PgPool) {
    let env = Env::new(pool).await;
    // The metadata of a decision stored before validation, or edited by hand in the database,
    // can lack the fields: nothing may panic and no placeholder may reach the prompt.
    env.put_ok(queue_decision()).await;
    sqlx::query("UPDATE documents SET metadata = '{\"decided_by\": [\"\", \"  \"]}'::jsonb")
        .execute(&env.pool)
        .await
        .unwrap();

    let body = env.ask(json!({ "question": QUEUE_QUESTION })).await;
    let prompt = request_text(&env.probe.calls()[0]);
    assert!(
        prompt.contains("[1] Queue storage (decision)\n"),
        "{prompt}"
    );
    assert!(
        !prompt.contains("None") && !prompt.contains("null"),
        "{prompt}"
    );
    assert_eq!(body["citations"][0]["kind"], "decision");
    assert!(body["citations"][0].get("metadata").is_none(), "{body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn sources_mode_shows_who_decided_and_when(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put_ok(queue_decision()).await;

    let body = env
        .ask(json!({ "question": QUEUE_QUESTION, "mode": "sources" }))
        .await;
    let answer = body["answer"].as_str().unwrap();
    assert!(
        answer.contains("[decision, decided by Ashish Bagri on 3 August 2026]"),
        "{answer}"
    );
    assert_eq!(
        body["citations"][0]["metadata"]["decided_by"],
        json!(["Ashish Bagri"])
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn deep_decision_citation_by_id_carries_the_metadata(pool: PgPool) {
    // Script: the model searches (asking for notes, which the request's filter overrides), then
    // answers citing the decision by its id.
    let env = Env::scripted(pool, Vec::new()).await;
    env.put_ok(queue_decision()).await;
    env.put_ok(note("n1", QUEUE_TEXT)).await;
    let id = env.id_of("d-queue").await;
    let probe = Probe::new(vec![
        reply(
            vec![Block::ToolUse {
                id: "t1".into(),
                name: "search".into(),
                input: json!({ "query": "postgres kafka event queue", "kind": ["note"] }),
            }],
            "tool_use",
        ),
        reply(
            vec![Block::Text {
                text: format!("Ashish Bagri decided on 3 August 2026 [doc:{id}]."),
            }],
            "end_turn",
        ),
    ]);
    let data = tempfile::tempdir().unwrap();
    let mut state = test_state(env.pool.clone(), data.path());
    state.llm = probe.clone();
    let router = test_router(state);

    let (status, body) = send(
        &router,
        "POST",
        "/v1/ask",
        &env.key,
        json!({ "question": QUEUE_QUESTION, "mode": "deep", "kind": ["decision"] })
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The search the model asked for was limited to decisions.
    let calls = probe.calls();
    let result = calls[1]
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            Block::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("a tool result");
    assert!(
        result.contains(&format!("doc:{id} (Queue storage) [decision, decided by")),
        "{result}"
    );
    assert!(!result.contains("(Note)"), "{result}");
    assert!(calls[1].messages.iter().any(|m| m.role == Role::Assistant));

    let citation = &body["citations"][0];
    assert_eq!(citation["label"], format!("doc:{id}"));
    assert_eq!(citation["document_id"], id);
    assert_eq!(citation["kind"], "decision");
    assert_eq!(citation["title"], "Queue storage");
    assert_eq!(citation["metadata"], full_metadata(), "{body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn get_document_returns_the_decision_metadata(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put_ok(queue_decision()).await;
    let id = env.id_of("d-queue").await;

    let (status, doc) = env
        .call("GET", &format!("/v1/documents/{id}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert_eq!(doc["kind"], "decision");
    assert_eq!(doc["metadata"], full_metadata());
    assert_eq!(doc["text"], QUEUE_TEXT);
}

// ---------------------------------------------------------------------------------------------
// validation

#[sqlx::test(migrations = "./migrations")]
async fn a_decision_needs_decided_by_and_decided_at(pool: PgPool) {
    let env = Env::new(pool).await;
    let with = |metadata: Value| decision("d1", "T", "text", metadata);
    let cases = [
        (
            "no metadata at all",
            json!({ "external_id": "d1", "title": "T", "text": "x", "kind": "decision" }),
            "metadata.decided_by is required",
        ),
        (
            "no decided_by",
            with(json!({ "decided_at": "2026-08-03" })),
            "metadata.decided_by is required",
        ),
        (
            "empty decided_by",
            with(json!({ "decided_by": [], "decided_at": "2026-08-03" })),
            "metadata.decided_by",
        ),
        (
            "blank name",
            with(json!({ "decided_by": ["  "], "decided_at": "2026-08-03" })),
            "metadata.decided_by",
        ),
        (
            "decided_by is not a list of strings",
            with(json!({ "decided_by": [1], "decided_at": "2026-08-03" })),
            "metadata.decided_by",
        ),
        (
            "no decided_at",
            with(json!({ "decided_by": ["Ana"] })),
            "metadata.decided_at is required",
        ),
        (
            "decided_at is not a date",
            with(json!({ "decided_by": ["Ana"], "decided_at": "last tuesday" })),
            "metadata.decided_at",
        ),
        (
            "where is not one of the three",
            with(json!({ "decided_by": ["Ana"], "decided_at": "2026-08-03", "where": "slack" })),
            "metadata.where must be one of repo_folder, factory_project, by_hand",
        ),
        (
            "pr is not a string",
            with(json!({ "decided_by": ["Ana"], "decided_at": "2026-08-03", "pr": 42 })),
            "metadata.pr",
        ),
    ];
    for (name, doc, message) in cases {
        let (status, body) = env.put(doc).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains(message), "{name}: {error}");
        assert!(error.contains("documents[0]"), "{name}: {error}");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM documents")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a rejected request writes nothing");
}

#[sqlx::test(migrations = "./migrations")]
async fn only_decided_by_and_decided_at_are_required(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put_ok(decision(
        "d1",
        "Small",
        "We use tabs.",
        json!({ "decided_by": ["Ana"], "decided_at": "2026-08-03T10:00:00+02:00" }),
    ))
    .await;
    let id = env.id_of("d1").await;
    let (_, doc) = env
        .call("GET", &format!("/v1/documents/{id}"), Value::Null)
        .await;
    assert_eq!(
        doc["metadata"],
        json!({ "decided_by": ["Ana"], "decided_at": "2026-08-03T10:00:00+02:00" })
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn other_kinds_take_any_metadata_as_before(pool: PgPool) {
    let env = Env::new(pool).await;
    let mut doc = note("n1", "text");
    doc["metadata"] = json!({ "decided_by": 5, "where": "anywhere" });
    env.put_ok(doc).await;
    // A kind that is not note, doc or decision is still refused.
    let mut bad = note("n2", "text");
    bad["kind"] = json!("session");
    let (status, body) = env.put(bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("note, doc or decision"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------------------------
// the kind filter

#[sqlx::test(migrations = "./migrations")]
async fn search_filters_by_kind_before_it_cuts_to_k(pool: PgPool) {
    let env = Env::new(pool).await;
    // Twenty notes that match the query much better than the decision does.
    for i in 0..20 {
        env.put_ok(note(
            &format!("n{i}"),
            "postgres kafka event queue postgres kafka event queue postgres kafka event queue",
        ))
        .await;
    }
    env.put_ok(queue_decision()).await;

    let (status, body) = env
        .call(
            "GET",
            "/v1/search?q=postgres+kafka+event+queue&kind=decision&k=1",
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hits = body["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{body}");
    assert_eq!(hits[0]["kind"], "decision");
    assert_eq!(hits[0]["metadata"], full_metadata());

    let (_, body) = env
        .call(
            "GET",
            "/v1/search?q=postgres+kafka+event+queue&kind=note&k=50",
            Value::Null,
        )
        .await;
    let hits = body["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 20, "{body}");
    assert!(
        hits.iter()
            .all(|h| h["kind"] == "note" && h.get("metadata").is_none())
    );

    let (status, body) = env
        .call("GET", "/v1/search?q=postgres&kind=decisions", Value::Null)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("decision"),
        "{body}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn ask_filters_by_kind(pool: PgPool) {
    let env = Env::new(pool).await;
    for i in 0..15 {
        env.put_ok(note(
            &format!("n{i}"),
            "postgres kafka event queue postgres kafka event queue postgres kafka event queue",
        ))
        .await;
    }
    env.put_ok(queue_decision()).await;

    let body = env
        .ask(json!({ "question": QUEUE_QUESTION, "kind": ["decision"], "k": 1 }))
        .await;
    let citations = body["citations"].as_array().unwrap();
    assert_eq!(citations.len(), 1, "{body}");
    assert_eq!(citations[0]["kind"], "decision");
    // The prompt holds the decision and no note.
    let prompt = request_text(&env.probe.calls()[0]);
    assert!(
        prompt.contains("(decision, decided by Ashish Bagri"),
        "{prompt}"
    );
    assert!(!prompt.contains("(note)"), "{prompt}");

    let body = env
        .ask(json!({ "question": QUEUE_QUESTION, "kind": ["note"] }))
        .await;
    let citations = body["citations"].as_array().unwrap();
    assert!(!citations.is_empty());
    assert!(citations.iter().all(|c| c["kind"] == "note"), "{body}");

    let (status, body) = env
        .call(
            "POST",
            "/v1/ask",
            json!({ "question": QUEUE_QUESTION, "kind": ["bogus"] }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn deep_ask_refuses_an_unknown_kind_before_it_asks_the_model(pool: PgPool) {
    let env = Env::scripted(pool, Vec::new()).await;
    let (status, body) = env
        .call(
            "POST",
            "/v1/ask",
            json!({ "question": QUEUE_QUESTION, "mode": "deep", "kind": ["bogus"] }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("bogus"), "{body}");
}

// ---------------------------------------------------------------------------------------------
// GET /v1/documents

/// The ids of a page, in order.
fn ids(page: &Value) -> Vec<String> {
    page["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect()
}

#[sqlx::test(migrations = "./migrations")]
async fn listing_decisions_pages_in_a_stable_order_without_gaps_or_overlap(pool: PgPool) {
    let env = Env::new(pool).await;
    let dates = [
        ("d1", "2026-01-05"),
        ("d2", "2026-03-01"),
        ("d3", "2026-03-01"),
        ("d4", "2026-02-10"),
        ("d5", "2026-04-20T10:30:00Z"),
    ];
    for (external_id, date) in dates {
        env.put_ok(decision(
            external_id,
            &format!("Decision {external_id}"),
            "text",
            json!({ "decided_by": ["Ana"], "decided_at": date }),
        ))
        .await;
    }
    env.put_ok(note("n1", "a note")).await;
    env.put_ok(json!({ "external_id": "doc1", "title": "A doc", "text": "a doc", "kind": "doc" }))
        .await;

    // Newest decision first; the two of 1 March by id, descending.
    let mut tied = [env.id_of("d2").await, env.id_of("d3").await];
    tied.sort();
    tied.reverse();
    let expected = vec![
        env.id_of("d5").await,
        tied[0].clone(),
        tied[1].clone(),
        env.id_of("d4").await,
        env.id_of("d1").await,
    ];

    let (status, first) = env
        .call("GET", "/v1/documents?kind=decision&limit=2", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(ids(&first), expected[0..2]);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a next page")
        .to_string();

    let (status, second) = env
        .call(
            "GET",
            &format!("/v1/documents?kind=decision&limit=2&cursor={cursor}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(ids(&second), expected[2..4]);
    let cursor = second["next_cursor"]
        .as_str()
        .expect("a third page")
        .to_string();

    let (_, third) = env
        .call(
            "GET",
            &format!("/v1/documents?kind=decision&limit=2&cursor={cursor}"),
            Value::Null,
        )
        .await;
    assert_eq!(ids(&third), expected[4..5]);
    assert!(third["next_cursor"].is_null(), "{third}");

    // One page of everything is the same list; the note and the doc are not in it.
    let (_, all) = env
        .call("GET", "/v1/documents?kind=decision", Value::Null)
        .await;
    assert_eq!(ids(&all), expected);
    assert!(all["next_cursor"].is_null());

    // A listed document carries its metadata and no text.
    let first_doc = &all["documents"][0];
    assert_eq!(first_doc["kind"], "decision");
    assert_eq!(first_doc["source"], "decisions");
    assert_eq!(first_doc["external_id"], "d5");
    assert_eq!(first_doc["title"], "Decision d5");
    assert_eq!(first_doc["metadata"]["decided_at"], "2026-04-20T10:30:00Z");
    assert!(first_doc.get("text").is_none());

    // Without a kind every kind is listed.
    let (_, everything) = env.call("GET", "/v1/documents", Value::Null).await;
    assert_eq!(everything["documents"].as_array().unwrap().len(), 7);
}

#[sqlx::test(migrations = "./migrations")]
async fn listing_documents_checks_the_request_and_the_tenant(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put_ok(queue_decision()).await;

    for uri in [
        "/v1/documents?kind=bogus",
        "/v1/documents?cursor=nonsense",
        "/v1/documents?cursor=1.not-a-uuid",
        "/v1/documents?limit=many",
    ] {
        let (status, body) = env.call("GET", uri, Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
    // A limit out of range is clamped, not refused.
    let (status, body) = env.call("GET", "/v1/documents?limit=0", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["documents"].as_array().unwrap().len(), 1);

    // A key without the read scope is refused, and a request without a key too.
    let (_, ingest_only) = create_tenant_key(&env.router, "ingest-only", &["ingest"]).await;
    let (status, _) = send(
        &env.router,
        "GET",
        "/v1/documents",
        &ingest_only,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&env.router, "GET", "/v1/documents", "bad-key", Vec::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Another tenant sees none of it.
    let (_, other) = create_tenant_key(&env.router, "globex", &["ingest", "read"]).await;
    let (status, body) = send(
        &env.router,
        "GET",
        "/v1/documents?kind=decision",
        &other,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "documents": [], "next_cursor": null }));
}
