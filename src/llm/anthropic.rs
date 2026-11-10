//! Anthropic Messages API over plain HTTP (there is no official Rust SDK).

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Block, Llm, LlmError, LlmRequest, LlmResponse, Message, ToolChoice, ToolDef, Usage};

const API_VERSION: &str = "2023-06-01";
const FALLBACKS_BETA: &str = "server-side-fallback-2026-07-01";
/// Models with these id prefixes take `fallbacks` and the beta header. Others (Haiku 4.5
/// among them) reject them.
const FALLBACK_MODEL_PREFIXES: [&str; 3] = ["claude-opus-5", "claude-sonnet-5-5", "claude-fable-5"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Retries after the first attempt: three calls at most.
const MAX_RETRIES: u32 = 2;
/// The wait before the first retry. It doubles for the second: 1s, then 2s.
const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(1);
/// A longer `retry-after` than this is capped, so a request never hangs for minutes.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

pub struct AnthropicLlm {
    http: reqwest::Client,
    base: String,
    key: HeaderValue,
    retry_base: Duration,
}

impl AnthropicLlm {
    pub fn new(base: &str, key: String) -> Result<Self> {
        let mut key = HeaderValue::from_str(&key).map_err(|_| {
            anyhow!("ANTHROPIC_API_KEY contains characters not allowed in a header")
        })?;
        key.set_sensitive(true);
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            key,
            retry_base: DEFAULT_RETRY_BASE,
        })
    }

    /// Replaces the 1s base of the retry backoff. Tests use a few milliseconds.
    pub fn with_retry_base(mut self, retry_base: Duration) -> Self {
        self.retry_base = retry_base;
        self
    }
}

/// Whether the model takes `fallbacks` and the `anthropic-beta` header.
pub fn supports_fallbacks(model: &str) -> bool {
    FALLBACK_MODEL_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529)
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "is_blank")]
    system: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<WireToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<WireOutputConfig<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallbacks: Option<&'static str>,
    cache_control: WireCacheControl,
}

fn is_blank(s: &&str) -> bool {
    s.is_empty()
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    content: Vec<WireBlock<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum WireBlock<'a> {
    Typed(TypedBlock<'a>),
    /// Sent unchanged.
    Raw(&'a Value),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TypedBlock<'a> {
    Text {
        text: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

#[derive(Serialize)]
struct WireTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

#[derive(Serialize)]
struct WireToolChoice {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct WireOutputConfig<'a> {
    effort: &'a str,
}

#[derive(Serialize)]
struct WireCacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

fn wire_block(b: &Block) -> WireBlock<'_> {
    match b {
        Block::Text { text } => WireBlock::Typed(TypedBlock::Text { text }),
        Block::ToolUse { id, name, input } => {
            WireBlock::Typed(TypedBlock::ToolUse { id, name, input })
        }
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => WireBlock::Typed(TypedBlock::ToolResult {
            tool_use_id,
            content,
            is_error: *is_error,
        }),
        Block::Raw(v) => WireBlock::Raw(v),
    }
}

fn wire_message(m: &Message) -> WireMessage<'_> {
    WireMessage {
        role: m.role.as_str(),
        content: m.content.iter().map(wire_block).collect(),
    }
}

fn wire_tool(t: &ToolDef) -> WireTool<'_> {
    WireTool {
        name: &t.name,
        description: &t.description,
        input_schema: &t.input_schema,
    }
}

fn wire_request(req: &LlmRequest) -> WireRequest<'_> {
    WireRequest {
        model: &req.model,
        max_tokens: req.max_tokens,
        system: &req.system,
        messages: req.messages.iter().map(wire_message).collect(),
        tools: req.tools.iter().map(wire_tool).collect(),
        // The tools stay in the request even when tool use is off: the API rejects a history
        // with tool_use blocks and no tools. Auto is the default and is not sent.
        tool_choice: (req.tool_choice == ToolChoice::None && !req.tools.is_empty())
            .then_some(WireToolChoice { kind: "none" }),
        output_config: req
            .effort
            .as_deref()
            .map(|effort| WireOutputConfig { effort }),
        fallbacks: supports_fallbacks(&req.model).then_some("default"),
        cache_control: WireCacheControl { kind: "ephemeral" },
    }
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    content: Vec<Value>,
    stop_reason: Option<String>,
    stop_details: Option<WireStopDetails>,
    usage: Option<WireUsage>,
    #[serde(default)]
    model: String,
}

#[derive(Deserialize)]
struct WireStopDetails {
    explanation: Option<String>,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireErrorBody {
    error: Option<WireErrorDetail>,
}

#[derive(Deserialize)]
struct WireErrorDetail {
    message: Option<String>,
}

fn parse_block(v: Value) -> Block {
    let parsed = match v.get("type").and_then(Value::as_str) {
        Some("text") => v
            .get("text")
            .and_then(Value::as_str)
            .map(|text| Block::Text {
                text: text.to_string(),
            }),
        Some("tool_use") => {
            let id = v.get("id").and_then(Value::as_str);
            let name = v.get("name").and_then(Value::as_str);
            match (id, name) {
                (Some(id), Some(name)) => Some(Block::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: v
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Default::default())),
                }),
                _ => None,
            }
        }
        _ => None,
    };
    parsed.unwrap_or(Block::Raw(v))
}

fn parse_response(body: &[u8]) -> Result<LlmResponse> {
    let wire: WireResponse =
        serde_json::from_slice(body).context("the model API sent an unreadable response")?;
    let stop_reason = wire.stop_reason.unwrap_or_default();
    if stop_reason == "refusal" {
        let explanation = wire
            .stop_details
            .and_then(|d| d.explanation)
            .unwrap_or_else(|| "declined".to_string());
        return Err(LlmError::Refused(explanation).into());
    }
    let usage = wire.usage.unwrap_or_default();
    Ok(LlmResponse {
        content: wire.content.into_iter().map(parse_block).collect(),
        stop_reason,
        usage: Usage {
            input_tokens: usage.input_tokens.unwrap_or(0),
            output_tokens: usage.output_tokens.unwrap_or(0),
            cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
            cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
        },
        model: wire.model,
    })
}

/// The `error.message` of an API error body, else the (shortened) body text.
fn error_message(status: StatusCode, body: &str) -> String {
    let message = serde_json::from_str::<WireErrorBody>(body)
        .ok()
        .and_then(|b| b.error)
        .and_then(|e| e.message);
    match message {
        Some(m) => m,
        None => {
            let text: String = body.chars().take(300).collect();
            if text.trim().is_empty() {
                status.canonical_reason().unwrap_or("error").to_string()
            } else {
                text
            }
        }
    }
}

fn retry_after(res: &reqwest::Response) -> Option<Duration> {
    let secs: f64 = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs).min(MAX_RETRY_AFTER))
}

#[async_trait]
impl Llm for AnthropicLlm {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn supports_tools(&self) -> bool {
        true
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        let url = format!("{}/v1/messages", self.base);
        let body = serde_json::to_vec(&wire_request(&req)).context("encoding the request")?;
        let fallbacks = supports_fallbacks(&req.model);

        let mut attempt: u32 = 0;
        loop {
            let mut builder = self
                .http
                .post(&url)
                .header("x-api-key", self.key.clone())
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json");
            if fallbacks {
                builder = builder.header("anthropic-beta", FALLBACKS_BETA);
            }
            // The backoff when the server gives no retry-after: base, then base * 2.
            let backoff = self.retry_base * 2u32.pow(attempt);
            let can_retry = attempt < MAX_RETRIES;

            let wait = match builder.body(body.clone()).send().await {
                Ok(res) => {
                    let status = res.status();
                    if status.is_success() {
                        let bytes = res
                            .bytes()
                            .await
                            .context("reading the model API response")?;
                        return parse_response(&bytes);
                    }
                    let wait = retry_after(&res).unwrap_or(backoff);
                    let text = res.text().await.unwrap_or_default();
                    if !(is_retryable(status) && can_retry) {
                        return Err(LlmError::Api {
                            status: status.as_u16(),
                            message: error_message(status, &text),
                        }
                        .into());
                    }
                    tracing::warn!(
                        status = status.as_u16(),
                        attempt,
                        "model API error, retrying"
                    );
                    wait
                }
                Err(e) => {
                    if !can_retry {
                        return Err(anyhow::Error::new(e).context("calling the model API"));
                    }
                    tracing::warn!(error = %e, attempt, "model API connection error, retrying");
                    backoff
                }
            };
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::llm::Role;

    fn llm(server: &MockServer) -> AnthropicLlm {
        AnthropicLlm::new(&server.uri(), "test-key".to_string())
            .unwrap()
            .with_retry_base(Duration::from_millis(1))
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![Block::Text { text: text.into() }],
        }
    }

    fn request(model: &str, effort: Option<&str>) -> LlmRequest {
        LlmRequest {
            model: model.into(),
            system: "Be brief.".into(),
            messages: vec![user("hello")],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_tokens: 1024,
            effort: effort.map(str::to_string),
        }
    }

    fn search_tool() -> ToolDef {
        ToolDef {
            name: "search".into(),
            description: "Search the index".into(),
            input_schema: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        }
    }

    fn text_reply(text: &str) -> Value {
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5-5",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 11, "output_tokens": 7, "cache_read_input_tokens": 3,
                      "cache_creation_input_tokens": 5}
        })
    }

    async fn mount_ok(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_reply("hi")))
            .mount(server)
            .await;
    }

    async fn sent(server: &MockServer) -> Vec<wiremock::Request> {
        server.received_requests().await.unwrap()
    }

    fn body_of(r: &wiremock::Request) -> Value {
        r.body_json().unwrap()
    }

    #[tokio::test]
    async fn sends_headers_effort_fallbacks_and_caching() {
        let server = MockServer::start().await;
        // The headers are matchers: a request without one gets a 404 and the call fails.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("content-type", "application/json"))
            .and(header("anthropic-beta", "server-side-fallback-2026-07-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_reply("hi")))
            .expect(1)
            .mount(&server)
            .await;

        let res = llm(&server)
            .complete(request("claude-opus-5-5", Some("low")))
            .await
            .unwrap();
        assert_eq!(
            res.content,
            vec![Block::Text {
                text: "hi".to_string()
            }]
        );
        assert_eq!(res.stop_reason, "end_turn");
        assert_eq!(res.model, "claude-opus-5-5");
        assert_eq!(
            res.usage,
            Usage {
                input_tokens: 11,
                output_tokens: 7,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 5
            }
        );

        let reqs = sent(&server).await;
        assert_eq!(reqs.len(), 1);
        let body = body_of(&reqs[0]);
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
        );
        assert_eq!(body["output_config"], json!({"effort": "low"}));
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        for key in [
            "thinking",
            "temperature",
            "top_p",
            "top_k",
            "tools",
            "tool_choice",
        ] {
            assert!(body.get(key).is_none(), "{key} must not be sent: {body}");
        }
    }

    #[tokio::test]
    async fn haiku_without_effort_gets_no_fallbacks_beta_or_output_config() {
        let server = MockServer::start().await;
        mount_ok(&server).await;

        llm(&server)
            .complete(request("claude-haiku-4-5", None))
            .await
            .unwrap();

        let reqs = sent(&server).await;
        assert!(reqs[0].headers.get("anthropic-beta").is_none());
        assert!(reqs[0].headers.get("x-api-key").is_some());
        let body = body_of(&reqs[0]);
        for key in ["fallbacks", "output_config", "thinking"] {
            assert!(body.get(key).is_none(), "{key} must not be sent: {body}");
        }
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn fallbacks_are_for_the_listed_model_families_only() {
        for model in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-sonnet-5-5",
            "claude-fable-5",
        ] {
            assert!(supports_fallbacks(model), "{model}");
        }
        for model in [
            "claude-haiku-4-5",
            "claude-sonnet-4-5",
            "claude-sonnet-5",
            "fake",
        ] {
            assert!(!supports_fallbacks(model), "{model}");
        }
    }

    #[tokio::test]
    async fn tool_choice_none_sends_the_tools_and_auto_sends_no_choice() {
        let server = MockServer::start().await;
        mount_ok(&server).await;
        let llm = llm(&server);

        let mut none = request("claude-opus-5-5", Some("low"));
        none.tools = vec![search_tool()];
        none.tool_choice = ToolChoice::None;
        llm.complete(none).await.unwrap();

        let mut auto = request("claude-opus-5-5", Some("low"));
        auto.tools = vec![search_tool()];
        llm.complete(auto).await.unwrap();

        // None without tools has nothing to turn off.
        let mut bare = request("claude-opus-5-5", Some("low"));
        bare.tool_choice = ToolChoice::None;
        llm.complete(bare).await.unwrap();

        let reqs = sent(&server).await;
        let none_body = body_of(&reqs[0]);
        assert_eq!(none_body["tool_choice"], json!({"type": "none"}));
        assert_eq!(
            none_body["tools"],
            json!([{
                "name": "search",
                "description": "Search the index",
                "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}
            }])
        );
        let auto_body = body_of(&reqs[1]);
        assert!(auto_body.get("tool_choice").is_none(), "{auto_body}");
        assert_eq!(auto_body["tools"].as_array().unwrap().len(), 1);
        let bare_body = body_of(&reqs[2]);
        assert!(bare_body.get("tool_choice").is_none(), "{bare_body}");
        assert!(bare_body.get("tools").is_none(), "{bare_body}");
    }

    #[tokio::test]
    async fn thinking_and_tool_use_round_trip_unchanged() {
        let thinking = json!({
            "type": "thinking",
            "thinking": "I should search first.",
            "signature": "sig-abc=="
        });
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "claude-opus-5-5",
                "content": [
                    thinking,
                    {"type": "tool_use", "id": "toolu_1", "name": "search",
                     "input": {"query": "plan limits", "limit": 5}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 20, "output_tokens": 9}
            })))
            .mount(&server)
            .await;
        let llm = llm(&server);

        let mut first = request("claude-opus-5-5", Some("medium"));
        first.tools = vec![search_tool()];
        let res = llm.complete(first.clone()).await.unwrap();

        assert_eq!(res.stop_reason, "tool_use");
        assert_eq!(res.usage.cache_read_input_tokens, 0);
        assert_eq!(res.usage.cache_creation_input_tokens, 0);
        assert_eq!(
            res.content,
            vec![
                Block::Raw(thinking.clone()),
                Block::ToolUse {
                    id: "toolu_1".into(),
                    name: "search".into(),
                    input: json!({"query": "plan limits", "limit": 5}),
                },
            ]
        );

        // The next request echoes the assistant turn, thinking block included, then the result.
        let mut second = first;
        second.messages.push(Message {
            role: Role::Assistant,
            content: res.content.clone(),
        });
        second.messages.push(Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "src/plans.rs:1-9".into(),
                is_error: true,
            }],
        });
        second.tool_choice = ToolChoice::None;
        llm.complete(second).await.unwrap();

        let reqs = sent(&server).await;
        let body = body_of(&reqs[1]);
        assert_eq!(
            body["messages"][1],
            json!({
                "role": "assistant",
                "content": [
                    thinking,
                    {"type": "tool_use", "id": "toolu_1", "name": "search",
                     "input": {"query": "plan limits", "limit": 5}}
                ]
            })
        );
        assert_eq!(
            body["messages"][2],
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "src/plans.rs:1-9",
                    "is_error": true
                }]
            })
        );
        assert_eq!(body["tool_choice"], json!({"type": "none"}));
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_refusal_is_llm_error_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "claude-opus-5-5",
                "content": [],
                "stop_reason": "refusal",
                "stop_details": {"type": "refusal", "explanation": "not allowed"},
                "usage": {"input_tokens": 1, "output_tokens": 0}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "claude-opus-5-5",
                "content": [],
                "stop_reason": "refusal"
            })))
            .mount(&server)
            .await;
        let llm = llm(&server);

        let err = llm
            .complete(request("claude-opus-5-5", Some("low")))
            .await
            .unwrap_err();
        match err.downcast_ref::<LlmError>() {
            Some(LlmError::Refused(why)) => assert_eq!(why, "not allowed"),
            other => panic!("expected Refused, got {other:?}"),
        }

        let err = llm
            .complete(request("claude-opus-5-5", Some("low")))
            .await
            .unwrap_err();
        match err.downcast_ref::<LlmError>() {
            Some(LlmError::Refused(why)) => assert_eq!(why, "declined"),
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_overloaded_responses_then_success_makes_three_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(529).set_body_json(json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}
            })))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        mount_ok(&server).await;

        let res = llm(&server)
            .complete(request("claude-opus-5-5", Some("low")))
            .await
            .unwrap();
        assert_eq!(res.stop_reason, "end_turn");
        assert_eq!(sent(&server).await.len(), 3);
    }

    #[tokio::test]
    async fn retries_stop_after_two() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "type": "error",
                "error": {"type": "api_error", "message": "Service unavailable"}
            })))
            .mount(&server)
            .await;

        let err = llm(&server)
            .complete(request("claude-opus-5-5", Some("low")))
            .await
            .unwrap_err();
        match err.downcast_ref::<LlmError>() {
            Some(LlmError::Api { status, message }) => {
                assert_eq!(*status, 503);
                assert_eq!(message, "Service unavailable");
            }
            other => panic!("expected Api, got {other:?}"),
        }
        assert_eq!(sent(&server).await.len(), 3);
    }

    #[tokio::test]
    async fn other_client_errors_are_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "max_tokens is too large"}
            })))
            .mount(&server)
            .await;

        let err = llm(&server)
            .complete(request("claude-opus-5-5", Some("low")))
            .await
            .unwrap_err();
        match err.downcast_ref::<LlmError>() {
            Some(LlmError::Api { status, message }) => {
                assert_eq!(*status, 400);
                assert_eq!(message, "max_tokens is too large");
            }
            other => panic!("expected Api, got {other:?}"),
        }
        assert_eq!(sent(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn retry_after_replaces_the_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_ok(&server).await;
        // A backoff of 30s would blow the timeout below, so only the header can make this pass.
        let llm = llm(&server).with_retry_base(Duration::from_secs(30));

        let res = tokio::time::timeout(
            Duration::from_secs(10),
            llm.complete(request("claude-opus-5-5", Some("low"))),
        )
        .await
        .expect("retry-after was ignored")
        .unwrap();
        assert_eq!(res.stop_reason, "end_turn");
        assert_eq!(sent(&server).await.len(), 2);
    }

    #[tokio::test]
    async fn connection_errors_are_retried_then_reported() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A server that accepts a connection and drops it at once.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(socket);
            }
        });
        let llm = AnthropicLlm::new(&format!("http://{addr}"), "test-key".into())
            .unwrap()
            .with_retry_base(Duration::from_millis(1));

        let err = llm
            .complete(request("claude-opus-5-5", Some("low")))
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<LlmError>().is_none(), "{err:#}");
        assert!(format!("{err:#}").contains("model API"), "{err:#}");
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
    }
}
