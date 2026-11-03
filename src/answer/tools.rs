//! The tools deep mode gives the model: `search`, `grep`, `read_file` and `get_session`.
//!
//! Each tool is a plain function of the tenant and a JSON input. All of them are tenant-scoped
//! (repo sources and sessions are looked up for the caller's tenant only), size-capped, and
//! report problems as a [`ToolOutput`] with `is_error: true` so the model can correct itself.
//! Nothing here panics on bad input.
//!
//! `grep` and `read_file` work on the repo checkouts at `<data_dir>/repos/<source_id>/checkout`.
//! The checkouts are made with `core.symlinks=false`, and both tools still refuse to follow a
//! symlink out of the checkout.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use globset::Glob;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::AppError;
use crate::http::AppState;
use crate::llm::ToolDef;
use crate::search::{Hit, SearchQuery, search};
use crate::store::{self, Source};

/// The most characters of content a tool returns. A longer output is cut to this many characters
/// and then [`TRUNCATED`] is appended, so a cut output is `MAX_OUTPUT_CHARS` plus the length of
/// the suffix characters long. The size is deterministic.
pub const MAX_OUTPUT_CHARS: usize = 20_000;
const TRUNCATED: &str = "\n…[output truncated]";

const SEARCH_DEFAULT_K: i64 = 6;
const SEARCH_MAX_K: i64 = 10;
/// Lines of a hit's text shown under its header.
const HIT_LINES: usize = 30;

const GREP_DEFAULT_MAX: i64 = 50;
const GREP_MAX_MAX: i64 = 200;
/// Files over this size are not searched.
const GREP_MAX_FILE_BYTES: u64 = 256 * 1024;
/// A file with a NUL byte in this many leading bytes is binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// A matching line is cut to this many characters.
const GREP_LINE_CHARS: usize = 200;

const READ_MAX_LINES: i64 = 400;
const READ_DEFAULT_LINES: i64 = 200;
/// Files over this size are refused by `read_file` (it scans the whole file to count lines).
const READ_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// What a tool returns to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(text: String) -> Self {
        Self {
            text: cap(text),
            is_error: false,
        }
    }

    fn err(message: impl Into<String>) -> Self {
        Self {
            text: cap(message.into()),
            is_error: true,
        }
    }
}

/// Cuts `text` to [`MAX_OUTPUT_CHARS`] characters (on a character boundary) and appends
/// [`TRUNCATED`] when it was longer.
fn cap(text: String) -> String {
    match text.char_indices().nth(MAX_OUTPUT_CHARS) {
        Some((cut, _)) => {
            let mut text = text;
            text.truncate(cut);
            text.push_str(TRUNCATED);
            text
        }
        None => text,
    }
}

/// The first `max` characters of `s`.
fn cut_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The four tools, with JSON schemas (`additionalProperties: false`).
pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "search".to_string(),
            description: "Hybrid keyword and semantic search over the indexed repos, docs and \
                          sessions. Returns the best chunks, each with a header and its first \
                          lines. Use it to find where something lives."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to look for."
                    },
                    "k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": SEARCH_MAX_K,
                        "description": "How many hits to return (default 6)."
                    },
                    "kind": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["code", "doc", "session", "note"] },
                        "description": "Only these document kinds."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "grep".to_string(),
            description: "Regex search over the files of the repo checkouts, line by line. \
                          Use it for exact names and for counting or listing every occurrence. \
                          Output is `<source>/<path>:<line>: <text>`, one match per line."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "A regular expression (Rust regex syntax)."
                    },
                    "source": {
                        "type": "string",
                        "description": "Only this repo source. Default: every repo source."
                    },
                    "path_glob": {
                        "type": "string",
                        "description": "Only files whose path relative to the repo root \
                                        matches this glob, for example `src/**/*.rs` or `docs/**`."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": GREP_MAX_MAX,
                        "description": "Stop after this many matches (default 50)."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "read_file".to_string(),
            description: "Reads lines of a file in a repo checkout. At most 400 lines per call; \
                          lines are numbered `N: text`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "The repo source name."
                    },
                    "path": {
                        "type": "string",
                        "description": "The file path relative to the repo root."
                    },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "First line to read (default 1)."
                    },
                    "end_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Last line to read (default start_line + 199, at most \
                                        400 lines after start_line)."
                    }
                },
                "required": ["source", "path"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "get_session".to_string(),
            description: "Returns the full text of an indexed agent session by its session id."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The session id, as shown in `session:<id>` search hits."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Runs one tool for `tenant_id`, over all of its sources. An unknown tool, invalid input or a
/// failure is a `ToolOutput` with `is_error: true`; the text of a success or an error is cut by
/// [`cap`].
pub async fn run_tool(state: &AppState, tenant_id: Uuid, name: &str, input: &Value) -> ToolOutput {
    run_tool_scoped(state, tenant_id, None, name, input).await
}

/// [`run_tool`] that sees only the sources named in `sources` (all of them when `None`). A source
/// outside them is an unknown source to grep and read_file, search leaves it out, and get_session
/// does not find its sessions.
pub async fn run_tool_scoped(
    state: &AppState,
    tenant_id: Uuid,
    sources: Option<&[String]>,
    name: &str,
    input: &Value,
) -> ToolOutput {
    match name {
        "search" => run_search(state, tenant_id, sources, input).await,
        "grep" => run_grep(state, tenant_id, sources, input).await,
        "read_file" => run_read_file(state, tenant_id, sources, input).await,
        "get_session" => run_get_session(state, tenant_id, sources, input).await,
        other => ToolOutput::err(format!("unknown tool: {}", cut_chars(other, 64))),
    }
}

/// Whether the source called `name` is one of `sources`; every source is when `sources` is `None`.
fn in_scope(sources: Option<&[String]>, name: &str) -> bool {
    sources.is_none_or(|names| names.iter().any(|n| n == name))
}

/// Parses the tool input. Wrong types, missing fields and unknown fields (the structs deny them)
/// are an error message.
fn parse_input<T: DeserializeOwned>(input: &Value) -> Result<T, String> {
    serde_json::from_value(input.clone()).map_err(|e| format!("invalid input: {e}"))
}

/// `value` when it is within `min..=max`, else an error naming the field.
fn in_range(field: &str, value: i64, min: i64, max: i64) -> Result<i64, String> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "invalid input: {field} must be between {min} and {max}"
        ))
    }
}

/// The tenant's repo sources within `scope`, ordered by name.
async fn repo_sources(
    state: &AppState,
    tenant_id: Uuid,
    scope: Option<&[String]>,
) -> Result<Vec<Source>, ToolOutput> {
    let mut sources = store::list_sources(&state.pool, tenant_id, Some("repo"))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "listing repo sources for a tool failed");
            ToolOutput::err("internal error")
        })?;
    sources.retain(|s| in_scope(scope, &s.name));
    Ok(sources)
}

/// `<data_dir>/repos/<source_id>/checkout`, the layout `repo` keeps.
fn checkout_path(state: &AppState, source_id: Uuid) -> PathBuf {
    let data_dir = std::path::absolute(&state.config.data_dir)
        .unwrap_or_else(|_| state.config.data_dir.clone());
    data_dir
        .join("repos")
        .join(source_id.to_string())
        .join("checkout")
}

// ---------------------------------------------------------------------------------------------
// search

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
    k: Option<i64>,
    kind: Option<Vec<String>>,
}

async fn run_search(
    state: &AppState,
    tenant_id: Uuid,
    scope: Option<&[String]>,
    input: &Value,
) -> ToolOutput {
    let input: SearchInput = match parse_input(input) {
        Ok(i) => i,
        Err(e) => return ToolOutput::err(e),
    };
    let k = match in_range("k", input.k.unwrap_or(SEARCH_DEFAULT_K), 1, SEARCH_MAX_K) {
        Ok(k) => k,
        Err(e) => return ToolOutput::err(e),
    };
    let hits = search(
        &state.pool,
        state.embedder.clone(),
        SearchQuery {
            tenant_id,
            q: input.query,
            k: usize::try_from(k).unwrap_or(1),
            kinds: input.kind,
            sources: scope.map(<[String]>::to_vec),
        },
    )
    .await;
    match hits {
        Ok(hits) if hits.is_empty() => ToolOutput::ok("(no results)".to_string()),
        Ok(hits) => ToolOutput::ok(hits.iter().map(render_hit).collect::<Vec<_>>().join("\n\n")),
        Err(AppError::BadRequest(message)) => ToolOutput::err(message),
        Err(e) => {
            tracing::error!(error = %e, "the search tool failed");
            ToolOutput::err("search failed")
        }
    }
}

/// A header line and the first [`HIT_LINES`] lines of the hit's text.
///
/// - a hit with a path (repo code and docs): `<source>/<path>:<start>-<end> (<kind>) <symbols>`;
/// - a session: `session:<external_id> (<title>)`;
/// - any other hit: `doc:<document_id> (<title>)`.
fn render_hit(hit: &Hit) -> String {
    let mut out = match (&hit.path, &hit.external_id) {
        (Some(path), _) => {
            let mut header = format!("{}/{}", hit.source, path);
            if let (Some(start), Some(end)) = (hit.start_line, hit.end_line) {
                let _ = write!(header, ":{start}-{end}");
            }
            let _ = write!(header, " ({})", hit.kind);
            if !hit.symbols.is_empty() {
                let _ = write!(header, " {}", hit.symbols.join(", "));
            }
            header
        }
        (None, Some(external_id)) if hit.kind == "session" => {
            format!("session:{external_id} ({})", hit.title)
        }
        (None, _) => format!("doc:{} ({})", hit.document_id, hit.title),
    };
    for line in hit.text.lines().take(HIT_LINES) {
        out.push('\n');
        out.push_str(line);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// grep

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    source: Option<String>,
    path_glob: Option<String>,
    max_results: Option<i64>,
}

async fn run_grep(
    state: &AppState,
    tenant_id: Uuid,
    scope: Option<&[String]>,
    input: &Value,
) -> ToolOutput {
    let input: GrepInput = match parse_input(input) {
        Ok(i) => i,
        Err(e) => return ToolOutput::err(e),
    };
    let max = match in_range(
        "max_results",
        input.max_results.unwrap_or(GREP_DEFAULT_MAX),
        1,
        GREP_MAX_MAX,
    ) {
        Ok(m) => usize::try_from(m).unwrap_or(1),
        Err(e) => return ToolOutput::err(e),
    };
    let regex = match RegexBuilder::new(&input.pattern)
        .size_limit(1 << 20)
        .build()
    {
        Ok(r) => r,
        Err(e) => return ToolOutput::err(format!("invalid regex: {e}")),
    };
    let glob = match input.path_glob.as_deref() {
        Some(g) => match Glob::new(g) {
            Ok(g) => Some(g.compile_matcher()),
            Err(e) => return ToolOutput::err(format!("invalid path_glob: {e}")),
        },
        None => None,
    };

    let mut sources = match repo_sources(state, tenant_id, scope).await {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Some(name) = input.source.as_deref() {
        sources.retain(|s| s.name == name);
        if sources.is_empty() {
            return ToolOutput::err("unknown source");
        }
    }
    let roots: Vec<(String, PathBuf)> = sources
        .into_iter()
        .map(|s| {
            let root = checkout_path(state, s.id);
            (s.name, root)
        })
        .collect();

    match tokio::task::spawn_blocking(move || grep_roots(&roots, &regex, glob.as_ref(), max)).await
    {
        Ok(text) => ToolOutput::ok(text),
        Err(e) => {
            tracing::error!(error = %e, "the grep task failed");
            ToolOutput::err("grep failed")
        }
    }
}

/// Greps each `(source name, checkout)` in order and renders the output. A checkout that does
/// not exist (a source that has not been pushed yet) contributes nothing.
///
/// Per checkout: hidden files are included and `.git` is not; `.gitignore` files are NOT honored
/// (every standard filter is off), so ignored files are searched; symlinks are never followed or
/// read; files over 256 KiB and files with a NUL in their first 8 KiB are skipped; entries in
/// each directory are visited in file name order, so the output is deterministic. The glob is
/// matched against the path relative to the checkout, with `/` separators.
///
/// Collects up to `max + 1` matches: the extra one tells "exactly `max` matches" from "stopped".
fn grep_roots(
    roots: &[(String, PathBuf)],
    regex: &regex::Regex,
    glob: Option<&globset::GlobMatcher>,
    max: usize,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut more = false;
    'roots: for (name, root) in roots {
        if !root.is_dir() {
            continue;
        }
        let walker = WalkBuilder::new(root)
            .standard_filters(false)
            .hidden(false)
            .follow_links(false)
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(|e| !(e.depth() > 0 && e.file_name() == ".git"))
            .build();
        for entry in walker {
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 {
                continue;
            }
            let path = entry.path();
            // `symlink_metadata` never follows: a symlink is not a regular file here.
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                continue;
            };
            if !meta.file_type().is_file() || meta.len() > GREP_MAX_FILE_BYTES {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative_text(relative);
            if glob.is_some_and(|g| !g.is_match(&relative)) {
                continue;
            }
            let Some(content) = read_text_file(path) else {
                continue;
            };
            for (i, line) in content.lines().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                if lines.len() == max {
                    more = true;
                    break 'roots;
                }
                lines.push(format!(
                    "{name}/{relative}:{}: {}",
                    i + 1,
                    cut_chars(line, GREP_LINE_CHARS)
                ));
            }
        }
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    if more {
        let _ = write!(out, "(stopped at {max} matches)");
    } else {
        let _ = write!(out, "({} matches)", lines.len());
    }
    out
}

/// The path components joined with `/`.
fn relative_text(relative: &Path) -> String {
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The file as text (invalid UTF-8 is replaced), or `None` when it cannot be read, grew past
/// the size limit, or looks binary.
fn read_text_file(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(GREP_MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > GREP_MAX_FILE_BYTES {
        return None;
    }
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------------------------
// read_file

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileInput {
    source: String,
    path: String,
    start_line: Option<i64>,
    end_line: Option<i64>,
}

async fn run_read_file(
    state: &AppState,
    tenant_id: Uuid,
    scope: Option<&[String]>,
    input: &Value,
) -> ToolOutput {
    let input: ReadFileInput = match parse_input(input) {
        Ok(i) => i,
        Err(e) => return ToolOutput::err(e),
    };
    let start = match in_range("start_line", input.start_line.unwrap_or(1), 1, i64::MAX) {
        Ok(s) => s,
        Err(e) => return ToolOutput::err(e),
    };
    let end = match input.end_line {
        Some(e) if e >= start => e,
        Some(_) => {
            return ToolOutput::err("invalid input: end_line must be at least start_line");
        }
        None => start.saturating_add(READ_DEFAULT_LINES - 1),
    };
    // At most READ_MAX_LINES lines per call: a longer range is cut, and the header says so.
    let end = end.min(start.saturating_add(READ_MAX_LINES - 1));

    let sources = match repo_sources(state, tenant_id, scope).await {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(source) = sources.into_iter().find(|s| s.name == input.source) else {
        return ToolOutput::err("unknown source");
    };
    let checkout = checkout_path(state, source.id);
    let label = format!("{}/{}", source.name, input.path);
    let path = input.path;
    // Both the bounds are positive here.
    let (start, end) = (
        usize::try_from(start).unwrap_or(usize::MAX),
        usize::try_from(end).unwrap_or(usize::MAX),
    );
    match tokio::task::spawn_blocking(move || read_lines(&checkout, &path, &label, start, end))
        .await
    {
        Ok(Ok(text)) => ToolOutput::ok(text),
        Ok(Err(message)) => ToolOutput::err(message),
        Err(e) => {
            tracing::error!(error = %e, "the read_file task failed");
            ToolOutput::err("read_file failed")
        }
    }
}

/// Reads lines `start..=end` (1-based, already clamped to at most 400) of `rel` inside
/// `checkout`. `rel` must be relative with no `..`; after canonicalization the file must still be
/// inside the canonical checkout, which also rejects a symlink that points out of it.
fn read_lines(
    checkout: &Path,
    rel: &str,
    label: &str,
    start: usize,
    end: usize,
) -> Result<String, String> {
    let rel_path = Path::new(rel);
    if rel.is_empty()
        || rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err("path must be relative to the repo root and must not contain `..`".to_string());
    }
    let root = std::fs::canonicalize(checkout).map_err(|_| "checkout not available".to_string())?;
    let file =
        std::fs::canonicalize(root.join(rel_path)).map_err(|_| "file not found".to_string())?;
    if !file.starts_with(&root) {
        return Err("path is outside the checkout".to_string());
    }
    let mut handle = File::open(&file).map_err(|_| "file not found".to_string())?;
    let meta = handle
        .metadata()
        .map_err(|_| "file not found".to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if meta.len() > READ_MAX_FILE_BYTES {
        return Err("file too large".to_string());
    }
    let mut sniff = Vec::new();
    (&mut handle)
        .take(BINARY_SNIFF_BYTES as u64)
        .read_to_end(&mut sniff)
        .map_err(|_| "could not read the file".to_string())?;
    if sniff.contains(&0) {
        return Err("binary file".to_string());
    }
    handle
        .seek(SeekFrom::Start(0))
        .map_err(|_| "could not read the file".to_string())?;

    let mut reader = BufReader::new(handle);
    let mut buf = Vec::new();
    let mut total = 0usize;
    let mut picked: Vec<String> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return Err("could not read the file".to_string()),
        }
        total += 1;
        if total >= start && total <= end {
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            picked.push(format!("{total}: {}", String::from_utf8_lossy(&buf)));
        }
    }
    if total == 0 {
        return Ok(format!("{label} (lines 0-0 of 0)"));
    }
    if start > total {
        return Err(format!(
            "start_line {start} is past the end of the file ({total} lines)"
        ));
    }
    let last = end.min(total);
    Ok(format!(
        "{label} (lines {start}-{last} of {total})\n{}",
        picked.join("\n")
    ))
}

// ---------------------------------------------------------------------------------------------
// get_session

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetSessionInput {
    session_id: String,
}

async fn run_get_session(
    state: &AppState,
    tenant_id: Uuid,
    scope: Option<&[String]>,
    input: &Value,
) -> ToolOutput {
    let input: GetSessionInput = match parse_input(input) {
        Ok(i) => i,
        Err(e) => return ToolOutput::err(e),
    };
    let not_found = || ToolOutput::err("session not found");
    // Postgres text cannot hold a NUL.
    if input.session_id.contains('\0') {
        return not_found();
    }
    let id = match store::find_session_document(&state.pool, tenant_id, &input.session_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return not_found(),
        Err(e) => {
            tracing::error!(error = %e, "looking up a session failed");
            return ToolOutput::err("internal error");
        }
    };
    match store::document_text(&state.pool, tenant_id, id).await {
        Some((row, text)) if in_scope(scope, &row.source) => ToolOutput::ok(text),
        _ => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: Option<&str>, kind: &str, external_id: Option<&str>) -> Hit {
        Hit {
            chunk_id: 1,
            document_id: Uuid::nil(),
            source: "sample".to_string(),
            kind: kind.to_string(),
            title: "A title".to_string(),
            uri: None,
            path: path.map(str::to_string),
            external_id: external_id.map(str::to_string),
            start_line: Some(3),
            end_line: Some(9),
            symbols: vec!["Plan".to_string(), "limits".to_string()],
            text: (1..=40)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            score: 0.0,
        }
    }

    #[test]
    fn cap_leaves_short_text_and_cuts_long_text_on_a_char_boundary() {
        assert_eq!(cap("short".to_string()), "short");
        let exact = "a".repeat(MAX_OUTPUT_CHARS);
        assert_eq!(cap(exact.clone()), exact);

        let long = "é".repeat(MAX_OUTPUT_CHARS + 5);
        let cut = cap(long);
        assert!(cut.ends_with("\n…[output truncated]"));
        assert_eq!(
            cut.chars().count(),
            MAX_OUTPUT_CHARS + TRUNCATED.chars().count()
        );
        assert!(cut.starts_with(&"é".repeat(MAX_OUTPUT_CHARS)));
    }

    #[test]
    fn tool_defs_are_the_four_tools_without_extra_properties() {
        let defs = tool_defs();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["search", "grep", "read_file", "get_session"]);
        for def in &defs {
            assert_eq!(def.input_schema["type"], "object", "{}", def.name);
            assert_eq!(
                def.input_schema["additionalProperties"],
                Value::Bool(false),
                "{}",
                def.name
            );
        }
    }

    #[test]
    fn hits_render_by_what_they_are() {
        let code = render_hit(&hit(Some("src/a.rs"), "code", None));
        let mut lines = code.lines();
        assert_eq!(
            lines.next(),
            Some("sample/src/a.rs:3-9 (code) Plan, limits")
        );
        assert_eq!(lines.next(), Some("line 1"));
        assert_eq!(code.lines().count(), 1 + HIT_LINES);

        let session = render_hit(&hit(None, "session", Some("sess-1")));
        assert!(session.starts_with("session:sess-1 (A title)\nline 1"));

        let note = render_hit(&hit(None, "note", None));
        assert!(note.starts_with(&format!("doc:{} (A title)\n", Uuid::nil())));
    }

    #[test]
    fn a_path_hit_without_symbols_has_no_trailing_space() {
        let mut h = hit(Some("README.md"), "doc", None);
        h.symbols.clear();
        h.text = String::new();
        assert_eq!(render_hit(&h), "sample/README.md:3-9 (doc)");
    }

    #[test]
    fn input_numbers_are_range_checked() {
        assert_eq!(in_range("k", 1, 1, 10), Ok(1));
        assert_eq!(in_range("k", 10, 1, 10), Ok(10));
        assert!(in_range("k", 0, 1, 10).is_err());
        assert!(in_range("k", 11, 1, 10).unwrap_err().contains("k must be"));
    }

    fn checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("src/b.rs"), "fn needle() {}\nfn other() {}\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "// needle one\n// needle two\n").unwrap();
        std::fs::write(root.join(".git/config"), "needle in git\n").unwrap();
        std::fs::write(root.join(".hidden/x.txt"), "needle hidden\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "needle ignored\n").unwrap();
        dir
    }

    fn run_grep_on(root: &Path, pattern: &str, glob: Option<&str>, max: usize) -> String {
        let regex = RegexBuilder::new(pattern).build().unwrap();
        let matcher = glob.map(|g| Glob::new(g).unwrap().compile_matcher());
        grep_roots(
            &[("repo".to_string(), root.to_path_buf())],
            &regex,
            matcher.as_ref(),
            max,
        )
    }

    #[test]
    fn grep_walks_in_name_order_includes_hidden_and_skips_dot_git() {
        let dir = checkout();
        let out = run_grep_on(dir.path(), "needle", None, 50);
        assert_eq!(
            out,
            "repo/.hidden/x.txt:1: needle hidden\n\
             repo/ignored.txt:1: needle ignored\n\
             repo/src/a.rs:1: // needle one\n\
             repo/src/a.rs:2: // needle two\n\
             repo/src/b.rs:1: fn needle() {}\n\
             (5 matches)"
        );
    }

    #[test]
    fn grep_stops_only_when_there_is_one_more_match() {
        let dir = checkout();
        assert!(run_grep_on(dir.path(), "needle", None, 5).ends_with("(5 matches)"));
        let out = run_grep_on(dir.path(), "needle", None, 4);
        assert!(out.ends_with("(stopped at 4 matches)"), "{out}");
        assert_eq!(out.lines().count(), 5);
    }

    #[test]
    fn grep_filters_by_glob_and_reports_no_match() {
        let dir = checkout();
        let out = run_grep_on(dir.path(), "needle", Some("src/**"), 50);
        assert!(
            out.lines()
                .all(|l| l.starts_with("repo/src/") || l.starts_with('('))
        );
        assert_eq!(run_grep_on(dir.path(), "absent", None, 50), "(0 matches)");
    }

    #[test]
    fn grep_cuts_long_lines_and_skips_binary_and_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("long.txt"),
            format!("needle {}\n", "x".repeat(500)),
        )
        .unwrap();
        let mut binary = b"needle\n".to_vec();
        binary.push(0);
        std::fs::write(root.join("bin.dat"), binary).unwrap();
        let big = format!("needle\n{}", "y".repeat(GREP_MAX_FILE_BYTES as usize));
        std::fs::write(root.join("big.txt"), big).unwrap();

        let out = run_grep_on(root, "needle", None, 50);
        let first = out.lines().next().unwrap();
        assert_eq!(
            first.len(),
            "repo/long.txt:1: ".len() + GREP_LINE_CHARS,
            "{first}"
        );
        assert!(out.ends_with("(1 matches)"), "{out}");
    }

    #[test]
    fn read_lines_checks_the_path_and_numbers_the_lines() {
        let dir = checkout();
        let root = dir.path();
        let out = read_lines(root, "src/a.rs", "repo/src/a.rs", 1, 200).unwrap();
        assert_eq!(
            out,
            "repo/src/a.rs (lines 1-2 of 2)\n1: // needle one\n2: // needle two"
        );
        let out = read_lines(root, "src/a.rs", "repo/src/a.rs", 2, 2).unwrap();
        assert_eq!(out, "repo/src/a.rs (lines 2-2 of 2)\n2: // needle two");

        assert!(read_lines(root, "../x", "l", 1, 2).is_err());
        assert!(read_lines(root, "src/../../x", "l", 1, 2).is_err());
        assert!(read_lines(root, "/etc/passwd", "l", 1, 2).is_err());
        assert!(read_lines(root, "", "l", 1, 2).is_err());
        assert_eq!(
            read_lines(root, "nope.rs", "l", 1, 2).unwrap_err(),
            "file not found"
        );
        assert!(
            read_lines(root, "src/a.rs", "l", 3, 4)
                .unwrap_err()
                .contains("past the end")
        );
        assert_eq!(
            read_lines(root, "src", "l", 1, 2).unwrap_err(),
            "not a regular file"
        );
    }

    #[test]
    fn read_lines_handles_empty_and_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        std::fs::write(dir.path().join("bin.dat"), [1u8, 0, 2]).unwrap();
        assert_eq!(
            read_lines(dir.path(), "empty.txt", "r/empty.txt", 1, 5).unwrap(),
            "r/empty.txt (lines 0-0 of 0)"
        );
        assert_eq!(
            read_lines(dir.path(), "bin.dat", "l", 1, 5).unwrap_err(),
            "binary file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_checkout_are_neither_searched_nor_read() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle secret\n").unwrap();
        let dir = checkout();
        let root = dir.path();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("link.txt"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("linkdir")).unwrap();

        let out = run_grep_on(root, "needle", None, 50);
        assert!(!out.contains("secret"), "{out}");
        assert!(!out.contains("link"), "{out}");
        assert_eq!(
            read_lines(root, "link.txt", "l", 1, 5).unwrap_err(),
            "path is outside the checkout"
        );
        assert_eq!(
            read_lines(root, "linkdir/secret.txt", "l", 1, 5).unwrap_err(),
            "path is outside the checkout"
        );
    }
}
