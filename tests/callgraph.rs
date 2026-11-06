mod common;

use std::time::{Duration, Instant};

use common::{create_tenant_key, test_router, test_state};
use lore::callgraph::{self, Resolution, Scope, parse_symbol};
use lore::chunk::chunk_file;
use lore::http::AppState;
use lore::search::{SearchQuery, search};
use lore::store::{NewDocument, ensure_source, index_text};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn add(state: &AppState, tenant: Uuid, source: &str, path: &str, text: &str) {
    let source = ensure_source(&state.pool, tenant, "repo", source, None)
        .await
        .unwrap();
    let chunks = chunk_file(path, text);
    index_text(
        state,
        tenant,
        &source,
        NewDocument {
            tenant_id: tenant,
            source_id: source.id,
            external_id: path.into(),
            kind: "code".into(),
            title: path.into(),
            uri: None,
            metadata: json!({"path":path}),
            content_hash: text.into(),
            body: None,
        },
        chunks,
        path,
    )
    .await
    .unwrap();
}

async fn setup(pool: PgPool) -> (AppState, Uuid) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) =
        create_tenant_key(&test_router(state.clone()), "graph", &["read", "ingest"]).await;
    (state, tenant)
}

#[sqlx::test(migrations = "./migrations")]
async fn extracted_aliases_resolve_and_related_search_finds_both_directions(pool: PgPool) {
    let (state, tenant) = setup(pool).await;
    add(&state, tenant, "app", "src/db.rs", "pub fn persist() {}\n").await;
    add(
        &state,
        tenant,
        "app",
        "src/orders.rs",
        "use crate::db::persist as save;\npub fn checkout() { save(); }\n",
    )
    .await;
    add(
        &state,
        tenant,
        "app",
        "src/http.rs",
        "use crate::orders::checkout;\npub fn handler() { checkout(); }\n",
    )
    .await;
    let scope = Scope {
        tenant_id: tenant,
        kinds: None,
        source: Some("app"),
    };
    let result = callgraph::callers(&state.pool, &scope, &parse_symbol("persist").unwrap())
        .await
        .unwrap();
    assert_eq!(result.definitions.len(), 1);
    assert_eq!(result.callers.len(), 1);
    assert_eq!(result.callers[0].resolution, Resolution::Imported);
    assert_eq!(
        result.callers[0].site.path.as_deref(),
        Some("src/orders.rs")
    );
    let ids = callgraph::related(&state.pool, &scope, "where checkout")
        .await
        .unwrap();
    assert_eq!(ids.len(), 3, "definition, caller and callee");
    let connected = callgraph::related(&state.pool, &scope, "how does checkout reach persist?")
        .await
        .unwrap();
    assert_eq!(connected.len(), 3, "natural-language connection intent");
    let search_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(state.pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let hits = tokio::time::timeout(
        Duration::from_secs(5),
        search(
            &search_pool,
            state.embedder.clone(),
            SearchQuery {
                tenant_id: tenant,
                q: "checkout".into(),
                k: 3,
                kinds: None,
                source: Some("app".into()),
            },
        ),
    )
    .await
    .expect("lexical, vector, related and hydration must share one connection")
    .unwrap();
    assert_eq!(hits[0].path.as_deref(), Some("src/orders.rs"));
    assert_eq!(hits.len(), 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn scopes_ambiguity_replacement_and_deletion_are_respected(pool: PgPool) {
    let (state, tenant) = setup(pool).await;
    add(&state, tenant, "app", "src/a.rs", "pub fn perform() {}\n").await;
    add(&state, tenant, "app", "src/b.rs", "pub fn perform() {}\n").await;
    add(
        &state,
        tenant,
        "app",
        "src/consumer.rs",
        "pub fn invoke() { perform(); }\n",
    )
    .await;
    add(
        &state,
        tenant,
        "other",
        "src/a.rs",
        "pub fn unrelated() { perform(); }\n",
    )
    .await;
    let scope = Scope {
        tenant_id: tenant,
        kinds: None,
        source: None,
    };
    let symbol = parse_symbol("perform").unwrap();
    let result = callgraph::callers(&state.pool, &scope, &symbol)
        .await
        .unwrap();
    assert_eq!(result.definitions.len(), 2);
    assert_eq!(result.callers.len(), 1, "never resolve across repo sources");
    assert_eq!(result.callers[0].resolution, Resolution::Ambiguous);
    assert_eq!(result.callers[0].definitions, 2);
    let qualified = callgraph::callers(&state.pool, &scope, &parse_symbol("a::perform").unwrap())
        .await
        .unwrap();
    assert_eq!(qualified.definitions.len(), 1);
    assert_eq!(qualified.callers[0].resolution, Resolution::Ambiguous);
    let (other_tenant, _) =
        create_tenant_key(&test_router(state.clone()), "private", &["read"]).await;
    assert!(
        callgraph::definitions(
            &state.pool,
            &Scope {
                tenant_id: other_tenant,
                ..scope
            },
            &symbol
        )
        .await
        .unwrap()
        .is_empty()
    );
    let kinds = vec!["doc".into()];
    assert!(
        callgraph::related(
            &state.pool,
            &Scope {
                kinds: Some(&kinds),
                ..scope
            },
            "perform"
        )
        .await
        .unwrap()
        .is_empty()
    );
    add(
        &state,
        tenant,
        "app",
        "src/consumer.rs",
        "pub fn invoke() {}\n",
    )
    .await;
    assert!(
        callgraph::callers(&state.pool, &scope, &symbol)
            .await
            .unwrap()
            .callers
            .is_empty()
    );
    sqlx::query("DELETE FROM documents WHERE tenant_id = $1 AND external_id = 'src/a.rs'")
        .bind(tenant)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        callgraph::definitions(&state.pool, &scope, &symbol)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn capped_definition_sets_do_not_infer_unique_edges(pool: PgPool) {
    let (state, tenant) = setup(pool).await;
    add(
        &state,
        tenant,
        "app",
        "src/defs.rs",
        "pub fn common_name() {}\n",
    )
    .await;
    add(
        &state,
        tenant,
        "app",
        "src/caller.rs",
        "pub fn caller() { common_name(); }\n",
    )
    .await;
    // Many same-named definitions, without embedding extra content.
    sqlx::query(
        "INSERT INTO chunks (tenant_id,document_id,ord,text,tsv,embedding,defines)
        SELECT tenant_id,document_id,n,text,tsv,embedding,defines FROM chunks
        CROSS JOIN generate_series(1,100) n WHERE defines @> ARRAY['common_name']",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let scope = Scope {
        tenant_id: tenant,
        kinds: None,
        source: None,
    };
    let result = callgraph::callers(&state.pool, &scope, &parse_symbol("common_name").unwrap())
        .await
        .unwrap();
    assert!(result.truncated);
    assert!(result.callers.is_empty());
    let absent = callgraph::definition_lookup(
        &state.pool,
        &scope,
        &parse_symbol("absent::common_name").unwrap(),
    )
    .await
    .unwrap();
    assert!(absent.definitions.is_empty());
    assert!(absent.truncated);
    assert!(
        callgraph::related(&state.pool, &scope, "common_name")
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn callers_are_bounded_and_report_truncation(pool: PgPool) {
    let (state, tenant) = setup(pool).await;
    add(
        &state,
        tenant,
        "app",
        "src/defs.rs",
        "pub fn operation() {}\n",
    )
    .await;
    add(
        &state,
        tenant,
        "app",
        "src/caller.rs",
        "pub fn caller() { operation(); }\n",
    )
    .await;
    sqlx::query(
        "INSERT INTO chunks (tenant_id,document_id,ord,text,tsv,embedding,calls)
        SELECT tenant_id,document_id,n,text,tsv,embedding,calls FROM chunks
        CROSS JOIN generate_series(1,210) n WHERE calls @> ARRAY['operation']",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let scope = Scope {
        tenant_id: tenant,
        kinds: None,
        source: None,
    };
    let result = callgraph::callers(&state.pool, &scope, &parse_symbol("operation").unwrap())
        .await
        .unwrap();
    assert_eq!(result.callers.len(), 200);
    assert!(result.truncated);
    assert!(
        result
            .callers
            .iter()
            .all(|c| c.resolution == Resolution::Unique)
    );
}

/// Reproducible local gate for the added branch, including its first call. Run separately to
/// avoid measuring concurrent integration-test database creation and index builds.
#[sqlx::test(migrations = "./migrations")]
#[ignore = "latency gate; run alone with --ignored --nocapture"]
async fn related_latency_under_50ms_on_20000_chunks(pool: PgPool) {
    let (state, tenant) = setup(pool).await;
    add(&state, tenant, "app", "src/db.rs", "pub fn persist() {}\n").await;
    add(
        &state,
        tenant,
        "app",
        "src/main.rs",
        "pub fn checkout() { persist(); }\n",
    )
    .await;
    sqlx::query("INSERT INTO chunks (tenant_id,document_id,ord,text,tsv,embedding,defines,calls)
        SELECT tenant_id,document_id,n,text,tsv,embedding,ARRAY['background_' || n],ARRAY['unrelated']
        FROM chunks CROSS JOIN generate_series(1,10000) n").execute(&state.pool).await.unwrap();
    // Worst bounded expansion: eight names, five definitions each, fifty caller candidates,
    // 256 entries per metadata array (up to 128 bytes), and 64 selected callee names.
    sqlx::query(
        "INSERT INTO chunks (tenant_id,document_id,ord,text,tsv,embedding,defines,calls)
        SELECT tenant_id,document_id,n,text,tsv,embedding,ARRAY['heavy_' || (n % 8)] ||
            ARRAY(SELECT lpad(i::text,128,'d') FROM generate_series(1,255) i),
            ARRAY(SELECT lpad(i::text,128,'c') FROM generate_series(1,256) i)
        FROM chunks CROSS JOIN generate_series(1,40) n WHERE defines = ARRAY['persist']",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chunks (tenant_id,document_id,ord,text,tsv,embedding,calls)
        SELECT tenant_id,document_id,n,text,tsv,embedding,
            ARRAY(SELECT 'heavy_' || i FROM generate_series(0,7) i) ||
            ARRAY(SELECT lpad(i::text,128,'u') FROM generate_series(1,248) i)
        FROM chunks CROSS JOIN generate_series(1,50) n WHERE defines = ARRAY['checkout']",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("ANALYZE chunks")
        .execute(&state.pool)
        .await
        .unwrap();
    let mut worst = Duration::ZERO;
    for i in 0..40 {
        let scope = Scope {
            tenant_id: tenant,
            kinds: None,
            source: if (i / 6) % 2 == 0 { None } else { Some("app") },
        };
        let query = [
            "checkout",
            "persist",
            "not_indexed",
            "background_1 background_2",
            "heavy_0 heavy_1 heavy_2 heavy_3 heavy_4",
            "heavy_0 heavy_1 heavy_2 heavy_3 heavy_4 heavy_5 heavy_6 heavy_7",
        ][i % 6];
        let start = Instant::now();
        let result = callgraph::related(&state.pool, &scope, query)
            .await
            .unwrap();
        let elapsed = start.elapsed();
        if i == 0 {
            eprintln!("first call: {elapsed:?}, {} hits", result.len());
        }
        if elapsed > worst {
            eprintln!("new worst for {query} ({:?}): {elapsed:?}", scope.source);
            worst = elapsed;
        }
    }
    eprintln!("related branch worst of 40: {worst:?}");
    assert!(worst < Duration::from_millis(50), "{worst:?}");
}
