mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, create_tenant_key, push_fixture, send, test_router, test_state_with};
use lore::answer::prompt::DEEP_SYSTEM;
use lore::answer::{AskRequest, Mode, ask};
use lore::error::AppError;
use lore::http::AppState;
use lore::llm::{
    Block, FakeLlm, Llm, LlmError, LlmRequest, LlmResponse, Message, Role, ToolChoice, Usage,
};
use lore::sessions::process_due_sessions;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const CLAUDE_CODE: &str = include_str!("fixtures/otlp/claude_code_session.json");
const SESSION_ID: &str = "sess-fixture-1";
const PLANS: &str = "src/billing/plans.rs";
const QUESTION: &str = "list every plan and its seat limit";
const TIME_IS_UP: &str = "Time is up. Answer now with what you have, with citations.";

/// One scripted model reply.
enum Step {
    Reply(LlmResponse),
    Refuse,
    Fail,
    /// Never returns: the call only ends when the caller's timeout drops it.
    Hang,
}

/// A model double. It records every request, plays its script first and then falls back to the
/// fake model. `tools` is what `supports_tools` says.
struct Probe {
    inner: FakeLlm,
    tools: bool,
    /// Waited out before the first call is answered.
    first_call_delay: Duration,
    script: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<LlmRequest>>,
}

impl Probe {
    fn with(inner: FakeLlm, tools: bool, script: Vec<Step>) -> Arc<Probe> {
        Arc::new(Probe {
            inner,
            tools,
            first_call_delay: Duration::ZERO,
            script: Mutex::new(script.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn fake() -> Arc<Probe> {
        Self::with(FakeLlm::new(), true, Vec::new())
    }

    fn scripted(script: Vec<Step>) -> Arc<Probe> {
        Self::with(FakeLlm::new(), true, script)
    }

    fn without_tools() -> Arc<Probe> {
        Self::with(FakeLlm::new(), false, Vec::new())
    }

    fn slow_first_call(inner: FakeLlm, delay: Duration) -> Arc<Probe> {
        Arc::new(Probe {
            inner,
            tools: true,
            first_call_delay: delay,
            script: Mutex::new(VecDeque::new()),
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
        self.tools
    }

    async fn complete(&self, req: LlmRequest) -> anyhow::Result<LlmResponse> {
        let first = {
            let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
            calls.push(req.clone());
            calls.len() == 1
        };
        if first && !self.first_call_delay.is_zero() {
            tokio::time::sleep(self.first_call_delay).await;
        }
        let step = self
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match step {
            Some(Step::Reply(res)) => Ok(res),
            Some(Step::Refuse) => Err(LlmError::Refused("no".to_string()).into()),
            Some(Step::Fail) => Err(anyhow::anyhow!("the model is down")),
            Some(Step::Hang) => std::future::pending().await,
            None => self.inner.complete(req).await,
        }
    }
}

fn reply(content: Vec<Block>, stop_reason: &str, input: u64, output: u64, model: &str) -> Step {
    Step::Reply(LlmResponse {
        content,
        stop_reason: stop_reason.to_string(),
        usage: Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        model: model.to_string(),
    })
}

fn answer_text(text: &str) -> Step {
    reply(
        vec![Block::Text { text: text.into() }],
        "end_turn",
        1,
        1,
        "scripted",
    )
}

fn tool_use(id: &str, name: &str, input: Value) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

/// A tenant `acme` with the sample repo pushed, and a way to ask as it.
struct Env {
    state: AppState,
    router: Router,
    tenant: Uuid,
    key: String,
    source_id: Uuid,
    _data: tempfile::TempDir,
}

impl Env {
    async fn new(
        pool: PgPool,
        llm: Arc<Probe>,
        change: impl FnOnce(&mut lore::config::Config),
    ) -> Env {
        let data = tempfile::tempdir().unwrap();
        let mut state = test_state_with(pool, data.path(), |cfg| {
            cfg.session_quiet_secs = 0;
            change(cfg);
        });
        state.llm = llm;
        let router = test_router(state.clone());
        let (tenant, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        let (source_id, _repo) = push_fixture(&router, &key).await;
        Env {
            state,
            router,
            tenant,
            key,
            source_id,
            _data: data,
        }
    }

    /// Ingests and processes the session fixture for `acme`.
    async fn add_session(&self) {
        let req = Request::post("/v1/traces")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.key))
            .body(Body::from(CLAUDE_CODE.to_string()))
            .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(process_due_sessions(&self.state, 20).await.unwrap(), 1);
    }

    async fn ask_deep(&self) -> (StatusCode, Value) {
        ask_deep_as(&self.router, &self.key).await
    }

    /// A second tenant with a read key and nothing indexed.
    async fn other_tenant(&self) -> (Uuid, String) {
        create_tenant_key(&self.router, "globex", &["ingest", "read"]).await
    }
}

async fn ask_deep_as(router: &Router, key: &str) -> (StatusCode, Value) {
    let body = json!({ "question": QUESTION, "mode": "deep" });
    send(
        router,
        "POST",
        "/v1/ask",
        key,
        body.to_string().into_bytes(),
    )
    .await
}

fn citations(body: &Value) -> &Vec<Value> {
    body["citations"].as_array().unwrap()
}

fn tool_results(m: &Message) -> Vec<(&str, &str, bool)> {
    m.content
        .iter()
        .map(|b| match b {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => (tool_use_id.as_str(), content.as_str(), *is_error),
            other => panic!("expected a tool result, got {other:?}"),
        })
        .collect()
}

#[sqlx::test(migrations = "./migrations")]
async fn deep_ask_uses_tools_and_cites_the_plans_file_without_the_source_prefix(pool: PgPool) {
    let probe = Probe::fake();
    let env = Env::new(pool, probe.clone(), |_| {}).await;

    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], "deep");
    assert_eq!(body["model"], "fake");
    assert!(body["tool_calls"].as_u64().unwrap() >= 1, "{body}");
    assert!(
        body["answer"]
            .as_str()
            .unwrap()
            .starts_with("Fake deep answer."),
        "{body}"
    );
    let plans = citations(&body)
        .iter()
        .find(|c| c["path"] == PLANS)
        .unwrap_or_else(|| panic!("no citation for {PLANS}: {body}"));
    assert_eq!(plans["kind"], "code");
    assert!(plans["document_id"].is_string(), "{plans}");
    assert!(
        plans["label"]
            .as_str()
            .unwrap()
            .starts_with("sample/src/billing/plans.rs:")
    );
    assert!(plans["start_line"].as_i64().unwrap() >= 1);
    assert!(plans["end_line"].as_i64().unwrap() >= plans["start_line"].as_i64().unwrap());

    // The loop sent the deep prompt and the question, and the fake answered from the tool result.
    let calls = probe.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].system, DEEP_SYSTEM);
    assert_eq!(calls[0].max_tokens, 16_000);
    assert_eq!(calls[0].model, "fake");
    assert_eq!(calls[0].tool_choice, ToolChoice::Auto);
    let names: Vec<&str> = calls[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["search", "grep", "read_file", "get_session"]);
    assert_eq!(
        calls[0].messages,
        vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: QUESTION.into()
            }],
        }]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_model_that_always_wants_tools_gets_a_final_call_without_them(pool: PgPool) {
    let probe = Probe::with(FakeLlm::always_tools(), true, Vec::new());
    let env = Env::new(pool, probe.clone(), |cfg| {
        cfg.deep_max_rounds = 3;
        cfg.deep_budget_secs = 30;
    })
    .await;

    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tool_calls"], 3);
    assert!(!body["answer"].as_str().unwrap().is_empty(), "{body}");

    let calls = probe.calls();
    assert_eq!(calls.len(), 4, "three tool rounds, then the final call");
    for call in &calls[..3] {
        assert_eq!(call.tool_choice, ToolChoice::Auto);
        assert!(!call.tools.is_empty());
    }
    let last = &calls[3];
    assert_eq!(last.tool_choice, ToolChoice::None);
    assert_eq!(last.tools, calls[0].tools, "the same tools stay declared");
    let closing = last.messages.last().unwrap();
    assert_eq!(closing.role, Role::User);
    assert_eq!(
        closing.content,
        vec![Block::Text {
            text: TIME_IS_UP.into()
        }]
    );
    // Question, then three (assistant, tool results) pairs, then the closing message.
    assert_eq!(last.messages.len(), 1 + 3 * 2 + 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn past_80_percent_of_the_budget_the_next_call_is_the_final_one(pool: PgPool) {
    // A 1 second budget and a first call that takes 0.9 seconds.
    let probe = Probe::slow_first_call(FakeLlm::always_tools(), Duration::from_millis(900));
    let env = Env::new(pool, probe.clone(), |cfg| cfg.deep_budget_secs = 1).await;

    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tool_calls"], 1);
    let calls = probe.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].tool_choice, ToolChoice::Auto);
    assert_eq!(calls[1].tool_choice, ToolChoice::None);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_second_deep_ask_over_the_daily_limit_is_a_429(pool: PgPool) {
    let env = Env::new(pool, Probe::fake(), |cfg| cfg.deep_daily_limit = 1).await;

    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"], "daily deep-mode limit reached");

    // The limit is per tenant, and fast mode is not counted.
    let (_, other_key) = env.other_tenant().await;
    let (status, body) = ask_deep_as(&env.router, &other_key).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let fast = json!({ "question": QUESTION }).to_string().into_bytes();
    let (status, _) = send(&env.router, "POST", "/v1/ask", &env.key, fast).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_provider_without_tools_gets_a_400_and_uses_no_allowance(pool: PgPool) {
    let env = Env::new(pool, Probe::without_tools(), |cfg| cfg.deep_daily_limit = 1).await;

    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"],
        "deep mode needs a tool-capable model provider (LORE_LLM=anthropic)"
    );
    // The same tenant, now on a provider with tools: the day's one question is still there.
    let mut state = env.state.clone();
    state.llm = Probe::fake();
    let router = test_router(state);
    let (status, body) = ask_deep_as(&router, &env.key).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn info(router: &Router) -> Value {
    let req = Request::get("/v1/info").body(Body::empty()).unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    body_json(res).await
}

#[sqlx::test(migrations = "./migrations")]
async fn info_lists_deep_only_when_the_provider_has_tools(pool: PgPool) {
    let env = Env::new(pool, Probe::fake(), |_| {}).await;
    assert_eq!(
        info(&env.router).await["modes"],
        json!(["fast", "sources", "deep"])
    );

    let mut state = env.state.clone();
    state.llm = Probe::without_tools();
    assert_eq!(
        info(&test_router(state)).await["modes"],
        json!(["fast", "sources"])
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_model_turn_is_echoed_unchanged_and_all_results_come_back_in_one_message(pool: PgPool) {
    let thinking = json!({"type": "thinking", "thinking": "hm", "signature": "sig"});
    let turn = vec![
        Block::Raw(thinking),
        Block::Text {
            text: "Looking.".into(),
        },
        tool_use("toolu_a", "search", json!({ "query": "plans" })),
        tool_use("toolu_b", "nope", json!({})),
        tool_use("toolu_c", "get_session", json!({ "session_id": "missing" })),
    ];
    let probe = Probe::scripted(vec![
        reply(turn.clone(), "tool_use", 10, 5, "m1"),
        reply(
            vec![Block::Text {
                text: "Done [src/billing/plans.rs:1-3].".into(),
            }],
            "end_turn",
            20,
            7,
            "m2",
        ),
    ]);
    let env = Env::new(pool, probe.clone(), |_| {}).await;

    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tool_calls"], 3);
    // Usage is summed over both calls; the model is the last response's.
    assert_eq!(
        body["usage"],
        json!({
            "input_tokens": 30,
            "output_tokens": 12,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0
        })
    );
    assert_eq!(body["model"], "m2");

    let calls = probe.calls();
    assert_eq!(calls.len(), 2);
    let sent = &calls[1].messages;
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1].role, Role::Assistant);
    assert_eq!(sent[1].content, turn, "the turn is echoed block for block");
    assert_eq!(sent[2].role, Role::User);
    let results = tool_results(&sent[2]);
    let ids: Vec<&str> = results.iter().map(|r| r.0).collect();
    assert_eq!(ids, ["toolu_a", "toolu_b", "toolu_c"]);
    assert!(!results[0].2, "search worked: {}", results[0].1);
    assert!(results[0].1.contains(PLANS), "{}", results[0].1);
    assert!(results[1].2, "an unknown tool is an error result");
    assert!(results[1].1.contains("unknown tool"), "{}", results[1].1);
    assert!(results[2].2, "an unknown session is an error result");
}

#[sqlx::test(migrations = "./migrations")]
async fn citations_keep_unmatched_paths_and_resolve_with_or_without_the_source_name(pool: PgPool) {
    let answer = "A [sample/src/billing/plans.rs:1-5] B [nowhere/x.rs:7-9] \
                  C [src/billing/plans.rs:1-5] D [sample/src/billing/plans.rs:1-5] E [README.md:4]";
    let probe = Probe::scripted(vec![answer_text(answer)]);
    let env = Env::new(pool, probe, |_| {}).await;
    // Give the source a link template, to check the url is rendered like a search hit's.
    sqlx::query("UPDATE sources SET web_url_template = $1 WHERE id = $2")
        .bind("https://git.example/blob/{commit}/{path}#L{start}-L{end}")
        .bind(env.source_id)
        .execute(&env.state.pool)
        .await
        .unwrap();

    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tool_calls"], 0);
    let cs = citations(&body);
    let labels: Vec<&str> = cs.iter().map(|c| c["label"].as_str().unwrap()).collect();
    assert_eq!(
        labels,
        [
            "sample/src/billing/plans.rs:1-5",
            "nowhere/x.rs:7-9",
            "src/billing/plans.rs:1-5",
            "README.md:4"
        ]
    );
    let doc = cs[0]["document_id"].as_str().expect("matched");
    assert_eq!(cs[0]["path"], PLANS);
    assert_eq!(cs[0]["kind"], "code");
    assert_eq!(cs[0]["start_line"], 1);
    assert_eq!(cs[0]["end_line"], 5);
    let url = cs[0]["url"].as_str().unwrap();
    assert!(url.starts_with("https://git.example/blob/"), "{url}");
    assert!(url.ends_with("/src/billing/plans.rs#L1-L5"), "{url}");

    assert_eq!(cs[1]["document_id"], Value::Null);
    assert_eq!(cs[1]["path"], "nowhere/x.rs");
    assert_eq!(cs[1]["start_line"], 7);
    assert_eq!(cs[1]["end_line"], 9);
    assert_eq!(cs[1]["url"], Value::Null);

    // Without the source name the path still matches, and it is the same document.
    assert_eq!(cs[2]["document_id"].as_str(), Some(doc));
    assert_eq!(cs[2]["path"], PLANS);
    // A doc file cited with one line.
    assert!(cs[3]["document_id"].is_string(), "{}", cs[3]);
    assert_eq!(cs[3]["start_line"], 4);
    assert_eq!(cs[3]["end_line"], 4);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_session_citation_resolves_by_external_id(pool: PgPool) {
    let probe = Probe::scripted(vec![answer_text(&format!(
        "See [session:{SESSION_ID}] and [session:no-such-session]."
    ))]);
    let env = Env::new(pool, probe, |_| {}).await;
    env.add_session().await;

    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cs = citations(&body);
    assert_eq!(cs.len(), 2, "{body}");
    assert_eq!(cs[0]["label"], format!("session:{SESSION_ID}"));
    assert!(cs[0]["document_id"].is_string(), "{}", cs[0]);
    assert_eq!(cs[0]["kind"], "session");
    assert!(cs[0]["title"].is_string());
    assert_eq!(cs[0]["path"], Value::Null);
    assert_eq!(cs[1]["label"], "session:no-such-session");
    assert_eq!(cs[1]["document_id"], Value::Null);
}

#[sqlx::test(migrations = "./migrations")]
async fn citations_only_resolve_inside_the_asking_tenant(pool: PgPool) {
    // The path with and without the source name: the second is looked up in every source.
    let text = format!(
        "[sample/src/billing/plans.rs:1-5] [src/billing/plans.rs:1-5] [session:{SESSION_ID}]"
    );
    let probe = Probe::scripted(vec![answer_text(&text), answer_text(&text)]);
    let env = Env::new(pool, probe, |_| {}).await;
    env.add_session().await;

    // Another tenant has the same source name and path, but its own documents.
    let (_, other_key) = env.other_tenant().await;
    let (status, body) = ask_deep_as(&env.router, &other_key).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cs = citations(&body);
    assert_eq!(cs.len(), 3);
    for c in cs {
        assert_eq!(c["document_id"], Value::Null, "{c}");
        assert_eq!(c["kind"], Value::Null);
        assert_eq!(c["title"], Value::Null);
        assert_eq!(c["url"], Value::Null);
    }

    // The owner of the documents does get them.
    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        citations(&body)
            .iter()
            .all(|c| c["document_id"].is_string())
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_tenants_own_documents_win_when_two_tenants_share_a_path(pool: PgPool) {
    let text = "[sample/src/billing/plans.rs:1-5]";
    let probe = Probe::scripted(vec![answer_text(text), answer_text(text)]);
    let env = Env::new(pool, probe, |_| {}).await;
    let (_, other_key) = env.other_tenant().await;
    push_fixture(&env.router, &other_key).await;

    let (_, mine) = env.ask_deep().await;
    let (_, theirs) = ask_deep_as(&env.router, &other_key).await;
    let mine_id = citations(&mine)[0]["document_id"].as_str().unwrap();
    let their_id = citations(&theirs)[0]["document_id"].as_str().unwrap();
    assert_ne!(mine_id, their_id);
    let owner: Uuid = sqlx::query_scalar("SELECT tenant_id FROM documents WHERE id = $1")
        .bind(mine_id.parse::<Uuid>().unwrap())
        .fetch_one(&env.state.pool)
        .await
        .unwrap();
    assert_eq!(owner, env.tenant);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_slot_is_freed_on_every_exit_path(pool: PgPool) {
    // One slot, and a day allowance of exactly the number of questions asked below.
    let probe = Probe::scripted(vec![Step::Fail, Step::Refuse, answer_text("Fine.")]);
    let env = Env::new(pool, probe, |cfg| {
        cfg.deep_concurrency = 1;
        cfg.deep_daily_limit = 3;
    })
    .await;

    let (status, _) = env.ask_deep().await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "the model declined to answer");
    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // All three counted against the day, whether they failed or not.
    let (status, body) = env.ask_deep().await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

fn deep_request() -> AskRequest {
    AskRequest {
        question: QUESTION.to_string(),
        mode: Mode::Deep,
        k: None,
        kind: None,
        source: None,
    }
}

// These two pause the clock, so the 20 second grace after the budget passes at once. They call
// `ask` directly and touch no database after the pause: tokio's auto-advance would otherwise
// run out sqlx's own timeouts while a query is in flight.

#[sqlx::test(migrations = "./migrations")]
async fn a_model_call_that_never_returns_is_a_timeout_error(pool: PgPool) {
    let probe = Probe::scripted(vec![Step::Hang]);
    let env = Env::new(pool, probe, |cfg| cfg.deep_budget_secs = 5).await;

    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let err = ask(&env.state, env.tenant, deep_request())
        .await
        .unwrap_err();
    match err {
        AppError::Internal(e) => assert_eq!(e.to_string(), "deep mode timed out"),
        other => panic!("{other:?}"),
    }
    // The budget plus the 20 second grace.
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(25) && waited < Duration::from_secs(26),
        "{waited:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_timeout_after_some_text_returns_that_text(pool: PgPool) {
    // A turn with text and a tool call that fails at once (an unknown tool), then a call that hangs.
    let probe = Probe::scripted(vec![
        reply(
            vec![
                Block::Text {
                    text: "Partial answer.".into(),
                },
                tool_use("toolu_x", "nope", json!({})),
            ],
            "tool_use",
            3,
            4,
            "m1",
        ),
        Step::Hang,
    ]);
    let env = Env::new(pool, probe, |cfg| cfg.deep_budget_secs = 5).await;

    tokio::time::pause();
    let res = ask(&env.state, env.tenant, deep_request()).await.unwrap();
    assert_eq!(res.answer, "Partial answer.");
    assert_eq!(res.tool_calls, 1);
    assert_eq!(res.usage.input_tokens, 3);
    assert_eq!(res.mode, Mode::Deep);
}

// Question length. Both modes refuse a question over 2000 characters (not bytes) with a 400 that
// names `question`, before deep mode takes a slot of the day.

const TOO_LONG: &str = "question must be at most 2000 characters";

async fn ask_with(env: &Env, question: &str, mode: &str) -> (StatusCode, Value) {
    let body = json!({ "question": question, "mode": mode });
    send(
        &env.router,
        "POST",
        "/v1/ask",
        &env.key,
        body.to_string().into_bytes(),
    )
    .await
}

#[sqlx::test(migrations = "./migrations")]
async fn a_fast_question_over_2000_characters_is_a_400(pool: PgPool) {
    let env = Env::new(pool, Probe::fake(), |_| {}).await;

    let (status, body) = ask_with(&env, &"a".repeat(2001), "fast").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], TOO_LONG);
    let (status, body) = ask_with(&env, &"\u{e9}".repeat(2001), "fast").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], TOO_LONG);

    // 2000 characters are fine, ASCII or multi-byte (4000 bytes), and so is padding around them.
    for ok in [
        "a".repeat(2000),
        "\u{e9}".repeat(2000),
        format!("  {}  ", "a".repeat(2000)),
    ] {
        let (status, body) = ask_with(&env, &ok, "fast").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_deep_question_over_2000_characters_is_a_400_and_uses_no_allowance(pool: PgPool) {
    let probe = Probe::fake();
    let env = Env::new(pool, probe.clone(), |cfg| cfg.deep_daily_limit = 1).await;

    for long in ["a".repeat(2001), "\u{e9}".repeat(2001)] {
        let (status, body) = ask_with(&env, &long, "deep").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], TOO_LONG);
    }
    assert!(probe.calls().is_empty(), "the model was called");

    // The day's one question is still there, and exactly 2000 characters (4000 bytes) pass.
    let (status, body) = ask_with(&env, &"\u{e9}".repeat(2000), "deep").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], "deep");
    // That one used it up.
    let (status, body) = ask_with(&env, QUESTION, "deep").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

// The slot. The first question holds its model call, so it holds its permit; the second one
// waits the 5 second slot wait on a paused clock and is refused; once the first has ended, a
// third one gets the slot. Nothing here touches the database after the pause (no citations).

#[sqlx::test(migrations = "./migrations")]
async fn a_running_question_holds_its_slot_and_the_next_one_waits_for_it(pool: PgPool) {
    let probe = Probe::scripted(vec![Step::Hang, answer_text("Second.")]);
    let env = Env::new(pool, probe.clone(), |cfg| {
        cfg.deep_concurrency = 1;
        cfg.deep_budget_secs = 5;
    })
    .await;

    tokio::time::pause();
    let first = {
        let state = env.state.clone();
        let tenant = env.tenant;
        tokio::spawn(async move { ask(&state, tenant, deep_request()).await })
    };
    while probe.calls().is_empty() {
        tokio::task::yield_now().await;
    }

    // The first question's call times out after the budget and the grace (25 s), which frees
    // the slot well within the 60 s a question waits for one.
    let started = tokio::time::Instant::now();
    let res = ask(&env.state, env.tenant, deep_request()).await.unwrap();
    assert_eq!(res.answer, "Second.");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(25) && waited < Duration::from_secs(26),
        "{waited:?}"
    );
    let err = first.await.unwrap().unwrap_err();
    assert!(matches!(err, AppError::Internal(_)), "{err:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_question_that_finds_no_slot_within_60_seconds_is_refused(pool: PgPool) {
    let probe = Probe::scripted(vec![Step::Hang]);
    let env = Env::new(pool, probe.clone(), |cfg| {
        cfg.deep_concurrency = 1;
        cfg.deep_budget_secs = 120;
    })
    .await;

    tokio::time::pause();
    let first = {
        let state = env.state.clone();
        let tenant = env.tenant;
        tokio::spawn(async move { ask(&state, tenant, deep_request()).await })
    };
    while probe.calls().is_empty() {
        tokio::task::yield_now().await;
    }

    let started = tokio::time::Instant::now();
    let err = ask(&env.state, env.tenant, deep_request())
        .await
        .unwrap_err();
    match err {
        AppError::TooManyRequests(m) => assert!(m.contains("too many deep-mode"), "{m}"),
        other => panic!("{other:?}"),
    }
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(60) && waited < Duration::from_secs(61),
        "{waited:?}"
    );
    assert_eq!(
        probe.calls().len(),
        1,
        "the refused question reached the model"
    );
    first.abort();
}

// Citation lookups. Documents are inserted directly: a lookup reads only `documents` and
// `sources`, and the tests need exact paths, ids and `updated_at`.

impl Env {
    async fn insert_source(&self, kind: &str, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO sources (id, tenant_id, kind, name) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(self.tenant)
            .bind(kind)
            .bind(name)
            .execute(&self.state.pool)
            .await
            .unwrap();
        id
    }

    /// A document of `kind` in `source`. `path` goes in `metadata.path`; `external_id` is the
    /// path, or the given session id. `age_mins` is how long ago it was last updated.
    async fn insert_doc(
        &self,
        source: Uuid,
        kind: &str,
        external_id: &str,
        path: Option<&str>,
        age_mins: i32,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let metadata = path.map_or_else(|| json!({}), |p| json!({ "path": p }));
        sqlx::query(
            "INSERT INTO documents (id, tenant_id, source_id, external_id, kind, title, metadata,
                                    content_hash, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'h', now() - make_interval(mins => $8))",
        )
        .bind(id)
        .bind(self.tenant)
        .bind(source)
        .bind(external_id)
        .bind(kind)
        .bind(external_id)
        .bind(metadata)
        .bind(age_mins)
        .execute(&self.state.pool)
        .await
        .unwrap();
        id
    }

    /// Asks (deep) with a model that answers `text`, and returns the `document_id` of each citation.
    async fn cited_ids(&self, text: &str) -> Vec<Option<Uuid>> {
        let probe = Probe::scripted(vec![answer_text(text)]);
        let mut state = self.state.clone();
        state.llm = probe;
        let router = test_router(state);
        let (status, body) = ask_deep_as(&router, &self.key).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        citations(&body)
            .iter()
            .map(|c| c["document_id"].as_str().map(|s| s.parse().unwrap()))
            .collect()
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_note_with_the_same_path_is_not_a_citation_target(pool: PgPool) {
    let env = Env::new(pool, Probe::fake(), |_| {}).await;
    let docs = env.insert_source("documents", "wiki").await;
    env.insert_doc(docs, "note", "n1", Some("notes/plan.md"), 0)
        .await;

    // With the source name (looked up in that source) and without (looked up everywhere).
    let ids = env
        .cited_ids("[wiki/notes/plan.md:1-2] [notes/plan.md:1-2]")
        .await;
    assert_eq!(ids, [None, None]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_cited_source_name_picks_that_sources_document_and_newest_wins_without_one(pool: PgPool) {
    let env = Env::new(pool, Probe::fake(), |_| {}).await;
    let alpha = env.insert_source("repo", "alpha").await;
    let beta = env.insert_source("repo", "beta").await;
    let in_alpha = env
        .insert_doc(alpha, "code", "lib/shared.rs", Some("lib/shared.rs"), 60)
        .await;
    // The other source's copy is newer.
    let in_beta = env
        .insert_doc(beta, "code", "lib/shared.rs", Some("lib/shared.rs"), 5)
        .await;

    let ids = env
        .cited_ids("[alpha/lib/shared.rs:1-2] [beta/lib/shared.rs:1-2] [lib/shared.rs:1-2]")
        .await;
    assert_eq!(ids, [Some(in_alpha), Some(in_beta), Some(in_beta)]);

    // Make alpha's copy the newer one: the unprefixed path follows, the prefixed ones do not.
    sqlx::query("UPDATE documents SET updated_at = now() WHERE id = $1")
        .bind(in_alpha)
        .execute(&env.state.pool)
        .await
        .unwrap();
    let ids = env
        .cited_ids("[beta/lib/shared.rs:1-2] [lib/shared.rs:1-2]")
        .await;
    assert_eq!(ids, [Some(in_beta), Some(in_alpha)]);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_longest_matching_source_name_wins(pool: PgPool) {
    let env = Env::new(pool, Probe::fake(), |_| {}).await;
    let a = env.insert_source("repo", "a").await;
    let ab = env.insert_source("repo", "a/b").await;
    // `a` has a file at `b/x.rs`, so reading `a/b/x.rs` as source `a` finds a different document.
    env.insert_doc(a, "code", "b/x.rs", Some("b/x.rs"), 0).await;
    let in_ab = env.insert_doc(ab, "code", "x.rs", Some("x.rs"), 0).await;

    let ids = env.cited_ids("[a/b/x.rs:1-3]").await;
    assert_eq!(ids, [Some(in_ab)]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_session_citation_takes_only_sessions_and_the_newest(pool: PgPool) {
    let env = Env::new(pool, Probe::fake(), |_| {}).await;
    let s1 = env.insert_source("traces", "sessions-1").await;
    let s2 = env.insert_source("traces", "sessions-2").await;
    let notes = env.insert_source("documents", "wiki").await;
    let _old = env.insert_doc(s1, "session", "dup-sess", None, 60).await;
    let newer = env.insert_doc(s2, "session", "dup-sess", None, 30).await;
    // The newest document of all has the same external id but is a note.
    env.insert_doc(notes, "note", "dup-sess", None, 1).await;
    // A note alone does not resolve.
    env.insert_doc(notes, "note", "only-a-note", None, 1).await;

    let ids = env
        .cited_ids("[session:dup-sess] [session:only-a-note]")
        .await;
    assert_eq!(ids, [Some(newer), None]);
}

#[sqlx::test(migrations = "./migrations")]
async fn at_most_50_citations_are_resolved_in_order(pool: PgPool) {
    let env = Env::new(pool, Probe::fake(), |_| {}).await;
    let text: String = (1..=51).map(|n| format!("[f{n}.rs:1] ")).collect();
    let probe = Probe::scripted(vec![answer_text(&text)]);
    let mut state = env.state.clone();
    state.llm = probe;
    let router = test_router(state);

    let (status, body) = ask_deep_as(&router, &env.key).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let labels: Vec<&str> = citations(&body)
        .iter()
        .map(|c| c["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels.len(), 50, "{labels:?}");
    assert_eq!(labels[0], "f1.rs:1");
    assert_eq!(labels[49], "f50.rs:1");
}
