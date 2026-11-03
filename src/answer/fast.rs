//! Fast mode: one hybrid search, one model call, an answer with numbered citations.

use std::sync::LazyLock;

use regex::Regex;
use uuid::Uuid;

use super::prompt::{FAST_SYSTEM, fast_user_message};
use super::{AskRequest, AskResponse, Citation, Mode, UsageOut};
use crate::error::AppError;
use crate::http::AppState;
use crate::llm::{Block, LlmError, LlmRequest, Message, Role, ToolChoice};
use crate::search::{Hit, SearchQuery, search};

/// Sources retrieved when the request does not say.
pub(super) const DEFAULT_K: usize = 12;
/// The most characters of sources and question sent to the model.
const MAX_PROMPT_CHARS: usize = 60_000;
const MAX_TOKENS: u32 = 2048;

pub(super) const NOTHING_FOUND: &str = "I found nothing indexed that matches this question.";

static CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d+)\]").expect("the citation pattern is valid"));

pub async fn answer(
    state: &AppState,
    tenant_id: Uuid,
    req: &AskRequest,
) -> Result<AskResponse, AppError> {
    let question = req.question.trim();
    let hits = search(
        &state.pool,
        state.embedder.clone(),
        SearchQuery {
            tenant_id,
            q: question.to_string(),
            k: req.k.unwrap_or(DEFAULT_K),
            kinds: req.kind.clone(),
            source: req.source.clone().filter(|s| !s.trim().is_empty()),
        },
    )
    .await?;
    if hits.is_empty() {
        return Ok(AskResponse {
            answer: NOTHING_FOUND.to_string(),
            mode: Mode::Fast,
            model: "none".to_string(),
            citations: Vec::new(),
            sources_considered: 0,
            tool_calls: 0,
            usage: UsageOut::default(),
            latency_ms: 0,
        });
    }

    let (message, included) = fast_user_message(question, &hits, MAX_PROMPT_CHARS);
    let res = state
        .llm
        .complete(LlmRequest {
            model: state.models.fast.model.clone(),
            effort: state.models.fast.effort.clone(),
            system: FAST_SYSTEM.to_string(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Block::Text { text: message }],
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_tokens: MAX_TOKENS,
        })
        .await
        .map_err(|e| match e.downcast_ref::<LlmError>() {
            Some(LlmError::Refused(_)) => {
                AppError::Unprocessable("the model declined to answer".to_string())
            }
            _ => AppError::Internal(e),
        })?;

    let answer: String = res
        .content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let citations = parse_citations(&answer, &hits[..included]);
    Ok(AskResponse {
        answer,
        mode: Mode::Fast,
        model: res.model,
        citations,
        sources_considered: included,
        tool_calls: 0,
        usage: UsageOut {
            input_tokens: res.usage.input_tokens,
            output_tokens: res.usage.output_tokens,
        },
        latency_ms: 0,
    })
}

/// The sources an answer cites with `[n]`: each number once, at its first appearance, in order.
/// Numbers outside `1..=sources.len()` are ignored.
fn parse_citations(answer: &str, sources: &[Hit]) -> Vec<Citation> {
    let mut seen: Vec<usize> = Vec::new();
    let mut citations = Vec::new();
    for caps in CITATION.captures_iter(answer) {
        let Ok(n) = caps[1].parse::<usize>() else {
            continue;
        };
        if n == 0 || n > sources.len() || seen.contains(&n) {
            continue;
        }
        seen.push(n);
        let hit = &sources[n - 1];
        citations.push(Citation {
            label: n.to_string(),
            document_id: Some(hit.document_id),
            kind: Some(hit.kind.clone()),
            title: Some(hit.title.clone()),
            path: hit.path.clone(),
            start_line: hit.start_line,
            end_line: hit.end_line,
            url: hit.uri.clone(),
        });
    }
    citations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, uri: Option<&str>) -> Hit {
        Hit {
            chunk_id: 1,
            document_id: Uuid::new_v4(),
            source: "sample".to_string(),
            kind: "code".to_string(),
            title: format!("title of {path}"),
            uri: uri.map(str::to_string),
            path: Some(path.to_string()),
            external_id: None,
            start_line: Some(3),
            end_line: Some(9),
            symbols: Vec::new(),
            text: String::new(),
            score: 0.03,
        }
    }

    fn labels(c: &[Citation]) -> Vec<&str> {
        c.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn keeps_the_first_appearance_of_each_number_in_order() {
        let sources = vec![hit("a.rs", None), hit("b.rs", None), hit("c.rs", None)];
        let c = parse_citations("Plans [3] and [1], again [3] [1] and [2].", &sources);
        assert_eq!(labels(&c), ["3", "1", "2"]);
    }

    #[test]
    fn drops_numbers_out_of_range() {
        let sources = vec![hit("a.rs", None), hit("b.rs", None)];
        let c = parse_citations(
            "[0] [3] [99999999999999999999999] [2] [x] [ 1 ] [1]",
            &sources,
        );
        // Zero, 3, the overflow, "[x]" and "[ 1 ]" are not citations.
        assert_eq!(labels(&c), ["2", "1"]);
    }

    #[test]
    fn no_markers_no_citations() {
        assert!(parse_citations("No sources here.", &[hit("a.rs", None)]).is_empty());
        assert!(parse_citations("[1]", &[]).is_empty());
    }

    #[test]
    fn a_citation_is_filled_from_its_hit() {
        let h = hit("src/billing/plans.rs", Some("https://example.test/plans"));
        let c = parse_citations("See [1].", std::slice::from_ref(&h));
        assert_eq!(
            c,
            vec![Citation {
                label: "1".to_string(),
                document_id: Some(h.document_id),
                kind: Some("code".to_string()),
                title: Some("title of src/billing/plans.rs".to_string()),
                path: Some("src/billing/plans.rs".to_string()),
                start_line: Some(3),
                end_line: Some(9),
                url: Some("https://example.test/plans".to_string()),
            }]
        );
    }

    #[test]
    fn a_session_citation_has_a_title_but_no_path_or_url() {
        let mut h = hit("unused", None);
        h.kind = "session".to_string();
        h.title = "How many plans do we have?".to_string();
        h.path = None;
        h.start_line = None;
        h.end_line = None;
        h.external_id = Some("sess-1".to_string());
        let c = parse_citations("See [1].", std::slice::from_ref(&h));
        assert_eq!(
            c,
            vec![Citation {
                label: "1".to_string(),
                document_id: Some(h.document_id),
                kind: Some("session".to_string()),
                title: Some("How many plans do we have?".to_string()),
                path: None,
                start_line: None,
                end_line: None,
                url: None,
            }]
        );
    }
}
