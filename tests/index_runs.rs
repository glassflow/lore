//! Index progress lives in the `index_runs` table: the writer, `GET /v1/repos/{id}/progress`,
//! and the admin pushes list that reads the same rows.

mod common;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use axum::Router;
use common::{
    bundle, commit_all, create_repo, create_tenant_key, fixture_repo, head, push_fixture,
    put_bundle, send, test_router, test_state,
};
use lore::embed::{Embedder, HashEmbedder};
use lore::http::AppState;
use lore::repo::IndexReport;
use lore::repo::progress::RunWriter;
use lore::store::IndexStats;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tempfile::TempDir;
use uuid::Uuid;

/// One app instance: its own data dir, and the pool it was given.
struct App {
    router: Router,
    state: AppState,
    _data: TempDir,
}

fn app(pool: PgPool) -> App {
    let data = tempfile::tempdir().unwrap();
    let state = test_state(pool, data.path());
    App {
        router: test_router(state.clone()),
        state,
        _data: data,
    }
}

/// A pool of its own on the same database: another process, as far as Postgres can tell.
async fn second_pool(pool: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .max_connections(3)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}

/// A tenant with a read and ingest key and one empty repo source.
async fn tenant_with_repo(app: &App, name: &str) -> (Uuid, String, Uuid) {
    let (tenant, key) = create_tenant_key(&app.router, name, &["ingest", "read"]).await;
    let (status, repo) = create_repo(&app.router, &key, "sample").await;
    assert_eq!(status, 200, "{repo}");
    (tenant, key, repo["id"].as_str().unwrap().parse().unwrap())
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    id: i64,
    stage: String,
    files_done: i32,
    files_total: Option<i32>,
    chunks: i32,
    chunks_embedded: i32,
    embed_ms: i64,
    error: Option<String>,
    finished: bool,
}

async fn rows(pool: &PgPool, source: Uuid) -> Vec<Row> {
    sqlx::query_as(
        "SELECT id, stage, files_done, files_total, chunks, chunks_embedded, embed_ms, error,
                finished_at IS NOT NULL AS finished
         FROM index_runs WHERE source_id = $1 ORDER BY id",
    )
    .bind(source)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Polls until the newest row of the source satisfies `ok`, at most 10 seconds.
async fn wait_for_row(pool: &PgPool, source: Uuid, ok: impl Fn(&Row) -> bool) -> Row {
    let start = Instant::now();
    loop {
        if let Some(row) = rows(pool, source).await.pop()
            && ok(&row)
        {
            return row;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "no such row yet");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn totals(chunks: usize, embedded: usize, ms: u64) -> IndexStats {
    IndexStats {
        chunks,
        embedded,
        embed_time: Duration::from_millis(ms),
    }
}

fn report(chunks: usize) -> IndexReport {
    IndexReport {
        indexed_commit: "a".repeat(40),
        files_indexed: 3,
        files_unchanged: 0,
        files_skipped: 0,
        files_deleted: 0,
        chunks,
    }
}

async fn progress(router: &Router, key: &str, source: Uuid) -> (axum::http::StatusCode, Value) {
    send(
        router,
        "GET",
        &format!("/v1/repos/{source}/progress"),
        key,
        Vec::new(),
    )
    .await
}

#[sqlx::test(migrations = "./migrations")]
async fn a_run_goes_from_queued_to_done_and_the_row_shows_the_counts(pool: PgPool) {
    let a = app(pool.clone());
    let (tenant, _, source) = tenant_with_repo(&a, "acme").await;
    let commit = "c".repeat(40);

    let run = RunWriter::new(pool.clone())
        .start(source, tenant, &commit, false)
        .await;
    let row = rows(&pool, source).await.pop().unwrap();
    assert_eq!(row.stage, "queued");
    assert_eq!(
        (row.files_done, row.files_total, row.finished),
        (0, None, false)
    );

    run.cloning();
    wait_for_row(&pool, source, |r| r.stage == "cloning").await;

    run.indexing(3, true);
    let row = wait_for_row(&pool, source, |r| r.stage == "indexing").await;
    assert_eq!((row.files_done, row.files_total), (0, Some(3)));
    let full: bool = sqlx::query_scalar("SELECT \"full\" FROM index_runs WHERE id = $1")
        .bind(row.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(full, "indexing(.., full) is stored");

    run.file_done(&totals(4, 4, 120));
    run.file_done(&totals(9, 7, 200));
    // The counts are written within a second.
    let row = wait_for_row(&pool, source, |r| r.files_done == 2).await;
    assert_eq!(row.stage, "indexing");
    assert_eq!((row.chunks, row.chunks_embedded, row.embed_ms), (9, 7, 200));

    run.done(&report(11)).await;
    let row = rows(&pool, source).await.pop().unwrap();
    assert_eq!(row.stage, "done");
    assert_eq!(
        (row.files_done, row.files_total, row.chunks),
        (2, Some(3), 11)
    );
    assert!(row.finished && row.error.is_none());
    let commit_stored: String = sqlx::query_scalar("SELECT commit FROM index_runs WHERE id = $1")
        .bind(row.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(commit_stored, commit);
}

/// Logs every write to `index_runs` into `index_run_writes`, so a test can count them.
async fn log_writes(pool: &PgPool) {
    for sql in [
        "CREATE TABLE index_run_writes (n bigserial PRIMARY KEY, run_id bigint, stage text,
           files_done int, at timestamptz DEFAULT clock_timestamp())",
        "CREATE FUNCTION log_run_write() RETURNS trigger AS $$
         BEGIN
           INSERT INTO index_run_writes (run_id, stage, files_done)
           VALUES (NEW.id, NEW.stage, NEW.files_done);
           RETURN NEW;
         END $$ LANGUAGE plpgsql",
        "CREATE TRIGGER log_run_write AFTER INSERT OR UPDATE ON index_runs
         FOR EACH ROW EXECUTE FUNCTION log_run_write()",
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }
}

async fn logged(pool: &PgPool) -> Vec<(String, i32)> {
    sqlx::query_as("SELECT stage, files_done FROM index_run_writes ORDER BY n")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn file_counts_are_written_once_a_second_and_the_last_ones_are_kept(pool: PgPool) {
    log_writes(&pool).await;
    let a = app(pool.clone());
    let (tenant, _, source) = tenant_with_repo(&a, "acme").await;

    let run = RunWriter::new(pool.clone())
        .start(source, tenant, &"c".repeat(40), true)
        .await;
    run.indexing(500, true);
    // A burst: 300 files in no time.
    for i in 1..=300 {
        run.file_done(&totals(i, i, 0));
    }
    // And a slow stretch: 25 files in 2.5 seconds.
    for i in 301..=325 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        run.file_done(&totals(i, i, 0));
    }
    run.done(&report(325)).await;

    let log = logged(&pool).await;
    let indexing: Vec<i32> = log
        .iter()
        .filter(|(stage, _)| stage == "indexing")
        .map(|(_, done)| *done)
        .collect();
    // One write for the stage, then at most one per second: 2.5 s and a burst is 5 or fewer.
    assert!(
        (2..=5).contains(&indexing.len()),
        "writes of indexing: {indexing:?}"
    );
    assert!(
        indexing.windows(2).all(|w| w[0] <= w[1]),
        "counts only go up: {indexing:?}"
    );
    // The last counts are in the final write.
    assert_eq!(log.last().unwrap(), &("done".to_string(), 325), "{log:?}");
    let row = rows(&pool, source).await.pop().unwrap();
    assert_eq!(
        (row.stage.as_str(), row.files_done, row.chunks),
        ("done", 325, 325)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_end_of_a_run_is_not_held_back_by_the_throttle(pool: PgPool) {
    let a = app(pool.clone());
    let (tenant, _, source) = tenant_with_repo(&a, "acme").await;
    let writer = RunWriter::new(pool.clone());

    // done, straight after a write of counts
    let run = writer.start(source, tenant, &"c".repeat(40), false).await;
    run.indexing(2, false);
    run.file_done(&totals(1, 1, 0));
    wait_for_row(&pool, source, |r| r.files_done == 1).await;
    run.file_done(&totals(2, 2, 0));
    let started = Instant::now();
    run.done(&report(2)).await;
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "done waited for the throttle: {:?}",
        started.elapsed()
    );
    let row = rows(&pool, source).await.pop().unwrap();
    assert_eq!((row.stage.as_str(), row.files_done), ("done", 2));

    // failed, the same way
    let run = writer.start(source, tenant, &"d".repeat(40), false).await;
    run.indexing(2, false);
    run.file_done(&totals(1, 1, 0));
    wait_for_row(&pool, source, |r| {
        r.stage == "indexing" && r.files_done == 1
    })
    .await;
    run.file_done(&totals(2, 2, 0));
    let started = Instant::now();
    run.failed("boom").await;
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "failed waited for the throttle: {:?}",
        started.elapsed()
    );
    let row = rows(&pool, source).await.pop().unwrap();
    assert_eq!((row.stage.as_str(), row.files_done), ("failed", 2));
    assert_eq!(row.error.as_deref(), Some("boom"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_live_run_is_written_again_when_no_file_finishes(pool: PgPool) {
    log_writes(&pool).await;
    let a = app(pool.clone());
    let (tenant, _, source) = tenant_with_repo(&a, "acme").await;
    let writer = RunWriter::with_intervals(
        pool.clone(),
        Duration::from_secs(1),
        Duration::from_millis(300),
    );
    let run = writer.start(source, tenant, &"c".repeat(40), false).await;
    run.indexing(1, false);
    wait_for_row(&pool, source, |r| r.stage == "indexing").await;
    let before = logged(&pool).await.len();
    // One big file: no file_done for a while.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let beats = logged(&pool).await.len() - before;
    assert!(beats >= 3, "heartbeats in 1.5 s at 0.3 s: {beats}");
    run.done(&report(1)).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn a_dropped_handle_fails_its_run_without_blocking(pool: PgPool) {
    let a = app(pool.clone());
    let (tenant, _, source) = tenant_with_repo(&a, "acme").await;
    let run = RunWriter::new(pool.clone())
        .start(source, tenant, &"c".repeat(40), false)
        .await;
    run.indexing(5, false);
    let started = Instant::now();
    drop(run);
    assert!(started.elapsed() < Duration::from_millis(50));
    let row = wait_for_row(&pool, source, |r| r.stage == "failed").await;
    assert_eq!(
        row.error.as_deref(),
        Some("the index stopped before it ended")
    );
    assert!(row.finished);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_error_is_cut_to_500_characters(pool: PgPool) {
    let a = app(pool.clone());
    let (tenant, _, source) = tenant_with_repo(&a, "acme").await;
    let run = RunWriter::new(pool.clone())
        .start(source, tenant, &"c".repeat(40), false)
        .await;
    run.failed(&"x".repeat(2000)).await;
    let row = rows(&pool, source).await.pop().unwrap();
    assert_eq!(row.error.unwrap().chars().count(), 500);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failed_push_writes_a_failed_run_with_a_short_reason(pool: PgPool) {
    let a = app(pool.clone());
    let (_, key, source) = tenant_with_repo(&a, "acme").await;
    let repo = fixture_repo();
    let bytes = bundle(&repo, &["HEAD"]);
    // The ref is not the bundle's HEAD.
    let wrong = "0".repeat(40);
    let (status, body) = put_bundle(&a.router, &key, source, bytes, &wrong, true).await;
    assert_eq!(status, 400, "{body}");

    let row = rows(&pool, source).await.pop().unwrap();
    assert_eq!(row.stage, "failed");
    assert_eq!(
        row.error.as_deref(),
        Some("ref does not match the bundle's HEAD")
    );
    assert!(row.finished);
    let (status, run) = progress(&a.router, &key, source).await;
    assert_eq!(status, 200, "{run}");
    assert_eq!(run["stage"], "failed");
    assert_eq!(run["commit"], wrong.as_str());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_database_error_in_progress_writes_never_fails_the_index(pool: PgPool) {
    let a = app(pool.clone());
    let (_, key) = create_tenant_key(&a.router, "acme", &["ingest", "read"]).await;
    // Every progress write fails from here on.
    sqlx::query("DROP TABLE index_runs")
        .execute(&pool)
        .await
        .unwrap();
    let (source, _repo) = push_fixture(&a.router, &key).await;
    let docs: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE source_id = $1")
        .bind(source)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(docs >= 6, "the push still indexed: {docs}");

    // The same through the writer: a run for a source that is not there, and calls on it.
    sqlx::query("CREATE TABLE index_runs (id bigserial PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();
    let run = RunWriter::new(pool.clone())
        .start(Uuid::new_v4(), Uuid::new_v4(), &"c".repeat(40), false)
        .await;
    run.cloning();
    run.indexing(1, false);
    run.file_done(&totals(1, 1, 0));
    run.done(&report(1)).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn the_progress_route_shows_the_latest_run_of_the_own_tenant_only(pool: PgPool) {
    let a = app(pool.clone());
    let (_, acme) = create_tenant_key(&a.router, "acme", &["ingest", "read"]).await;
    let (_, globex) = create_tenant_key(&a.router, "globex", &["ingest", "read"]).await;
    let (_, read_only) = create_tenant_key(&a.router, "reader", &["read"]).await;
    let (_, ingest_only) = create_tenant_key(&a.router, "writer", &["ingest"]).await;
    let (source, repo) = push_fixture(&a.router, &acme).await;
    let first = head(&repo);

    // A second, incremental push is the latest run.
    std::fs::write(repo.path().join("NOTES.md"), "# Notes\n\nfresh\n").unwrap();
    commit_all(&repo, "notes");
    let second = head(&repo);
    let bytes = bundle(&repo, &["HEAD", &format!("^{first}")]);
    let (status, report) = put_bundle(&a.router, &acme, source, bytes, &second, false).await;
    assert_eq!(status, 200, "{report}");

    let (status, run) = progress(&a.router, &acme, source).await;
    assert_eq!(status, 200, "{run}");
    let mut fields: Vec<&str> = run
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "chunks",
            "chunks_embedded",
            "commit",
            "embed_ms",
            "error",
            "files_done",
            "files_total",
            "finished_at",
            "full",
            "id",
            "stage",
            "stale",
            "started_at",
            "updated_at",
        ]
    );
    assert_eq!(run["stage"], "done");
    assert_eq!(run["commit"], second.as_str());
    assert_eq!(run["full"], false);
    assert_eq!(
        (&run["files_total"], &run["files_done"]),
        (&json!(1), &json!(1))
    );
    assert_eq!(run["chunks"], report["chunks"]);
    assert_eq!(run["stale"], false);
    assert_eq!(run["error"], Value::Null);
    assert!(run["finished_at"].is_string() && run["started_at"].is_string());
    assert_eq!(run["id"], 2);

    // A key that only reads works; a key that only ingests does not.
    let (status, run) = progress(&a.router, &read_only, source).await;
    assert_eq!(status, 404, "another tenant's repo: {run}");
    let (status, run) = progress(&a.router, &ingest_only, source).await;
    assert_eq!(status, 403, "{run}");

    // Another tenant gets the answer for an id that does not exist.
    let (status, other) = progress(&a.router, &globex, source).await;
    assert_eq!(status, 404);
    let (status, unknown) = progress(&a.router, &globex, Uuid::new_v4()).await;
    assert_eq!(status, 404);
    assert_eq!(other, unknown);
    assert_eq!(other, json!({ "error": "not found" }));

    // No key, no answer.
    let (status, _) = progress(&a.router, "lk_nothing", source).await;
    assert_eq!(status, 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_repo_without_a_run_is_a_404_with_its_own_code(pool: PgPool) {
    let a = app(pool);
    let (_, key, source) = tenant_with_repo(&a, "acme").await;
    let (status, body) = progress(&a.router, &key, source).await;
    assert_eq!(status, 404);
    assert_eq!(body, json!({ "error": "no_index_run" }));
}

/// An embedder that waits for a signal in its `block_at`th call.
struct Gated {
    calls: AtomicUsize,
    block_at: usize,
    release: Mutex<mpsc::Receiver<()>>,
}

impl Embedder for Gated {
    fn name(&self) -> &str {
        "gated"
    }

    fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.block_at {
            // Also returns when the test is gone and dropped the sender.
            let _ = self.release.lock().unwrap().recv();
        }
        HashEmbedder.embed_documents(texts)
    }

    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        HashEmbedder.embed_query(text)
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn an_index_in_progress_is_visible_to_another_instance_and_survives(pool: PgPool) {
    // Instance one indexes with an embedder that stops in its fourth file.
    let (release, gate) = mpsc::channel();
    let mut one = app(pool.clone());
    one.state.embedder = std::sync::Arc::new(Gated {
        calls: AtomicUsize::new(0),
        block_at: 4,
        release: Mutex::new(gate),
    });
    one.router = test_router(one.state.clone());
    let (_, key, source) = tenant_with_repo(&one, "acme").await;
    let repo = fixture_repo();
    let sha = head(&repo);
    let bytes = bundle(&repo, &["HEAD"]);
    let pushing = {
        let (router, key, sha) = (one.router.clone(), key.clone(), sha.clone());
        tokio::spawn(async move { put_bundle(&router, &key, source, bytes, &sha, true).await })
    };

    // Instance two: built fresh, its own pool, nothing shared with one but the database.
    let two = app(second_pool(&pool).await);
    let start = Instant::now();
    let seen = loop {
        let (status, run) = progress(&two.router, &key, source).await;
        if status == 200 && run["stage"] == "indexing" && run["files_done"].as_i64() > Some(0) {
            break run;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "never saw progress: {status} {run}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(seen["stale"], false, "{seen}");
    assert_eq!(seen["commit"], sha.as_str());
    assert_eq!(seen["files_total"], 7);
    assert_eq!(seen["finished_at"], Value::Null);
    assert!(seen["files_done"].as_i64().unwrap() < 7, "{seen}");
    // The index transaction is open and unseen, but the progress is.
    let docs: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE source_id = $1")
        .bind(source)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(docs, 0, "nothing is committed yet");

    // The admin page sees it too.
    let (status, pushes) = send(&two.router, "GET", "/v1/admin/pushes", "t", Vec::new()).await;
    assert_eq!(status, 200, "{pushes}");
    assert_eq!(pushes["running"][0]["state"], "indexing");
    assert_eq!(pushes["running"][0]["id"], seen["id"]);

    // Let the index end: instance two sees it done.
    release.send(()).unwrap();
    let (status, report) = pushing.await.unwrap();
    assert_eq!(status, 200, "{report}");
    let (status, run) = progress(&two.router, &key, source).await;
    assert_eq!(status, 200, "{run}");
    assert_eq!(run["stage"], "done");
    assert_eq!(
        (&run["files_done"], &run["files_total"]),
        (&json!(7), &json!(7))
    );
    assert_eq!(run["chunks"], report["chunks"]);
    assert!(run["finished_at"].is_string());
    assert_eq!(run["stale"], false);
    assert_eq!(run["id"], seen["id"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_run_nobody_writes_any_more_is_stale_and_is_not_rewritten(pool: PgPool) {
    // The writer of a process that will be killed: it never beats again in this test.
    let a = app(pool.clone());
    let (tenant, key, source) = tenant_with_repo(&a, "acme").await;
    let writer = RunWriter::with_intervals(
        pool.clone(),
        Duration::from_secs(1),
        Duration::from_secs(3600),
    );
    let run = writer.start(source, tenant, &"c".repeat(40), false).await;
    run.indexing(10, false);
    wait_for_row(&pool, source, |r| r.stage == "indexing").await;

    // Read by another instance while fresh.
    let two = app(second_pool(&pool).await);
    let (status, fresh) = progress(&two.router, &key, source).await;
    assert_eq!(status, 200);
    assert_eq!(
        (&fresh["stage"], &fresh["stale"]),
        (&json!("indexing"), &json!(false))
    );

    // The process died: its row is not touched for two minutes.
    sqlx::query("UPDATE index_runs SET updated_at = now() - interval '120 seconds'")
        .execute(&pool)
        .await
        .unwrap();
    let old: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM index_runs")
            .fetch_one(&pool)
            .await
            .unwrap();
    let (status, stale) = progress(&two.router, &key, source).await;
    assert_eq!(status, 200);
    assert_eq!(stale["stage"], "indexing", "the stage is kept: {stale}");
    assert_eq!(stale["stale"], true);
    // And the read changed nothing.
    let (stage, now): (String, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT stage, updated_at FROM index_runs")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((stage.as_str(), now), ("indexing", old));

    // An ended run is never stale, however old.
    run.done(&report(1)).await;
    sqlx::query("UPDATE index_runs SET updated_at = now() - interval '1 day'")
        .execute(&pool)
        .await
        .unwrap();
    let (_, ended) = progress(&two.router, &key, source).await;
    assert_eq!(
        (&ended["stage"], &ended["stale"]),
        (&json!("done"), &json!(false))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_admin_list_reads_the_table_with_running_runs_first(pool: PgPool) {
    let a = app(pool.clone());
    let (tenant, _, source) = tenant_with_repo(&a, "acme").await;
    // Runs written by "another process": a pool of its own.
    let other = RunWriter::new(second_pool(&pool).await);
    let ended = other.start(source, tenant, &"a".repeat(40), true).await;
    ended.done(&report(4)).await;
    let live = other.start(source, tenant, &"b".repeat(40), false).await;
    live.cloning();
    let lost = other.start(source, tenant, &"c".repeat(40), false).await;
    lost.indexing(3, false);
    wait_for_row(&pool, source, |r| r.stage == "indexing").await;
    // The third one's process died long ago.
    sqlx::query(
        "UPDATE index_runs SET updated_at = now() - interval '5 minutes' WHERE commit = $1",
    )
    .bind("c".repeat(40))
    .execute(&pool)
    .await
    .unwrap();

    // An instance built after all that.
    let b = app(second_pool(&pool).await);
    let (status, list) = send(&b.router, "GET", "/v1/admin/pushes", "t", Vec::new()).await;
    assert_eq!(status, 200, "{list}");
    let running = list["running"].as_array().unwrap();
    assert_eq!(running.len(), 1, "{list}");
    assert_eq!(running[0]["commit"], "b".repeat(40));
    assert_eq!(running[0]["state"], "cloning");
    assert_eq!(running[0]["tenant"], "acme");
    assert_eq!(running[0]["source"], "sample");
    assert_eq!(running[0]["source_id"], source.to_string());
    assert_eq!(running[0]["stale"], false);
    let recent = list["recent"].as_array().unwrap();
    let commits: Vec<&str> = recent
        .iter()
        .map(|p| p["commit"].as_str().unwrap())
        .collect();
    assert_eq!(commits, ["c".repeat(40), "a".repeat(40)]);
    assert_eq!(
        (&recent[0]["state"], &recent[0]["stale"]),
        (&json!("indexing"), &json!(true))
    );
    assert_eq!(
        (&recent[1]["state"], &recent[1]["chunks"]),
        (&json!("done"), &json!(4))
    );
    drop((live, lost));
}

#[sqlx::test(migrations = "./migrations")]
async fn the_runs_of_a_deleted_source_go_with_it(pool: PgPool) {
    let a = app(pool.clone());
    let (_, key) = create_tenant_key(&a.router, "acme", &["ingest", "read"]).await;
    let (source, _repo) = push_fixture(&a.router, &key).await;
    assert_eq!(rows(&pool, source).await.len(), 1);
    let (status, body) = send(
        &a.router,
        "DELETE",
        &format!("/v1/admin/sources/{source}"),
        "t",
        Vec::new(),
    )
    .await;
    assert_eq!(status, 204, "{body}");
    assert!(rows(&pool, source).await.is_empty());
}
