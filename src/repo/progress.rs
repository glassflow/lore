//! Progress of index runs, kept in the `index_runs` table. A run is `queued` for its source's
//! lock, `cloning` (fetching the commit into the mirror), `indexing` its files, and ends `done`
//! or `failed`.
//!
//! The indexer reports through a [`RunHandle`] from [`RunWriter::start`]. The handle's methods
//! are plain calls that never wait: they put the latest counts in a `watch` channel, and a
//! task owned by the handle writes them to Postgres through the pool, never through the index
//! transaction (which nobody can see until it commits). The task writes
//! - a stage change, and the end of the run, at once;
//! - file counts at most once per second;
//! - the same row again every 5 seconds when nothing changed, so `updated_at` says the run is
//!   alive even while one big file embeds.
//!
//! A failed write is logged at warn level and never fails the index. A process that dies
//! leaves its row unfinished; [`IndexRun::stale`] says so on read, and nothing rewrites the row.
//! The writer needs a [`PgPool`] and nothing else, so a process without an HTTP server can use
//! it too.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use super::IndexReport;
use crate::store::IndexStats;

/// File counts are written at most this often.
pub const WRITE_EVERY: Duration = Duration::from_secs(1);
/// A live run is written again after this long without a write.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
/// The longest error kept.
const MAX_ERROR_CHARS: usize = 500;
/// How long `done` and `failed` wait for the last write.
const FINISH_WAIT: Duration = Duration::from_secs(10);
/// How often the write of a run's end is tried.
const FINAL_TRIES: u32 = 3;

/// How many runs of one source are kept. A run that is going is never removed; of the others,
/// the newest ones stay, so the admin page's recent list and a source's latest run are always
/// there, and the table does not grow with every push forever.
pub const KEEP_RUNS_PER_SOURCE: i64 = 50;

/// The longest the prune of a source's old runs may take.
const PRUNE_TIMEOUT: Duration = Duration::from_secs(5);

/// The run has not ended (`r` is the alias of `index_runs`).
const NOT_ENDED: &str = "r.stage NOT IN ('done','failed')";
/// The run was written in the last 90 seconds. Older and not ended means stale: the process
/// that ran it is gone.
const FRESH: &str = "r.updated_at >= now() - interval '90 seconds'";

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Waiting for another run of the same source to end.
    Queued,
    /// Fetching the commit into the mirror and listing the files to index.
    Cloning,
    /// Indexing files: `files_done` of `files_total`.
    Indexing,
    Done,
    Failed,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Queued => "queued",
            Stage::Cloning => "cloning",
            Stage::Indexing => "indexing",
            Stage::Done => "done",
            Stage::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Stage::Done | Stage::Failed)
    }
}

/// The latest numbers of a run, as the handle sets them and the writer stores them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    stage: Stage,
    commit: String,
    full: bool,
    files_done: usize,
    files_total: Option<usize>,
    chunks: usize,
    chunks_embedded: usize,
    embed_ms: u64,
    error: Option<String>,
}

impl Snapshot {
    fn queued(commit: &str, full: bool) -> Self {
        Self {
            stage: Stage::Queued,
            commit: commit.to_string(),
            full,
            files_done: 0,
            files_total: None,
            chunks: 0,
            chunks_embedded: 0,
            embed_ms: 0,
            error: None,
        }
    }
}

/// Starts runs. It holds a pool and the two intervals, so any process can build one.
#[derive(Clone)]
pub struct RunWriter {
    pool: PgPool,
    write_every: Duration,
    heartbeat_every: Duration,
}

impl RunWriter {
    pub fn new(pool: PgPool) -> Self {
        Self::with_intervals(pool, WRITE_EVERY, HEARTBEAT_EVERY)
    }

    /// Like [`RunWriter::new`] with other intervals, for tests.
    pub fn with_intervals(pool: PgPool, write_every: Duration, heartbeat_every: Duration) -> Self {
        Self {
            pool,
            write_every,
            heartbeat_every,
        }
    }

    /// Records a run of `commit` for a source, `queued`. It waits a moment for the row to exist,
    /// but not for long: the insert can wait for a lock that another run of the source holds,
    /// and the run goes on without it. The run ends through the returned handle; a handle
    /// dropped before that marks the run failed.
    pub async fn start(
        &self,
        source_id: Uuid,
        tenant_id: Uuid,
        commit: &str,
        full: bool,
    ) -> RunHandle {
        let (tx, rx) = watch::channel(Snapshot::queued(commit, full));
        let (inserted_tx, inserted_rx) = oneshot::channel();
        let new = NewRun {
            source_id,
            tenant_id,
            commit: commit.to_string(),
            full,
        };
        let task = tokio::spawn(write_run(self.clone(), new, rx, inserted_tx));
        let _ = tokio::time::timeout(Duration::from_secs(2), inserted_rx).await;
        RunHandle {
            tx,
            task: Some(task),
            ended: false,
        }
    }
}

struct NewRun {
    source_id: Uuid,
    tenant_id: Uuid,
    commit: String,
    full: bool,
}

/// The indexer's handle on its run. The calls never wait and never fail.
pub struct RunHandle {
    tx: watch::Sender<Snapshot>,
    task: Option<JoinHandle<()>>,
    ended: bool,
}

impl RunHandle {
    /// The source's lock is held: the commit is being fetched.
    pub fn cloning(&self) {
        self.tx.send_modify(|s| s.stage = Stage::Cloning);
    }

    /// The commit is known now. A run that starts before its commit is fetched (`lore index`)
    /// records the ref it fetches, such as `HEAD`, until this is called.
    pub fn commit(&self, commit: &str) {
        self.tx.send_modify(|s| s.commit = commit.to_string());
    }

    /// Vectors embedded and milliseconds spent embedding so far.
    pub fn embedding(&self) -> (usize, u64) {
        let s = self.tx.borrow();
        (s.chunks_embedded, s.embed_ms)
    }

    /// The changes are listed: `files_total` files to index or delete.
    pub fn indexing(&self, files_total: usize, full: bool) {
        self.tx.send_modify(|s| {
            s.stage = Stage::Indexing;
            s.files_total = Some(files_total);
            s.files_done = 0;
            s.full = full;
        });
    }

    /// One more file is done; `totals` are the run's counts so far.
    pub fn file_done(&self, totals: &IndexStats) {
        self.tx.send_modify(|s| {
            s.files_done += 1;
            s.chunks = totals.chunks;
            s.chunks_embedded = totals.embedded;
            s.embed_ms = u64::try_from(totals.embed_time.as_millis()).unwrap_or(u64::MAX);
        });
    }

    /// The run is done. Waits for the last write (for a while at most).
    pub async fn done(mut self, report: &IndexReport) {
        self.tx.send_modify(|s| {
            s.stage = Stage::Done;
            s.chunks = report.chunks;
            s.error = None;
        });
        self.finish().await;
    }

    /// The run failed with `error`, which is cut to 500 characters. Waits for the last write
    /// (for a while at most).
    pub async fn failed(mut self, error: &str) {
        let error = error.chars().take(MAX_ERROR_CHARS).collect();
        self.tx.send_modify(|s| {
            s.stage = Stage::Failed;
            s.error = Some(error);
        });
        self.finish().await;
    }

    async fn finish(&mut self) {
        self.ended = true;
        if let Some(task) = self.task.take()
            && tokio::time::timeout(FINISH_WAIT, task).await.is_err()
        {
            tracing::warn!("the last write of an index run is taking long; not waiting for it");
        }
    }
}

impl Drop for RunHandle {
    /// A run dropped before it ends (its task was cancelled, or it panicked) is failed. The
    /// writer task stores that; nothing here waits.
    fn drop(&mut self) {
        if !self.ended {
            self.tx.send_modify(|s| {
                s.stage = Stage::Failed;
                s.error = Some("the index stopped before it ended".to_string());
            });
        }
    }
}

/// The writer task of one run: inserts the row, then stores each new snapshot by the rules in
/// the module text, and ends after it stored an end state.
async fn write_run(
    writer: RunWriter,
    new: NewRun,
    mut rx: watch::Receiver<Snapshot>,
    inserted: oneshot::Sender<()>,
) {
    let row = insert_run(&writer.pool, &new).await;
    let _ = inserted.send(());
    let id = match row {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, source = %new.source_id, "could not record an index run");
            return;
        }
    };
    // Old rows go now, after the new one exists and the push has stopped waiting for it. A
    // failure only leaves them for the next run.
    match tokio::time::timeout(
        PRUNE_TIMEOUT,
        prune_runs(&writer.pool, new.source_id, KEEP_RUNS_PER_SOURCE),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, source = %new.source_id, "could not prune old index runs");
        }
        Err(_) => tracing::warn!(source = %new.source_id, "pruning old index runs took too long"),
    }
    let mut written = Snapshot::queued(&new.commit, new.full);
    let mut last_write = Instant::now();
    let mut last_failed = false;
    let mut final_tries = 0;
    loop {
        let snap = rx.borrow_and_update().clone();
        let now = Instant::now();
        let throttled = last_write + writer.write_every;
        let heartbeat = last_write + writer.heartbeat_every;
        // When the next write is due. A stage change and an end go at once. After a failed
        // write everything waits, so a broken database is not hammered.
        let at = if snap == written {
            heartbeat
        } else if last_failed {
            if snap.stage.is_terminal() {
                throttled
            } else {
                heartbeat
            }
        } else if snap.stage != written.stage || snap.stage.is_terminal() {
            now
        } else {
            throttled
        };
        if at <= now {
            match store(&writer.pool, id, &snap).await {
                Ok(()) => {
                    written = snap.clone();
                    last_failed = false;
                }
                Err(e) => {
                    tracing::warn!(error = %e, run = id, "could not write index progress");
                    last_failed = true;
                    if snap.stage.is_terminal() {
                        final_tries += 1;
                    }
                }
            }
            last_write = Instant::now();
            if snap.stage.is_terminal() && (!last_failed || final_tries >= FINAL_TRIES) {
                return;
            }
            continue;
        }
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    // The handle is gone, and its drop failed the run: this is the last
                    // snapshot.
                    let last = rx.borrow().clone();
                    if let Err(e) = store(&writer.pool, id, &last).await {
                        tracing::warn!(error = %e, run = id, "could not write index progress");
                    }
                    return;
                }
            }
            _ = tokio::time::sleep_until(at) => {}
        }
    }
}

/// Inserts the row. A lock wait (a tenant delete holds the tenant row) ends after 2 seconds, so
/// the insert cannot hold a pool connection for long. When that timeout fires (or the insert
/// fails for any other reason) the run has no row: the push goes on, and `GET .../progress`
/// shows the previous run (or 404 `no_index_run`) and the admin page shows nothing for it. The
/// caller logs the error and does not retry.
async fn insert_run(pool: &PgPool, new: &NewRun) -> sqlx::Result<i64> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '2s'")
        .execute(&mut *tx)
        .await?;
    let id = sqlx::query_scalar(
        "INSERT INTO index_runs (source_id, tenant_id, commit, \"full\", stage)
         VALUES ($1, $2, $3, $4, 'queued') RETURNING id",
    )
    .bind(new.source_id)
    .bind(new.tenant_id)
    .bind(&new.commit)
    .bind(new.full)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Deletes the source's runs beyond the newest `keep`, except runs that are going (not ended and
/// not stale). A stale run, whose process is gone, counts as old like an ended one. Returns how
/// many rows went.
pub async fn prune_runs(pool: &PgPool, source_id: Uuid, keep: i64) -> sqlx::Result<u64> {
    let done = sqlx::query(&format!(
        "DELETE FROM index_runs r
         WHERE r.source_id = $1
           AND NOT ({NOT_ENDED} AND {FRESH})
           AND r.id < COALESCE(
             (SELECT min(k.id) FROM (SELECT id FROM index_runs
                                     WHERE source_id = $1 ORDER BY id DESC LIMIT $2) k),
             0)"
    ))
    .bind(source_id)
    .bind(keep)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

fn db_int(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Stores a snapshot. The database's clock sets `updated_at` and `finished_at`, so processes
/// on different hosts agree on what is old.
async fn store(pool: &PgPool, id: i64, s: &Snapshot) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE index_runs SET stage = $2, \"full\" = $3, files_done = $4, files_total = $5,
           chunks = $6, chunks_embedded = $7, embed_ms = $8, error = $9, updated_at = now(),
           finished_at = CASE WHEN $10 THEN COALESCE(finished_at, now()) END,
           commit = $11
         WHERE id = $1",
    )
    .bind(id)
    .bind(s.stage.as_str())
    .bind(s.full)
    .bind(db_int(s.files_done))
    .bind(s.files_total.map(db_int))
    .bind(db_int(s.chunks))
    .bind(db_int(s.chunks_embedded))
    .bind(i64::try_from(s.embed_ms).unwrap_or(i64::MAX))
    .bind(s.error.as_deref())
    .bind(s.stage.is_terminal())
    .bind(&s.commit)
    .execute(pool)
    .await?;
    Ok(())
}

/// One run, as `GET /v1/repos/{id}/progress` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct IndexRun {
    pub id: i64,
    #[serde(skip)]
    pub source_id: Uuid,
    #[serde(skip)]
    pub tenant_id: Uuid,
    pub stage: String,
    pub commit: String,
    /// Whether the whole tree is indexed: asked for by the client, or the source's first run.
    pub full: bool,
    /// Files to index or delete. `None` until the commit is fetched and the changes are listed.
    pub files_total: Option<i32>,
    pub files_done: i32,
    /// Chunks written by this run so far (unchanged files add none).
    pub chunks: i32,
    /// How many vectors the embedder computed for those chunks. The others reused a vector
    /// from the embedding cache or from the file's earlier chunks.
    pub chunks_embedded: i32,
    /// Wall-clock milliseconds spent embedding, the CPU-heavy part of a run. It includes waits
    /// for the embedder while another run uses it.
    pub embed_ms: i64,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Why a failed run failed.
    pub error: Option<String>,
    /// The run has not ended and was not written for 90 seconds: its process is gone. The
    /// stored stage is left as it was.
    pub stale: bool,
}

/// The columns of [`IndexRun`], with `stale` worked out by the database's clock.
fn run_columns() -> String {
    format!(
        "r.id, r.source_id, r.tenant_id, r.stage, r.commit, r.full, r.files_total, r.files_done,
         r.chunks, r.chunks_embedded, r.embed_ms, r.started_at, r.updated_at, r.finished_at,
         r.error, ({NOT_ENDED} AND NOT {FRESH}) AS stale"
    )
}

/// The newest run of a source, or `None` when it never had one.
pub async fn latest_run(pool: &PgPool, source_id: Uuid) -> sqlx::Result<Option<IndexRun>> {
    sqlx::query_as(&format!(
        "SELECT {} FROM index_runs r WHERE r.source_id = $1 ORDER BY r.id DESC LIMIT 1",
        run_columns()
    ))
    .bind(source_id)
    .fetch_optional(pool)
    .await
}

/// A run with the names the admin page shows.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NamedRun {
    #[sqlx(flatten)]
    pub run: IndexRun,
    pub tenant: String,
    pub source: String,
}

/// The runs going now (not ended, not stale), oldest first, and the `recent` newest others,
/// newest first.
pub async fn admin_runs(
    pool: &PgPool,
    recent: i64,
) -> sqlx::Result<(Vec<NamedRun>, Vec<NamedRun>)> {
    let select = format!(
        "SELECT {}, t.name AS tenant, s.name AS source
         FROM index_runs r
         JOIN sources s ON s.id = r.source_id
         JOIN tenants t ON t.id = r.tenant_id",
        run_columns()
    );
    let running = sqlx::query_as(&format!(
        "{select} WHERE {NOT_ENDED} AND {FRESH} ORDER BY r.id"
    ))
    .fetch_all(pool)
    .await?;
    let ended = sqlx::query_as(&format!(
        "{select} WHERE NOT ({NOT_ENDED} AND {FRESH}) ORDER BY r.id DESC LIMIT $1"
    ))
    .bind(recent)
    .fetch_all(pool)
    .await?;
    Ok((running, ended))
}

/// Whether a run of this source is going: not ended and not stale.
pub async fn source_has_live_run(
    conn: impl sqlx::PgExecutor<'_>,
    source_id: Uuid,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM index_runs r
         WHERE r.source_id = $1 AND {NOT_ENDED} AND {FRESH})"
    ))
    .bind(source_id)
    .fetch_one(conn)
    .await
}

/// Whether a run of any source of this tenant is going: not ended and not stale.
pub async fn tenant_has_live_run(
    conn: impl sqlx::PgExecutor<'_>,
    tenant_id: Uuid,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM index_runs r
         WHERE r.tenant_id = $1 AND {NOT_ENDED} AND {FRESH})"
    ))
    .bind(tenant_id)
    .fetch_one(conn)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_name_themselves_and_say_whether_they_end_a_run() {
        let all = [
            (Stage::Queued, "queued", false),
            (Stage::Cloning, "cloning", false),
            (Stage::Indexing, "indexing", false),
            (Stage::Done, "done", true),
            (Stage::Failed, "failed", true),
        ];
        for (stage, name, terminal) in all {
            assert_eq!(stage.as_str(), name);
            assert_eq!(stage.is_terminal(), terminal);
            assert_eq!(serde_json::to_value(stage).unwrap(), name);
        }
    }

    #[test]
    fn db_counts_do_not_overflow() {
        assert_eq!(db_int(7), 7);
        assert_eq!(db_int(usize::MAX), i32::MAX);
    }
}
