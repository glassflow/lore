//! Answers with citations. Fast mode (one search, one model call) lives in [`fast`]; the prompt
//! it sends is built in [`prompt`]. Sources mode (the same search, no model: the hits as bullets)
//! lives in [`sources`]. Deep mode (an agent loop over the [`tools`]) lives in [`deep`]. The
//! request and response types here are shared by the HTTP route, the CLI, the web page and MCP.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AppError;
use crate::http::AppState;
use crate::search::MAX_QUERY_CHARS;

pub mod deep;
pub mod fast;
pub mod prompt;
pub mod sources;
pub mod tools;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Fast,
    /// The sources fast mode would read, as bullets, with no model call.
    Sources,
    Deep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRequest {
    pub question: String,
    #[serde(default)]
    pub mode: Mode,
    /// Sources to retrieve in fast and sources mode (the search clamps it to 1..=50). Default 12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<usize>,
    /// Only these document kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Vec<String>>,
    /// Only the source with this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Only the sources with these names. Combined with `source`; neither means every source.
    /// Deep mode's tools keep to them too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<String>>,
}

impl AskRequest {
    /// The source names this question is limited to, from `source` and `sources`; `None` means
    /// every source.
    pub fn source_scope(&self) -> Option<Vec<String>> {
        crate::search::source_names(self.source.iter().chain(self.sources.iter().flatten()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// `"1"` in fast mode; `"path:10-20"` or `"session:<id>"` in deep mode.
    pub label: String,
    pub document_id: Option<Uuid>,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub path: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageOut {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskResponse {
    pub answer: String,
    pub mode: Mode,
    pub model: String,
    pub citations: Vec<Citation>,
    pub sources_considered: usize,
    pub tool_calls: u32,
    pub usage: UsageOut,
    pub latency_ms: u64,
}

/// Answers `req.question` from the tenant's indexed material. A blank question and one over
/// 2000 characters are a `BadRequest`, and so is deep mode with a model provider that cannot use tools.
pub async fn ask(
    state: &AppState,
    tenant_id: Uuid,
    req: AskRequest,
) -> Result<AskResponse, AppError> {
    let started = Instant::now();
    let question = req.question.trim();
    if question.is_empty() {
        return Err(AppError::BadRequest("question is required".to_string()));
    }
    // Checked here, before the mode split and before deep mode takes a slot of the day, so a
    // refused question costs nothing. Every mode searches with the question, which has the same
    // limit.
    if question.chars().count() > MAX_QUERY_CHARS {
        return Err(AppError::BadRequest(format!(
            "question must be at most {MAX_QUERY_CHARS} characters"
        )));
    }
    let mut res = match req.mode {
        Mode::Fast => fast::answer(state, tenant_id, &req).await?,
        Mode::Sources => sources::answer(state, tenant_id, &req).await?,
        Mode::Deep => deep::answer(state, tenant_id, &req).await?,
    };
    res.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(res)
}
