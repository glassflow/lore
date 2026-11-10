mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use common::{create_tenant_key, push_fixture, send, test_router, test_state};
use lore::llm::{AnthropicLlm, ModeModel, Models};
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const QUESTION: &str = "list every plan and its seat limit";

fn reply(content: Value, stop_reason: &str, usage: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": content,
        "stop_reason": stop_reason,
        "usage": usage
    }))
}

/// The sample repo indexed for `acme`, answered by the Anthropic client against `server`.
async fn indexed(pool: PgPool, data: &tempfile::TempDir, server: &MockServer) -> (Router, String) {
    let mut state = test_state(pool, data.path());
    state.llm = Arc::new(
        AnthropicLlm::new(&server.uri(), "test-key".into())
            .unwrap()
            .with_retry_base(Duration::from_millis(1)),
    );
    let mode = ModeModel {
        model: "claude-haiku-4-5".into(),
        effort: None,
    };
    state.models = Models {
        fast: mode.clone(),
        deep: mode,
    };
    let router = test_router(state);
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    push_fixture(&router, &key).await;
    (router, key)
}

async fn ask(router: &Router, key: &str, mode: &str) -> Value {
    let body = json!({ "question": QUESTION, "mode": mode });
    let (status, body) = send(
        router,
        "POST",
        "/v1/ask",
        key,
        body.to_string().into_bytes(),
    )
    .await;
    assert!(status.is_success(), "{status}: {body}");
    body
}

#[sqlx::test(migrations = "./migrations")]
async fn fast_mode_reports_the_cache_tokens(pool: PgPool) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(reply(
            json!([{"type": "text", "text": "Three plans [1]."}]),
            "end_turn",
            json!({
                "input_tokens": 4,
                "output_tokens": 381,
                "cache_read_input_tokens": 9000,
                "cache_creation_input_tokens": 1200
            }),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let data = tempfile::tempdir().unwrap();
    let (router, key) = indexed(pool, &data, &server).await;

    let body = ask(&router, &key, "fast").await;
    assert_eq!(
        body["usage"],
        json!({
            "input_tokens": 4,
            "output_tokens": 381,
            "cache_read_input_tokens": 9000,
            "cache_creation_input_tokens": 1200
        })
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn deep_mode_sums_the_cache_tokens_over_its_calls(pool: PgPool) {
    let server = MockServer::start().await;
    // Mounted first, so it answers the first call; the second falls through to the answer.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(reply(
            json!([{"type": "tool_use", "id": "toolu_1", "name": "search",
                    "input": {"query": "plan seat limit"}}]),
            "tool_use",
            json!({
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 3000
            }),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(reply(
            json!([{"type": "text", "text": "Three plans [src/billing/plans.rs:1-3]."}]),
            "end_turn",
            json!({
                "input_tokens": 5,
                "output_tokens": 40,
                "cache_read_input_tokens": 3000,
                "cache_creation_input_tokens": 200
            }),
        ))
        .mount(&server)
        .await;
    let data = tempfile::tempdir().unwrap();
    let (router, key) = indexed(pool, &data, &server).await;

    let body = ask(&router, &key, "deep").await;
    assert_eq!(body["tool_calls"], 1, "{body}");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(
        body["usage"],
        json!({
            "input_tokens": 15,
            "output_tokens": 60,
            "cache_read_input_tokens": 3000,
            "cache_creation_input_tokens": 3200
        })
    );
}
