use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct CacheCounter(AtomicUsize);

impl Embedder for CacheCounter {
    fn name(&self) -> &str {
        "hash"
    }
    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.0.fetch_add(texts.len(), Ordering::SeqCst);
        HashEmbedder.embed_documents(texts)
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        HashEmbedder.embed_query(text)
    }
}

async fn cache_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM embedding_cache")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn cold_warm_pool(pool: PgPool, mode: lore::config::EmbeddingCache) {
    let small = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let mut env = Env::new(small, false).await;
    Arc::make_mut(&mut env.state.config).embedding_cache = mode;
    let counter = Arc::new(CacheCounter::default());
    env.state.embedder = counter.clone();
    env.router = test_router(env.state.clone());
    let (status, report) = env.push(true).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let first = counter.0.load(Ordering::SeqCst);
    assert!(first > 0);
    assert_eq!(env.push(true).await.0, StatusCode::OK);
    assert_eq!(
        counter.0.load(Ordering::SeqCst),
        first,
        "unchanged same-source push"
    );

    let (_, other) = create_repo(&env.router, &env.key, "copy").await;
    let source = other["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        put_bundle(
            &env.router,
            &env.key,
            source,
            bundle(&env.repo, &["HEAD"]),
            &head(&env.repo),
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    let same_tenant = counter.0.load(Ordering::SeqCst);
    let want_same = if mode == lore::config::EmbeddingCache::Off {
        2 * first
    } else {
        first
    };
    assert_eq!(same_tenant, want_same);

    let (_, key) = create_tenant_key(&env.router, "second", &["ingest", "read"]).await;
    let (_, other) = create_repo(&env.router, &key, "copy").await;
    let source = other["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        put_bundle(
            &env.router,
            &key,
            source,
            bundle(&env.repo, &["HEAD"]),
            &head(&env.repo),
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    let want_cross = if mode == lore::config::EmbeddingCache::Shared {
        same_tenant
    } else {
        same_tenant + first
    };
    assert_eq!(counter.0.load(Ordering::SeqCst), want_cross);
    if mode == lore::config::EmbeddingCache::Off {
        assert_eq!(cache_rows(&pool).await, 0);
    } else {
        assert!(cache_rows(&pool).await > 0);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn cache_cold_warm_one_connection_and_second_tenant_zero_embed(pool: PgPool) {
    cold_warm_pool(pool, lore::config::EmbeddingCache::Shared).await;
}
#[sqlx::test(migrations = "./migrations")]
async fn cache_tenant_scope_on_atomic_repo_path(pool: PgPool) {
    cold_warm_pool(pool, lore::config::EmbeddingCache::Tenant).await;
}
#[sqlx::test(migrations = "./migrations")]
async fn cache_off_on_atomic_repo_path(pool: PgPool) {
    cold_warm_pool(pool, lore::config::EmbeddingCache::Off).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_sources_with_reverse_key_order_finish(pool: PgPool) {
    let env = Env::new(pool.clone(), false).await;
    let forward = "# Alpha\nreverse-key first\n# Beta\nreverse-key second\n";
    let backward = "# Beta\nreverse-key second\n# Alpha\nreverse-key first\n";
    let keys = |text| {
        lore::chunk::chunk_file("reverse.md", text)
            .iter()
            .map(|c| {
                lore::store::embedding_cache_key(
                    None,
                    "hash",
                    &lore::chunk::embed_text("reverse.md", c),
                )
            })
            .collect::<Vec<_>>()
    };
    let a = keys(forward);
    let mut b = keys(backward);
    b.reverse();
    assert_eq!(a.len(), 2);
    assert_eq!(a, b, "same exact keys encountered in reverse order");

    std::fs::write(env.repo.path().join("reverse.md"), forward).unwrap();
    commit_all(&env.repo, "forward key order");
    let second_repo = fixture_repo();
    std::fs::write(second_repo.path().join("reverse.md"), backward).unwrap();
    commit_all(&second_repo, "reverse key order");
    let (_, source) = create_repo(&env.router, &env.key, "second").await;
    let source = source["id"].as_str().unwrap().parse().unwrap();
    let first_gate = Gate::new(Some("reverse-key"), false, false);
    let second_gate = Gate::new(Some("reverse-key"), false, false);
    let first = env.start_push(env.gated_router(first_gate.clone()), true);
    let router = env.gated_router(second_gate.clone());
    let key = env.key.clone();
    let bytes = bundle(&second_repo, &["HEAD"]);
    let sha = head(&second_repo);
    let second =
        tokio::spawn(async move { put_bundle(&router, &key, source, bytes, &sha, true).await });
    first_gate.wait().await;
    second_gate.wait().await;
    assert_eq!(
        cache_rows(&pool).await,
        0,
        "uncommitted pushes have staged only"
    );
    first_gate.release();
    second_gate.release();
    for task in [first, second] {
        let (status, report) = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{report}");
    }
    assert!(cache_rows(&pool).await > 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn cache_publication_error_does_not_fail_a_committed_push(pool: PgPool) {
    let env = Env::new(pool.clone(), false).await;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_cache() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected cache publication failure'; END $$;
         CREATE TRIGGER reject_cache BEFORE INSERT ON embedding_cache
         FOR EACH ROW EXECUTE FUNCTION reject_cache();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (status, report) = env.push(true).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(env.commit().await, Some(head(&env.repo)));
    assert_eq!(contents(&env.search().await).len(), 4);
    assert_eq!(cache_rows(&pool).await, 0);
    sqlx::query("DROP TRIGGER reject_cache ON embedding_cache")
        .execute(&pool)
        .await
        .unwrap();
    env.change();
    assert_eq!(env.push(false).await.0, StatusCode::OK);
    assert_eq!(contents(&env.search().await), new_contents());
    assert!(cache_rows(&pool).await > 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn cache_inserts_occur_in_a_transaction_after_index_commit(pool: PgPool) {
    let env = Env::new(pool.clone(), false).await;
    sqlx::raw_sql(
        "CREATE FUNCTION require_index_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NOT EXISTS (SELECT 1 FROM sources WHERE indexed_commit IS NOT NULL) OR
              EXISTS (SELECT 1 FROM documents WHERE xmin::text = pg_current_xact_id()::text) THEN
             RAISE EXCEPTION 'cache insert before document publication';
           END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER require_index_commit BEFORE INSERT ON embedding_cache
         FOR EACH ROW EXECUTE FUNCTION require_index_commit();",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(env.push(true).await.0, StatusCode::OK);
    assert!(
        cache_rows(&pool).await > 0,
        "cache fills only after an independent publication transaction"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn standalone_cache_publication_error_keeps_document_visible(pool: PgPool) {
    let env = Env::new(pool.clone(), false).await;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_cache() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected cache publication failure'; END $$;
         CREATE TRIGGER reject_cache BEFORE INSERT ON embedding_cache
         FOR EACH ROW EXECUTE FUNCTION reject_cache();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (status, body) = send(
        &env.router,
        "POST",
        "/v1/documents",
        &env.key,
        json!({
            "source": "notes",
            "documents": [{
                "external_id": "cache-note",
                "title": "Cache note",
                "text": "cachenotefresh remains published"
            }]
        })
        .to_string()
        .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(cache_rows(&pool).await, 0);
    let (status, body) = send(
        &env.router,
        "POST",
        "/v1/search",
        &env.key,
        json!({"q": "cachenotefresh", "limit": 20})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["hits"].as_array().unwrap().iter().any(|hit| {
        hit["text"]
            .as_str()
            .is_some_and(|text| text.contains("cachenotefresh remains published"))
    }));
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_push_does_not_publish_staged_cache_or_any_document(pool: PgPool) {
    let env = Env::new(pool.clone(), false).await;
    let gate = Gate::new(Some("repoatomic old beta"), false, true);
    let push = env.start_push(env.gated_router(gate.clone()), true);
    gate.wait().await;
    assert_eq!(cache_rows(&pool).await, 0);
    gate.release();
    assert_eq!(push.await.unwrap().0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(cache_rows(&pool).await, 0);
    assert!(env.search().await["hits"].as_array().unwrap().is_empty());
    assert_eq!(env.commit().await, None);
    assert_eq!(env.push(true).await.0, StatusCode::OK);
    assert!(cache_rows(&pool).await > 0);
}

/// Real HTTP push, fresh process, 1930 files x 12 chunks. Run separately in shared/off modes.
#[sqlx::test(migrations = "./migrations")]
#[ignore = "coordinated 23k chunk retained-vector benchmark"]
async fn staging_retention_benchmark(pool: PgPool) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("lore::store=debug"))
        .with_test_writer()
        .try_init();
    let files: usize = std::env::var("LORE_CACHE_BENCH_FILES")
        .unwrap_or_else(|_| "1930".into())
        .parse()
        .unwrap();
    let off = std::env::var("LORE_CACHE_BENCH_MODE").is_ok_and(|v| v == "off");
    let mut env = Env::new(pool.clone(), false).await;
    if off {
        Arc::make_mut(&mut env.state.config).embedding_cache = lore::config::EmbeddingCache::Off;
    }
    env.router = test_router(env.state.clone());
    for n in 0..files {
        let text = (0..12)
            .map(|section| format!("# Subject {section}\natomicbulk file {n} section {section}\n"))
            .collect::<String>();
        std::fs::write(env.repo.path().join(format!("bulk{n:05}.md")), text).unwrap();
    }
    std::fs::write(env.repo.path().join("zzz.md"), "atomicbulk gate").unwrap();
    commit_all(&env.repo, "23k staging fixture");
    let gate = Gate::new(Some("atomicbulk gate"), false, false);
    let started = std::time::Instant::now();
    let push = env.start_push(env.gated_router(gate.clone()), true);
    tokio::time::timeout(Duration::from_secs(240), gate.reached.notified())
        .await
        .unwrap();
    assert!(env.search().await["hits"].as_array().unwrap().is_empty());
    assert_eq!(cache_rows(&pool).await, 0);
    println!(
        "stage_ready files={files} mode={} elapsed={:?}",
        if off { "off" } else { "shared" },
        started.elapsed()
    );
    gate.release();
    let (status, report) = push.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(report["chunks"].as_u64().unwrap() >= files as u64 * 12);
    println!(
        "stage_complete files={files} mode={} report={report} cache_rows={} wall={:?}",
        if off { "off" } else { "shared" },
        cache_rows(&pool).await,
        started.elapsed()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn cancelled_cold_push_discards_staging_and_retries(pool: PgPool) {
    let env = Env::new(pool.clone(), false).await;
    let gate = Gate::new(Some("repoatomic old beta"), false, false);
    let push = env.start_push(env.gated_router(gate.clone()), true);
    gate.wait().await;
    assert_eq!(cache_rows(&pool).await, 0);
    push.abort();
    gate.release();
    assert!(push.await.unwrap_err().is_cancelled());
    assert!(env.search().await["hits"].as_array().unwrap().is_empty());
    assert_eq!(env.commit().await, None);
    assert_eq!(cache_rows(&pool).await, 0);
    assert_eq!(env.push(true).await.0, StatusCode::OK);
    assert!(cache_rows(&pool).await > 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn index_publication_failure_discards_new_cache_entries(pool: PgPool) {
    let env = Env::new(pool.clone(), true).await;
    let old = env.search().await;
    let old_commit = env.commit().await;
    let old_cache = cache_rows(&pool).await;
    env.change();
    sqlx::raw_sql(
        "CREATE FUNCTION reject_publication() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN RAISE EXCEPTION 'injected publication failure'; END $$;
         CREATE TRIGGER reject_publication BEFORE UPDATE OF indexed_commit ON sources
         FOR EACH ROW EXECUTE FUNCTION reject_publication();",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(env.push(false).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(env.search().await, old);
    assert_eq!(env.commit().await, old_commit);
    assert_eq!(cache_rows(&pool).await, old_cache);
    sqlx::query("DROP TRIGGER reject_publication ON sources")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(env.push(false).await.0, StatusCode::OK);
    assert_eq!(contents(&env.search().await), new_contents());
    assert!(cache_rows(&pool).await > old_cache);
}
