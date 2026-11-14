//! `lore index`: the real binary against a test database, with a `file://` remote (allowed by
//! `LORE_SYNC_ALLOW_FILE_REMOTES`) and the hash embedder. It talks to Postgres directly, so the
//! tests give it the DSN of the `sqlx::test` database.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use common::{
    bundle, commit_all, create_repo, create_tenant_key, fixture_repo, git, head, put_bundle, send,
    test_router, test_state,
};
use lore::repo::lock::SourceAdvisoryLock;
use serde_json::Value;
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;

/// A token no test output may contain.
const TOKEN: &str = "ghp_FakeIndexToken0000000000000000000001";

/// The DSN of the database behind `pool` (the `sqlx::test` database), from `DATABASE_URL`.
fn dsn(pool: &PgPool) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL is set for database tests");
    let mut url = reqwest::Url::parse(&base).expect("DATABASE_URL parses");
    url.set_path(&format!(
        "/{}",
        pool.connect_options().get_database().expect("a database")
    ));
    url.to_string()
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> Value {
        assert_eq!(self.code, Some(0), "lore index failed: {}", self.stderr);
        assert_eq!(self.stdout.trim().lines().count(), 1, "{}", self.stdout);
        serde_json::from_str(self.stdout.trim()).expect("stdout is one JSON line")
    }
}

struct Env {
    pool: PgPool,
    /// The server side: tenants, pushes and searches go through this router.
    router: Router,
    /// The server's data dir (kept alive).
    _server_data: TempDir,
    /// The Job's data dir: its mirror and checkout.
    job_data: TempDir,
}

impl Env {
    fn new(pool: PgPool) -> Env {
        let server_data = tempfile::tempdir().unwrap();
        let router = test_router(test_state(pool.clone(), server_data.path()));
        Env {
            pool,
            router,
            _server_data: server_data,
            job_data: tempfile::tempdir().unwrap(),
        }
    }

    /// A tenant with a key, and its repo source `sample`.
    async fn tenant(&self, name: &str) -> (Uuid, String, Uuid) {
        let (tenant, key) = create_tenant_key(&self.router, name, &["ingest", "read"]).await;
        let (status, repo) = create_repo(&self.router, &key, "sample").await;
        assert_eq!(status, 200, "{repo}");
        (tenant, key, repo["id"].as_str().unwrap().parse().unwrap())
    }

    fn command(&self, tenant: Uuid, source: Uuid, remote: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        cmd.args([
            "index",
            "--tenant",
            &tenant.to_string(),
            "--repo",
            &source.to_string(),
            "--remote",
            remote,
        ])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("LORE_DATABASE_URL", dsn(&self.pool))
        .env("LORE_DATA_DIR", self.job_data.path())
        .env("LORE_EMBEDDER", "hash")
        .env("LORE_SYNC_ALLOW_FILE_REMOTES", "true")
        .env("LORE_LOG", "debug")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
        cmd
    }

    async fn index(&self, tenant: Uuid, source: Uuid, remote: &str) -> Run {
        run(self.command(tenant, source, remote)).await
    }
}

async fn run(mut cmd: Command) -> Run {
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await
        .unwrap()
        .expect("the lore binary runs");
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn file_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

/// Every chunk of a source with what identifies it, vector included, in a stable order.
type ChunkRow = (
    String,
    String,
    String,
    String,
    String,
    i32,
    String,
    Option<i32>,
    Option<i32>,
    Vec<String>,
    String,
);

async fn chunk_rows(pool: &PgPool, source: Uuid) -> Vec<ChunkRow> {
    sqlx::query_as(
        "SELECT d.external_id, d.kind, d.title, d.content_hash, d.metadata::text, c.ord, c.text,
                c.start_line, c.end_line, c.symbols, c.embedding::text
         FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.source_id = $1 ORDER BY d.external_id, c.ord",
    )
    .bind(source)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A fingerprint of everything a tenant owns, down to every column of every row.
async fn fingerprint(pool: &PgPool, tenant: Uuid) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for (table, order) in [
        ("sources", "id::text"),
        ("documents", "id::text"),
        ("chunks", "id"),
        ("index_runs", "id"),
    ] {
        let sql = format!(
            "SELECT count(*), COALESCE(md5(string_agg(t::text, ',' ORDER BY t.{order})), '')
             FROM {table} t WHERE t.tenant_id = $1"
        );
        out.push(
            sqlx::query_as(&sql)
                .bind(tenant)
                .fetch_one(pool)
                .await
                .unwrap(),
        );
    }
    out
}

async fn indexed_commit(pool: &PgPool, source: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
        .bind(source)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// (stage, commit, error) of the runs of a source, oldest first.
async fn runs(pool: &PgPool, source: Uuid) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as("SELECT stage, commit, error FROM index_runs WHERE source_id = $1 ORDER BY id")
        .bind(source)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn search(router: &Router, key: &str, q: &str) -> Vec<Value> {
    let (status, body) = send(
        router,
        "GET",
        &format!("/v1/search?q={}", q.replace(' ', "%20")),
        key,
        Vec::new(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["hits"].as_array().unwrap().clone()
}

/// Without ids, which differ between tenants.
fn strip_ids(mut hit: Value) -> Value {
    if let Some(o) = hit.as_object_mut() {
        o.retain(|k, _| !k.ends_with("id") && k != "id");
    }
    hit
}

#[sqlx::test(migrations = "./migrations")]
async fn an_index_gives_the_same_result_as_a_full_push_of_the_commit(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (job_tenant, job_key, job_source) = env.tenant("jobs").await;
    let (_, push_key, push_source) = env.tenant("pushers").await;
    let repo = fixture_repo();
    let sha = head(&repo);

    // The reference: a full push of the same commit.
    let bytes = bundle(&repo, &["HEAD"]);
    let (status, pushed) = put_bundle(&env.router, &push_key, push_source, bytes, &sha, true).await;
    assert_eq!(status, 200, "{pushed}");

    let out = env
        .index(job_tenant, job_source, &file_url(repo.path()))
        .await;
    let done = out.json();
    assert_eq!(done["commit"], sha.as_str());
    assert_eq!(done["full"], true);
    assert_eq!(done["files_indexed"], pushed["files_indexed"]);
    assert_eq!(done["files_skipped"], pushed["files_skipped"]);
    assert_eq!(done["chunks"], pushed["chunks"]);
    assert!(done["chunks"].as_u64().unwrap() > 3);
    assert!(done["embed_ms"].is_u64() && done["chunks_embedded"].is_u64());
    assert_eq!(done["files_unchanged"], 0);

    // The same documents and chunks: hashes, texts, lines, symbols and vectors.
    let from_job = chunk_rows(&pool, job_source).await;
    let from_push = chunk_rows(&pool, push_source).await;
    assert!(!from_job.is_empty());
    assert_eq!(from_job, from_push);
    assert_eq!(indexed_commit(&pool, job_source).await, Some(sha.clone()));

    // The same search results for a few questions.
    for q in [
        "plan limits",
        "max seats per plan",
        "billing handler",
        "seed",
    ] {
        let a: Vec<Value> = search(&env.router, &job_key, q)
            .await
            .into_iter()
            .map(strip_ids)
            .collect();
        let b: Vec<Value> = search(&env.router, &push_key, q)
            .await
            .into_iter()
            .map(strip_ids)
            .collect();
        assert!(!a.is_empty(), "{q:?} finds something");
        assert_eq!(a, b, "{q:?}");
    }

    // The run shows in index_runs, done, with the commit and the counts.
    let (stage, commit, error): (String, String, Option<String>) =
        runs(&pool, job_source).await.pop().expect("a run row");
    assert_eq!(
        (stage.as_str(), commit.as_str(), error),
        ("done", sha.as_str(), None)
    );
    let (full, total, done_files, chunks): (bool, Option<i32>, i32, i32) = sqlx::query_as(
        "SELECT \"full\", files_total, files_done, chunks FROM index_runs WHERE source_id = $1",
    )
    .bind(job_source)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(full);
    assert_eq!(total, Some(done_files));
    assert_eq!(chunks as u64, done["chunks"].as_u64().unwrap());

    // The checkout is built like a push builds it, so deep answers can read files.
    let checkout = env
        .job_data
        .path()
        .join("repos")
        .join(job_source.to_string())
        .join("checkout");
    assert!(checkout.join("src/billing/plans.rs").is_file());

    // Running it again indexes nothing new and succeeds.
    let again = env
        .index(job_tenant, job_source, &file_url(repo.path()))
        .await
        .json();
    assert_eq!(again["chunks"], 0);
    assert_eq!(again["files_indexed"], 0);
    assert_eq!(again["commit"], sha.as_str());
    assert_eq!(runs(&pool, job_source).await.len(), 2);

    // A file that left the remote leaves the index and the checkout, as in a full push.
    std::fs::remove_file(repo.path().join("scripts/seed.py")).unwrap();
    commit_all(&repo, "drop the seed script");
    let next = env
        .index(job_tenant, job_source, &file_url(repo.path()))
        .await
        .json();
    assert_eq!(next["files_deleted"], 1, "{next}");
    assert_eq!(next["commit"], head(&repo).as_str());
    assert_eq!(indexed_commit(&pool, job_source).await, Some(head(&repo)));
    let docs: Vec<String> =
        sqlx::query_scalar("SELECT external_id FROM documents WHERE source_id = $1")
            .bind(job_source)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(!docs.contains(&"scripts/seed.py".to_string()), "{docs:?}");
    assert!(!checkout.join("scripts/seed.py").exists());

    // Rewritten history (the old commit is no ancestor) is indexed all the same: a full index
    // never needs the old commit.
    git(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(repo.path().join("other.md"), "# Other\n\nRewritten.\n").unwrap();
    commit_all(&repo, "rewritten");
    let rewritten = env
        .index(job_tenant, job_source, &file_url(repo.path()))
        .await
        .json();
    assert_eq!(rewritten["commit"], head(&repo).as_str());
    assert_eq!(indexed_commit(&pool, job_source).await, Some(head(&repo)));
    let docs: Vec<String> =
        sqlx::query_scalar("SELECT external_id FROM documents WHERE source_id = $1")
            .bind(job_source)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(docs.contains(&"other.md".to_string()), "{docs:?}");

    // Every run of the command was a full one.
    let fulls: Vec<bool> =
        sqlx::query_scalar("SELECT \"full\" FROM index_runs WHERE source_id = $1")
            .bind(job_source)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(fulls.len() >= 4 && fulls.iter().all(|f| *f), "{fulls:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn an_index_touches_only_its_own_tenant(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (a, _, a_source) = env.tenant("jobs").await;
    let (b, b_key, b_source) = env.tenant("bystander").await;
    let repo = fixture_repo();
    let sha = head(&repo);
    let bytes = bundle(&repo, &["HEAD"]);
    let (status, _) = put_bundle(&env.router, &b_key, b_source, bytes, &sha, true).await;
    assert_eq!(status, 200);
    let before = fingerprint(&pool, b).await;
    assert!(before.iter().all(|(n, _)| *n > 0), "{before:?}");

    env.index(a, a_source, &file_url(repo.path())).await.json();

    assert_eq!(fingerprint(&pool, b).await, before);
    assert_eq!(indexed_commit(&pool, b_source).await, Some(sha));
    // And the job's own rows carry its own tenant.
    let foreign: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM documents WHERE source_id = $1 AND tenant_id <> $2)
              + (SELECT count(*) FROM chunks c JOIN documents d ON d.id = c.document_id
                 WHERE d.source_id = $1 AND c.tenant_id <> $2)
              + (SELECT count(*) FROM index_runs WHERE source_id = $1 AND tenant_id <> $2)",
    )
    .bind(a_source)
    .bind(a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(foreign, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_source_of_another_tenant_or_none_is_refused(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (a, _, a_source) = env.tenant("owner").await;
    let (b, _, _) = env.tenant("other").await;
    let repo = fixture_repo();
    let remote = file_url(repo.path());
    let before_a = fingerprint(&pool, a).await;
    let before_b = fingerprint(&pool, b).await;

    // Tenant b names tenant a's source.
    let wrong = env.index(b, a_source, &remote).await;
    assert_eq!(wrong.code, Some(1), "{}", wrong.stderr);
    assert!(wrong.stdout.is_empty());
    assert!(
        wrong.stderr.contains("not found for tenant"),
        "{}",
        wrong.stderr
    );
    // A source that does not exist, and a tenant that does not exist, read the same.
    let none = env.index(a, Uuid::new_v4(), &remote).await;
    assert_eq!(none.code, Some(1));
    assert!(
        none.stderr.contains("not found for tenant"),
        "{}",
        none.stderr
    );
    let ghost = env.index(Uuid::new_v4(), a_source, &remote).await;
    assert_eq!(ghost.code, Some(1));
    assert!(
        ghost.stderr.contains("not found for tenant"),
        "{}",
        ghost.stderr
    );

    // Nothing was written for either tenant, not even a run row.
    assert_eq!(fingerprint(&pool, a).await, before_a);
    assert_eq!(fingerprint(&pool, b).await, before_b);
    assert_eq!(indexed_commit(&pool, a_source).await, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_second_index_of_a_source_fails_at_once_while_the_first_holds_the_lock(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (tenant, _, source) = env.tenant("jobs").await;
    let repo = fixture_repo();
    let remote = file_url(repo.path());

    let held = SourceAdvisoryLock::try_acquire(&dsn(&pool), source)
        .await
        .unwrap()
        .expect("the lock is free");
    // The same lock is refused to a second session, and a different source is not affected.
    assert!(
        SourceAdvisoryLock::try_acquire(&dsn(&pool), source)
            .await
            .unwrap()
            .is_none()
    );
    let other = SourceAdvisoryLock::try_acquire(&dsn(&pool), Uuid::new_v4())
        .await
        .unwrap()
        .expect("another source has its own lock");
    other.release().await;

    let started = std::time::Instant::now();
    let busy = env.index(tenant, source, &remote).await;
    assert_eq!(busy.code, Some(3), "{}", busy.stderr);
    assert!(busy.stderr.contains("another index"), "{}", busy.stderr);
    assert!(busy.stdout.is_empty());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "it waited"
    );
    assert!(
        runs(&pool, source).await.is_empty(),
        "a refused run leaves no row"
    );
    assert_eq!(indexed_commit(&pool, source).await, None);

    // Released, the index runs, and leaves the lock free again.
    held.release().await;
    env.index(tenant, source, &remote).await.json();
    let again = SourceAdvisoryLock::try_acquire(&dsn(&pool), source)
        .await
        .unwrap();
    assert!(again.is_some(), "the finished run released its lock");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failed_index_exits_non_zero_and_marks_its_run_failed(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (tenant, _, source) = env.tenant("jobs").await;

    // A remote that is not there: nothing listens on port 1.
    let mut cmd = env.command(tenant, source, "https://127.0.0.1:1/acme/app.git");
    cmd.env("LORE_INDEX_TOKEN", TOKEN);
    let failed = run(cmd).await;
    assert_eq!(failed.code, Some(1), "{}", failed.stderr);
    assert!(failed.stdout.is_empty(), "{}", failed.stdout);
    assert!(
        failed.stderr.contains("could not fetch the remote"),
        "{}",
        failed.stderr
    );
    assert!(!failed.stdout.contains(TOKEN) && !failed.stderr.contains(TOKEN));

    let rows = runs(&pool, source).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (stage, commit, error) = &rows[0];
    assert_eq!((stage.as_str(), commit.as_str()), ("failed", "HEAD"));
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.starts_with("fetch failed")),
        "{error:?}"
    );
    assert!(!error.as_deref().unwrap().contains(TOKEN));
    assert_eq!(indexed_commit(&pool, source).await, None);
    let (last_error, last_commit): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT last_sync_error, last_sync_commit FROM sources WHERE id = $1")
            .bind(source)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(last_error.is_some() && last_commit.is_none());
    // The lock is free after the failure.
    assert!(
        SourceAdvisoryLock::try_acquire(&dsn(&pool), source)
            .await
            .unwrap()
            .is_some()
    );

    // A remote that is not a repo fails the same way, with a real run row too.
    let empty = tempfile::tempdir().unwrap();
    let failed = env.index(tenant, source, &file_url(empty.path())).await;
    assert_eq!(failed.code, Some(1), "{}", failed.stderr);
    assert_eq!(runs(&pool, source).await.last().unwrap().0, "failed");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_remote_that_is_not_a_plain_https_url_is_refused_before_anything_runs(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (tenant, _, source) = env.tenant("jobs").await;
    for remote in [
        "https://user:hunter2@example.com/acme/app.git",
        "http://example.com/acme/app.git",
        "ssh://git@example.com/acme/app.git",
        "/srv/repos/app",
    ] {
        let mut cmd = env.command(tenant, source, remote);
        cmd.env("LORE_INDEX_TOKEN", TOKEN);
        let out = run(cmd).await;
        assert_eq!(out.code, Some(1), "{remote}: {}", out.stderr);
        assert!(out.stderr.contains("remote_url must be"), "{}", out.stderr);
        assert!(!out.stderr.contains("hunter2") && !out.stderr.contains(TOKEN));
    }
    assert!(runs(&pool, source).await.is_empty());
}

/// A `git` that behaves like a remote which refuses the fetch and echoes the credentials it was
/// sent, as a hostile or misconfigured one could. Every other git command is the real git.
fn fake_git(dir: &Path) {
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let script = format!(
        r#"#!/bin/sh
for a in "$@"; do
  if [ "$a" = fetch ]; then
    env > "{dir}/env.dump"
    echo "$*" > "{dir}/args.dump"
    echo "fatal: unable to access 'https://example.invalid/app.git/': token $FAKE_GIT_ECHO header $GIT_CONFIG_VALUE_0 basic ${{GIT_CONFIG_VALUE_0#Authorization: Basic }} key $GIT_CONFIG_KEY_0" >&2
    exit 128
  fi
done
exec {real} "$@"
"#,
        dir = dir.display(),
        real = real.trim()
    );
    let path = dir.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn the_token_reaches_git_only_as_a_header_and_never_leaves(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (tenant, _, source) = env.tenant("jobs").await;
    let fake = tempfile::tempdir().unwrap();
    fake_git(fake.path());

    let mut cmd = env.command(tenant, source, "https://example.invalid/app.git");
    let path = format!(
        "{}:{}",
        fake.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.env("PATH", path)
        .env("LORE_INDEX_TOKEN", TOKEN)
        .env("FAKE_GIT_ECHO", TOKEN);
    let out = run(cmd).await;
    assert_eq!(out.code, Some(1), "{}", out.stderr);

    // The remote echoed the raw token and the header; both are scrubbed everywhere.
    let basic = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"))
    };
    let (_, _, error) = runs(&pool, source).await.pop().unwrap();
    let error = error.expect("an error is recorded");
    for (what, text) in [
        ("stdout", &out.stdout),
        ("stderr", &out.stderr),
        ("index_runs.error", &error),
    ] {
        assert!(!text.contains(TOKEN), "{what}: {text}");
        assert!(!text.contains(&basic), "{what}: {text}");
    }
    assert!(error.contains("[token]"), "{error}");
    let last: Option<String> =
        sqlx::query_scalar("SELECT last_sync_error FROM sources WHERE id = $1")
            .bind(source)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!last.unwrap().contains(TOKEN));

    // Git got it as http.extraHeader in its environment (so it is readable from the git
    // process's /proc/<pid>/environ while it runs), but not in its arguments, and neither
    // LORE_INDEX_TOKEN nor LORE_DATABASE_URL themselves.
    let args = std::fs::read_to_string(fake.path().join("args.dump")).unwrap();
    assert!(!args.contains(TOKEN) && !args.contains(&basic), "{args}");
    let dump = std::fs::read_to_string(fake.path().join("env.dump")).unwrap();
    assert!(
        dump.contains(&format!("GIT_CONFIG_VALUE_0=Authorization: Basic {basic}")),
        "{dump}"
    );
    assert!(dump.contains("GIT_CONFIG_KEY_0=http.extraHeader"));
    assert!(!dump.contains("LORE_INDEX_TOKEN"), "{dump}");
    assert!(!dump.contains("LORE_DATABASE_URL"), "{dump}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_successful_index_with_a_token_prints_and_logs_no_token(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (tenant, _, source) = env.tenant("jobs").await;
    let repo = fixture_repo();
    let mut cmd = env.command(tenant, source, &file_url(repo.path()));
    cmd.env("LORE_INDEX_TOKEN", TOKEN).env("LORE_LOG", "trace");
    let out = run(cmd).await;
    out.json();
    assert!(!out.stdout.contains(TOKEN) && !out.stderr.contains(TOKEN));
    // The logs are on stderr, never mixed into the one result line.
    assert!(out.stderr.contains("embedder ready"), "{}", out.stderr);
}

/// How many sessions of this database wait for a row lock taken with `FOR NO KEY UPDATE`.
async fn waiting_for_the_row(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND wait_event_type = 'Lock'
           AND query LIKE '%FOR NO KEY UPDATE%' AND query NOT LIKE '%pg_stat_activity%'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn wait_for_waiters(pool: &PgPool, n: i64) {
    for _ in 0..600 {
        if waiting_for_the_row(pool).await >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("{n} sessions never queued on the source row");
}

async fn indexed_paths(pool: &PgPool, source: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT external_id FROM documents WHERE source_id = $1 ORDER BY external_id",
    )
    .bind(source)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A server push computes its diff from the indexed commit it read, then queues on the source
/// row. A `lore index` (another process, so no in-process lock) that queued first commits a
/// newer commit in between. The push must not apply its diff on top of that: it has to ask for
/// a full bundle, and leave the index as the job made it.
#[sqlx::test(migrations = "./migrations")]
async fn a_push_that_queued_behind_an_index_does_not_apply_its_stale_diff(pool: PgPool) {
    let env = Env::new(pool.clone());
    let (tenant, key, source) = env.tenant("jobs").await;
    let repo = fixture_repo();

    // A: the server's full push. B adds ghost.md, and the job's remote is a clone at B. C drops
    // ghost.md again and adds a file.
    let a = head(&repo);
    let (status, body) = put_bundle(
        &env.router,
        &key,
        source,
        bundle(&repo, &["HEAD"]),
        &a,
        true,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    std::fs::write(repo.path().join("ghost.md"), "# Ghost\n\nOnly B has me.\n").unwrap();
    commit_all(&repo, "B adds ghost.md");
    let b = head(&repo);
    let job_remote = tempfile::tempdir().unwrap();
    git(
        repo.path(),
        &["clone", "-q", ".", &job_remote.path().to_string_lossy()],
    );
    std::fs::remove_file(repo.path().join("ghost.md")).unwrap();
    std::fs::write(repo.path().join("c.md"), "# See\n\nOnly C has me.\n").unwrap();
    commit_all(&repo, "C drops ghost.md");
    let c = head(&repo);

    // Hold the source row, so both writers queue on it, the job first.
    let mut hold = pool.begin().await.unwrap();
    let held: Uuid = sqlx::query_scalar("SELECT id FROM sources WHERE id = $1 FOR NO KEY UPDATE")
        .bind(source)
        .fetch_one(&mut *hold)
        .await
        .unwrap();
    assert_eq!(held, source);
    let job = tokio::spawn(run(env.command(
        tenant,
        source,
        &file_url(job_remote.path()),
    )));
    wait_for_waiters(&pool, 1).await;
    let push = {
        let router = env.router.clone();
        let key = key.clone();
        let bytes = bundle(&repo, &[&format!("{a}..HEAD")]);
        let c = c.clone();
        tokio::spawn(async move { put_bundle(&router, &key, source, bytes, &c, false).await })
    };
    wait_for_waiters(&pool, 2).await;
    hold.rollback().await.unwrap();

    let job_out = job.await.unwrap();
    assert_eq!(job_out.json()["commit"], b.as_str());
    let (status, body) = push.await.unwrap();
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "need_full_bundle", "{body}");

    // Nothing of the refused push was applied: the index is B, ghost.md included, c.md absent.
    assert_eq!(indexed_commit(&pool, source).await, Some(b));
    let docs = indexed_paths(&pool, source).await;
    assert!(docs.contains(&"ghost.md".to_string()), "{docs:?}");
    assert!(!docs.contains(&"c.md".to_string()), "{docs:?}");

    // The refused push had already moved the server's indexed ref and checkout to C. They are
    // put back to A, where this push started, and are not left ahead of the database.
    let server_repo = env
        ._server_data
        .path()
        .join("repos")
        .join(source.to_string());
    let mirror = server_repo.join("mirror.git");
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/heads/lore-indexed"]).trim(),
        a
    );
    assert!(
        !server_repo.join("checkout/c.md").exists(),
        "the checkout is still at the refused commit"
    );
    assert!(server_repo.join("checkout/README.md").exists());

    // The client's answer to the 409, a full push of C, lands cleanly and drops ghost.md.
    let (status, body) = put_bundle(
        &env.router,
        &key,
        source,
        bundle(&repo, &["HEAD"]),
        &c,
        true,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(indexed_commit(&pool, source).await, Some(c));
    let docs = indexed_paths(&pool, source).await;
    assert!(!docs.contains(&"ghost.md".to_string()), "{docs:?}");
    assert!(docs.contains(&"c.md".to_string()), "{docs:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn the_lock_connection_is_kept_alive_while_held_and_freed_on_release(pool: PgPool) {
    let source = Uuid::new_v4();
    let ping = std::time::Duration::from_millis(100);
    let held = SourceAdvisoryLock::try_acquire_pinging(&dsn(&pool), source, ping)
        .await
        .unwrap()
        .expect("the lock is free");

    // The backend that holds the advisory lock, and when it last ran a statement.
    let state_change = || async {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT a.state_change FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
             WHERE l.locktype = 'advisory' AND l.granted AND l.pid <> pg_backend_pid()
               AND a.datname = current_database()",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let first = state_change().await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let later = state_change().await;
    assert!(
        later > first,
        "the idle connection was used: {first:?} {later:?}"
    );

    // Still held after several ping intervals.
    assert!(
        SourceAdvisoryLock::try_acquire(&dsn(&pool), source)
            .await
            .unwrap()
            .is_none()
    );

    held.release().await;
    let again = SourceAdvisoryLock::try_acquire(&dsn(&pool), source)
        .await
        .unwrap()
        .expect("released, the lock is free");
    // Dropping without release frees it too (the task closes the connection).
    drop(again);
    let mut freed = false;
    for _ in 0..50 {
        if let Some(l) = SourceAdvisoryLock::try_acquire(&dsn(&pool), source)
            .await
            .unwrap()
        {
            l.release().await;
            freed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(freed, "a dropped lock is freed");
}

/// A TCP forwarder to the test database whose traffic can be frozen: the connection stays open
/// and nothing moves on it, like a half-open connection behind a NAT that lost its state.
struct Proxy {
    /// The DSN of the database through the proxy.
    dsn: String,
    frozen: Arc<AtomicBool>,
}

async fn pump(
    mut from: impl tokio::io::AsyncRead + Unpin,
    mut to: impl tokio::io::AsyncWrite + Unpin,
    frozen: Arc<AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        while frozen.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

async fn proxy(pool: &PgPool) -> Proxy {
    use tokio::net::{TcpListener, TcpStream};
    let direct = reqwest::Url::parse(&dsn(pool)).unwrap();
    let upstream = format!(
        "{}:{}",
        direct.host_str().unwrap(),
        direct.port().unwrap_or(5432)
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let frozen = Arc::new(AtomicBool::new(false));
    let flag = frozen.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = TcpStream::connect(&upstream).await else {
                continue;
            };
            let (client_read, client_write) = client.into_split();
            let (server_read, server_write) = server.into_split();
            tokio::spawn(pump(client_read, server_write, flag.clone()));
            tokio::spawn(pump(server_read, client_write, flag.clone()));
        }
    });
    let mut via = direct;
    via.set_host(Some("127.0.0.1")).unwrap();
    via.set_port(Some(port)).unwrap();
    Proxy {
        dsn: via.to_string(),
        frozen,
    }
}

/// On a half-open connection a query never gets an answer. Releasing the lock, which sends an
/// unlock, must not hang: each step has a timeout.
#[sqlx::test(migrations = "./migrations")]
async fn releasing_a_lock_on_a_dead_connection_returns(pool: PgPool) {
    let proxy = proxy(&pool).await;
    let source = Uuid::new_v4();
    let held = SourceAdvisoryLock::try_acquire_timed(
        &proxy.dsn,
        source,
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_millis(300),
    )
    .await
    .unwrap()
    .expect("the lock is free");

    proxy.frozen.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(5), held.release())
        .await
        .expect("release returned although the connection never answers");
}

/// The same for the keepalive: a ping that gets no answer must not keep `release` waiting.
#[sqlx::test(migrations = "./migrations")]
async fn a_keepalive_on_a_dead_connection_does_not_block_the_release(pool: PgPool) {
    let proxy = proxy(&pool).await;
    let source = Uuid::new_v4();
    let held = SourceAdvisoryLock::try_acquire_timed(
        &proxy.dsn,
        source,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(300),
    )
    .await
    .unwrap()
    .expect("the lock is free");

    proxy.frozen.store(true, Ordering::SeqCst);
    // A ping is now on the wire and unanswered.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), held.release())
        .await
        .expect("release returned although a ping is unanswered");
}
