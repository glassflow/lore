//! The MCP server: lore's search, ask and document tools for agents, over streamable HTTP at
//! `/mcp`.
//!
//! The endpoint is stateless (no sessions) and answers with plain JSON. An axum middleware
//! authenticates every request with the same tenant keys as the REST API and puts the [`Caller`]
//! in the request extensions; rmcp copies the request `Parts` into the tool context, so each tool
//! reads the caller from there. Every tool is scoped to the caller's tenant.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use uuid::Uuid;

use crate::answer::{self, AskRequest, AskResponse, Citation, Mode};
use crate::auth::{Caller, Scope, caller_from_headers};
use crate::error::AppError;
use crate::http::AppState;
use crate::search::{Hit, SearchQuery, query_terms, search};
use crate::store::{DocumentRow, document_text, list_source_summaries};

/// What the server tells a client about itself. Agents read it, so it says what each tool costs:
/// left alone, an agent tends to call `ask`, which runs a second model, where `search` would do.
const INSTRUCTIONS: &str = "lore indexes code, documents and agent sessions. Start with `search`: it is fast, runs no language model, and returns the matching lines with their paths and line numbers. `get_document` reads one document in full. `ask` has lore's own model write an answer, which takes seconds to a minute: prefer `search` when you can read the code yourself. `list_sources` names what is indexed.";

/// Hits returned by `search` when `k` is not given.
const DEFAULT_SEARCH_K: usize = 8;

/// A `search` hit shows at most this many of its lines: those that best match the query. A hit
/// up to [`EXCERPT_SLACK`] lines longer is shown whole.
const EXCERPT_LINES: usize = 12;
const EXCERPT_SLACK: usize = 3;
/// A shown line is cut to this many characters (seed rows and minified files can be thousands).
const EXCERPT_LINE_CHARS: usize = 160;
/// A hit header lists at most this many symbols.
const HEADER_SYMBOLS: usize = 8;

/// The longest `get_document` text, in characters, header included.
pub const MAX_DOCUMENT_CHARS: usize = 100_000;

/// Builds the `/mcp` service. It is stateless, answers with plain JSON, and checks the `Host`
/// header only when `LORE_MCP_ALLOWED_HOSTS` is set (the bearer key is the real protection).
pub fn service(state: AppState) -> StreamableHttpService<LoreMcp, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    let config = match state.config.mcp_allowed_hosts.clone() {
        Some(hosts) => config.with_allowed_hosts(hosts),
        None => config.disable_allowed_hosts(),
    };
    StreamableHttpService::new(
        move || Ok(LoreMcp::new(state.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

/// Authenticates a request to `/mcp`: a missing or bad key is a 401 and a key without the read
/// scope is a 403, both with the standard error body, before rmcp sees the request.
pub async fn require_caller(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let caller = match caller_from_headers(&state, req.headers()).await {
        Ok(caller) => caller,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = caller.require(Scope::Read) {
        return e.into_response();
    }
    req.extensions_mut().insert(caller);
    next.run(req).await
}

#[derive(Clone)]
pub struct LoreMcp {
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl LoreMcp {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// What to look for: a name, a phrase or a question.
    query: String,
    /// How many passages to return, 1 to 50 (default 8). Other values are clamped.
    k: Option<i64>,
    /// Only these document kinds: `code`, `doc`, `session` or `note`.
    kind: Option<Vec<String>>,
    /// Only this source: a repo or document source name, as `list_sources` shows it.
    source: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AskMode {
    #[default]
    Fast,
    Sources,
    Deep,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskArgs {
    /// The question, in plain language.
    question: String,
    /// `fast` (default, about 5 s) answers from the best passages in one model call. `sources` (well under a second) lists those passages as bullets and calls no model. `deep` (10 to 60 s) lets lore's model search, grep and read files; it needs a tool-capable provider.
    mode: Option<AskMode>,
    /// Only this source: a repo or document source name, as `list_sources` shows it.
    source: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetDocumentArgs {
    /// The `document_id` of a search hit or an ask citation (a UUID).
    document_id: String,
}

fn ok(content: Vec<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(
        content.into_iter().map(ContentBlock::text).collect(),
    ))
}

/// A tool-level error: the caller sees the message and the call is marked as failed.
fn failed(message: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(message)]))
}

/// An application error as a tool error. Internal errors are logged and reported without detail.
fn app_failed(e: AppError) -> Result<CallToolResult, ErrorData> {
    if let AppError::Internal(inner) = &e {
        tracing::error!(error = ?inner, "mcp tool failed");
    }
    failed(e.message())
}

/// The caller the auth middleware stored on the request.
fn caller_of(parts: &Parts) -> Result<Caller, ErrorData> {
    parts
        .extensions
        .get::<Caller>()
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("request was not authenticated", None))
}

#[tool_router]
impl LoreMcp {
    #[tool(
        description = "Find code, documents and agent sessions by name and by meaning. Fast: well under a second, no language model. Returns the best hits as text: a header per hit (source, path:lines, kind, symbols, document id), then up to 12 numbered lines of it that best match the query. Often enough to answer; read the file, or call get_document, when you need more.",
        annotations(read_only_hint = true)
    )]
    async fn search(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let caller = caller_of(&parts)?;
        let k = args.k.map_or(DEFAULT_SEARCH_K, |k| {
            crate::search::clamp_k(usize::try_from(k).unwrap_or(0))
        });
        let terms = query_terms(&args.query);
        let query = SearchQuery {
            tenant_id: caller.tenant_id,
            q: args.query,
            k,
            kinds: args.kind.filter(|kinds| !kinds.is_empty()),
            source: args.source.filter(|s| !s.trim().is_empty()),
        };
        let hits = match search(&self.state.pool, self.state.embedder.clone(), query).await {
            Ok(hits) => hits,
            Err(e) => return app_failed(e),
        };
        ok(vec![render_hits(&hits, &terms)])
    }

    #[tool(
        description = "Have lore's own language model answer a question from the indexed material, with citations. Slow: about 5 s in fast mode and 10 to 60 s in deep mode, and deep questions over the tenant's limit wait for a free slot. If you can search and read the code yourself, `search` is faster and cheaper; use `ask` when you want a written answer instead. Returns the answer, its sources and a line with the mode, model and time.",
        annotations(read_only_hint = true)
    )]
    async fn ask(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<AskArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let caller = caller_of(&parts)?;
        let mode = match args.mode.unwrap_or_default() {
            AskMode::Fast => Mode::Fast,
            AskMode::Sources => Mode::Sources,
            AskMode::Deep => Mode::Deep,
        };
        let req = AskRequest {
            question: args.question,
            mode,
            k: None,
            kind: None,
            source: args.source.filter(|s| !s.trim().is_empty()),
        };
        match answer::ask(&self.state, caller.tenant_id, req).await {
            Ok(res) => ok(vec![ask_text(&res)]),
            Err(e) => app_failed(e),
        }
    }

    #[tool(
        description = "Read one document in full (up to 100,000 characters), by the document_id of a search hit or an ask citation.",
        annotations(read_only_hint = true)
    )]
    async fn get_document(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let caller = caller_of(&parts)?;
        let Ok(id) = Uuid::parse_str(args.document_id.trim()) else {
            return failed("document_id must be a UUID");
        };
        match document_text(&self.state.pool, caller.tenant_id, id).await {
            Some((row, text)) => ok(vec![render_document(&row, &text)]),
            None => failed("document not found"),
        }
    }

    #[tool(
        description = "List the indexed sources with their kind, the commit a repo is indexed at, and the number of documents each holds.",
        annotations(read_only_hint = true)
    )]
    async fn list_sources(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let caller = caller_of(&parts)?;
        let sources = match list_source_summaries(&self.state.pool, caller.tenant_id).await {
            Ok(sources) => sources,
            Err(e) => return app_failed(e.into()),
        };
        match serde_json::to_string(&sources) {
            Ok(json) => ok(vec![json]),
            Err(e) => app_failed(AppError::Internal(e.into())),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LoreMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("lore", crate::VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// The `search` result: the hits, a blank line between them, or `No hits.`.
fn render_hits(hits: &[Hit], terms: &[String]) -> String {
    if hits.is_empty() {
        return "No hits.".to_string();
    }
    let rendered: Vec<String> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| render_hit(i + 1, h, terms))
        .collect();
    rendered.join("\n\n")
}

/// `[1] sample src/billing/plans.rs:3-14 (code: Plan, Plan::limits) id=<uuid>`, then the excerpt.
fn render_hit(n: usize, h: &Hit, terms: &[String]) -> String {
    let place = match (&h.path, h.start_line, h.end_line) {
        (Some(path), Some(start), Some(end)) => format!("{path}:{start}-{end}"),
        (Some(path), _, _) => path.clone(),
        (None, _, _) => h.title.clone(),
    };
    let mut about = h.kind.clone();
    if !h.symbols.is_empty() {
        let mut symbols = h.symbols[..h.symbols.len().min(HEADER_SYMBOLS)].join(", ");
        if h.symbols.len() > HEADER_SYMBOLS {
            symbols.push_str(", …");
        }
        about = format!("{about}: {symbols}");
    }
    let mut out = format!("[{n}] {} {place} ({about}) id={}", h.source, h.document_id);
    for line in excerpt(&h.text, h.start_line, terms) {
        out.push('\n');
        out.push_str(&line);
    }
    out
}

/// The lines of a hit worth showing, numbered when the hit has line numbers. A short hit is shown
/// whole; a longer one by the window of [`EXCERPT_LINES`] lines with the most query words (the
/// first such window, so a hit that matches only by meaning shows its start). The window loses its
/// common indentation and each line is cut to [`EXCERPT_LINE_CHARS`] characters.
fn excerpt(text: &str, start_line: Option<i32>, terms: &[String]) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (from, len) = if lines.len() <= EXCERPT_LINES + EXCERPT_SLACK {
        (0, lines.len())
    } else {
        let matches: Vec<usize> = lines
            .iter()
            .map(|l| {
                let l = l.to_lowercase();
                terms.iter().filter(|t| l.contains(t.as_str())).count()
            })
            .collect();
        let mut best = (0, 0);
        let mut sum: usize = matches[..EXCERPT_LINES].iter().sum();
        best.1 = sum;
        for from in 1..=lines.len() - EXCERPT_LINES {
            sum = sum + matches[from + EXCERPT_LINES - 1] - matches[from - 1];
            if sum > best.1 {
                best = (from, sum);
            }
        }
        (best.0, EXCERPT_LINES)
    };
    let shown = &lines[from..from + len];
    let indent = shown
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    shown
        .iter()
        .enumerate()
        .map(|(i, line)| {
            // A blank line shorter than the indent, or one whose indent is wider characters, keeps
            // all of it.
            let body = line.get(indent..).unwrap_or(line);
            let body = cut(body.trim_end(), EXCERPT_LINE_CHARS);
            match start_line {
                Some(start) => format!("{}: {body}", i64::from(start) + (from + i) as i64),
                None => body,
            }
        })
        .collect()
}

/// `s` cut to `max` characters, the last of them `…` when it was longer.
fn cut(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some(_) => {
            let keep: String = s.chars().take(max - 1).collect();
            format!("{keep}…")
        }
        None => s.to_string(),
    }
}

/// The `ask` result: the answer and its sources, then `(fast mode, <model>, 4321 in / 381 out,
/// 4.9 s)`, with the number of tool calls for deep mode and no model for sources mode.
fn ask_text(res: &AskResponse) -> String {
    let secs = res.latency_ms as f64 / 1000.0;
    let tokens = res.usage.tokens_text();
    let footer = match res.mode {
        Mode::Fast => format!("(fast mode, {}, {tokens}, {secs:.1} s)", res.model),
        Mode::Sources => format!("(sources mode, no model, {secs:.1} s)"),
        Mode::Deep => format!(
            "(deep mode, {} tool calls, {}, {tokens}, {secs:.1} s)",
            res.tool_calls, res.model
        ),
    };
    format!("{}\n\n{footer}", answer_text(&res.answer, &res.citations))
}

/// The answer, a blank line, `Sources:` and one line per citation.
fn answer_text(answer: &str, citations: &[Citation]) -> String {
    let mut out = format!("{answer}\n\nSources:\n");
    if citations.is_empty() {
        out.push_str("(none)");
    } else {
        let lines: Vec<String> = citations.iter().map(citation_line).collect();
        out.push_str(&lines.join("\n"));
    }
    out
}

/// `[1] src/billing/plans.rs:3-10 (https://...)`: the path with its lines, else the title, and the
/// link when there is one.
fn citation_line(c: &Citation) -> String {
    let place = match (&c.path, c.start_line, c.end_line) {
        (Some(path), Some(start), Some(end)) => format!("{path}:{start}-{end}"),
        (Some(path), _, _) => path.clone(),
        (None, _, _) => c
            .title
            .clone()
            .or_else(|| c.document_id.map(|id| id.to_string()))
            .unwrap_or_default(),
    };
    match &c.url {
        Some(url) => format!("[{}] {place} ({url})", c.label),
        None => format!("[{}] {place}", c.label),
    }
}

/// `# <title>\n<kind> · <path or uri>\n\n` and the text, cut to [`MAX_DOCUMENT_CHARS`]
/// characters in all.
fn render_document(row: &DocumentRow, text: &str) -> String {
    let place = row.path.as_deref().or(row.uri.as_deref());
    let mut out = format!("# {}\n{}", row.title, row.kind);
    if let Some(place) = place {
        out.push_str(" · ");
        out.push_str(place);
    }
    out.push_str("\n\n");
    out.push_str(text);
    match out.char_indices().nth(MAX_DOCUMENT_CHARS) {
        Some((cut, _)) => {
            out.truncate(cut);
            out
        }
        None => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: Option<&str>, uri: Option<&str>) -> DocumentRow {
        DocumentRow {
            id: Uuid::nil(),
            source_id: Uuid::nil(),
            source: "sample".to_string(),
            kind: "code".to_string(),
            title: "plans.rs".to_string(),
            uri: uri.map(str::to_string),
            metadata: serde_json::json!({}),
            path: path.map(str::to_string),
        }
    }

    fn cite(label: &str) -> Citation {
        Citation {
            label: label.to_string(),
            document_id: None,
            kind: None,
            title: None,
            path: None,
            start_line: None,
            end_line: None,
            url: None,
        }
    }

    #[test]
    fn document_header_has_title_kind_and_path() {
        let s = render_document(
            &row(Some("src/billing/plans.rs"), Some("https://x/y")),
            "body",
        );
        assert_eq!(s, "# plans.rs\ncode · src/billing/plans.rs\n\nbody");
    }

    #[test]
    fn document_header_falls_back_to_the_uri_then_to_the_kind() {
        let s = render_document(&row(None, Some("https://x/y")), "body");
        assert_eq!(s, "# plans.rs\ncode · https://x/y\n\nbody");
        let s = render_document(&row(None, None), "body");
        assert_eq!(s, "# plans.rs\ncode\n\nbody");
    }

    #[test]
    fn document_is_cut_at_100000_characters_header_included() {
        let header = "# plans.rs\ncode · p\n\n";
        let r = row(Some("p"), None);
        let exact = "a".repeat(MAX_DOCUMENT_CHARS - header.chars().count());
        let s = render_document(&r, &exact);
        assert_eq!(s.chars().count(), MAX_DOCUMENT_CHARS);
        assert!(s.ends_with('a'));

        let s = render_document(&r, &format!("{exact}bcd"));
        assert_eq!(s.chars().count(), MAX_DOCUMENT_CHARS);
        assert!(!s.contains('b'));
    }

    #[test]
    fn document_cut_counts_characters_not_bytes() {
        let r = row(None, None);
        let s = render_document(&r, &"é".repeat(MAX_DOCUMENT_CHARS));
        assert_eq!(s.chars().count(), MAX_DOCUMENT_CHARS);
        assert!(s.len() > MAX_DOCUMENT_CHARS);
        assert!(s.ends_with('é'));
    }

    #[test]
    fn sources_list_one_line_per_citation() {
        let mut a = cite("1");
        a.path = Some("src/billing/plans.rs".to_string());
        a.start_line = Some(3);
        a.end_line = Some(10);
        a.url = Some("https://git/x".to_string());
        let mut b = cite("2");
        b.title = Some("Standup notes".to_string());
        let mut c = cite("3");
        c.path = Some("README.md".to_string());
        assert_eq!(
            answer_text("Three plans.", &[a, b, c]),
            "Three plans.\n\nSources:\n[1] src/billing/plans.rs:3-10 (https://git/x)\n[2] Standup notes\n[3] README.md"
        );
    }

    #[test]
    fn sources_without_citations_say_none() {
        assert_eq!(answer_text("No idea.", &[]), "No idea.\n\nSources:\n(none)");
    }

    fn response(mode: Mode) -> AskResponse {
        AskResponse {
            answer: "Three plans.".to_string(),
            mode,
            model: "m1".to_string(),
            citations: vec![],
            sources_considered: 0,
            tool_calls: 3,
            usage: Default::default(),
            latency_ms: 4_870,
        }
    }

    #[test]
    fn ask_text_ends_with_the_mode_model_and_time() {
        assert_eq!(
            ask_text(&response(Mode::Fast)),
            "Three plans.\n\nSources:\n(none)\n\n(fast mode, m1, 0 in / 0 out, 4.9 s)"
        );
        let mut deep = response(Mode::Deep);
        deep.usage.input_tokens = 4;
        deep.usage.cache_read_input_tokens = 9000;
        deep.usage.output_tokens = 381;
        assert!(ask_text(&deep).ends_with(
            "\n\n(deep mode, 3 tool calls, m1, 9004 in (9000 cached) / 381 out, 4.9 s)"
        ));
    }

    fn terms(q: &str) -> Vec<String> {
        query_terms(q)
    }

    #[test]
    fn query_terms_are_lowercased_words_of_3_or_more_without_stop_words() {
        assert_eq!(
            terms("How many billing plans are there? plan_key, Plans"),
            ["billing", "plans", "plan_key"]
        );
        assert!(terms("is it ok").is_empty());
    }

    #[test]
    fn a_short_hit_is_shown_whole_and_numbered_from_its_first_line() {
        let text = "fn a() {\n    one\n}";
        assert_eq!(
            excerpt(text, Some(7), &terms("zzz")),
            ["7: fn a() {", "8:     one", "9: }"]
        );
        assert_eq!(excerpt(text, None, &[]), ["fn a() {", "    one", "}"]);
        let fifteen: String = (1..=15).map(|i| format!("l{i}\n")).collect();
        assert_eq!(excerpt(&fifteen, Some(1), &[]).len(), 15);
    }

    #[test]
    fn a_long_hit_shows_the_12_lines_with_the_most_query_words() {
        let mut lines: Vec<String> = (1..=40).map(|i| format!("filler {i}")).collect();
        lines[24] = "the Billing plan".to_string();
        lines[27] = "billing again".to_string();
        lines[30] = "plan".to_string();
        let shown = excerpt(&lines.join("\n"), Some(101), &terms("billing plan"));
        assert_eq!(shown.len(), 12);
        // Lines 25 to 31 hold the matches; the first window holding all three starts at line 20.
        assert_eq!(shown[0], "120: filler 20");
        assert_eq!(shown[11], "131: plan");
    }

    #[test]
    fn a_long_hit_without_matches_shows_its_start() {
        let text: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let shown = excerpt(&text, Some(1), &terms("nothing here"));
        assert_eq!(shown.first().unwrap(), "1: line 1");
        assert_eq!(shown.last().unwrap(), "12: line 12");
    }

    #[test]
    fn excerpts_lose_common_indentation_and_long_lines_are_cut() {
        let text = format!("        a\n\n            b\n        {}", "x".repeat(300));
        let shown = excerpt(&text, None, &[]);
        assert_eq!(shown[0], "a");
        assert_eq!(shown[1], "");
        assert_eq!(shown[2], "    b");
        assert_eq!(shown[3].chars().count(), EXCERPT_LINE_CHARS);
        assert!(shown[3].ends_with('…'));
        // Indentation in wider characters is kept rather than cut through.
        assert_eq!(excerpt("\u{3000}a\n  b", None, &[]), ["\u{3000}a", "b"]);
    }

    fn hit(path: Option<&str>, symbols: &[&str]) -> Hit {
        Hit {
            chunk_id: 1,
            document_id: Uuid::nil(),
            source: "sample".to_string(),
            kind: "code".to_string(),
            title: "Standup notes".to_string(),
            uri: None,
            path: path.map(str::to_string),
            external_id: None,
            start_line: path.map(|_| 3),
            end_line: path.map(|_| 4),
            symbols: symbols.iter().map(|s| s.to_string()).collect(),
            text: "pub enum Plan {\n    Free }".to_string(),
            score: 0.03,
        }
    }

    #[test]
    fn a_hit_header_has_source_place_kind_symbols_and_id() {
        let id = Uuid::nil();
        assert_eq!(
            render_hit(1, &hit(Some("src/plans.rs"), &["Plan"]), &[]),
            format!(
                "[1] sample src/plans.rs:3-4 (code: Plan) id={id}\n3: pub enum Plan {{\n4:     Free }}"
            )
        );
        let many: Vec<String> = (1..=10).map(|i| format!("s{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let header = render_hit(2, &hit(None, &many), &[]);
        assert!(
            header.starts_with(&format!(
                "[2] sample Standup notes (code: s1, s2, s3, s4, s5, s6, s7, s8, …) id={id}\npub enum"
            )),
            "{header}"
        );
    }

    #[test]
    fn no_hits_say_so() {
        assert_eq!(render_hits(&[], &[]), "No hits.");
        let two = render_hits(&[hit(None, &[]), hit(None, &[])], &[]);
        assert_eq!(two.matches("\n\n[2] ").count(), 1, "{two}");
    }
}
