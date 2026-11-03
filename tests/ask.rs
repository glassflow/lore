mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{create_tenant_key, push_fixture, send, test_router, test_state};
use lore::llm::{Llm, LlmError, LlmRequest, LlmResponse};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const PLANS: &str = "src/billing/plans.rs";
const QUESTION: &str = "how many plans are there and what are their limits?";

/// A model that always declines.
struct RefusingLlm;

#[async_trait::async_trait]
impl Llm for RefusingLlm {
    fn name(&self) -> &str {
        "refusing"
    }

    fn supports_tools(&self) -> bool {
        false
    }

    async fn complete(&self, _req: LlmRequest) -> anyhow::Result<LlmResponse> {
        Err(LlmError::Refused("no".to_string()).into())
    }
}

/// `POST /v1/ask` with a JSON body.
async fn ask(router: &Router, key: &str, body: Value) -> (StatusCode, Value) {
    send(
        router,
        "POST",
        "/v1/ask",
        key,
        body.to_string().into_bytes(),
    )
    .await
}

/// A router with the fixture repo indexed, and a key with the given scopes for its tenant.
async fn indexed(pool: PgPool, data: &tempfile::TempDir) -> (Router, String) {
    let router = test_router(test_state(pool, data.path()));
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    push_fixture(&router, &key).await;
    (router, key)
}

#[sqlx::test(migrations = "./migrations")]
async fn fast_answer_cites_the_plans_file(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let (router, key) = indexed(pool, &data).await;

    let (status, body) = ask(&router, &key, json!({ "question": QUESTION })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], "fast");
    assert_eq!(body["model"], "fake");
    assert!(
        body["answer"]
            .as_str()
            .unwrap()
            .starts_with("Fake answer from"),
        "{body}"
    );
    let citations = body["citations"].as_array().unwrap();
    let plans = citations
        .iter()
        .find(|c| c["path"] == PLANS)
        .unwrap_or_else(|| panic!("no citation for {PLANS}: {body}"));
    assert_eq!(plans["kind"], "code");
    assert!(plans["document_id"].is_string());
    assert!(plans["start_line"].as_i64().unwrap() >= 1);
    // The labels are the source numbers the answer cites, in order, each once.
    let labels: Vec<&str> = citations
        .iter()
        .map(|c| c["label"].as_str().unwrap())
        .collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(labels.len(), sorted.len(), "{labels:?}");
    assert_eq!(labels[0], "1");
    let considered = body["sources_considered"].as_u64().unwrap() as usize;
    assert!((citations.len()..=12).contains(&considered), "{body}");
    assert_eq!(body["tool_calls"], 0);
    assert!(body["usage"]["input_tokens"].as_u64().unwrap() > 0);
    assert!(body["usage"]["output_tokens"].as_u64().unwrap() > 0);
    assert!(body["latency_ms"].is_u64());
}

#[sqlx::test(migrations = "./migrations")]
async fn k_limits_the_sources(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let (router, key) = indexed(pool, &data).await;

    let (status, body) = ask(&router, &key, json!({ "question": QUESTION, "k": 2 })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sources_considered"], 2);
    assert!(
        body["answer"]
            .as_str()
            .unwrap()
            .starts_with("Fake answer from 2 sources.")
    );
    assert!(body["citations"].as_array().unwrap().len() <= 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn kind_and_source_filters_apply(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let (router, key) = indexed(pool, &data).await;

    let body = json!({ "question": QUESTION, "kind": ["doc"] });
    let (status, body) = ask(&router, &key, body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let citations = body["citations"].as_array().unwrap();
    assert!(!citations.is_empty(), "{body}");
    assert!(citations.iter().all(|c| c["kind"] == "doc"), "{body}");

    let body = json!({ "question": QUESTION, "source": "sample" });
    let (status, body) = ask(&router, &key, body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["model"], "fake");

    let body = json!({ "question": QUESTION, "source": "no-such-source" });
    let (status, body) = ask(&router, &key, body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["model"], "none");
}

#[sqlx::test(migrations = "./migrations")]
async fn nothing_indexed_skips_the_model(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let router = test_router(test_state(pool, data.path()));
    let (_, key) = create_tenant_key(&router, "empty", &["read"]).await;

    let (status, body) = ask(&router, &key, json!({ "question": QUESTION })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["model"], "none");
    assert_eq!(
        body["answer"],
        "I found nothing indexed that matches this question."
    );
    assert_eq!(body["mode"], "fast");
    assert_eq!(body["citations"], json!([]));
    assert_eq!(body["sources_considered"], 0);
    assert_eq!(body["tool_calls"], 0);
    assert_eq!(
        body["usage"],
        json!({ "input_tokens": 0, "output_tokens": 0 })
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn another_tenants_documents_are_not_used(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let (router, _) = indexed(pool, &data).await;
    let (_, other) = create_tenant_key(&router, "other", &["read"]).await;

    let (status, body) = ask(&router, &other, json!({ "question": QUESTION })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["model"], "none");
}

#[sqlx::test(migrations = "./migrations")]
async fn bad_requests_are_400(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let router = test_router(test_state(pool, data.path()));
    let (_, key) = create_tenant_key(&router, "acme", &["read"]).await;

    for body in [
        json!({ "question": "" }),
        json!({ "question": "  \n " }),
        json!({}),
        json!({ "question": "q", "mode": "slow" }),
        json!({ "question": "q", "kind": ["nope"] }),
    ] {
        let (status, res) = ask(&router, &key, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {res}");
        assert!(res["error"].is_string(), "{body}: {res}");
    }

    // A blank question is refused by ask itself, before any search.
    for question in ["", "  \n "] {
        let (_, res) = ask(&router, &key, json!({ "question": question })).await;
        assert_eq!(res["error"], "question is required");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_key_is_required_with_the_read_scope(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let router = test_router(test_state(pool, data.path()));
    let (_, ingest_only) = create_tenant_key(&router, "acme", &["ingest"]).await;

    let req = Request::post("/v1/ask")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "question": QUESTION }).to_string()))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let (status, _) = ask(&router, "lk_wrong", json!({ "question": QUESTION })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = ask(&router, &ingest_only, json!({ "question": QUESTION })).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_refusal_is_a_422(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let mut state = test_state(pool, data.path());
    state.llm = Arc::new(RefusingLlm);
    let router = test_router(state);
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    push_fixture(&router, &key).await;

    let (status, body) = ask(&router, &key, json!({ "question": QUESTION })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body, json!({ "error": "the model declined to answer" }));
}

/// Sources mode lists the hits as bullets and never calls the model: here the model declines
/// everything, and the answer still comes back.
#[sqlx::test(migrations = "./migrations")]
async fn sources_mode_lists_the_hits_without_the_model(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let mut state = test_state(pool, data.path());
    state.llm = Arc::new(RefusingLlm);
    let router = test_router(state);
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    push_fixture(&router, &key).await;

    let (status, body) = ask(
        &router,
        &key,
        json!({ "question": QUESTION, "mode": "sources", "k": 4 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], "sources");
    assert_eq!(body["model"], "none");
    assert_eq!(
        body["usage"],
        json!({ "input_tokens": 0, "output_tokens": 0 })
    );
    let citations = body["citations"].as_array().unwrap();
    assert_eq!(citations.len(), 4);
    assert_eq!(body["sources_considered"], 4);
    let answer = body["answer"].as_str().unwrap();
    let bullets: Vec<&str> = answer.lines().collect();
    assert_eq!(bullets.len(), 4, "{answer}");
    for (i, (line, c)) in bullets.iter().zip(citations).enumerate() {
        let label = (i + 1).to_string();
        assert_eq!(c["label"], label.as_str());
        assert!(line.starts_with(&format!("- [{label}] `")), "{line}");
        assert!(line.contains("(sample)"), "{line}");
    }
    assert!(
        citations.iter().any(|c| c["path"] == PLANS),
        "no citation for {PLANS}: {body}"
    );
    let plans = bullets
        .iter()
        .find(|b| b.contains(PLANS))
        .expect("a bullet for the plans file");
    assert!(plans.contains("Plan"), "{plans}");
}

#[sqlx::test(migrations = "./migrations")]
async fn info_needs_no_key(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let router = test_router(test_state(pool, data.path()));

    let res = router
        .clone()
        .oneshot(Request::get("/v1/info").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        format!(
            r#"{{"version":"{}","llm":"fake","embedder":"hash","modes":["fast","sources","deep"]}}"#,
            env!("CARGO_PKG_VERSION")
        )
    );
}
