//! Hybrid search: Postgres full-text ranking (good at exact names) fused with pgvector similarity
//! (good at meaning) by Reciprocal Rank Fusion. A third list lifts the first few enumerations among
//! the vector hits (enums, constant lists, seed rows): they answer "which X are there" but are
//! easily outranked by the many chunks that only use X. The HTTP route, the CLI, fast answers and
//! the MCP tools all call [`search`].
//!
//! Queries are runtime-checked and bind every value. The one built string is the tsquery text of
//! the lexical branch, which is itself passed as a bind value.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use pgvector::Vector;
use sqlx::PgPool;
use uuid::Uuid;

use crate::embed::{Embedder, embed_query_async};
use crate::error::AppError;
use crate::repo::is_full_sha;

/// Hits returned when the caller does not say.
pub const DEFAULT_K: usize = 10;
/// The most hits one search returns.
pub const MAX_K: usize = 50;
/// The longest accepted query, in characters.
pub const MAX_QUERY_CHARS: usize = 2000;
/// Document kinds (`documents.kind`), as the migration lists them.
/// A hit carries at most this many bytes of `metadata.path`. The documents API refuses a longer
/// path, and a document stored before that limit has its path cut here.
pub const MAX_PATH_BYTES: usize = 2048;

pub const DOCUMENT_KINDS: [&str; 4] = ["code", "doc", "session", "note"];

/// A snippet is the first lines of a chunk.
const SNIPPET_LINES: usize = 40;

/// Rows taken from each branch before fusion.
const BRANCH_LIMIT: i64 = 50;
/// How many of the vector hits that are enumerations get a place in the enumeration list.
const ENUMERATION_LIMIT: usize = 3;
/// The `k` constant of Reciprocal Rank Fusion.
const RRF_K: f64 = 60.0;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub tenant_id: Uuid,
    pub q: String,
    /// Clamped to 1..=50.
    pub k: usize,
    /// Document kinds to keep (`code`, `doc`, `session`, `note`); `None` keeps all.
    pub kinds: Option<Vec<String>>,
    /// A source name; `None` searches every source of the tenant.
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub chunk_id: i64,
    pub document_id: Uuid,
    /// The source name.
    pub source: String,
    /// The document kind.
    pub kind: String,
    pub title: String,
    /// A rendered web link for repo files whose source has a template, else `documents.uri`.
    pub uri: Option<String>,
    /// `documents.metadata->>'path'` cut to `MAX_PATH_BYTES`: repo files and documents that set it.
    pub path: Option<String>,
    /// The session id for `session` hits, `None` for every other kind.
    pub external_id: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub symbols: Vec<String>,
    pub text: String,
    /// The Reciprocal Rank Fusion score: higher is better.
    pub score: f64,
}

/// The columns of a hit, plus what `uri` is rendered from.
#[derive(sqlx::FromRow)]
struct HitRow {
    id: i64,
    document_id: Uuid,
    source: String,
    kind: String,
    title: String,
    doc_uri: Option<String>,
    path: Option<String>,
    external_id: Option<String>,
    commit: Option<String>,
    template: Option<String>,
    start_line: Option<i32>,
    end_line: Option<i32>,
    symbols: Vec<String>,
    text: String,
}

impl HitRow {
    fn into_hit(self, score: f64) -> Hit {
        let path = self.path.map(|p| cut_bytes(p, MAX_PATH_BYTES));
        let rendered = match (
            self.template.as_deref(),
            path.as_deref(),
            self.commit.as_deref(),
            self.start_line,
            self.end_line,
        ) {
            (Some(template), Some(path), Some(commit), Some(start), Some(end))
                if is_full_sha(commit) =>
            {
                Some(render_web_url(template, path, commit, start, end))
            }
            _ => None,
        };
        Hit {
            chunk_id: self.id,
            document_id: self.document_id,
            source: self.source,
            kind: self.kind,
            title: self.title,
            uri: rendered.or(self.doc_uri),
            path,
            external_id: self.external_id,
            start_line: self.start_line,
            end_line: self.end_line,
            symbols: self.symbols,
            text: self.text,
            score,
        }
    }
}

/// `s` cut to at most `max` bytes, on a char boundary.
fn cut_bytes(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// The first 40 lines of `text`, joined by `\n`, for the HTTP route. The MCP `search` tool shows
/// an excerpt instead (`mcp::excerpt`).
pub fn snippet(text: &str) -> String {
    text.lines()
        .take(SNIPPET_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Hybrid search over the tenant's chunks. Returns at most `k` hits, best first. Another
/// tenant's chunks are never visible. A blank or oversized `q` and an unknown kind are a
/// `BadRequest`.
pub async fn search(
    pool: &PgPool,
    embedder: Arc<dyn Embedder>,
    query: SearchQuery,
) -> Result<Vec<Hit>, AppError> {
    let q = query.q.trim();
    if q.is_empty() {
        return Err(AppError::BadRequest("q must not be empty".to_string()));
    }
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_QUERY_CHARS} characters"
        )));
    }
    // Postgres text cannot hold a NUL.
    if q.contains('\0') || query.source.as_deref().is_some_and(|s| s.contains('\0')) {
        return Err(AppError::BadRequest(
            "q and source must not contain NUL".to_string(),
        ));
    }
    let k = clamp_k(query.k);
    let kinds = validate_kinds(query.kinds)?;
    let filter = Filter {
        tenant_id: query.tenant_id,
        kinds: kinds.as_deref(),
        source: query.source.as_deref(),
    };

    let lexical = lexical_branch(pool, q, &filter).await?;
    let vector_hits = vector_branch(pool, embedder, q, &filter).await?;
    let vector: Vec<i64> = vector_hits.iter().map(|(id, _)| *id).collect();
    // The enumerations the vector branch found, in its order: an enumeration far from the query
    // in meaning is not lifted, however few enumerations there are.
    let enumerations: Vec<i64> = vector_hits
        .iter()
        .filter(|(_, enumerates)| *enumerates)
        .map(|(id, _)| *id)
        .take(ENUMERATION_LIMIT)
        .collect();

    let mut fused = fuse(&[&lexical, &vector, &enumerations]);
    fused.truncate(k);
    if fused.is_empty() {
        return Ok(Vec::new());
    }

    // One query for all rows; the tenant filter applies here too. Rows come back in any order,
    // so they are put in the fused order afterwards.
    let ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
    let rows: Vec<HitRow> = sqlx::query_as(
        "SELECT c.id, c.document_id, s.name AS source, d.kind, d.title, d.uri AS doc_uri,
                left(d.metadata->>'path', 2048) AS path,
                CASE WHEN d.kind = 'session' THEN d.external_id END AS external_id,
                d.metadata->>'commit' AS commit,
                s.web_url_template AS template, c.start_line, c.end_line, c.symbols, c.text
         FROM chunks c
         JOIN documents d ON d.id = c.document_id
         JOIN sources s ON s.id = d.source_id
         WHERE c.id = ANY($1) AND c.tenant_id = $2",
    )
    .bind(&ids)
    .bind(query.tenant_id)
    .fetch_all(pool)
    .await?;
    let mut by_id: HashMap<i64, HitRow> = rows.into_iter().map(|r| (r.id, r)).collect();
    // A chunk deleted since the branches ran is simply missing.
    Ok(fused
        .into_iter()
        .filter_map(|(id, score)| by_id.remove(&id).map(|row| row.into_hit(score)))
        .collect())
}

/// The filters both branches share.
struct Filter<'a> {
    tenant_id: Uuid,
    kinds: Option<&'a [String]>,
    source: Option<&'a str>,
}

async fn lexical_branch(pool: &PgPool, q: &str, f: &Filter<'_>) -> Result<Vec<i64>, AppError> {
    let lexemes: Vec<String> =
        sqlx::query_scalar("SELECT tsvector_to_array(to_tsvector('english', $1))")
            .bind(q)
            .fetch_one(pool)
            .await?;
    let Some(tsquery) = or_query(&lexemes) else {
        return Ok(Vec::new());
    };
    let ids = sqlx::query_scalar(
        "SELECT c.id
         FROM chunks c
         JOIN documents d ON d.id = c.document_id
         JOIN sources s ON s.id = d.source_id
         WHERE c.tenant_id = $1 AND c.tsv @@ to_tsquery('simple', $2)
           AND ($3::text[] IS NULL OR d.kind = ANY($3)) AND ($4::text IS NULL OR s.name = $4)
         ORDER BY ts_rank_cd(c.tsv, to_tsquery('simple', $2), 32) DESC, c.id
         LIMIT $5",
    )
    .bind(f.tenant_id)
    .bind(&tsquery)
    .bind(f.kinds)
    .bind(f.source)
    .bind(BRANCH_LIMIT)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// The nearest chunks, best first, each with whether it is an enumeration.
async fn vector_branch(
    pool: &PgPool,
    embedder: Arc<dyn Embedder>,
    q: &str,
    f: &Filter<'_>,
) -> Result<Vec<(i64, bool)>, AppError> {
    let embedding = embed_query_async(embedder, q.to_string()).await?;
    // The filters are applied after the index scan, so let it keep scanning until it has enough
    // rows that pass them.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'relaxed_order'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL hnsw.ef_search = 100")
        .execute(&mut *tx)
        .await?;
    let hits = sqlx::query_as(
        "SELECT c.id, c.enumerates
         FROM chunks c
         JOIN documents d ON d.id = c.document_id
         JOIN sources s ON s.id = d.source_id
         WHERE c.tenant_id = $1
           AND ($2::text[] IS NULL OR d.kind = ANY($2)) AND ($3::text IS NULL OR s.name = $3)
         ORDER BY c.embedding <=> $4, c.id
         LIMIT $5",
    )
    .bind(f.tenant_id)
    .bind(f.kinds)
    .bind(f.source)
    .bind(Vector::from(embedding))
    .bind(BRANCH_LIMIT)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(hits)
}

/// `k` limited to 1..=50.
pub fn clamp_k(k: usize) -> usize {
    k.clamp(1, MAX_K)
}

/// Checks the kinds against the document kinds. `None`, or an empty list, means all kinds.
fn validate_kinds(kinds: Option<Vec<String>>) -> Result<Option<Vec<String>>, AppError> {
    let Some(kinds) = kinds.filter(|k| !k.is_empty()) else {
        return Ok(None);
    };
    if let Some(bad) = kinds.iter().find(|k| !DOCUMENT_KINDS.contains(&k.as_str())) {
        return Err(AppError::BadRequest(format!(
            "unknown kind {bad:?}; the kinds are {}",
            DOCUMENT_KINDS.join(", ")
        )));
    }
    Ok(Some(kinds))
}

/// The tsquery text that matches any of the lexemes: each one single-quoted, joined by ` | `.
/// Inside the quotes a `'` is doubled and a backslash is escaped, so a lexeme can never end the
/// quote or turn into query syntax. `None` when there are no lexemes.
fn or_query(lexemes: &[String]) -> Option<String> {
    if lexemes.is_empty() {
        return None;
    }
    let quoted: Vec<String> = lexemes
        .iter()
        .map(|l| format!("'{}'", l.replace('\\', "\\\\").replace('\'', "''")))
        .collect();
    Some(quoted.join(" | "))
}

/// Reciprocal Rank Fusion of ranked id lists (best first), the lexical list first: an id scores
/// `1 / (60 + rank)` for each list it is in, with 1-based ranks. Returns `(id, score)` best
/// first. Ties go to the id with the better lexical rank (an id missing from the lexical list
/// comes after those in it), then to the lower id. Ranks inside one list are distinct, so the id
/// only keeps the order total; equal scores of different ids with equal lexical ranks cannot
/// happen. Ties inside a branch (equal `ts_rank_cd` or equal distance) are broken by id in SQL.
fn fuse(lists: &[&[i64]]) -> Vec<(i64, f64)> {
    struct Entry {
        score: f64,
        lexical_rank: usize,
    }
    let mut entries: HashMap<i64, Entry> = HashMap::new();
    for (n, list) in lists.iter().enumerate() {
        for (i, id) in list.iter().enumerate() {
            let e = entries.entry(*id).or_insert(Entry {
                score: 0.0,
                lexical_rank: usize::MAX,
            });
            if n == 0 {
                e.lexical_rank = i + 1;
            }
            e.score += 1.0 / (RRF_K + (i + 1) as f64);
        }
    }
    let mut out: Vec<(i64, Entry)> = entries.into_iter().collect();
    out.sort_by(|(a_id, a), (b_id, b)| {
        b.score
            .total_cmp(&a.score)
            .then(a.lexical_rank.cmp(&b.lexical_rank))
            .then(a_id.cmp(b_id))
    });
    out.into_iter().map(|(id, e)| (id, e.score)).collect()
}

/// Fills a source's web URL template for a chunk. Only `{path}`, `{commit}`, `{start}` and `{end}`
/// are substituted, in one pass over the template, so nothing that is substituted is scanned
/// again; any other `{...}` stays literal. The path is percent-encoded per segment (the `/`
/// between segments stays), so a path cannot add a query, a fragment or a new segment. The
/// commit must be a 40 character hex sha: the caller checks that.
pub fn render_web_url(template: &str, path: &str, commit: &str, start: i32, end: i32) -> String {
    let mut out = String::with_capacity(template.len() + path.len() + 64);
    let mut rest = template;
    while let Some(at) = rest.find('{') {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        if let Some(r) = tail.strip_prefix("{path}") {
            push_encoded_path(&mut out, path);
            rest = r;
        } else if let Some(r) = tail.strip_prefix("{commit}") {
            out.push_str(commit);
            rest = r;
        } else if let Some(r) = tail.strip_prefix("{start}") {
            let _ = write!(out, "{start}");
            rest = r;
        } else if let Some(r) = tail.strip_prefix("{end}") {
            let _ = write!(out, "{end}");
            rest = r;
        } else {
            out.push('{');
            rest = tail.strip_prefix('{').unwrap_or(tail);
        }
    }
    out.push_str(rest);
    out
}

/// Percent-encodes every byte but `A-Z a-z 0-9 - . _ ~` in each `/` separated segment.
fn push_encoded_path(out: &mut String, path: &str) {
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for b in segment.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(b));
            } else {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn snippet_is_the_first_40_lines() {
        let text: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let s = snippet(&text);
        assert_eq!(s.lines().count(), 40);
        assert!(s.starts_with("line 1\n"));
        assert!(s.ends_with("line 40"));
        assert_eq!(snippet("a\nb"), "a\nb");
        assert_eq!(snippet(""), "");
    }

    #[test]
    fn k_is_clamped_to_1_through_50() {
        assert_eq!(clamp_k(0), 1);
        assert_eq!(clamp_k(1), 1);
        assert_eq!(clamp_k(10), 10);
        assert_eq!(clamp_k(50), 50);
        assert_eq!(clamp_k(51), 50);
        assert_eq!(clamp_k(usize::MAX), 50);
    }

    #[test]
    fn kinds_are_checked_against_the_vocabulary() {
        assert_eq!(validate_kinds(None).unwrap(), None);
        assert_eq!(validate_kinds(Some(vec![])).unwrap(), None);
        let ok = vec!["code".to_string(), "note".to_string()];
        assert_eq!(validate_kinds(Some(ok.clone())).unwrap(), Some(ok));
        let err = validate_kinds(Some(vec!["code".to_string(), "docs".to_string()])).unwrap_err();
        let AppError::BadRequest(msg) = err else {
            panic!("want BadRequest");
        };
        assert!(msg.contains("\"docs\""), "{msg}");
        assert!(msg.contains("code, doc, session, note"), "{msg}");
    }

    #[test]
    fn or_query_quotes_and_escapes_lexemes() {
        assert_eq!(or_query(&[]), None);
        let one = vec!["plan".to_string()];
        assert_eq!(or_query(&one).unwrap(), "'plan'");
        let many = vec!["plan".to_string(), "limit".to_string()];
        assert_eq!(or_query(&many).unwrap(), "'plan' | 'limit'");
        let hostile = vec!["it's".to_string(), r"a\b".to_string(), r"end\".to_string()];
        assert_eq!(or_query(&hostile).unwrap(), r"'it''s' | 'a\\b' | 'end\\'");
        // Syntax characters stay inside the quotes.
        let syntax = vec!["(x) & !y | z:*".to_string()];
        assert_eq!(or_query(&syntax).unwrap(), "'(x) & !y | z:*'");
    }

    #[test]
    fn fusion_adds_the_reciprocal_ranks() {
        let fused = fuse(&[&[1, 2], &[2, 3]]);
        let ids: Vec<i64> = fused.iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![2, 1, 3]);
        let score = |id: i64| fused.iter().find(|f| f.0 == id).unwrap().1;
        assert!((score(2) - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
        assert!((score(1) - 1.0 / 61.0).abs() < 1e-12);
        assert!((score(3) - 1.0 / 62.0).abs() < 1e-12);
    }

    #[test]
    fn a_third_list_adds_its_reciprocal_ranks() {
        // First place in the third list scores like first place in any list: it ties with the
        // vector-only 9 and wins on the lower id, and it beats the vector's second place.
        let fused = fuse(&[&[1, 2], &[9, 8, 2], &[5]]);
        let ids: Vec<i64> = fused.iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![2, 1, 5, 9, 8]);
        let score = |id: i64| fused.iter().find(|f| f.0 == id).unwrap().1;
        assert!((score(5) - 1.0 / 61.0).abs() < 1e-12);
        assert!((score(2) - (1.0 / 62.0 + 1.0 / 63.0)).abs() < 1e-12);
    }

    #[test]
    fn one_branch_alone_keeps_its_order() {
        let ids = |v: Vec<(i64, f64)>| v.into_iter().map(|f| f.0).collect::<Vec<_>>();
        assert_eq!(ids(fuse(&[&[], &[7, 5, 9]])), vec![7, 5, 9]);
        assert_eq!(ids(fuse(&[&[7, 5, 9], &[]])), vec![7, 5, 9]);
        assert!(fuse(&[&[], &[]]).is_empty());
    }

    #[test]
    fn equal_scores_go_to_the_better_lexical_rank_then_the_lower_id() {
        // Lexical-only rank 1 and vector-only rank 1 score the same. The lexical one wins even
        // with the higher id.
        let ids: Vec<i64> = fuse(&[&[9], &[3]]).into_iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![9, 3]);
        // Lexical 1 + vector 2 against lexical 2 + vector 1: equal sums, lexical rank decides.
        let ids: Vec<i64> = fuse(&[&[8, 4], &[4, 8]]).into_iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![8, 4]);
        // The result is the same whatever the order of the ids in the lists.
        let ids: Vec<i64> = fuse(&[&[4, 8], &[8, 4]]).into_iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![4, 8]);
    }

    #[test]
    fn web_url_fills_the_four_placeholders() {
        let url = render_web_url(
            "https://github.com/acme/app/blob/{commit}/{path}#L{start}-L{end}",
            "src/billing/plans.rs",
            SHA,
            7,
            12,
        );
        assert_eq!(
            url,
            format!("https://github.com/acme/app/blob/{SHA}/src/billing/plans.rs#L7-L12")
        );
        // Placeholders may repeat, and a template may omit some.
        assert_eq!(
            render_web_url("https://x/{start}/{start}/{end}", "p", SHA, 1, 2),
            "https://x/1/1/2"
        );
        assert_eq!(
            render_web_url("https://x/static", "p", SHA, 1, 2),
            "https://x/static"
        );
    }

    #[test]
    fn web_url_encodes_each_path_segment() {
        let url = render_web_url("https://x/{path}", "a b/c#d/e?f/g%h/h\u{e9}.rs", SHA, 1, 1);
        assert_eq!(url, "https://x/a%20b/c%23d/e%3Ff/g%25h/h%C3%A9.rs");
        // Unreserved characters stay, and so do the slashes between segments.
        assert_eq!(
            render_web_url("https://x/{path}", "src/a-b_c.d~e/f.rs", SHA, 1, 1),
            "https://x/src/a-b_c.d~e/f.rs"
        );
        // Characters that could change the meaning of a URL are all encoded.
        let url = render_web_url(
            "https://x/{path}",
            "a:b@c&d=e+f;g,h\"i<j>k{l}m\\n|o",
            SHA,
            1,
            1,
        );
        assert_eq!(
            url,
            "https://x/a%3Ab%40c%26d%3De%2Bf%3Bg%2Ch%22i%3Cj%3Ek%7Bl%7Dm%5Cn%7Co"
        );
    }

    #[test]
    fn web_url_substitutes_in_one_pass() {
        // A path that spells a placeholder is encoded, and is not substituted a second time.
        let url = render_web_url("https://x/{path}?c={commit}", "{commit}/{start}", SHA, 1, 2);
        assert_eq!(url, format!("https://x/%7Bcommit%7D/%7Bstart%7D?c={SHA}"));
    }

    #[test]
    fn web_url_keeps_unknown_placeholders_literal() {
        assert_eq!(
            render_web_url("https://x/{branch}/{path}/{PATH}/{", "f.rs", SHA, 1, 2),
            "https://x/{branch}/f.rs/{PATH}/{"
        );
        assert_eq!(
            render_web_url("https://x/{{path}}", "f.rs", SHA, 1, 2),
            "https://x/{f.rs}"
        );
        assert_eq!(
            render_web_url("https://x/}{", "f.rs", SHA, 1, 2),
            "https://x/}{"
        );
        // Non-ASCII text in the template is kept as is.
        assert_eq!(
            render_web_url("https://x/\u{e9}/{path}", "f", SHA, 1, 2),
            "https://x/\u{e9}/f"
        );
    }
}
