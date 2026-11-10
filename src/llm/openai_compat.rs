//! An OpenAI-compatible chat completions endpoint (llama-server and similar). Text only in v1.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};

use super::{Block, Llm, LlmError, LlmRequest, LlmResponse, Message, Usage};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub struct OpenAiLlm {
    http: reqwest::Client,
    base: String,
    key: Option<HeaderValue>,
}

impl OpenAiLlm {
    /// `base` is the URL that `/chat/completions` hangs off, for example
    /// `http://localhost:8081/v1`.
    pub fn new(base: &str, key: Option<String>) -> Result<Self> {
        let key = match key {
            None => None,
            Some(k) => {
                let mut v = HeaderValue::from_str(&format!("Bearer {k}")).map_err(|_| {
                    anyhow!("LORE_OPENAI_API_KEY contains characters not allowed in a header")
                })?;
                v.set_sensitive(true);
                Some(v)
            }
        };
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            key,
        })
    }
}

#[derive(Serialize)]
struct WireRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<WireMessage>,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    choices: Vec<WireChoice>,
    usage: Option<WireUsage>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireErrorBody {
    error: Option<WireErrorDetail>,
}

#[derive(Deserialize)]
struct WireErrorDetail {
    message: Option<String>,
}

fn text_of(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .concat()
}

fn wire_request(req: &LlmRequest) -> Result<WireRequest> {
    if !req.tools.is_empty() {
        bail!("the openai provider does not support tools");
    }
    let mut messages = vec![WireMessage {
        role: "system",
        content: req.system.clone(),
    }];
    for m in &req.messages {
        if m.content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { .. } | Block::ToolResult { .. }))
        {
            bail!("the openai provider does not support tool_use or tool_result blocks");
        }
        messages.push(WireMessage {
            role: m.role.as_str(),
            content: text_of(m),
        });
    }
    Ok(WireRequest {
        model: req.model.clone(),
        max_tokens: req.max_tokens,
        messages,
    })
}

#[async_trait]
impl Llm for OpenAiLlm {
    fn name(&self) -> &str {
        "openai"
    }

    fn supports_tools(&self) -> bool {
        false
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        let body = wire_request(&req)?;
        let mut builder = self
            .http
            .post(format!("{}/chat/completions", self.base))
            .json(&body);
        if let Some(key) = &self.key {
            builder = builder.header(reqwest::header::AUTHORIZATION, key.clone());
        }
        let res = builder.send().await.context("calling the model API")?;
        let status = res.status();
        let bytes = res
            .bytes()
            .await
            .context("reading the model API response")?;
        if !status.is_success() {
            let message = serde_json::from_slice::<WireErrorBody>(&bytes)
                .ok()
                .and_then(|b| b.error)
                .and_then(|e| e.message)
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(300).collect());
            return Err(LlmError::Api {
                status: status.as_u16(),
                message,
            }
            .into());
        }
        let wire: WireResponse =
            serde_json::from_slice(&bytes).context("the model API sent an unreadable response")?;
        let choice = wire
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("the model API sent no choices"))?;
        let stop_reason = match choice.finish_reason.as_deref() {
            None | Some("stop") => "end_turn".to_string(),
            Some("length") => "max_tokens".to_string(),
            Some(other) => other.to_string(),
        };
        let usage = wire.usage;
        Ok(LlmResponse {
            content: vec![Block::Text {
                text: choice.message.content.unwrap_or_default(),
            }],
            stop_reason,
            usage: Usage {
                input_tokens: usage.as_ref().and_then(|u| u.prompt_tokens).unwrap_or(0),
                output_tokens: usage
                    .as_ref()
                    .and_then(|u| u.completion_tokens)
                    .unwrap_or(0),
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            model: wire.model.unwrap_or(req.model),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::llm::{Role, ToolChoice, ToolDef};

    fn request() -> LlmRequest {
        LlmRequest {
            model: "qwen".into(),
            system: "Be brief.".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![
                        Block::Text {
                            text: "first ".into(),
                        },
                        Block::Text {
                            text: "second".into(),
                        },
                    ],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![Block::Text { text: "ok".into() }],
                },
                Message {
                    role: Role::User,
                    content: vec![Block::Text {
                        text: "why?".into(),
                    }],
                },
            ],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_tokens: 256,
            effort: None,
        }
    }

    #[tokio::test]
    async fn request_shape_and_response_parsing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "qwen-2.5",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Because."},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 30, "completion_tokens": 4, "total_tokens": 34}
            })))
            .expect(1)
            .mount(&server)
            .await;
        // A base URL with a trailing slash works too.
        let llm =
            OpenAiLlm::new(&format!("{}/v1/", server.uri()), Some("test-key".into())).unwrap();

        let res = llm.complete(request()).await.unwrap();
        assert_eq!(
            res.content,
            vec![Block::Text {
                text: "Because.".into()
            }]
        );
        assert_eq!(res.stop_reason, "end_turn");
        assert_eq!(res.model, "qwen-2.5");
        assert_eq!(
            res.usage,
            Usage {
                input_tokens: 30,
                output_tokens: 4,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0
            }
        );

        let reqs = server.received_requests().await.unwrap();
        let body: Value = reqs[0].body_json().unwrap();
        assert_eq!(
            body,
            json!({
                "model": "qwen",
                "max_tokens": 256,
                "messages": [
                    {"role": "system", "content": "Be brief."},
                    {"role": "user", "content": "first second"},
                    {"role": "assistant", "content": "ok"},
                    {"role": "user", "content": "why?"}
                ]
            })
        );
    }

    #[tokio::test]
    async fn no_key_means_no_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "x"}, "finish_reason": "length"}]
            })))
            .mount(&server)
            .await;
        let llm = OpenAiLlm::new(&server.uri(), None).unwrap();

        let res = llm.complete(request()).await.unwrap();
        assert_eq!(res.stop_reason, "max_tokens");
        assert_eq!(res.model, "qwen");
        assert_eq!(res.usage, Usage::default());
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs[0].headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn tools_and_tool_blocks_are_errors_without_a_call() {
        let server = MockServer::start().await;
        let llm = OpenAiLlm::new(&server.uri(), None).unwrap();

        let mut with_tools = request();
        with_tools.tools = vec![ToolDef {
            name: "search".into(),
            description: "d".into(),
            input_schema: json!({}),
        }];
        assert!(llm.complete(with_tools).await.is_err());

        let mut with_use = request();
        with_use.messages.push(Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "1".into(),
                name: "search".into(),
                input: json!({}),
            }],
        });
        assert!(llm.complete(with_use).await.is_err());

        let mut with_result = request();
        with_result.messages.push(Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "1".into(),
                content: "r".into(),
                is_error: false,
            }],
        });
        assert!(llm.complete(with_result).await.is_err());

        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn error_statuses_carry_the_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": {"message": "context too long", "type": "invalid_request_error"}
            })))
            .mount(&server)
            .await;
        let llm = OpenAiLlm::new(&server.uri(), None).unwrap();

        let err = llm.complete(request()).await.unwrap_err();
        match err.downcast_ref::<LlmError>() {
            Some(LlmError::Api { status, message }) => {
                assert_eq!(*status, 400);
                assert_eq!(message, "context too long");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }
}
