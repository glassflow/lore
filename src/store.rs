//! SQL for sources, documents and chunks: the storage every source kind (repos, traces, documents)
//! shares. Queries are runtime-checked. Every function that reaches a tenant-owned row takes the
//! tenant id, so another tenant's rows are never visible.
//!
//! Vocabulary: a "source kind" is `sources.kind` (`repo`, `traces`, `documents`); a "document kind"
//! is `documents.kind` (`code`, `doc`, `session`, `note`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use pgvector::Vector;
use serde::Serialize;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::chunk::{Chunk, embed_text, lexical_text};
use crate::embed::{DIM, embed_documents_async};
use crate::error::AppError;
use crate::http::AppState;
use crate::sessions::is_pending;

const SOURCE_KINDS: [&str; 3] = ["repo", "traces", "documents"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Source {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub kind: String,
    pub name: String,
    pub web_url_template: Option<String>,
    pub indexed_commit: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewDocument {
    pub tenant_id: Uuid,
    pub source_id: Uuid,
    pub external_id: String,
    /// Document kind: `code`, `doc`, `session` or `note`.
    pub kind: String,
    pub title: String,
    pub uri: Option<String>,
    pub metadata: serde_json::Value,
    pub content_hash: String,
    /// The exact text as it was sent, kept only for documents of the documents API: chunks are cut
    /// and overlap, so they cannot give back the original. Repo files and sessions leave it `None`
    /// and are rebuilt from their chunks by [`document_text`].
    pub body: Option<String>,
}

/// A chunk with the texts that were derived from it: `lexical` feeds the full-text index and
/// `embedding` is the vector of `embed_text(title, chunk)`.
#[derive(Debug, Clone)]
pub struct IndexedChunk {
    pub chunk: Chunk,
    pub lexical: String,
    pub embedding: Vec<f32>,
}

const SOURCE_COLUMNS: &str = "id, tenant_id, kind, name, web_url_template, indexed_commit";

/// Returns the source `(tenant, kind, name)`, creating it when missing. A given template replaces
/// the stored one; `None` keeps it. The template is rendered as a link by the web page, so it must
/// be an `http://` or `https://` URL.
pub async fn ensure_source(
    pool: &PgPool,
    tenant_id: Uuid,
    kind: &str,
    name: &str,
    web_url_template: Option<&str>,
) -> Result<Source, AppError> {
    if !SOURCE_KINDS.contains(&kind) {
        return Err(AppError::BadRequest(
            "source kind must be repo, traces or documents".to_string(),
        ));
    }
    if let Some(t) = web_url_template
        && !(t.starts_with("http://") || t.starts_with("https://"))
    {
        return Err(AppError::BadRequest(
            "web_url_template must start with http:// or https://".to_string(),
        ));
    }
    let source = sqlx::query_as(&format!(
        "INSERT INTO sources (id, tenant_id, kind, name, web_url_template)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, kind, name) DO UPDATE SET
           web_url_template = COALESCE(EXCLUDED.web_url_template, sources.web_url_template),
           updated_at = CASE WHEN EXCLUDED.web_url_template IS NULL
                             THEN sources.updated_at ELSE now() END
         RETURNING {SOURCE_COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(kind)
    .bind(name)
    .bind(web_url_template)
    .fetch_one(pool)
    .await?;
    Ok(source)
}

pub async fn get_source(pool: &PgPool, tenant_id: Uuid, id: Uuid) -> sqlx::Result<Option<Source>> {
    sqlx::query_as(&format!(
        "SELECT {SOURCE_COLUMNS} FROM sources WHERE id = $1 AND tenant_id = $2"
    ))
    .bind(id)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
}

/// The source `(tenant, kind, name)`, or `None` when the tenant has none.
pub async fn find_source(
    pool: &PgPool,
    tenant_id: Uuid,
    kind: &str,
    name: &str,
) -> sqlx::Result<Option<Source>> {
    sqlx::query_as(&format!(
        "SELECT {SOURCE_COLUMNS} FROM sources
         WHERE tenant_id = $1 AND kind = $2 AND name = $3"
    ))
    .bind(tenant_id)
    .bind(kind)
    .bind(name)
    .fetch_optional(pool)
    .await
}

/// The tenant's sources, optionally only those of one source kind.
pub async fn list_sources(
    pool: &PgPool,
    tenant_id: Uuid,
    kind: Option<&str>,
) -> sqlx::Result<Vec<Source>> {
    sqlx::query_as(&format!(
        "SELECT {SOURCE_COLUMNS} FROM sources
         WHERE tenant_id = $1 AND ($2::text IS NULL OR kind = $2)
         ORDER BY kind, name"
    ))
    .bind(tenant_id)
    .bind(kind)
    .fetch_all(pool)
    .await
}

/// The id of the tenant's `session` document with this `external_id` (the session id), or `None`.
/// If several traces sources hold the same session id, the most recently updated one wins.
pub async fn find_session_document(
    pool: &PgPool,
    tenant_id: Uuid,
    external_id: &str,
) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar(
        "SELECT id FROM documents
         WHERE tenant_id = $1 AND kind = 'session' AND external_id = $2
         ORDER BY updated_at DESC, id LIMIT 1",
    )
    .bind(tenant_id)
    .bind(external_id)
    .fetch_optional(pool)
    .await
}

/// A source with its document count, for `list_sources` in the MCP tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct SourceSummary {
    pub name: String,
    pub kind: String,
    pub indexed_commit: Option<String>,
    pub documents: i64,
}

/// The tenant's sources with how many documents each holds, ordered like [`list_sources`].
pub async fn list_source_summaries(
    pool: &PgPool,
    tenant_id: Uuid,
) -> sqlx::Result<Vec<SourceSummary>> {
    sqlx::query_as(
        "SELECT s.name, s.kind, s.indexed_commit, count(d.id) AS documents
         FROM sources s
         LEFT JOIN documents d ON d.source_id = s.id AND d.tenant_id = s.tenant_id
         WHERE s.tenant_id = $1
         GROUP BY s.id
         ORDER BY s.kind, s.name",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
}

/// What `document_text` knows about a document besides its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct DocumentRow {
    pub id: Uuid,
    pub source_id: Uuid,
    /// The source name.
    pub source: String,
    /// The document kind (`code`, `doc`, `session`, `note`).
    pub kind: String,
    pub title: String,
    pub uri: Option<String>,
    pub metadata: serde_json::Value,
    /// `metadata->>'path'`: only repo files have one.
    pub path: Option<String>,
}

/// The document and its text, or `None` when the id is unknown or belongs to another tenant. The
/// text is the stored body when the document has one (documents API), otherwise its chunks in
/// `ord` order, where lines that overlapping chunks share are written once, see [`join_chunks`].
/// The text is not capped; callers cut it to their own limit.
///
/// A database error is logged and also gives `None`: callers see "not found" rather than a
/// distinct failure, which keeps the signature simple for the MCP tool and the later reader of
/// the same text.
pub async fn document_text(
    pool: &PgPool,
    tenant_id: Uuid,
    document_id: Uuid,
) -> Option<(DocumentRow, String)> {
    match load_document(pool, tenant_id, document_id).await {
        Ok(found) => found,
        Err(e) => {
            tracing::error!(error = %e, %document_id, "reading a document failed");
            None
        }
    }
}

/// A document row with its stored body.
#[derive(sqlx::FromRow)]
struct StoredDocument {
    #[sqlx(flatten)]
    row: DocumentRow,
    body: Option<String>,
}

async fn load_document(
    pool: &PgPool,
    tenant_id: Uuid,
    document_id: Uuid,
) -> sqlx::Result<Option<(DocumentRow, String)>> {
    let found: Option<StoredDocument> = sqlx::query_as(
        "SELECT d.id, d.source_id, s.name AS source, d.kind, d.title, d.uri, d.metadata,
                d.metadata->>'path' AS path, d.body
         FROM documents d
         JOIN sources s ON s.id = d.source_id
         WHERE d.id = $1 AND d.tenant_id = $2",
    )
    .bind(document_id)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;
    let Some(StoredDocument { row, body }) = found else {
        return Ok(None);
    };
    if let Some(body) = body {
        return Ok(Some((row, body)));
    }
    let chunks: Vec<(String, i32, i32)> = sqlx::query_as(
        "SELECT text, start_line, end_line FROM chunks
         WHERE document_id = $1 AND tenant_id = $2 ORDER BY ord",
    )
    .bind(document_id)
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    Ok(Some((row, join_chunks(&chunks))))
}

/// Joins `(text, start_line, end_line)` chunks, given in `ord` order, with `\n`. Window chunks
/// repeat the last lines of the previous one, so a chunk starting at or before the highest line
/// already written has its first `written - start + 1` lines dropped (all of it when it ends
/// there too). Chunks that do not overlap, or leave gaps between them, are joined as they are.
fn join_chunks(chunks: &[(String, i32, i32)]) -> String {
    let mut out = String::new();
    let mut written = 0;
    for (text, start, end) in chunks {
        let mut rest = text.as_str();
        if *start <= written {
            let skip = (written - *start) as usize + 1;
            // Every line drops: the whole chunk was already written.
            let Some(tail) = rest.splitn(skip + 1, '\n').nth(skip) else {
                continue;
            };
            rest = tail;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(rest);
        written = *end;
    }
    out
}

/// Records the commit a repo source has been indexed up to. `source_id` comes from a `Source` the
/// caller already fetched for its tenant.
pub async fn set_indexed_commit(pool: &PgPool, source_id: Uuid, sha: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE sources SET indexed_commit = $2, updated_at = now() WHERE id = $1")
        .bind(source_id)
        .bind(sha)
        .execute(pool)
        .await?;
    Ok(())
}

/// Inserts the document, or updates the existing `(source, external id)` row, and returns its id.
/// The source must belong to `doc.tenant_id`, otherwise `NotFound`.
pub async fn upsert_document(
    tx: &mut Transaction<'_, Postgres>,
    doc: &NewDocument,
) -> Result<Uuid, AppError> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO documents
           (id, tenant_id, source_id, external_id, kind, title, uri, metadata, content_hash, body)
         SELECT $1, s.tenant_id, s.id, $4, $5, $6, $7, $8, $9, $10
         FROM sources s WHERE s.id = $3 AND s.tenant_id = $2
         ON CONFLICT (source_id, external_id) DO UPDATE SET
           kind = EXCLUDED.kind,
           title = EXCLUDED.title,
           uri = EXCLUDED.uri,
           metadata = EXCLUDED.metadata,
           content_hash = EXCLUDED.content_hash,
           body = EXCLUDED.body,
           updated_at = now()
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(doc.tenant_id)
    .bind(doc.source_id)
    .bind(&doc.external_id)
    .bind(&doc.kind)
    .bind(&doc.title)
    .bind(&doc.uri)
    .bind(&doc.metadata)
    .bind(&doc.content_hash)
    .bind(&doc.body)
    .fetch_optional(&mut **tx)
    .await?;
    id.ok_or(AppError::NotFound)
}

/// The stored content hash of a document, for skipping files that did not change.
pub async fn document_hash(
    pool: &PgPool,
    source_id: Uuid,
    external_id: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT content_hash FROM documents WHERE source_id = $1 AND external_id = $2",
    )
    .bind(source_id)
    .bind(external_id)
    .fetch_optional(pool)
    .await
}

/// The external ids of every document of a source.
pub async fn list_external_ids(pool: &PgPool, source_id: Uuid) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT external_id FROM documents WHERE source_id = $1")
        .bind(source_id)
        .fetch_all(pool)
        .await
}

/// Replaces all chunks of a document: deletes the old ones, then inserts `chunks` with `ord` set
/// to the index. Symbols weigh more (A) than the body (B) in the full-text vector. The document
/// must belong to `tenant_id`, otherwise `NotFound`.
pub async fn replace_chunks(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    document_id: Uuid,
    chunks: &[IndexedChunk],
) -> Result<(), AppError> {
    let owned: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM documents WHERE id = $1 AND tenant_id = $2")
            .bind(document_id)
            .bind(tenant_id)
            .fetch_optional(&mut **tx)
            .await?;
    if owned.is_none() {
        return Err(AppError::NotFound);
    }
    sqlx::query("DELETE FROM chunks WHERE document_id = $1 AND tenant_id = $2")
        .bind(document_id)
        .bind(tenant_id)
        .execute(&mut **tx)
        .await?;
    for (ord, c) in chunks.iter().enumerate() {
        insert_chunk(tx, tenant_id, document_id, ord, c).await?;
    }
    Ok(())
}

async fn insert_chunk(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    document_id: Uuid,
    ord: usize,
    c: &IndexedChunk,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO chunks
           (tenant_id, document_id, ord, text, start_line, end_line, symbols, tsv, embedding,
            enumerates)
         VALUES ($1, $2, $3, $4, $5, $6, $7,
                 setweight(to_tsvector('english', $8::text), 'A')
                   || setweight(to_tsvector('english', $9::text), 'B'),
                 $10, $11)",
    )
    .bind(tenant_id)
    .bind(document_id)
    .bind(i32::try_from(ord).unwrap_or(i32::MAX))
    .bind(&c.chunk.text)
    .bind(i32::try_from(c.chunk.start_line).unwrap_or(i32::MAX))
    .bind(i32::try_from(c.chunk.end_line).unwrap_or(i32::MAX))
    .bind(&c.chunk.symbols)
    .bind(c.chunk.symbols.join(" "))
    .bind(&c.lexical)
    .bind(Vector::from(c.embedding.clone()))
    .bind(c.chunk.enumerates)
    .execute(conn)
    .await?;
    Ok(())
}

/// Deletes a document; its chunks go with it (cascade). Returns whether a document was deleted.
pub async fn delete_document(
    tx: &mut Transaction<'_, Postgres>,
    source_id: Uuid,
    external_id: &str,
) -> sqlx::Result<bool> {
    let res = sqlx::query("DELETE FROM documents WHERE source_id = $1 AND external_id = $2")
        .bind(source_id)
        .bind(external_id)
        .execute(&mut **tx)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// The key under which a stored embedding is found again: the part of `embed_text` that comes
/// after the title (the symbols, then the text).
fn embedding_key(text: &str, symbols: &[String]) -> String {
    format!("{}\n{}", symbols.join(" "), text)
}

/// The embeddings of the chunks the document `(source, external_id)` holds now, by
/// [`embedding_key`]. Empty when the document is new or its stored title is not `title_for_embed`,
/// because then the stored vectors were made from another `embed_text`. All callers of
/// `index_text` store the title they embed with, so a stored title that differs means the title
/// changed and every chunk has to be embedded again.
async fn stored_embeddings(
    pool: &PgPool,
    tenant_id: Uuid,
    source_id: Uuid,
    external_id: &str,
    title_for_embed: &str,
) -> sqlx::Result<HashMap<String, Vec<f32>>> {
    let rows: Vec<(String, Vec<String>, Vector)> = sqlx::query_as(
        "SELECT c.text, c.symbols, c.embedding
         FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.tenant_id = $1 AND c.tenant_id = $1 AND d.source_id = $2
           AND d.external_id = $3 AND d.title = $4",
    )
    .bind(tenant_id)
    .bind(source_id)
    .bind(external_id)
    .bind(title_for_embed)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(text, symbols, v)| (embedding_key(&text, &symbols), v.to_vec()))
        .collect())
}

/// The shared indexing path (repos, sessions, documents): embeds the chunks, then stores the
/// document and its chunks in one short transaction. Returns the chunk count.
///
/// A chunk whose text and symbols equal those of a chunk the document already has (and whose
/// title is unchanged) keeps the stored embedding, so only new or changed chunks reach the
/// embedder. This is what keeps a growing session cheap to index again.
///
/// The embedding runs before the transaction opens, so no connection is held while the model
/// works. `title_for_embed` is the path or title passed to `embed_text`.
pub async fn index_text(
    state: &AppState,
    tenant_id: Uuid,
    source: &Source,
    doc: NewDocument,
    chunks: Vec<Chunk>,
    title_for_embed: &str,
) -> Result<usize> {
    let stats =
        index_text_with_stats(state, tenant_id, source, doc, chunks, title_for_embed).await?;
    Ok(stats.chunks)
}

/// What one [`index_text_with_stats`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Chunks stored.
    pub chunks: usize,
    /// Chunks the embedder computed a vector for. The others kept their stored vector.
    pub embedded: usize,
    /// Wall-clock time spent embedding, including any wait for the embedder while other pushes
    /// use it.
    pub embed_time: Duration,
}

/// [`index_text`], also saying how many chunks were embedded and how long that took.
pub async fn index_text_with_stats(
    state: &AppState,
    tenant_id: Uuid,
    source: &Source,
    doc: NewDocument,
    chunks: Vec<Chunk>,
    title_for_embed: &str,
) -> Result<IndexStats> {
    if source.tenant_id != tenant_id || doc.tenant_id != tenant_id || doc.source_id != source.id {
        bail!("document does not belong to the source and tenant");
    }
    let lexicals: Vec<String> = chunks.iter().map(lexical_text).collect();
    let stored = stored_embeddings(
        &state.pool,
        tenant_id,
        source.id,
        &doc.external_id,
        title_for_embed,
    )
    .await?;
    let mut embeddings: Vec<Option<Vec<f32>>> = chunks
        .iter()
        .map(|c| {
            stored
                .get(&embedding_key(&c.text, &c.symbols))
                .filter(|v| v.len() == DIM)
                .cloned()
        })
        .collect();
    let missing: Vec<usize> = (0..chunks.len())
        .filter(|i| embeddings[*i].is_none())
        .collect();
    let embedded = missing.len();
    let mut embed_time = Duration::ZERO;
    if !missing.is_empty() {
        let texts: Vec<String> = missing
            .iter()
            .map(|i| embed_text(title_for_embed, &chunks[*i]))
            .collect();
        let started = Instant::now();
        let fresh = embed_documents_async(state.embedder.clone(), texts).await?;
        embed_time = started.elapsed();
        if fresh.len() != missing.len() {
            bail!(
                "embedder returned {} vectors for {} chunks",
                fresh.len(),
                missing.len()
            );
        }
        for (i, v) in missing.into_iter().zip(fresh) {
            embeddings[i] = Some(v);
        }
    }
    let embeddings: Vec<Vec<f32>> = embeddings.into_iter().flatten().collect();
    if embeddings.len() != chunks.len() {
        bail!(
            "embedder returned {} vectors for {} chunks",
            embeddings.len(),
            chunks.len()
        );
    }
    let indexed: Vec<IndexedChunk> = chunks
        .into_iter()
        .zip(lexicals)
        .zip(embeddings)
        .map(|((chunk, lexical), embedding)| IndexedChunk {
            chunk,
            lexical,
            embedding,
        })
        .collect();

    let mut tx = state.pool.begin().await?;
    let document_id = upsert_document(&mut tx, &doc).await?;
    replace_chunks(&mut tx, tenant_id, document_id, &indexed).await?;
    tx.commit().await?;
    Ok(IndexStats {
        chunks: indexed.len(),
        embedded,
        embed_time,
    })
}

/// Rows per `INSERT` of spans: 14 binds each stays well under Postgres's 65535 limit.
const SPAN_INSERT_CHUNK: usize = 1000;

/// Upserts spans on `(tenant, trace, span)` and queues each distinct session with
/// `last_span_at = now()`. A span that appears twice in `rows` keeps its last copy, because one
/// `INSERT ... ON CONFLICT` cannot touch a row twice.
///
/// A pending snapshot (`glassflow.span.pending = true`) never replaces a finished span, neither
/// one stored earlier nor one earlier in `rows`. A finished span replaces a pending one, and a
/// pending snapshot replaces a pending one.
pub async fn upsert_spans(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    rows: &[crate::otlp::SpanRow],
) -> sqlx::Result<()> {
    // The last copy wins, except that a pending copy never replaces a finished one, the same
    // rule the `ON CONFLICT` clause below applies against the stored span.
    let mut at: HashMap<(&str, &str), usize> = HashMap::new();
    let mut unique: Vec<&crate::otlp::SpanRow> = Vec::new();
    for r in rows {
        match at.get(&(r.trace_id.as_str(), r.span_id.as_str())) {
            Some(&i) => {
                if !is_pending(r) || is_pending(unique[i]) {
                    unique[i] = r;
                }
            }
            None => {
                at.insert((r.trace_id.as_str(), r.span_id.as_str()), unique.len());
                unique.push(r);
            }
        }
    }
    // A fixed lock order, so two concurrent requests over the same spans cannot deadlock.
    unique.sort_unstable_by(|a, b| (&a.trace_id, &a.span_id).cmp(&(&b.trace_id, &b.span_id)));

    for chunk in unique.chunks(SPAN_INSERT_CHUNK) {
        let mut q = sqlx::QueryBuilder::<Postgres>::new(
            "INSERT INTO spans (tenant_id, trace_id, span_id, parent_span_id, session_id, name, \
             kind, start_time, end_time, status_code, status_message, resource, attributes, events) ",
        );
        q.push_values(chunk, |mut b, r| {
            b.push_bind(tenant_id)
                .push_bind(&r.trace_id)
                .push_bind(&r.span_id)
                .push_bind(&r.parent_span_id)
                .push_bind(&r.session_id)
                .push_bind(&r.name)
                .push_bind(&r.kind)
                .push_bind(r.start_time)
                .push_bind(r.end_time)
                .push_bind(&r.status_code)
                .push_bind(&r.status_message)
                .push_bind(r.resource.as_ref())
                .push_bind(&r.attributes)
                .push_bind(&r.events);
        });
        q.push(
            " ON CONFLICT (tenant_id, trace_id, span_id) DO UPDATE SET \
             parent_span_id = EXCLUDED.parent_span_id, session_id = EXCLUDED.session_id, \
             name = EXCLUDED.name, kind = EXCLUDED.kind, start_time = EXCLUDED.start_time, \
             end_time = EXCLUDED.end_time, status_code = EXCLUDED.status_code, \
             status_message = EXCLUDED.status_message, resource = EXCLUDED.resource, \
             attributes = EXCLUDED.attributes, events = EXCLUDED.events, received_at = now() \
             WHERE (EXCLUDED.attributes -> 'glassflow.span.pending') IS DISTINCT FROM 'true'::jsonb \
             OR (spans.attributes -> 'glassflow.span.pending') IS NOT DISTINCT FROM 'true'::jsonb",
        );
        q.build().execute(&mut **tx).await?;
    }

    let mut sessions: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
    sessions.sort_unstable();
    sessions.dedup();
    for chunk in sessions.chunks(SPAN_INSERT_CHUNK) {
        let mut q = sqlx::QueryBuilder::<Postgres>::new(
            "INSERT INTO session_queue (tenant_id, session_id, last_span_at) ",
        );
        q.push_values(chunk, |mut b, session| {
            b.push_bind(tenant_id).push_bind(*session).push("now()");
        });
        q.push(" ON CONFLICT (tenant_id, session_id) DO UPDATE SET last_span_at = now()");
        q.build().execute(&mut **tx).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::join_chunks;

    fn chunk(start: i32, end: i32) -> (String, i32, i32) {
        let text = (start..=end)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        (text, start, end)
    }

    #[test]
    fn chunks_are_joined_without_repeating_lines() {
        assert_eq!(join_chunks(&[]), "");
        // Overlap by one line, no overlap, and a gap.
        assert_eq!(
            join_chunks(&[chunk(1, 3), chunk(3, 5), chunk(6, 7), chunk(9, 9)]),
            "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl9"
        );
    }

    #[test]
    fn a_chunk_inside_what_is_written_adds_nothing() {
        assert_eq!(
            join_chunks(&[chunk(1, 6), chunk(2, 4)]),
            "l1\nl2\nl3\nl4\nl5\nl6"
        );
    }

    #[test]
    fn a_skipped_chunk_does_not_move_the_written_line_back() {
        // The second chunk is inside the first and adds nothing, so the third still overlaps the first.
        assert_eq!(
            join_chunks(&[chunk(1, 10), chunk(5, 6), chunk(8, 12)]),
            "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12"
        );
    }
}
