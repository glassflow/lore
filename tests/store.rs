mod common;

use std::sync::Arc;

use anyhow::Result;
use common::{create_tenant_key, test_router, test_state};
use lore::chunk::{Chunk, ChunkKind, chunk_file, chunk_markdown, embed_text};
use lore::embed::{DIM, Embedder, HashEmbedder};
use lore::error::AppError;
use lore::http::AppState;
use lore::store::{
    NewDocument, Source, delete_document, document_hash, document_text, ensure_source,
    find_session_document, get_source, index_text, list_source_summaries, list_sources,
    replace_chunks, set_indexed_commit, upsert_document,
};
use pgvector::Vector;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

fn state(pool: &PgPool) -> AppState {
    test_state(pool.clone(), &std::env::temp_dir())
}

async fn tenant(state: &AppState, name: &str) -> Uuid {
    create_tenant_key(&test_router(state.clone()), name, &["read"])
        .await
        .0
}

fn chunk(text: &str, lines: (u32, u32), symbols: &[&str]) -> Chunk {
    Chunk {
        text: text.to_string(),
        start_line: lines.0,
        end_line: lines.1,
        symbols: symbols.iter().map(|s| s.to_string()).collect(),
        kind: ChunkKind::Item,
        enumerates: false,
        defines: Vec::new(),
        calls: Vec::new(),
    }
}

/// Two chunks: the first has a symbol, the second a camel-case identifier in its symbols.
fn two_chunks() -> Vec<Chunk> {
    vec![
        chunk("pub fn limits(plan: Plan) -> Limits", (1, 3), &["limits"]),
        chunk("seats per day", (4, 6), &["MaxSeatsPerPlan"]),
    ]
}

fn doc(source: &Source, external_id: &str, hash: &str) -> NewDocument {
    NewDocument {
        tenant_id: source.tenant_id,
        source_id: source.id,
        external_id: external_id.to_string(),
        kind: "code".to_string(),
        title: format!("title of {external_id}"),
        uri: Some(format!("src/{external_id}")),
        metadata: json!({ "lang": "rust" }),
        content_hash: hash.to_string(),
        body: None,
    }
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Whether the tsvector of the chunk with this `ord` matches the tsquery.
async fn tsv_matches(pool: &PgPool, query: &str, ord: i32) -> bool {
    sqlx::query_scalar("SELECT tsv @@ to_tsquery('english', $1) FROM chunks WHERE ord = $2")
        .bind(query)
        .bind(ord)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn status(e: &AppError) -> u16 {
    e.status().as_u16()
}

#[sqlx::test(migrations = "./migrations")]
async fn indexing_stores_ordered_chunks_with_weighted_tsv(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(&pool, t, "repo", "app", None).await.unwrap();

    let n = index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "h1"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    assert_eq!(n, 2);

    let rows: Vec<(i32, String, i32, i32, Vec<String>, Uuid)> = sqlx::query_as(
        "SELECT ord, text, start_line, end_line, symbols, tenant_id FROM chunks ORDER BY ord",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0, rows[1].0), (0, 1));
    assert_eq!(rows[0].1, "pub fn limits(plan: Plan) -> Limits");
    assert_eq!((rows[0].2, rows[0].3), (1, 3));
    assert_eq!((rows[1].2, rows[1].3), (4, 6));
    assert_eq!(rows[1].4, vec!["MaxSeatsPerPlan".to_string()]);
    assert!(rows.iter().all(|r| r.5 == t));

    let empty: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE tsv = ''::tsvector")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(empty, 0, "every chunk has a non-empty tsv");

    // A symbol matches with weight A, a body word with weight B, and the split identifier words
    // (added by lexical_text) are searchable too.
    assert!(
        tsv_matches(&pool, "limit:A", 0).await,
        "symbol limits is weight A"
    );
    assert!(!tsv_matches(&pool, "plan:A", 0).await);
    assert!(
        tsv_matches(&pool, "plan:B", 0).await,
        "body word plan is weight B"
    );
    assert!(
        tsv_matches(&pool, "seat:B", 1).await,
        "body word seats is weight B"
    );
    assert!(!tsv_matches(&pool, "seat:A", 1).await);
    assert!(
        tsv_matches(&pool, "maxseatsperplan:A", 1).await,
        "symbol is weight A"
    );
    assert!(
        tsv_matches(&pool, "max & per", 1).await,
        "split identifier words are indexed"
    );
    assert!(!tsv_matches(&pool, "limit", 1).await);
}

#[sqlx::test(migrations = "./migrations")]
async fn stored_embeddings_are_384_dims_of_the_embed_text(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    let chunks = two_chunks();
    index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "h1"),
        chunks.clone(),
        "src/a.rs",
    )
    .await
    .unwrap();

    let dims: Vec<i32> =
        sqlx::query_scalar("SELECT vector_dims(embedding) FROM chunks ORDER BY ord")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(dims, vec![384, 384]);
    assert_eq!(DIM, 384);

    let stored: Vec<Vector> = sqlx::query_scalar("SELECT embedding FROM chunks ORDER BY ord")
        .fetch_all(&pool)
        .await
        .unwrap();
    for (i, c) in chunks.iter().enumerate() {
        let want = HashEmbedder
            .embed_query(&embed_text("src/a.rs", c))
            .unwrap();
        assert_eq!(stored[i].as_slice(), want.as_slice(), "chunk {i}");
    }
    assert_ne!(stored[0].as_slice(), stored[1].as_slice());
}

#[sqlx::test(migrations = "./migrations")]
async fn reindexing_replaces_chunks_and_updates_the_document(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "h1"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    let first_id: Uuid = sqlx::query_scalar("SELECT id FROM documents")
        .fetch_one(&pool)
        .await
        .unwrap();
    let first_chunk_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM chunks ORDER BY ord")
        .fetch_all(&pool)
        .await
        .unwrap();

    // Same chunk count: still 2 rows, but new ones.
    let n = index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "h2"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    assert_eq!(n, 2);
    assert_eq!(count(&pool, "chunks").await, 2);
    let second_chunk_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM chunks ORDER BY ord")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(
        second_chunk_ids
            .iter()
            .all(|id| !first_chunk_ids.contains(id))
    );

    // Fewer chunks, new metadata: the old second chunk is gone and the document row is updated.
    let mut changed = doc(&src, "a.rs", "h3");
    changed.kind = "doc".to_string();
    changed.title = "new title".to_string();
    changed.uri = None;
    changed.metadata = json!({ "lang": "md", "n": 1 });
    let n = index_text(
        &state,
        t,
        &src,
        changed,
        vec![chunk("only chunk", (1, 1), &[])],
        "a.rs",
    )
    .await
    .unwrap();
    assert_eq!(n, 1);
    assert_eq!(count(&pool, "documents").await, 1);
    assert_eq!(count(&pool, "chunks").await, 1);
    let text: String = sqlx::query_scalar("SELECT text FROM chunks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(text, "only chunk");

    let row: (
        Uuid,
        String,
        String,
        Option<String>,
        serde_json::Value,
        String,
    ) = sqlx::query_as("SELECT id, kind, title, uri, metadata, content_hash FROM documents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, first_id, "the upsert keeps the document id");
    assert_eq!(row.1, "doc");
    assert_eq!(row.2, "new title");
    assert_eq!(row.3, None);
    assert_eq!(row.4, json!({ "lang": "md", "n": 1 }));
    assert_eq!(row.5, "h3");

    // Zero chunks: the document stays, its chunks go.
    let n = index_text(&state, t, &src, doc(&src, "a.rs", "h4"), vec![], "a.rs")
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(count(&pool, "documents").await, 1);
    assert_eq!(count(&pool, "chunks").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_document_removes_its_chunks_only(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "h1"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    index_text(
        &state,
        t,
        &src,
        doc(&src, "b.rs", "h2"),
        two_chunks(),
        "b.rs",
    )
    .await
    .unwrap();
    assert_eq!(count(&pool, "chunks").await, 4);

    let mut tx = pool.begin().await.unwrap();
    assert!(delete_document(&mut tx, src.id, "a.rs").await.unwrap());
    tx.commit().await.unwrap();
    assert_eq!(count(&pool, "documents").await, 1);
    assert_eq!(
        count(&pool, "chunks").await,
        2,
        "cascade removed a.rs chunks"
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.external_id = 'b.rs'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 2);

    let mut tx = pool.begin().await.unwrap();
    assert!(!delete_document(&mut tx, src.id, "a.rs").await.unwrap());
    assert!(
        !delete_document(&mut tx, Uuid::new_v4(), "b.rs")
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(count(&pool, "chunks").await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_a_source_cascades_to_documents_and_chunks(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "h1"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM sources WHERE id = $1")
        .bind(src.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(count(&pool, "documents").await, 0);
    assert_eq!(count(&pool, "chunks").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn same_external_id_in_two_sources_is_two_documents(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let one = ensure_source(&pool, t, "repo", "one", None).await.unwrap();
    let two = ensure_source(&pool, t, "repo", "two", None).await.unwrap();
    index_text(
        &state,
        t,
        &one,
        doc(&one, "a.rs", "h1"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    index_text(
        &state,
        t,
        &two,
        doc(&two, "a.rs", "h2"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    assert_eq!(count(&pool, "documents").await, 2);
    assert_eq!(count(&pool, "chunks").await, 4);
    assert_eq!(
        document_hash(&pool, one.id, "a.rs")
            .await
            .unwrap()
            .as_deref(),
        Some("h1")
    );
    assert_eq!(
        document_hash(&pool, two.id, "a.rs")
            .await
            .unwrap()
            .as_deref(),
        Some("h2")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn document_hash_is_the_stored_hash_or_none(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    assert_eq!(document_hash(&pool, src.id, "a.rs").await.unwrap(), None);

    index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "hash-1"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    assert_eq!(
        document_hash(&pool, src.id, "a.rs")
            .await
            .unwrap()
            .as_deref(),
        Some("hash-1")
    );
    assert_eq!(document_hash(&pool, src.id, "b.rs").await.unwrap(), None);
    assert_eq!(
        document_hash(&pool, Uuid::new_v4(), "a.rs").await.unwrap(),
        None
    );

    index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "hash-2"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();
    assert_eq!(
        document_hash(&pool, src.id, "a.rs")
            .await
            .unwrap()
            .as_deref(),
        Some("hash-2")
    );

    let mut tx = pool.begin().await.unwrap();
    delete_document(&mut tx, src.id, "a.rs").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(document_hash(&pool, src.id, "a.rs").await.unwrap(), None);
}

#[sqlx::test(migrations = "./migrations")]
async fn ensure_source_is_idempotent_and_keeps_or_updates_the_template(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;

    let a = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    assert_eq!(
        (a.tenant_id, a.kind.as_str(), a.name.as_str()),
        (t, "repo", "app")
    );
    assert_eq!(a.web_url_template, None);
    assert_eq!(a.indexed_commit, None);

    let again = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    assert_eq!(again, a);
    assert_eq!(count(&pool, "sources").await, 1);

    let tpl = "https://github.com/o/r/blob/{sha}/{path}";
    let with = ensure_source(&pool, t, "repo", "app", Some(tpl))
        .await
        .unwrap();
    assert_eq!(with.id, a.id);
    assert_eq!(with.web_url_template.as_deref(), Some(tpl));

    // None keeps the stored template; a new one replaces it.
    let kept = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    assert_eq!(kept.web_url_template.as_deref(), Some(tpl));
    let http = "http://git.local/{path}";
    let replaced = ensure_source(&pool, t, "repo", "app", Some(http))
        .await
        .unwrap();
    assert_eq!(replaced.id, a.id);
    assert_eq!(replaced.web_url_template.as_deref(), Some(http));
    assert_eq!(count(&pool, "sources").await, 1);

    // The name is unique per (tenant, source kind).
    let other_kind = ensure_source(&pool, t, "documents", "app", None)
        .await
        .unwrap();
    assert_ne!(other_kind.id, a.id);
    assert_eq!(count(&pool, "sources").await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn ensure_source_rejects_non_http_templates_and_unknown_kinds(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;

    for bad in [
        "javascript:alert(1)",
        "data:text/html,x",
        "ftp://host/x",
        "//host/x",
        " https://host/x",
        "",
    ] {
        let err = ensure_source(&pool, t, "repo", "app", Some(bad))
            .await
            .expect_err(bad);
        assert!(matches!(err, AppError::BadRequest(_)), "{bad}: {err}");
        assert_eq!(status(&err), 400, "{bad}");
    }
    assert_eq!(count(&pool, "sources").await, 0, "nothing was created");

    // A bad template on an existing source leaves the stored one alone.
    ensure_source(&pool, t, "repo", "app", Some("https://ok/{path}"))
        .await
        .unwrap();
    let err = ensure_source(&pool, t, "repo", "app", Some("javascript:x"))
        .await
        .unwrap_err();
    assert_eq!(status(&err), 400);
    let src = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    assert_eq!(src.web_url_template.as_deref(), Some("https://ok/{path}"));

    let err = ensure_source(&pool, t, "bogus", "app", None)
        .await
        .unwrap_err();
    assert_eq!(status(&err), 400);
    assert_eq!(count(&pool, "sources").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn sources_are_invisible_to_other_tenants(pool: PgPool) {
    let state = state(&pool);
    let a = tenant(&state, "acme").await;
    let b = tenant(&state, "globex").await;
    let src_a = ensure_source(&pool, a, "repo", "app", None).await.unwrap();

    assert_eq!(
        get_source(&pool, a, src_a.id).await.unwrap().as_ref(),
        Some(&src_a)
    );
    assert_eq!(get_source(&pool, b, src_a.id).await.unwrap(), None);
    assert_eq!(get_source(&pool, a, Uuid::new_v4()).await.unwrap(), None);
    assert_eq!(
        list_sources(&pool, a, None).await.unwrap(),
        vec![src_a.clone()]
    );
    assert!(list_sources(&pool, b, None).await.unwrap().is_empty());
    assert!(
        list_sources(&pool, b, Some("repo"))
            .await
            .unwrap()
            .is_empty()
    );

    // The same name in another tenant is a different source.
    let src_b = ensure_source(&pool, b, "repo", "app", Some("https://b/{path}"))
        .await
        .unwrap();
    assert_ne!(src_b.id, src_a.id);
    assert_eq!(src_b.tenant_id, b);
    assert_eq!(src_a.web_url_template, None);
    assert_eq!(get_source(&pool, a, src_b.id).await.unwrap(), None);
    assert_eq!(list_sources(&pool, a, None).await.unwrap(), vec![src_a]);
    assert_eq!(list_sources(&pool, b, None).await.unwrap(), vec![src_b]);
}

#[sqlx::test(migrations = "./migrations")]
async fn documents_and_chunks_cannot_cross_tenants(pool: PgPool) {
    let state = state(&pool);
    let a = tenant(&state, "acme").await;
    let b = tenant(&state, "globex").await;
    let src_a = ensure_source(&pool, a, "repo", "app", None).await.unwrap();
    index_text(
        &state,
        a,
        &src_a,
        doc(&src_a, "a.rs", "h1"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();

    // Tenant b cannot index into tenant a's source, whichever side lies about the tenant.
    let mut as_b = doc(&src_a, "evil.rs", "x");
    as_b.tenant_id = b;
    assert!(
        index_text(&state, b, &src_a, as_b.clone(), two_chunks(), "evil.rs")
            .await
            .is_err()
    );
    assert!(
        index_text(&state, a, &src_a, as_b, two_chunks(), "evil.rs")
            .await
            .is_err()
    );
    let mut as_a = doc(&src_a, "evil.rs", "x");
    as_a.tenant_id = a;
    assert!(
        index_text(&state, b, &src_a, as_a, two_chunks(), "evil.rs")
            .await
            .is_err()
    );
    assert_eq!(count(&pool, "documents").await, 1);
    assert_eq!(count(&pool, "chunks").await, 2);

    // The store functions refuse on their own, without index_text's check.
    let mut tx = pool.begin().await.unwrap();
    let mut cross = doc(&src_a, "evil.rs", "x");
    cross.tenant_id = b;
    let err = upsert_document(&mut tx, &cross).await.unwrap_err();
    assert_eq!(status(&err), 404);
    tx.rollback().await.unwrap();

    let doc_a: Uuid = sqlx::query_scalar("SELECT id FROM documents")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let err = replace_chunks(&mut tx, b, doc_a, &[]).await.unwrap_err();
    assert_eq!(status(&err), 404);
    tx.rollback().await.unwrap();
    assert_eq!(count(&pool, "chunks").await, 2, "a's chunks are untouched");
}

#[sqlx::test(migrations = "./migrations")]
async fn set_indexed_commit_records_the_sha_per_source(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let one = ensure_source(&pool, t, "repo", "one", None).await.unwrap();
    let two = ensure_source(&pool, t, "repo", "two", None).await.unwrap();

    let sha = "0123456789abcdef0123456789abcdef01234567";
    set_indexed_commit(&pool, one.id, sha).await.unwrap();
    let got = get_source(&pool, t, one.id).await.unwrap().unwrap();
    assert_eq!(got.indexed_commit.as_deref(), Some(sha));
    assert_eq!(
        get_source(&pool, t, two.id)
            .await
            .unwrap()
            .unwrap()
            .indexed_commit,
        None
    );

    // ensure_source keeps it, and a later call replaces it.
    let kept = ensure_source(&pool, t, "repo", "one", Some("https://x/{path}"))
        .await
        .unwrap();
    assert_eq!(kept.indexed_commit.as_deref(), Some(sha));
    let next = "fedcba9876543210fedcba9876543210fedcba98";
    set_indexed_commit(&pool, one.id, next).await.unwrap();
    let got = get_source(&pool, t, one.id).await.unwrap().unwrap();
    assert_eq!(got.indexed_commit.as_deref(), Some(next));
}

#[sqlx::test(migrations = "./migrations")]
async fn list_sources_filters_by_source_kind(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let repo = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    let docs = ensure_source(&pool, t, "documents", "wiki", None)
        .await
        .unwrap();
    let traces = ensure_source(&pool, t, "traces", "prod", None)
        .await
        .unwrap();
    let repo2 = ensure_source(&pool, t, "repo", "lib", None).await.unwrap();

    let all = list_sources(&pool, t, None).await.unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(
        list_sources(&pool, t, Some("repo")).await.unwrap(),
        vec![repo, repo2]
    );
    assert_eq!(
        list_sources(&pool, t, Some("documents")).await.unwrap(),
        vec![docs]
    );
    assert_eq!(
        list_sources(&pool, t, Some("traces")).await.unwrap(),
        vec![traces]
    );
    assert!(
        list_sources(&pool, t, Some("nope"))
            .await
            .unwrap()
            .is_empty()
    );
}

/// Returns `count` vectors of `dim` dimensions (`count` defaults to one per text).
struct OddEmbedder {
    dim: usize,
    count: Option<usize>,
}

impl Embedder for OddEmbedder {
    fn name(&self) -> &str {
        "odd"
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let n = self.count.unwrap_or(texts.len());
        Ok(vec![vec![0.5; self.dim]; n])
    }

    fn embed_query(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(vec![0.5; self.dim])
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failed_index_changes_nothing(pool: PgPool) {
    let mut state = state(&pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(&pool, t, "repo", "app", None).await.unwrap();
    index_text(
        &state,
        t,
        &src,
        doc(&src, "a.rs", "h1"),
        two_chunks(),
        "a.rs",
    )
    .await
    .unwrap();

    // The chunk insert fails (3 dimensions, not 384) after the document upsert ran: the whole
    // transaction rolls back, so the old title, hash and chunks stay.
    state.embedder = Arc::new(OddEmbedder {
        dim: 3,
        count: None,
    });
    let mut changed = doc(&src, "a.rs", "h2");
    changed.title = "changed".to_string();
    let new_chunks = vec![chunk("new", (1, 1), &[])];
    assert!(
        index_text(&state, t, &src, changed.clone(), new_chunks.clone(), "a.rs")
            .await
            .is_err()
    );

    // The embedder returns the wrong number of vectors: refused before any write.
    state.embedder = Arc::new(OddEmbedder {
        dim: 384,
        count: Some(1),
    });
    assert!(
        index_text(&state, t, &src, changed, two_chunks(), "a.rs")
            .await
            .is_err()
    );

    let row: (String, String) = sqlx::query_as("SELECT title, content_hash FROM documents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row, ("title of a.rs".to_string(), "h1".to_string()));
    let texts: Vec<String> = sqlx::query_scalar("SELECT text FROM chunks ORDER BY ord")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0], "pub fn limits(plan: Plan) -> Limits");
}

/// `index_text` refuses up front (its own check, not a later database error) with this message.
fn assert_refused(result: Result<usize>) {
    let err = result.expect_err("mismatched indexing must fail");
    assert!(
        err.to_string().contains("does not belong"),
        "unexpected error: {err}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn index_text_refuses_mismatched_tenant_source_or_document(pool: PgPool) {
    let state = state(&pool);
    let a = tenant(&state, "acme").await;
    let b = tenant(&state, "globex").await;
    let src_a = ensure_source(&pool, a, "repo", "app", None).await.unwrap();
    let src_b = ensure_source(&pool, b, "repo", "app", None).await.unwrap();

    // The caller's tenant is not the source's tenant.
    let r = index_text(
        &state,
        b,
        &src_a,
        doc(&src_a, "a.rs", "h"),
        two_chunks(),
        "a.rs",
    )
    .await;
    assert_refused(r);

    // The document names another tenant than the source.
    let mut foreign = doc(&src_a, "a.rs", "h");
    foreign.tenant_id = b;
    assert_refused(index_text(&state, a, &src_a, foreign, two_chunks(), "a.rs").await);

    // The document names another source than the one passed.
    let mut elsewhere = doc(&src_a, "a.rs", "h");
    elsewhere.source_id = src_b.id;
    assert_refused(index_text(&state, a, &src_a, elsewhere, two_chunks(), "a.rs").await);

    assert_eq!(count(&pool, "documents").await, 0);
    assert_eq!(count(&pool, "chunks").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn document_text_joins_the_chunks_in_order_for_the_owning_tenant_only(pool: PgPool) {
    let state = state(&pool);
    let a = tenant(&state, "acme").await;
    let b = tenant(&state, "globex").await;
    let src = ensure_source(&pool, a, "repo", "app", None).await.unwrap();
    let mut d = doc(&src, "a.rs", "h1");
    d.metadata = json!({ "path": "src/a.rs" });
    index_text(&state, a, &src, d, two_chunks(), "a.rs")
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM documents")
        .fetch_one(&pool)
        .await
        .unwrap();

    let (row, text) = document_text(&pool, a, id).await.expect("owner reads it");
    assert_eq!(row.id, id);
    assert_eq!(row.source, "app");
    assert_eq!(row.kind, "code");
    assert_eq!(row.title, "title of a.rs");
    assert_eq!(row.uri.as_deref(), Some("src/a.rs"));
    assert_eq!(row.path.as_deref(), Some("src/a.rs"));
    assert_eq!(text, "pub fn limits(plan: Plan) -> Limits\nseats per day");

    assert!(document_text(&pool, b, id).await.is_none(), "other tenant");
    assert!(document_text(&pool, a, Uuid::new_v4()).await.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn document_text_returns_the_stored_body_and_not_the_joined_chunks(pool: PgPool) {
    let state = state(&pool);
    let a = tenant(&state, "acme").await;
    let b = tenant(&state, "globex").await;
    let src = ensure_source(&pool, a, "documents", "notes", None)
        .await
        .unwrap();
    // The body keeps what the chunks cannot: CRLF, blank lines and a trailing newline.
    let sent = "one\r\n\r\n\r\ntwo\n";
    let mut d = doc(&src, "n1", "h1");
    d.kind = "note".to_string();
    d.body = Some(sent.to_string());
    index_text(&state, a, &src, d, two_chunks(), "n1")
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM documents")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_, text) = document_text(&pool, a, id).await.expect("owner reads it");
    assert_eq!(text, sent);
    assert!(document_text(&pool, b, id).await.is_none(), "other tenant");

    // Writing the document again replaces the body, and a document without one is rebuilt from
    // its chunks.
    let mut d = doc(&src, "n1", "h2");
    d.kind = "note".to_string();
    d.body = Some("replaced".to_string());
    index_text(&state, a, &src, d, two_chunks(), "n1")
        .await
        .unwrap();
    assert_eq!(document_text(&pool, a, id).await.unwrap().1, "replaced");
    let mut d = doc(&src, "n1", "h3");
    d.kind = "note".to_string();
    index_text(&state, a, &src, d, two_chunks(), "n1")
        .await
        .unwrap();
    assert_eq!(
        document_text(&pool, a, id).await.unwrap().1,
        "pub fn limits(plan: Plan) -> Limits\nseats per day"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_empty_body_is_a_body(pool: PgPool) {
    let state = state(&pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(&pool, t, "documents", "notes", None)
        .await
        .unwrap();
    let mut d = doc(&src, "n1", "h1");
    d.body = Some(String::new());
    index_text(&state, t, &src, d, two_chunks(), "n1")
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM documents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(document_text(&pool, t, id).await.unwrap().1, "");
}

/// Stores `chunks` as one document and reads its text back.
async fn stored_text(pool: &PgPool, chunks: Vec<Chunk>) -> String {
    let state = state(pool);
    let t = tenant(&state, "acme").await;
    let src = ensure_source(pool, t, "repo", "app", None).await.unwrap();
    index_text(
        &state,
        t,
        &src,
        doc(&src, "long.txt", "h1"),
        chunks,
        "long.txt",
    )
    .await
    .unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM documents")
        .fetch_one(pool)
        .await
        .unwrap();
    document_text(pool, t, id).await.expect("owner reads it").1
}

#[sqlx::test(migrations = "./migrations")]
async fn document_text_does_not_repeat_the_lines_chunks_overlap_by(pool: PgPool) {
    // 150 lines make three overlapping windows of the line chunker.
    let original: String = (1..=150)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_file("notes.txt", &original);
    assert!(chunks.len() > 2, "{} chunks", chunks.len());
    assert!(chunks[1].start_line <= chunks[0].end_line, "no overlap");
    assert_eq!(stored_text(&pool, chunks).await, original);
}

#[sqlx::test(migrations = "./migrations")]
async fn document_text_of_overlapping_markdown_windows_is_the_original(pool: PgPool) {
    let original = format!(
        "# Plan\n{}",
        (1..=300)
            .map(|n| format!("item {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let chunks = chunk_markdown(&original, "plan");
    assert!(chunks.len() > 2, "{} chunks", chunks.len());
    assert!(chunks[1].start_line <= chunks[0].end_line, "no overlap");
    assert_eq!(stored_text(&pool, chunks).await, original);
}

#[sqlx::test(migrations = "./migrations")]
async fn document_text_of_overlapping_code_windows_is_the_original(pool: PgPool) {
    // One function longer than the item limit is split into overlapping pieces.
    let body: String = (1..=300)
        .map(|n| format!("    let v{n} = {n};\n"))
        .collect();
    let original = format!("fn big() {{\n{body}}}");
    let chunks = chunk_file("big.rs", &original);
    assert!(chunks.len() > 2, "{} chunks", chunks.len());
    assert!(chunks[1].start_line <= chunks[0].end_line, "no overlap");
    assert_eq!(stored_text(&pool, chunks).await, original);
}

#[sqlx::test(migrations = "./migrations")]
async fn source_summaries_count_documents_per_tenant(pool: PgPool) {
    let state = state(&pool);
    let a = tenant(&state, "acme").await;
    let b = tenant(&state, "globex").await;
    let app = ensure_source(&pool, a, "repo", "app", None).await.unwrap();
    ensure_source(&pool, a, "repo", "empty", None)
        .await
        .unwrap();
    ensure_source(&pool, b, "repo", "theirs", None)
        .await
        .unwrap();
    for (id, hash) in [("a.rs", "h1"), ("b.rs", "h2")] {
        index_text(&state, a, &app, doc(&app, id, hash), two_chunks(), id)
            .await
            .unwrap();
    }
    set_indexed_commit(&pool, app.id, "abc123").await.unwrap();

    let got = list_source_summaries(&pool, a).await.unwrap();
    let got: Vec<_> = got
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                s.kind.as_str(),
                s.indexed_commit.as_deref(),
                s.documents,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("app", "repo", Some("abc123"), 2),
            ("empty", "repo", None, 0)
        ]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn find_session_document_is_tenant_scoped_session_only_and_newest_first(pool: PgPool) {
    let st = state(&pool);
    let a = tenant(&st, "acme").await;
    let b = tenant(&st, "globex").await;
    let traces_1 = ensure_source(&pool, a, "traces", "traces-1", None)
        .await
        .unwrap();
    let traces_2 = ensure_source(&pool, a, "traces", "traces-2", None)
        .await
        .unwrap();
    let notes = ensure_source(&pool, a, "documents", "notes", None)
        .await
        .unwrap();
    let traces_b = ensure_source(&pool, b, "traces", "traces-b", None)
        .await
        .unwrap();

    let put = |source: &Source, kind: &str, hash: &str| {
        let mut d = doc(source, "sess-1", hash);
        d.kind = kind.to_string();
        d
    };
    let mut ids = Vec::new();
    for d in [
        put(&traces_1, "session", "h1"),
        put(&traces_2, "session", "h2"),
        put(&notes, "note", "h3"),
        put(&traces_b, "session", "h4"),
    ] {
        let mut tx = pool.begin().await.unwrap();
        ids.push(upsert_document(&mut tx, &d).await.unwrap());
        tx.commit().await.unwrap();
    }
    let (first, second, note, other_tenant) = (ids[0], ids[1], ids[2], ids[3]);
    let stamp = |id: Uuid, when: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query("UPDATE documents SET updated_at = $2::timestamptz WHERE id = $1")
                .bind(id)
                .bind(when)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    // The note is the newest of all, so only the kind filter keeps it out.
    stamp(first, "2026-01-01T00:00:00Z").await;
    stamp(second, "2026-02-01T00:00:00Z").await;
    stamp(note, "2026-03-01T00:00:00Z").await;
    stamp(other_tenant, "2026-04-01T00:00:00Z").await;

    // Newest session wins, and neither the note nor the other tenant's session is picked.
    assert_eq!(
        find_session_document(&pool, a, "sess-1").await.unwrap(),
        Some(second)
    );
    // Flip the order: the same query now picks the other source's document.
    stamp(first, "2026-05-01T00:00:00Z").await;
    assert_eq!(
        find_session_document(&pool, a, "sess-1").await.unwrap(),
        Some(first)
    );
    // Each tenant sees only its own.
    assert_eq!(
        find_session_document(&pool, b, "sess-1").await.unwrap(),
        Some(other_tenant)
    );
    // A tenant with no such session, and an id that does not exist.
    let c = tenant(&st, "initech").await;
    assert_eq!(
        find_session_document(&pool, c, "sess-1").await.unwrap(),
        None
    );
    assert_eq!(find_session_document(&pool, a, "nope").await.unwrap(), None);
    // A note alone is not a session.
    let only_note = ensure_source(&pool, c, "documents", "notes", None)
        .await
        .unwrap();
    let mut d = doc(&only_note, "sess-1", "h5");
    d.kind = "note".to_string();
    let mut tx = pool.begin().await.unwrap();
    upsert_document(&mut tx, &d).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        find_session_document(&pool, c, "sess-1").await.unwrap(),
        None
    );
}

fn span(name: &str, pending: bool) -> lore::otlp::SpanRow {
    let now = chrono::Utc::now();
    lore::otlp::SpanRow {
        trace_id: "t1".to_string(),
        span_id: "s1".to_string(),
        parent_span_id: None,
        session_id: "sess-1".to_string(),
        name: name.to_string(),
        kind: "internal".to_string(),
        start_time: now,
        end_time: now,
        status_code: "ok".to_string(),
        status_message: None,
        resource: Arc::new(json!({})),
        attributes: if pending {
            json!({ "glassflow.span.pending": true })
        } else {
            json!({})
        },
        events: json!([]),
    }
}

async fn put_spans(pool: &PgPool, tenant_id: Uuid, rows: &[lore::otlp::SpanRow]) {
    let mut tx = pool.begin().await.unwrap();
    lore::store::upsert_spans(&mut tx, tenant_id, rows)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn stored_span(pool: &PgPool, tenant_id: Uuid) -> String {
    sqlx::query_scalar("SELECT name FROM spans WHERE tenant_id = $1 AND span_id = 's1'")
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_pending_span_never_replaces_a_finished_one_in_the_database(pool: PgPool) {
    let st = state(&pool);
    let t = tenant(&st, "acme").await;

    put_spans(&pool, t, &[span("finished", false)]).await;
    put_spans(&pool, t, &[span("late snapshot", true)]).await;
    assert_eq!(stored_span(&pool, t).await, "finished");

    // A finished span still replaces a finished one.
    put_spans(&pool, t, &[span("finished again", false)]).await;
    assert_eq!(stored_span(&pool, t).await, "finished again");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_finished_span_replaces_a_pending_one_and_a_pending_one_replaces_a_pending_one(
    pool: PgPool,
) {
    let st = state(&pool);
    let t = tenant(&st, "acme").await;

    put_spans(&pool, t, &[span("first snapshot", true)]).await;
    put_spans(&pool, t, &[span("second snapshot", true)]).await;
    assert_eq!(stored_span(&pool, t).await, "second snapshot");
    put_spans(&pool, t, &[span("finished", false)]).await;
    assert_eq!(stored_span(&pool, t).await, "finished");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_pending_copy_after_a_finished_one_in_the_same_batch_loses(pool: PgPool) {
    let st = state(&pool);
    let t = tenant(&st, "acme").await;

    put_spans(&pool, t, &[span("finished", false), span("snapshot", true)]).await;
    assert_eq!(stored_span(&pool, t).await, "finished");

    // The last copy still wins among spans of the same kind, in either order.
    let other = tenant(&st, "globex").await;
    put_spans(
        &pool,
        other,
        &[span("snapshot", true), span("finished", false)],
    )
    .await;
    assert_eq!(stored_span(&pool, other).await, "finished");
    put_spans(&pool, other, &[span("one", false), span("two", false)]).await;
    assert_eq!(stored_span(&pool, other).await, "two");
    put_spans(&pool, other, &[span("three", true), span("four", true)]).await;
    assert_eq!(stored_span(&pool, other).await, "two");
}
