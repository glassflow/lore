//! Model providers behind the `Llm` trait: `anthropic` (raw HTTP), `openai_compat`
//! (llama-server and friends, text only) and a deterministic `fake`.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Serialize;

use crate::config::{Config, LlmKind};

pub mod anthropic;
pub mod fake;
pub mod openai_compat;

pub use self::anthropic::AnthropicLlm;
pub use self::fake::FakeLlm;
pub use self::openai_compat::OpenAiLlm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Any other block from the model (for example `thinking`): echoed back unchanged.
    Raw(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Never "any" or "tool": Claude Opus 5.5 rejects forced tool use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: ToolChoice,
    pub max_tokens: u32,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmResponse {
    pub content: Vec<Block>,
    pub stop_reason: String,
    pub usage: Usage,
    pub model: String,
}

/// Errors callers may want to tell apart. They travel inside the `anyhow::Error` that
/// `Llm::complete` returns: use `err.downcast_ref::<LlmError>()`.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// The model declined to answer (`stop_reason` "refusal").
    #[error("the model refused: {0}")]
    Refused(String),
    /// The API answered with an error status.
    #[error("model API error (status {status}): {message}")]
    Api { status: u16, message: String },
}

#[async_trait]
pub trait Llm: Send + Sync {
    /// `"anthropic"`, `"openai"` or `"fake"`.
    fn name(&self) -> &str;
    fn supports_tools(&self) -> bool;
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModeModel {
    pub model: String,
    pub effort: Option<String>,
}

/// The model and effort each answer mode uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Models {
    pub fast: ModeModel,
    pub deep: ModeModel,
}

/// Builds the provider chosen by `LORE_LLM`, with the models of the two answer modes. A missing
/// required setting is an error that names the variable.
pub fn from_config(cfg: &Config) -> Result<(Arc<dyn Llm>, Models)> {
    match cfg.llm {
        LlmKind::Anthropic => {
            let key = cfg.anthropic_api_key.clone().ok_or_else(|| {
                anyhow!("ANTHROPIC_API_KEY is required when LORE_LLM=anthropic (the default)")
            })?;
            let llm = AnthropicLlm::new(&cfg.anthropic_base_url, key)?;
            let models = Models {
                fast: ModeModel {
                    model: cfg.fast_model.clone(),
                    effort: cfg.fast_effort.clone(),
                },
                deep: ModeModel {
                    model: cfg.deep_model.clone(),
                    effort: cfg.deep_effort.clone(),
                },
            };
            Ok((Arc::new(llm), models))
        }
        LlmKind::Openai => {
            let base = cfg
                .openai_base_url
                .clone()
                .ok_or_else(|| anyhow!("LORE_OPENAI_BASE_URL is required when LORE_LLM=openai"))?;
            let model = cfg
                .openai_model
                .clone()
                .ok_or_else(|| anyhow!("LORE_OPENAI_MODEL is required when LORE_LLM=openai"))?;
            let llm = OpenAiLlm::new(&base, cfg.openai_api_key.clone())?;
            let mode = ModeModel {
                model,
                effort: None,
            };
            let models = Models {
                fast: mode.clone(),
                deep: mode,
            };
            Ok((Arc::new(llm), models))
        }
        LlmKind::Fake => {
            let mode = ModeModel {
                model: "fake".to_string(),
                effort: None,
            };
            let models = Models {
                fast: mode.clone(),
                deep: mode,
            };
            Ok((Arc::new(FakeLlm::new()), models))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone()))
            .unwrap()
    }

    // `Arc<dyn Llm>` is not Debug, so unwrap_err is not available.
    fn err_text(cfg: &Config) -> String {
        match from_config(cfg) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn anthropic_needs_its_key() {
        let msg = err_text(&cfg(&[]));
        assert!(msg.contains("ANTHROPIC_API_KEY"), "{msg}");
        let msg = err_text(&cfg(&[("ANTHROPIC_API_KEY", "")]));
        assert!(msg.contains("ANTHROPIC_API_KEY"), "{msg}");
    }

    #[test]
    fn anthropic_models_come_from_config() {
        let (llm, models) = from_config(&cfg(&[
            ("ANTHROPIC_API_KEY", "test-key"),
            ("LORE_FAST_MODEL", "claude-haiku-4-5"),
            ("LORE_FAST_EFFORT", ""),
        ]))
        .unwrap();
        assert_eq!(llm.name(), "anthropic");
        assert!(llm.supports_tools());
        assert_eq!(models.fast.model, "claude-haiku-4-5");
        assert_eq!(models.fast.effort, None);
        assert_eq!(models.deep.model, "claude-opus-5-5");
        assert_eq!(models.deep.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn openai_needs_base_url_and_model() {
        let msg = err_text(&cfg(&[("LORE_LLM", "openai")]));
        assert!(msg.contains("LORE_OPENAI_BASE_URL"), "{msg}");
        let msg = err_text(&cfg(&[
            ("LORE_LLM", "openai"),
            ("LORE_OPENAI_BASE_URL", "http://localhost:8081/v1"),
        ]));
        assert!(msg.contains("LORE_OPENAI_MODEL"), "{msg}");
    }

    #[test]
    fn openai_uses_one_model_without_effort() {
        let (llm, models) = from_config(&cfg(&[
            ("LORE_LLM", "openai"),
            ("LORE_OPENAI_BASE_URL", "http://localhost:8081/v1"),
            ("LORE_OPENAI_MODEL", "qwen"),
            ("LORE_FAST_MODEL", "ignored"),
        ]))
        .unwrap();
        assert_eq!(llm.name(), "openai");
        assert!(!llm.supports_tools());
        assert_eq!(models.fast, models.deep);
        assert_eq!(models.fast.model, "qwen");
        assert_eq!(models.fast.effort, None);
    }

    #[test]
    fn fake_models_are_named_fake() {
        let (llm, models) = from_config(&cfg(&[("LORE_LLM", "fake")])).unwrap();
        assert_eq!(llm.name(), "fake");
        assert_eq!(models.fast.model, "fake");
        assert_eq!(models.deep.model, "fake");
        assert_eq!(models.fast.effort, None);
        assert_eq!(models.deep.effort, None);
    }
}
