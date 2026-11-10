//! Deep mode: the model drives four tools (search, grep, read_file, get_session) for a few rounds
//! within a time budget, then answers with `[path:start-end]` and `[session:<id>]` citations.
//! A per-tenant guard caps how many deep questions run at once and how many a day.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, Utc};
use regex::Regex;
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use super::prompt::DEEP_SYSTEM;
use super::tools::{ToolOutput, run_tool, tool_defs};
use super::{AskRequest, AskResponse, Citation, Mode, UsageOut};
use crate::error::AppError;
use crate::http::AppState;
use crate::llm::{Block, LlmError, LlmRequest, LlmResponse, Message, Role, ToolChoice};
use crate::repo::is_full_sha;
use crate::search::render_web_url;
use crate::store;

const MAX_TOKENS: u32 = 16_000;
/// How long one tool may run. A slower tool becomes an error result the model can read.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
/// Added to the remaining budget for the timeout of a model call.
const CALL_GRACE: Duration = Duration::from_secs(20);
/// How long a question waits for a free slot before it is refused with a 429. Agents send several
/// questions at once and a deep answer takes 10 to 60 s, so a wait of a few seconds refused most.
const SLOT_WAIT: Duration = Duration::from_secs(60);
/// The most citations resolved for one answer.
const MAX_CITATIONS: usize = 50;

const TIME_IS_UP: &str = "Time is up. Answer now with what you have, with citations.";

/// `[path:10-20]` or `[path:10]`.
static CODE_CITATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[([A-Za-z0-9_./-]+):(\d+)(?:-(\d+))?\]").expect("the code pattern is valid")
});

/// `[session:<id>]`.
static SESSION_CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[session:([^\]]+)\]").expect("the session pattern is valid"));

/// One tenant's deep-mode state.
struct TenantGuard {
    slots: std::sync::Arc<Semaphore>,
    day: NaiveDate,
    asked: u32,
}

/// Caps deep-mode cost per tenant: `LORE_DEEP_CONCURRENCY` questions at once and
/// `LORE_DEEP_DAILY_LIMIT` per UTC day. lore runs as a single replica and the counts are in
/// memory, so a restart resets the day. The map holds one entry per tenant that asked.
#[derive(Default)]
pub struct DeepGuard {
    tenants: Mutex<HashMap<Uuid, TenantGuard>>,
}

impl DeepGuard {
    /// Reserves a slot and one question of today's allowance for `tenant_id`. The slot is held by
    /// the returned permit and freed when it drops, on every exit path. A full day is a 429 at
    /// once; no free slot within `wait` is a 429 too. A question that is refused does not use
    /// up any of the day's allowance.
    pub async fn admit(
        &self,
        tenant_id: Uuid,
        concurrency: usize,
        daily_limit: u32,
        wait: Duration,
        today: NaiveDate,
    ) -> Result<OwnedSemaphorePermit, AppError> {
        // Look first, so a tenant that is out of questions does not wait for a slot.
        let slots = self.with_tenant(tenant_id, concurrency, today, |g| {
            if g.asked >= daily_limit {
                return Err(daily_limit_reached());
            }
            Ok(g.slots.clone())
        })?;
        let permit = match tokio::time::timeout(wait, slots.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(e)) => return Err(AppError::Internal(e.into())),
            Err(_) => {
                return Err(AppError::TooManyRequests(
                    "too many deep-mode questions are running, try again shortly".to_string(),
                ));
            }
        };
        // Another question may have used the last of the day while this one waited.
        self.with_tenant(tenant_id, concurrency, today, |g| {
            if g.asked >= daily_limit {
                return Err(daily_limit_reached());
            }
            g.asked += 1;
            Ok(())
        })?;
        Ok(permit)
    }

    /// Runs `f` on the tenant's entry (created on first use, and reset when the day changed).
    fn with_tenant<T>(
        &self,
        tenant_id: Uuid,
        concurrency: usize,
        today: NaiveDate,
        f: impl FnOnce(&mut TenantGuard) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let mut tenants = self.tenants.lock().unwrap_or_else(PoisonError::into_inner);
        let guard = tenants.entry(tenant_id).or_insert_with(|| TenantGuard {
            slots: std::sync::Arc::new(Semaphore::new(concurrency)),
            day: today,
            asked: 0,
        });
        if guard.day != today {
            guard.day = today;
            guard.asked = 0;
        }
        f(guard)
    }
}

fn daily_limit_reached() -> AppError {
    AppError::TooManyRequests("daily deep-mode limit reached".to_string())
}

pub async fn answer(
    state: &AppState,
    tenant_id: Uuid,
    req: &AskRequest,
) -> Result<AskResponse, AppError> {
    if !state.llm.supports_tools() {
        return Err(AppError::BadRequest(
            "deep mode needs a tool-capable model provider (LORE_LLM=anthropic)".to_string(),
        ));
    }
    // Held until the answer is built or an error returns.
    let _permit = state
        .deep_guard
        .admit(
            tenant_id,
            state.config.deep_concurrency,
            state.config.deep_daily_limit,
            SLOT_WAIT,
            Utc::now().date_naive(),
        )
        .await?;

    let started = Instant::now();
    let budget = Duration::from_secs(state.config.deep_budget_secs);
    let max_rounds = state.config.deep_max_rounds;
    let tools = tool_defs();
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![Block::Text {
            text: req.question.trim().to_string(),
        }],
    }];
    let mut usage = UsageOut::default();
    let mut tool_calls = 0u32;
    let mut round = 0u32;
    // The text of the latest response that had any: what to return if a later call times out.
    let mut in_hand = String::new();
    let mut final_call = false;
    let mut model = state.models.deep.model.clone();

    let answer = loop {
        if !final_call && time_is_up(started.elapsed(), budget, round, max_rounds) {
            final_call = true;
            messages.push(Message {
                role: Role::User,
                content: vec![Block::Text {
                    text: TIME_IS_UP.to_string(),
                }],
            });
        }
        let request = LlmRequest {
            model: state.models.deep.model.clone(),
            effort: state.models.deep.effort.clone(),
            system: DEEP_SYSTEM.to_string(),
            messages: messages.clone(),
            tools: tools.clone(),
            tool_choice: if final_call {
                ToolChoice::None
            } else {
                ToolChoice::Auto
            },
            max_tokens: MAX_TOKENS,
        };
        let limit = budget.saturating_sub(started.elapsed()) + CALL_GRACE;
        let res = match tokio::time::timeout(limit, state.llm.complete(request)).await {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => {
                return Err(match e.downcast_ref::<LlmError>() {
                    Some(LlmError::Refused(_)) => {
                        AppError::Unprocessable("the model declined to answer".to_string())
                    }
                    _ => AppError::Internal(e),
                });
            }
            Err(_) if !in_hand.is_empty() => break in_hand,
            Err(_) => return Err(AppError::Internal(anyhow::anyhow!("deep mode timed out"))),
        };
        usage.add(res.usage);
        model.clone_from(&res.model);
        let text = response_text(&res);
        if !text.is_empty() {
            in_hand.clone_from(&text);
        }
        let uses = tool_uses(&res);
        if final_call || res.stop_reason != "tool_use" || uses.is_empty() {
            break text;
        }

        // Echo the turn exactly as the model sent it, then answer every tool call in one message.
        let results =
            futures::future::join_all(uses.iter().map(|(_, name, input)| {
                timed(TOOL_TIMEOUT, run_tool(state, tenant_id, name, input))
            }))
            .await;
        tool_calls += u32::try_from(uses.len()).unwrap_or(u32::MAX);
        messages.push(Message {
            role: Role::Assistant,
            content: res.content,
        });
        messages.push(Message {
            role: Role::User,
            content: uses
                .into_iter()
                .zip(results)
                .map(|((id, _, _), out)| Block::ToolResult {
                    tool_use_id: id,
                    content: out.text,
                    is_error: out.is_error,
                })
                .collect(),
        });
        round += 1;
    };

    let citations = resolve_citations(&state.pool, tenant_id, &answer).await?;
    tracing::info!(
        rounds = round,
        tool_calls,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "deep answer built"
    );
    Ok(AskResponse {
        answer,
        mode: Mode::Deep,
        model,
        citations,
        sources_considered: 0,
        tool_calls,
        usage,
        latency_ms: 0,
    })
}

/// True once more than 80% of the budget has passed or `max_rounds` tool rounds are done.
fn time_is_up(elapsed: Duration, budget: Duration, round: u32, max_rounds: u32) -> bool {
    elapsed > budget.mul_f64(0.8) || round >= max_rounds
}

/// Runs `fut`, or gives an error result the model can read if it takes longer than `limit`.
async fn timed(limit: Duration, fut: impl Future<Output = ToolOutput>) -> ToolOutput {
    match tokio::time::timeout(limit, fut).await {
        Ok(out) => out,
        Err(_) => ToolOutput {
            text: format!("the tool timed out after {} seconds", limit.as_secs()),
            is_error: true,
        },
    }
}

/// The text blocks of a response, joined.
fn response_text(res: &LlmResponse) -> String {
    res.content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The tool calls of a response as `(id, name, input)`, in order.
fn tool_uses(res: &LlmResponse) -> Vec<(String, String, Value)> {
    res.content
        .iter()
        .filter_map(|b| match b {
            Block::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

/// A citation as the model wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cited {
    Code {
        label: String,
        path: String,
        start: Option<i32>,
        end: Option<i32>,
    },
    Session {
        label: String,
        id: String,
    },
}

impl Cited {
    fn label(&self) -> &str {
        match self {
            Cited::Code { label, .. } | Cited::Session { label, .. } => label,
        }
    }
}

/// The citations in `answer`, in order of appearance, each label once. `[session:...]` is a
/// session citation whatever follows the colon, so a code match that starts where a session
/// match starts is dropped.
fn parse_citations(answer: &str) -> Vec<Cited> {
    let mut found: Vec<(usize, Cited)> = Vec::new();
    for caps in SESSION_CITATION.captures_iter(answer) {
        if let (Some(whole), Some(id)) = (caps.get(0), caps.get(1)) {
            found.push((
                whole.start(),
                Cited::Session {
                    label: inner(whole.as_str()).to_string(),
                    id: id.as_str().to_string(),
                },
            ));
        }
    }
    for caps in CODE_CITATION.captures_iter(answer) {
        let (Some(whole), Some(path), Some(start)) = (caps.get(0), caps.get(1), caps.get(2)) else {
            continue;
        };
        if found.iter().any(|(at, _)| *at == whole.start()) {
            continue;
        }
        found.push((
            whole.start(),
            Cited::Code {
                label: inner(whole.as_str()).to_string(),
                path: path.as_str().to_string(),
                start: start.as_str().parse().ok(),
                end: caps.get(3).and_then(|m| m.as_str().parse().ok()),
            },
        ));
    }
    found.sort_by_key(|(at, _)| *at);
    let mut out: Vec<Cited> = Vec::new();
    for (_, cited) in found {
        if out.iter().all(|c| c.label() != cited.label()) {
            out.push(cited);
        }
    }
    out
}

/// A match without its square brackets.
fn inner(matched: &str) -> &str {
    let matched = matched.strip_prefix('[').unwrap_or(matched);
    matched.strip_suffix(']').unwrap_or(matched)
}

/// What a code or doc lookup returns.
#[derive(sqlx::FromRow)]
struct CodeDocRow {
    id: Uuid,
    kind: String,
    title: String,
    uri: Option<String>,
    path: Option<String>,
    commit: Option<String>,
    template: Option<String>,
}

/// What a session lookup returns.
#[derive(sqlx::FromRow)]
struct SessionDocRow {
    id: Uuid,
    title: String,
    uri: Option<String>,
}

/// Matches the citations in `answer` to the tenant's documents. A path that starts with the name
/// of one of the tenant's sources is looked up in that source; if that finds nothing, or no
/// source name matches, the path is looked up as written in all of the tenant's sources. A
/// citation with no document keeps its label, path and lines and has no `document_id`.
async fn resolve_citations(
    pool: &PgPool,
    tenant_id: Uuid,
    answer: &str,
) -> Result<Vec<Citation>, AppError> {
    let cited = parse_citations(answer);
    if cited.is_empty() {
        return Ok(Vec::new());
    }
    let sources = store::list_sources(pool, tenant_id, None).await?;
    let mut citations = Vec::new();
    for c in cited.into_iter().take(MAX_CITATIONS) {
        citations.push(match c {
            Cited::Session { label, id } => {
                let row = find_session(pool, tenant_id, &id).await?;
                Citation {
                    label,
                    document_id: row.as_ref().map(|r| r.id),
                    kind: row.as_ref().map(|_| "session".to_string()),
                    title: row.as_ref().map(|r| r.title.clone()),
                    path: None,
                    start_line: None,
                    end_line: None,
                    url: row.and_then(|r| r.uri),
                }
            }
            Cited::Code {
                label,
                path,
                start,
                end,
            } => {
                let end = end.or(start);
                let prefixed = sources
                    .iter()
                    .filter_map(|s| {
                        let rest = path.strip_prefix(s.name.as_str())?.strip_prefix('/')?;
                        Some((s, rest))
                    })
                    .max_by_key(|(s, _)| s.name.len());
                let mut row = None;
                if let Some((source, rest)) = prefixed {
                    row = find_code_doc(pool, tenant_id, rest, Some(source.id)).await?;
                }
                if row.is_none() {
                    row = find_code_doc(pool, tenant_id, &path, None).await?;
                }
                match row {
                    Some(r) => {
                        let url = web_url(&r, start, end).or(r.uri.clone());
                        Citation {
                            label,
                            document_id: Some(r.id),
                            kind: Some(r.kind),
                            title: Some(r.title),
                            path: r.path.or(Some(path)),
                            start_line: start,
                            end_line: end,
                            url,
                        }
                    }
                    None => Citation {
                        label,
                        document_id: None,
                        kind: None,
                        title: None,
                        path: Some(path),
                        start_line: start,
                        end_line: end,
                        url: None,
                    },
                }
            }
        });
    }
    Ok(citations)
}

/// The source's web URL for the cited lines, when the source has a template and the document a
/// full commit sha, like the URLs search hits carry.
fn web_url(row: &CodeDocRow, start: Option<i32>, end: Option<i32>) -> Option<String> {
    match (
        row.template.as_deref(),
        row.path.as_deref(),
        row.commit.as_deref(),
        start,
        end,
    ) {
        (Some(template), Some(path), Some(commit), Some(start), Some(end))
            if is_full_sha(commit) =>
        {
            Some(render_web_url(template, path, commit, start, end))
        }
        _ => None,
    }
}

/// The tenant's code or doc document with this repo-relative path, in one source or in any.
async fn find_code_doc(
    pool: &PgPool,
    tenant_id: Uuid,
    path: &str,
    source_id: Option<Uuid>,
) -> sqlx::Result<Option<CodeDocRow>> {
    sqlx::query_as(
        "SELECT d.id, d.kind, d.title, d.uri, d.metadata->>'path' AS path,
                d.metadata->>'commit' AS commit, s.web_url_template AS template
         FROM documents d
         JOIN sources s ON s.id = d.source_id AND s.tenant_id = d.tenant_id
         WHERE d.tenant_id = $1
           AND d.kind IN ('code', 'doc')
           AND d.metadata->>'path' = $2
           AND ($3::uuid IS NULL OR d.source_id = $3)
         ORDER BY d.updated_at DESC, d.id
         LIMIT 1",
    )
    .bind(tenant_id)
    .bind(path)
    .bind(source_id)
    .fetch_optional(pool)
    .await
}

/// The tenant's session document with this external id (the most recent one).
async fn find_session(
    pool: &PgPool,
    tenant_id: Uuid,
    external_id: &str,
) -> sqlx::Result<Option<SessionDocRow>> {
    sqlx::query_as(
        "SELECT id, title, uri FROM documents
         WHERE tenant_id = $1 AND kind = 'session' AND external_id = $2
         ORDER BY updated_at DESC, id LIMIT 1",
    )
    .bind(tenant_id)
    .bind(external_id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(label: &str, path: &str, start: Option<i32>, end: Option<i32>) -> Cited {
        Cited::Code {
            label: label.into(),
            path: path.into(),
            start,
            end,
        }
    }

    fn session(id: &str) -> Cited {
        Cited::Session {
            label: format!("session:{id}"),
            id: id.into(),
        }
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[test]
    fn code_citations_with_a_range_or_one_line() {
        let got = parse_citations("See [sample/src/a.rs:10-20] and [b.py:7].");
        assert_eq!(
            got,
            vec![
                code(
                    "sample/src/a.rs:10-20",
                    "sample/src/a.rs",
                    Some(10),
                    Some(20)
                ),
                code("b.py:7", "b.py", Some(7), None),
            ]
        );
    }

    #[test]
    fn session_citations_win_for_session_labels() {
        let got = parse_citations("[session:abc-1] [session:12] [session:3-4] [session:a b]");
        assert_eq!(
            got,
            vec![
                session("abc-1"),
                session("12"),
                session("3-4"),
                session("a b")
            ]
        );
    }

    #[test]
    fn a_path_named_like_a_session_is_still_code() {
        let got = parse_citations("[sessions/a.rs:1-2] [x/session:5-6]");
        assert_eq!(
            got,
            vec![
                code("sessions/a.rs:1-2", "sessions/a.rs", Some(1), Some(2)),
                code("x/session:5-6", "x/session", Some(5), Some(6)),
            ]
        );
    }

    #[test]
    fn the_first_occurrence_of_each_label_wins_in_order() {
        let got = parse_citations(
            "[b.rs:1-2] [session:s1] [a.rs:3-4] [b.rs:1-2] [session:s1] [b.rs:1-3]",
        );
        let labels: Vec<&str> = got.iter().map(Cited::label).collect();
        assert_eq!(labels, ["b.rs:1-2", "session:s1", "a.rs:3-4", "b.rs:1-3"]);
    }

    #[test]
    fn text_that_is_not_a_citation_is_ignored() {
        assert!(parse_citations("[1] [a b.rs:1-2] [a.rs:x] a.rs:1-2 [] [:1-2]").is_empty());
    }

    #[test]
    fn a_line_number_too_big_for_i32_is_dropped() {
        let got = parse_citations("[a.rs:99999999999-3]");
        assert_eq!(got, vec![code("a.rs:99999999999-3", "a.rs", None, Some(3))]);
    }

    #[test]
    fn time_is_up_after_80_percent_of_the_budget_or_the_round_cap() {
        let budget = Duration::from_secs(30);
        assert!(!time_is_up(Duration::from_secs(24), budget, 0, 12));
        assert!(time_is_up(Duration::from_millis(24_001), budget, 0, 12));
        assert!(!time_is_up(Duration::ZERO, budget, 11, 12));
        assert!(time_is_up(Duration::ZERO, budget, 12, 12));
    }

    #[tokio::test]
    async fn a_slow_tool_becomes_an_error_result() {
        let out = timed(Duration::from_millis(20), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            ToolOutput {
                text: "late".into(),
                is_error: false,
            }
        })
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("timed out"), "{}", out.text);
        let out = timed(Duration::from_secs(5), async {
            ToolOutput {
                text: "ok".into(),
                is_error: false,
            }
        })
        .await;
        assert_eq!(out.text, "ok");
        assert!(!out.is_error);
    }

    const SHORT: Duration = Duration::from_millis(30);

    #[tokio::test]
    async fn a_full_tenant_waits_then_gets_a_429_and_a_freed_slot_works() {
        let guard = DeepGuard::default();
        let tenant = Uuid::new_v4();
        let first = guard.admit(tenant, 2, 100, SHORT, day(1)).await.unwrap();
        let _second = guard.admit(tenant, 2, 100, SHORT, day(1)).await.unwrap();
        let started = std::time::Instant::now();
        let err = guard
            .admit(tenant, 2, 100, SHORT, day(1))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::TooManyRequests(_)), "{err:?}");
        // It waited for the given time and no longer.
        assert!(started.elapsed() >= SHORT);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(first);
        drop(guard.admit(tenant, 2, 100, SHORT, day(1)).await.unwrap());
    }

    #[tokio::test]
    async fn a_slot_freed_during_the_wait_is_taken() {
        let guard = DeepGuard::default();
        let tenant = Uuid::new_v4();
        let held = guard.admit(tenant, 1, 100, SHORT, day(1)).await.unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(held);
        });
        let waited = guard
            .admit(tenant, 1, 100, Duration::from_secs(5), day(1))
            .await;
        assert!(waited.is_ok(), "{waited:?}");
    }

    #[tokio::test]
    async fn another_tenant_has_its_own_slots() {
        let guard = DeepGuard::default();
        let _a = guard
            .admit(Uuid::new_v4(), 1, 100, SHORT, day(1))
            .await
            .unwrap();
        drop(
            guard
                .admit(Uuid::new_v4(), 1, 100, SHORT, day(1))
                .await
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn the_daily_limit_holds_per_tenant_and_resets_the_next_day() {
        let guard = DeepGuard::default();
        let tenant = Uuid::new_v4();
        drop(guard.admit(tenant, 1, 2, SHORT, day(1)).await.unwrap());
        drop(guard.admit(tenant, 1, 2, SHORT, day(1)).await.unwrap());
        match guard.admit(tenant, 1, 2, SHORT, day(1)).await.unwrap_err() {
            AppError::TooManyRequests(m) => assert_eq!(m, "daily deep-mode limit reached"),
            other => panic!("{other:?}"),
        }
        drop(
            guard
                .admit(Uuid::new_v4(), 1, 2, SHORT, day(1))
                .await
                .unwrap(),
        );
        drop(guard.admit(tenant, 1, 2, SHORT, day(2)).await.unwrap());
    }

    #[tokio::test]
    async fn a_refused_question_does_not_use_up_the_day() {
        let guard = DeepGuard::default();
        let tenant = Uuid::new_v4();
        let held = guard.admit(tenant, 1, 2, SHORT, day(1)).await.unwrap();
        // Refused for lack of a slot: not counted.
        assert!(guard.admit(tenant, 1, 2, SHORT, day(1)).await.is_err());
        drop(held);
        drop(guard.admit(tenant, 1, 2, SHORT, day(1)).await.unwrap());
        assert!(guard.admit(tenant, 1, 2, SHORT, day(1)).await.is_err());
    }
}
