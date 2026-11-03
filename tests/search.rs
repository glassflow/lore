mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    bundle, commit_all, create_tenant_key, fixture_repo, head, push_fixture, put_bundle, send,
    test_router, test_state,
};
use lore::chunk::{Chunk, ChunkKind};
use lore::embed::{Embedder, FastEmbedder, HashEmbedder};
use lore::http::AppState;
use lore::search::{SearchQuery, search};
use lore::store::{NewDocument, ensure_source, index_text};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const PLANS: &str = "src/billing/plans.rs";
const PRICING: &str = "docs/pricing.md";

/// Percent-encodes everything but unreserved characters, for a query string value.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `GET /v1/search?<query>` with a key.
async fn get(router: &Router, key: &str, query: &str) -> (StatusCode, Value) {
    send(
        router,
        "GET",
        &format!("/v1/search?{query}"),
        key,
        Vec::new(),
    )
    .await
}

/// `GET /v1/search?q=<q>` with a key; the search must succeed.
async fn hits(router: &Router, key: &str, q: &str, extra: &str) -> Vec<Value> {
    let (status, body) = get(router, key, &format!("q={}{extra}", enc(q))).await;
    assert_eq!(status, StatusCode::OK, "{q:?}{extra}: {body}");
    body["hits"].as_array().expect("hits is an array").clone()
}

fn paths(hits: &[Value]) -> Vec<&str> {
    hits.iter()
        .map(|h| h["path"].as_str().unwrap_or("-"))
        .collect()
}

fn chunk(text: &str, start: u32, end: u32, symbols: &[&str]) -> Chunk {
    Chunk {
        text: text.to_string(),
        start_line: start,
        end_line: end,
        symbols: symbols.iter().map(|s| s.to_string()).collect(),
        kind: ChunkKind::Lines,
        enumerates: false,
    }
}

/// A document of `kind` in the source `source_name` (kind `documents`) with the given chunks.
/// Returns the tenant's source id. Every argument is a field of the document, so the test
/// calls read as a table.
#[allow(clippy::too_many_arguments)]
async fn add_doc(
    state: &AppState,
    tenant_id: Uuid,
    source_name: &str,
    external_id: &str,
    kind: &str,
    title: &str,
    uri: Option<&str>,
    chunks: Vec<Chunk>,
) -> Uuid {
    let source = ensure_source(&state.pool, tenant_id, "documents", source_name, None)
        .await
        .unwrap();
    let doc = NewDocument {
        tenant_id,
        source_id: source.id,
        external_id: external_id.to_string(),
        kind: kind.to_string(),
        title: title.to_string(),
        uri: uri.map(str::to_string),
        metadata: json!({}),
        content_hash: format!("hash-{external_id}"),
        body: None,
    };
    index_text(state, tenant_id, &source, doc, chunks, title)
        .await
        .unwrap();
    source.id
}

/// A router over a state with the hash embedder, one tenant `acme` and a read+ingest key.
async fn setup(pool: PgPool) -> (AppState, Router, Uuid, String) {
    let state = test_state(pool, &std::env::temp_dir());
    let router = test_router(state.clone());
    let (tenant, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    (state, router, tenant, key)
}

#[sqlx::test(migrations = "./migrations")]
async fn plan_limits_ranks_plans_rs_first_with_the_documented_shape(pool: PgPool) {
    let (_state, router, _tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;

    let found = hits(&router, &key, "plan limits", "").await;
    assert!(found.len() <= 10);
    assert_eq!(found[0]["path"], PLANS);
    // The `Plan` enum above it may rank first: it is an enumeration, and its doc comment is about
    // plan limits too. The shape is checked on the function's hit.
    let first = found
        .iter()
        .find(|h| h["start_line"] == 7)
        .expect("the limits function is a hit");
    assert_eq!(first["path"], PLANS);
    assert_eq!(first["source"], "sample");
    assert_eq!(first["kind"], "code");
    assert_eq!(first["title"], PLANS);
    assert!(first["chunk_id"].is_i64());
    assert!(first["document_id"].is_string());
    // The function chunk starts at its doc comment and ends at the closing brace.
    assert_eq!(first["start_line"], 7);
    assert_eq!(first["end_line"], 14);
    assert!(
        first["symbols"]
            .as_array()
            .unwrap()
            .contains(&json!("limits"))
    );
    assert!(first["snippet"].as_str().unwrap().contains("pub fn limits"));
    assert!(first.get("text").is_none(), "text is replaced by snippet");
    // A repo without a web URL template has no link.
    assert_eq!(first["uri"], Value::Null);

    // Best first, and the fusion score only ever adds reciprocal ranks.
    let scores: Vec<f64> = found.iter().map(|h| h["score"].as_f64().unwrap()).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
    assert!(
        scores[0] > 1.0 / 61.0,
        "the top hit is in more than one list"
    );
    assert!(scores[0] <= 3.0 / 61.0 + 1e-12);
}

#[sqlx::test(migrations = "./migrations")]
async fn doc_kind_filter_finds_the_pricing_page(pool: PgPool) {
    let (_state, router, _tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;

    let found = hits(&router, &key, "pricing plans", "&kind=doc").await;
    assert_eq!(found[0]["path"], PRICING);
    assert!(found.iter().all(|h| h["kind"] == "doc"), "{found:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn kind_filter_keeps_only_the_listed_kinds(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;
    add_doc(
        &state,
        tenant,
        "notes",
        "n1",
        "note",
        "Plan limits note",
        None,
        vec![chunk("plan limits were raised last week", 1, 1, &[])],
    )
    .await;
    add_doc(
        &state,
        tenant,
        "notes",
        "s1",
        "session",
        "Plan limits session",
        None,
        vec![chunk("we talked about plan limits", 1, 1, &[])],
    )
    .await;

    let kinds_of = |found: &[Value]| -> Vec<String> {
        let mut kinds: Vec<String> = found
            .iter()
            .map(|h| h["kind"].as_str().unwrap().to_string())
            .collect();
        kinds.sort();
        kinds.dedup();
        kinds
    };

    let all = hits(&router, &key, "plan limits", "&k=50").await;
    assert_eq!(kinds_of(&all), ["code", "doc", "note", "session"]);

    // Code is excluded unless asked for.
    let found = hits(&router, &key, "plan limits", "&kind=note").await;
    assert_eq!(kinds_of(&found), ["note"]);
    assert_eq!(found[0]["title"], "Plan limits note");
    let found = hits(&router, &key, "plan limits", "&kind=note,session").await;
    assert_eq!(kinds_of(&found), ["note", "session"]);
    let found = hits(&router, &key, "plan limits", "&kind=code").await;
    assert_eq!(kinds_of(&found), ["code"]);
    assert_eq!(found[0]["path"], PLANS);

    // An empty value is no filter; a name outside the vocabulary is a 400 that lists the kinds.
    let found = hits(&router, &key, "plan limits", "&kind=&k=50").await;
    assert_eq!(kinds_of(&found), ["code", "doc", "note", "session"]);
    let (status, body) = get(&router, &key, "q=plan&kind=code,docs").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["error"].as_str().unwrap();
    assert!(message.contains("\"docs\""), "{message}");
    assert!(message.contains("code, doc, session, note"), "{message}");
}

#[sqlx::test(migrations = "./migrations")]
async fn source_filter_matches_the_source_name(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;
    add_doc(
        &state,
        tenant,
        "wiki",
        "w1",
        "doc",
        "Wiki plan limits",
        None,
        vec![chunk("plan limits on the wiki", 1, 1, &[])],
    )
    .await;

    let found = hits(&router, &key, "plan limits", "&source=wiki&k=50").await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["source"], "wiki");

    let found = hits(&router, &key, "plan limits", "&source=sample&k=50").await;
    assert!(found.len() > 1);
    assert!(found.iter().all(|h| h["source"] == "sample"), "{found:?}");

    // A name that matches no source gives no hits, not an error.
    let found = hits(&router, &key, "plan limits", "&source=nope").await;
    assert!(found.is_empty());
    // Matching is exact, not a prefix.
    let found = hits(&router, &key, "plan limits", "&source=wik").await;
    assert!(found.is_empty());
    // A blank source is no filter.
    let found = hits(&router, &key, "plan limits", "&source=&k=50").await;
    assert!(found.iter().any(|h| h["source"] == "wiki"));
    assert!(found.iter().any(|h| h["source"] == "sample"));
}

#[sqlx::test(migrations = "./migrations")]
async fn k_defaults_to_10_and_is_clamped_to_1_through_50(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    // 60 chunks that all contain the word, so both branches fill up.
    let chunks: Vec<Chunk> = (1..=60)
        .map(|i| chunk(&format!("limit number {i}"), i, i, &[]))
        .collect();
    add_doc(
        &state, tenant, "notes", "many", "note", "Many", None, chunks,
    )
    .await;

    let count = |k: &str| {
        let router = router.clone();
        let key = key.clone();
        let extra = k.to_string();
        async move { hits(&router, &key, "limit", &extra).await.len() }
    };
    assert_eq!(count("").await, 10, "default");
    assert_eq!(count("&k=3").await, 3);
    assert_eq!(count("&k=50").await, 50);
    assert_eq!(count("&k=51").await, 50, "capped at 50");
    assert_eq!(count("&k=100000").await, 50);
    assert_eq!(count("&k=0").await, 1, "raised to 1");
    assert_eq!(count("&k=-7").await, 1, "negative is raised to 1");

    // Not a number: rejected by the query extractor.
    let (status, _) = get(&router, &key, "q=limit&k=lots").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The library function clamps too.
    let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder);
    for (k, want) in [(0, 1), (500, 50)] {
        let query = SearchQuery {
            tenant_id: tenant,
            q: "limit".to_string(),
            k,
            kinds: None,
            source: None,
        };
        let found = search(&state.pool, embedder.clone(), query).await.unwrap();
        assert_eq!(found.len(), want, "k = {k}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_second_tenant_sees_nothing_of_the_first(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;
    add_doc(
        &state,
        tenant,
        "notes",
        "n1",
        "note",
        "Zebra",
        None,
        vec![chunk("zebra crossing plan limits", 1, 1, &[])],
    )
    .await;
    let (other, other_key) = create_tenant_key(&router, "other", &["read", "ingest"]).await;

    // Nothing indexed yet: empty, whatever the filters name.
    assert!(
        hits(&router, &other_key, "plan limits", "")
            .await
            .is_empty()
    );
    assert!(
        hits(&router, &other_key, "zebra", "&source=sample&kind=code")
            .await
            .is_empty()
    );
    assert!(
        hits(&router, &other_key, "plan limits", "&source=notes")
            .await
            .is_empty()
    );

    // With documents of its own, the other tenant sees only those, even when it names a source
    // that exists for the first tenant.
    add_doc(
        &state,
        other,
        "notes",
        "n9",
        "note",
        "Giraffe",
        None,
        vec![chunk("giraffe neck length", 1, 1, &[])],
    )
    .await;
    let found = hits(
        &router,
        &other_key,
        "zebra plan limits",
        "&source=notes&k=50",
    )
    .await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["title"], "Giraffe");
    let found = hits(&router, &other_key, "zebra plan limits", "&k=50").await;
    assert!(found.iter().all(|h| h["title"] == "Giraffe"), "{found:?}");

    // The first tenant still finds its own.
    let found = hits(&router, &key, "zebra", "&source=notes").await;
    assert_eq!(found[0]["title"], "Zebra");

    // Through the library call too.
    let query = SearchQuery {
        tenant_id: other,
        q: "plan limits".to_string(),
        k: 50,
        kinds: Some(vec!["code".to_string()]),
        source: Some("sample".to_string()),
    };
    let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder);
    assert!(
        search(&state.pool, embedder, query)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn another_tenants_chunks_do_not_crowd_out_a_hit(pool: PgPool) {
    let (state, router, tenant, _key) = setup(pool).await;
    // 60 chunks that match the query better than anything the other tenant has. Each branch
    // keeps 50 rows, so unless the tenant filter is part of the branch queries, they fill both.
    let chunks: Vec<Chunk> = (1..=60)
        .map(|i| chunk("zebra crossing", i, i, &[]))
        .collect();
    add_doc(
        &state, tenant, "notes", "zebras", "note", "Zebras", None, chunks,
    )
    .await;
    let (other, other_key) = create_tenant_key(&router, "other", &["read", "ingest"]).await;
    add_doc(
        &state,
        other,
        "notes",
        "stripes",
        "note",
        "Stripes",
        None,
        vec![chunk("zebra stripes", 1, 1, &[])],
    )
    .await;

    let found = hits(&router, &other_key, "zebra crossing", "&k=50").await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["title"], "Stripes");
    // Rank 1 in both branches: neither branch lost it to the other tenant's chunks.
    let score = found[0]["score"].as_f64().unwrap();
    assert!((score - 2.0 / 61.0).abs() < 1e-12, "{score}");
}

#[sqlx::test(migrations = "./migrations")]
async fn only_stop_words_search_by_vector_alone(pool: PgPool) {
    let (state, router, _tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;

    let lexemes: Vec<String> =
        sqlx::query_scalar("SELECT tsvector_to_array(to_tsvector('english', 'the and of'))")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(lexemes.is_empty(), "the premise: no lexemes");

    let found = hits(&router, &key, "the and of", "").await;
    assert!(!found.is_empty(), "the vector branch still answers");
    // Without a lexical branch the best possible score is vector rank 1, plus rank 1 of the
    // enumeration list, which is drawn from the vector hits.
    let top = found[0]["score"].as_f64().unwrap();
    assert!(
        (1.0 / 61.0 - 1e-12..=2.0 / 61.0 + 1e-12).contains(&top),
        "{top}"
    );
}

/// Three notes with the same text, so they tie on full-text rank and on distance.
async fn add_twins(state: &AppState, tenant: Uuid) {
    for i in 1..=3 {
        add_doc(
            state,
            tenant,
            "notes",
            &format!("twin{i}"),
            "note",
            &format!("Twin {i}"),
            None,
            vec![chunk("identical text about the heron", 1, 1, &[])],
        )
        .await;
    }
}

/// The chunk ids of the hits, which must be three and in ascending order.
fn assert_ascending_ids(found: &[Value]) {
    let ids: Vec<i64> = found
        .iter()
        .map(|h| h["chunk_id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn equal_chunks_are_ordered_by_chunk_id(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    add_twins(&state, tenant).await;

    // Equal full-text rank and equal distance in both branches: the lower id comes first.
    assert_ascending_ids(&hits(&router, &key, "heron", "").await);
}

#[sqlx::test(migrations = "./migrations")]
async fn vector_ties_are_ordered_by_chunk_id(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    add_twins(&state, tenant).await;

    // With no lexemes only the vector branch ranks the twins, and its ties go by id too.
    assert_ascending_ids(&hits(&router, &key, "the and of", "&kind=note").await);
}

#[sqlx::test(migrations = "./migrations")]
async fn camel_case_identifiers_are_found_by_their_words(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;
    add_doc(
        &state,
        tenant,
        "notes",
        "n1",
        "note",
        "Camel",
        None,
        vec![chunk(
            "const quotaGate = BurstWindowLimit;",
            1,
            1,
            &["SeatAllowanceChecker"],
        )],
    )
    .await;

    // Words inside a camelCase name in the text, and inside a symbol, reach the full-text index.
    // Notes only: the fixture's enumerations can be lifted for any query in so small a tenant.
    for q in ["quota gate", "burst window", "allowance"] {
        let found = hits(&router, &key, q, "&kind=note").await;
        assert_eq!(found[0]["title"], "Camel", "{q}");
        assert!(found[0]["score"].as_f64().unwrap() > 1.0 / 61.0, "{q}");
    }

    // The identifier typed whole is one lexeme and still matches itself.
    let found = hits(&router, &key, "BurstWindowLimit", "").await;
    assert_eq!(found[0]["title"], "Camel");
    assert!(found[0]["score"].as_f64().unwrap() > 1.0 / 61.0);
    // A camelCase query is not split: it only matches text that has the whole identifier.
    let found = hits(&router, &key, "quotaGateway", "&kind=note").await;
    assert!(
        found
            .iter()
            .all(|h| h["score"].as_f64().unwrap() <= 1.0 / 61.0 + 1e-12),
        "quotaGateway has no lexical match: {found:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn web_url_is_rendered_with_the_chunk_lines_and_an_escaped_path(pool: PgPool) {
    let (_state, router, _tenant, key) = setup(pool).await;
    let template = "https://git.example.com/acme/sample/blob/{commit}/{path}#L{start}-L{end}";
    let body = json!({ "name": "sample", "web_url_template": template });
    let (status, repo_json) = send(
        &router,
        "POST",
        "/v1/repos",
        &key,
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{repo_json}");
    let source_id: Uuid = repo_json["id"].as_str().unwrap().parse().unwrap();

    let repo = fixture_repo();
    let odd = "docs/q&a #1 %é?.md";
    std::fs::write(repo.path().join(odd), "Quokka facts\n").unwrap();
    commit_all(&repo, "add an oddly named file");
    let sha = head(&repo);
    let (status, report) = put_bundle(
        &router,
        &key,
        source_id,
        bundle(&repo, &["HEAD"]),
        &sha,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");

    let found = hits(&router, &key, "plan limits", "").await;
    assert_eq!(found[0]["path"], PLANS);
    assert_eq!(
        found[0]["uri"],
        format!(
            "https://git.example.com/acme/sample/blob/{sha}/src/billing/plans.rs#L{}-L{}",
            found[0]["start_line"], found[0]["end_line"]
        )
    );

    // Spaces, #, ?, % and non-ASCII are percent-encoded; the / between segments is kept.
    let found = hits(&router, &key, "Quokka", "").await;
    assert_eq!(found[0]["path"], odd);
    assert_eq!(
        found[0]["uri"],
        format!(
            "https://git.example.com/acme/sample/blob/{sha}/docs/q%26a%20%231%20%25%C3%A9%3F.md#L1-L1"
        )
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn without_a_rendered_url_the_document_uri_is_used(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    // A source with a template, but the note has no path or commit to fill it with.
    let source = ensure_source(
        &state.pool,
        tenant,
        "documents",
        "wiki",
        Some("https://wiki.example.com/{path}"),
    )
    .await
    .unwrap();
    let note = |external_id: &str, uri: Option<&str>| NewDocument {
        tenant_id: tenant,
        source_id: source.id,
        external_id: external_id.to_string(),
        kind: "note".to_string(),
        title: format!("Note {external_id}"),
        uri: uri.map(str::to_string),
        metadata: json!({}),
        content_hash: external_id.to_string(),
        body: None,
    };
    for (id, uri, text) in [
        ("a", Some("https://wiki.example.com/a"), "alpaca fleece"),
        ("b", None, "badger sett"),
    ] {
        let chunks = vec![chunk(text, 1, 1, &[])];
        index_text(&state, tenant, &source, note(id, uri), chunks, id)
            .await
            .unwrap();
    }

    let found = hits(&router, &key, "alpaca", "").await;
    assert_eq!(found[0]["title"], "Note a");
    assert_eq!(found[0]["uri"], "https://wiki.example.com/a");
    assert_eq!(found[0]["path"], Value::Null);
    let found = hits(&router, &key, "badger", "").await;
    assert_eq!(found[0]["title"], "Note b");
    assert_eq!(found[0]["uri"], Value::Null);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_snippet_is_the_first_40_lines(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    let text: String = (1..=100).map(|i| format!("marmot line {i}\n")).collect();
    add_doc(
        &state,
        tenant,
        "notes",
        "long",
        "note",
        "Long",
        None,
        vec![chunk(&text, 1, 100, &[])],
    )
    .await;

    let found = hits(&router, &key, "marmot", "").await;
    let snippet = found[0]["snippet"].as_str().unwrap();
    assert_eq!(snippet.lines().count(), 40);
    assert!(snippet.starts_with("marmot line 1\n"));
    assert!(snippet.ends_with("marmot line 40"));
    assert_eq!(found[0]["start_line"], 1);
    assert_eq!(found[0]["end_line"], 100);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_route_needs_a_key_with_the_read_scope(pool: PgPool) {
    let (_state, router, _tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;

    // No key, and a key the server does not know.
    let req = Request::get("/v1/search?q=plan")
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let (status, _) = get(
        &router,
        "lk_0000000000000000000000000000000000000000",
        "q=plan",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A key with only the ingest scope is known but not allowed: 403.
    let (_, ingest_only) = create_tenant_key(&router, "writer", &["ingest"]).await;
    let (status, body) = get(&router, &ingest_only, "q=plan").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A read-only key works.
    let (_, read_only) = create_tenant_key(&router, "reader", &["read"]).await;
    let (status, _) = get(&router, &read_only, "q=plan").await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_missing_or_blank_q_is_a_400(pool: PgPool) {
    let (_state, router, _tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;

    for query in ["", "k=5", "q=", "q=%20%20", "q=%09%0A", "q=&k=5&kind=code"] {
        let (status, body) = get(&router, &key, query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query:?}: {body}");
        assert_eq!(body["error"], "q is required", "{query:?}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn hostile_queries_never_cause_a_500(pool: PgPool) {
    let (state, router, _tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;
    let chunks_before: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&state.pool)
        .await
        .unwrap();

    let hostile = [
        "'",
        "''''",
        "\\",
        "\\'",
        "plan\\",
        "it's",
        "' | 'plan",
        "plan' & !limits",
        // URLs are the one place where a lexeme keeps quotes and tsquery operators.
        "http://a.com/it's?q=1&r=(2)|!x",
        "http://a.com/p'q:*",
        "a & b | !c",
        "plan <-> limits",
        "(((",
        ")))",
        ":*",
        "plan:*",
        "!",
        "&",
        "|",
        "*",
        "'; DROP TABLE chunks; --",
        "\" OR 1=1 --",
        "$1 $2 ${x}",
        "%s %d %n",
        "<script>alert(1)</script>",
        "\u{e9}\u{4e2d}\u{6587} \u{1f600}",
        "limits\u{0301}",
        "\u{feff}plan",
        "\t\r\n plan \r\n",
    ];
    for q in hostile {
        let (status, body) = get(&router, &key, &format!("q={}", enc(q))).await;
        // Blank after trimming is the one 400; everything else must be answered.
        let want = if q.trim().is_empty() {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        };
        assert_eq!(status, want, "{q:?}: {body}");
    }

    // Long queries: 2000 characters are accepted, 2001 are not, in ASCII and in multi-byte text.
    let long_words = "limit ".repeat(333);
    assert_eq!(long_words.chars().count(), 1998);
    let cases = [
        ("a".repeat(2000), StatusCode::OK),
        ("a".repeat(2001), StatusCode::BAD_REQUEST),
        (long_words, StatusCode::OK),
        ("\u{e9}".repeat(2000), StatusCode::OK),
        ("\u{e9}".repeat(2001), StatusCode::BAD_REQUEST),
        ("a".repeat(30_000), StatusCode::BAD_REQUEST),
    ];
    for (q, want) in cases {
        let (status, body) = get(&router, &key, &format!("q={}", enc(&q))).await;
        assert_eq!(status, want, "{} chars: {body}", q.chars().count());
        if want == StatusCode::BAD_REQUEST {
            assert!(body["error"].as_str().unwrap().contains("2000"), "{body}");
        }
    }

    // A NUL cannot be stored in text, so it is rejected up front, in q and in source.
    let (status, _) = get(&router, &key, "q=plan%00limits").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&router, &key, "q=plan&source=a%00b").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Hostile filters are only data too.
    let (status, _) = get(
        &router,
        &key,
        "q=plan&source=%27%3B%20DROP%20TABLE%20chunks",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&router, &key, "q=plan&kind=%27,code").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Nothing was changed, and the search still works.
    let chunks_after: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(chunks_after, chunks_before);
    assert_eq!(
        hits(&router, &key, "plan limits", "").await[0]["path"],
        PLANS
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn hostile_terms_still_match_their_plain_words(pool: PgPool) {
    let (state, router, tenant, key) = setup(pool).await;
    push_fixture(&router, &key).await;
    add_doc(
        &state,
        tenant,
        "notes",
        "url",
        "note",
        "Url note",
        None,
        vec![chunk(
            "see http://a.com/it's?q=1&r=(2)|!x for more",
            1,
            1,
            &[],
        )],
    )
    .await;

    // A lexeme with a quote and tsquery operators in it is matched as the text it is.
    for q in [
        "http://a.com/it's?q=1&r=(2)|!x",
        "see http://a.com/it's?q=1&r=(2)",
    ] {
        let found = hits(&router, &key, q, "").await;
        assert_eq!(found[0]["title"], "Url note", "{q:?}");
        assert!(found[0]["score"].as_f64().unwrap() > 1.0 / 61.0, "{q:?}");
    }

    // Tsquery syntax around a word does not change what the word matches.
    for q in [
        "!!plan limits))",
        "plan' | 'limits",
        "plan\\ limits\\",
        "(plan & limits)",
    ] {
        let found = hits(&router, &key, q, "").await;
        assert_eq!(found[0]["path"], PLANS, "{q:?}");
    }
}

/// The same checks as the two ranking tests above, with the real model. Run it with
/// `cargo test -- --ignored fastembed_ranking`; the first run downloads about 130 MB.
#[ignore = "downloads the bge-small-en-v1.5 model (about 130 MB) on first run"]
#[sqlx::test(migrations = "./migrations")]
async fn fastembed_ranking(pool: PgPool) {
    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".fastembed_cache");
    let embedder = FastEmbedder::new(&cache).expect("the model loads");
    let mut state = test_state(pool, &std::env::temp_dir());
    state.embedder = Arc::new(embedder);
    let router = test_router(state);
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    push_fixture(&router, &key).await;

    let found = hits(&router, &key, "plan limits", "").await;
    assert_eq!(found[0]["path"], PLANS, "{:?}", paths(&found));

    let found = hits(&router, &key, "pricing plans", "&kind=doc").await;
    assert_eq!(found[0]["path"], PRICING, "{:?}", paths(&found));
}
