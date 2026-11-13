//! Git bundle ingest. A repo source keeps a bare mirror and a checkout under
//! `<data_dir>/repos/<source_id>/`. A pushed bundle is fetched into the mirror, and only the files
//! that changed since the last indexed commit are chunked, embedded and stored.
//!
//! Security rules, because a bundle comes from a client and its content is untrusted:
//! - File content is read from git objects (`git cat-file blob <commit>:<path>`), never through
//!   the checkout, so a symlink in the repo can never make the server read a host file.
//! - The mirror and the checkout use `core.symlinks=false`, so git writes no symlink at all.
//!   Symlinks (mode 120000) and submodules (160000) are skipped and counted.
//! - Every git call is an argument array (no shell). Paths are only parsed from `-z` output and
//!   only appear in the `<commit>:<path>` form, which cannot be read as an option. The commit is
//!   40 lowercase hex characters, checked before use.
//! - The server's git ignores the host's git configuration and never prompts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::Command;
use uuid::Uuid;

use crate::chunk::{CHUNKER_VERSION, Language, chunk_file, language_for};
use crate::config::Config;
use crate::embed::Embedder;
use crate::error::AppError;
use crate::store::{
    IndexStats, NewDocument, PendingEmbeddings, Source, delete_document, document_hash, get_source,
    index_repo_text, list_external_ids, set_indexed_commit,
};

pub mod lock;
pub mod progress;
pub mod sync;

use progress::{RunHandle, RunWriter};

/// Files bigger than this are not indexed.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// A path with one of these as a segment is not indexed.
const SKIP_DIRS: [&str; 8] = [
    "node_modules",
    "vendor",
    "dist",
    "build",
    "target",
    ".git",
    ".venv",
    "__pycache__",
];

/// Files with these names are not indexed.
const SKIP_NAMES: [&str; 5] = [
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
    "go.sum",
];

/// Files whose name ends with one of these are not indexed.
const SKIP_SUFFIXES: [&str; 13] = [
    ".min.js", ".map", ".svg", ".png", ".jpg", ".gif", ".ico", ".pdf", ".zip", ".gz", ".woff",
    ".woff2", ".ttf",
];

/// Every bundle file starts with one of these header lines (16 bytes each).
const BUNDLE_HEADERS: [&[u8]; 2] = [b"# v2 git bundle\n", b"# v3 git bundle\n"];

/// What one bundle push did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexReport {
    pub indexed_commit: String,
    pub files_indexed: usize,
    pub files_unchanged: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub chunks: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// The mirror cannot apply this bundle (it was lost or reset while the database kept the
    /// indexed commit). The client must send a full bundle with `full=true`.
    #[error("need a full bundle")]
    NeedFull,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<sqlx::Error> for IngestError {
    fn from(e: sqlx::Error) -> Self {
        IngestError::Other(e.into())
    }
}

impl From<GitError> for IngestError {
    fn from(e: GitError) -> Self {
        IngestError::Other(e.into())
    }
}

impl From<IngestError> for AppError {
    fn from(e: IngestError) -> Self {
        match e {
            IngestError::NeedFull => AppError::Conflict("need_full_bundle".to_string()),
            IngestError::BadRequest(m) => AppError::BadRequest(m),
            // The response says only "internal error"; the cause (git's stderr included) is
            // logged when the error becomes a response.
            IngestError::Other(e) => AppError::Internal(e),
        }
    }
}

/// A failed git call: it could not start, or exited non-zero. `stderr` stays on the server.
#[derive(Debug, thiserror::Error)]
#[error("git {args} failed ({status}): {stderr}")]
pub struct GitError {
    args: String,
    status: String,
    pub stderr: String,
}

/// What indexing needs, and nothing more: the database, the configuration, the embedder and the
/// per-source locks. The server builds one from its `AppState` (`AppState::index_ctx`); `lore
/// index` builds one without an HTTP server and without a model provider, so the indexing path
/// can never call an LLM.
#[derive(Clone)]
pub struct IndexCtx {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub embedder: Arc<dyn Embedder>,
    pub source_locks: Arc<SourceLocks>,
}

/// One lock per source, so two pushes to the same repo never run at once. lore runs as a single
/// replica, so an in-process lock is enough. An entry is added the first time a source (one that
/// exists in the database) is ingested and then reused, so the map holds at most one entry per
/// source.
#[derive(Default)]
pub struct SourceLocks {
    map: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
}

impl SourceLocks {
    /// The lock of a source.
    pub fn get(&self, source_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        map.entry(source_id).or_default().clone()
    }

    /// How many sources have a lock entry.
    pub fn len(&self) -> usize {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Runs `git` with the given arguments and returns stdout. The environment keeps host
/// configuration out (`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_NOSYSTEM`), turns prompts off, makes
/// pathspecs literal and forces English messages (the caller matches on stderr).
async fn git(args: &[&str]) -> Result<Vec<u8>, GitError> {
    git_env(args, &[]).await
}

/// [`git`] with extra environment variables, set after the defaults.
async fn git_env(args: &[&str], envs: &[(&str, &str)]) -> Result<Vec<u8>, GitError> {
    let fail = |status: String, stderr: String| GitError {
        args: args.join(" "),
        status,
        stderr,
    };
    let out = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("LC_ALL", "C")
        // Not for git: the database password and the `lore index` token.
        .env_remove("LORE_DATABASE_URL")
        .env_remove("LORE_INDEX_TOKEN")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| fail(format!("spawn: {e}"), String::new()))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(fail(
            out.status.to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// `<data_dir>/repos/<source_id>/mirror.git` and `.../checkout`, absolute, as UTF-8 strings (git
/// takes them as arguments). The source id is a uuid from the database, never client input.
struct RepoPaths {
    dir: PathBuf,
    mirror: String,
    checkout: String,
}

impl RepoPaths {
    fn new(data_dir: &Path, source_id: Uuid) -> anyhow::Result<Self> {
        let dir = std::path::absolute(data_dir)
            .context("resolving the data directory")?
            .join("repos")
            .join(source_id.to_string());
        let text = |p: PathBuf| {
            p.into_os_string()
                .into_string()
                .map_err(|p| anyhow!("data directory path is not valid UTF-8: {p:?}"))
        };
        Ok(Self {
            mirror: text(dir.join("mirror.git"))?,
            checkout: text(dir.join("checkout"))?,
            dir,
        })
    }

    /// `git --git-dir=<mirror> <args>`
    async fn mirror_git(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let git_dir = format!("--git-dir={}", self.mirror);
        let mut full = vec![git_dir.as_str()];
        full.extend_from_slice(args);
        git(&full).await
    }

    /// `git --git-dir=<mirror> --work-tree=<checkout> -c core.symlinks=false <args>`
    async fn checkout_git(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let git_dir = format!("--git-dir={}", self.mirror);
        let work_tree = format!("--work-tree={}", self.checkout);
        let mut full = vec![
            git_dir.as_str(),
            work_tree.as_str(),
            "-c",
            "core.symlinks=false",
        ];
        full.extend_from_slice(args);
        git(&full).await
    }
}

/// The bytes each repo source takes on disk (its mirror and checkout), by source id, from the
/// directories under `<data_dir>/repos`. Blocking: it walks every file, so call it off the
/// async runtime. Unreadable entries are skipped, and a missing directory gives an empty map.
pub fn disk_usage(data_dir: &Path) -> HashMap<Uuid, u64> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("repos")) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let id: Uuid = e.file_name().to_str()?.parse().ok()?;
            Some((id, dir_bytes(&e.path())))
        })
        .collect()
}

/// The total length of the files under `dir`. Symlinks are counted as links, never followed.
fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// Removes the mirror and checkout of each source, `<data_dir>/repos/<source_id>`. A missing
/// directory is fine; any other failure is logged and the next directory is still removed.
pub async fn remove_repo_dirs(data_dir: &Path, source_ids: &[Uuid]) {
    for id in source_ids {
        let dir = data_dir.join("repos").join(id.to_string());
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => tracing::info!(source = %id, "repo directory removed"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(source = %id, dir = %dir.display(), error = %e, "could not remove the repo directory")
            }
        }
    }
}

/// True for exactly 40 lowercase hex characters.
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Ingests one bundle: fetches it into the source's mirror, checks that its `HEAD` is
/// `expected_ref`, refreshes the checkout and indexes the files that changed.
///
/// `full` (or a source never indexed before) indexes every file of the tree and deletes the
/// documents of paths that are gone or skipped. Otherwise only the diff from the last indexed
/// commit is processed. A per-source lock is held for the whole call.
///
/// Once the request is checked, the push is recorded in the `index_runs` table, with its progress
/// and how it ended.
/// All database changes commit together; errors and cancellation roll them back. The checkout
/// is live, not part of that transaction: grep/read_file do not share search's snapshot guarantee.
pub async fn ingest_bundle(
    state: &IndexCtx,
    source: &Source,
    bundle: &Path,
    expected_ref: &str,
    full: bool,
) -> Result<IndexReport, IngestError> {
    if !is_full_sha(expected_ref) {
        return Err(IngestError::BadRequest(
            "ref must be a full 40 character lowercase hex commit sha".to_string(),
        ));
    }
    if source.kind != "repo" {
        return Err(IngestError::BadRequest("source is not a repo".to_string()));
    }
    // Without this check git would treat a file holding "gitdir: <path>" as a pointer to a
    // repository on this host and fetch from it.
    check_bundle_header(bundle).await?;
    let bundle_arg = bundle
        .to_str()
        .ok_or_else(|| anyhow!("bundle path is not valid UTF-8"))?;

    let push = RunWriter::new(state.pool.clone())
        .start(source.id, source.tenant_id, expected_ref, full)
        .await;
    let result = ingest_locked(state, source, bundle_arg, expected_ref, full, &push).await;
    match &result {
        Ok(report) => push.done(report).await,
        Err(e) => push.failed(&push_error(e)).await,
    }
    result
}

/// How a failed push reads in its run row and on the admin page. An internal error stays in the server log, as it
/// does for the HTTP response.
fn push_error(e: &IngestError) -> String {
    match e {
        IngestError::NeedFull => "need a full bundle (the CLI sends one next)".to_string(),
        IngestError::BadRequest(m) => m.clone(),
        IngestError::Other(_) => "internal error (see the server log)".to_string(),
    }
}

/// The body of [`ingest_bundle`]: takes the source's lock, then fetches and indexes, reporting
/// each step to `push`.
async fn ingest_locked(
    state: &IndexCtx,
    source: &Source,
    bundle_arg: &str,
    expected_ref: &str,
    full: bool,
    push: &RunHandle,
) -> Result<IndexReport, IngestError> {
    let lock = state.source_locks.get(source.id);
    let _guard = lock.lock().await;
    push.cloning();

    // The caller read the source before waiting for the lock, so the last indexed commit may
    // have moved since.
    let source = get_source(&state.pool, source.tenant_id, source.id)
        .await?
        .ok_or_else(|| anyhow!("source {} vanished", source.id))?;

    let paths = RepoPaths::new(&state.config.data_dir, source.id)?;
    ensure_mirror(&paths).await?;
    fetch_bundle(&paths, bundle_arg, expected_ref).await?;
    index_commit(state, &source, &paths, expected_ref, full, push).await
}

/// Indexes `commit`, which the mirror already holds, and records it as the source's indexed
/// commit. `full` (or a source never indexed before) indexes the whole tree; otherwise only the
/// diff from the last indexed commit, which is `NeedFull` when the mirror lacks that commit or
/// when another process moved the indexed commit while this waited for the row lock. The caller
/// holds the source's lock and passes the source as read under it.
async fn index_commit(
    state: &IndexCtx,
    source: &Source,
    paths: &RepoPaths,
    commit: &str,
    full: bool,
    push: &RunHandle,
) -> Result<IndexReport, IngestError> {
    paths
        .mirror_git(&["update-ref", "refs/heads/lore-indexed", commit])
        .await?;

    let full_mode = full || source.indexed_commit.is_none();
    let changes = if full_mode {
        Changes::from_tree(paths, commit).await?
    } else {
        // `indexed_commit` is only None in full mode, so it is set here.
        let indexed = source.indexed_commit.as_deref().unwrap_or_default();
        Changes::from_diff(paths, indexed, commit).await?
    };

    refresh_checkout(paths, commit).await?;

    // MVCC keeps the old repo visible until every document and the commit pointer are ready.
    // Keep all lookups on this connection: even a one-connection pool can finish a push.
    let mut tx = state.pool.begin().await?;
    // Parent first: a tenant cascade must not lock the tenant while we hold its source and
    // subsequently need a tenant FK lock for newly inserted chunks.
    let tenant: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM tenants WHERE id = $1 FOR KEY SHARE")
            .bind(source.tenant_id)
            .fetch_optional(&mut *tx)
            .await?;
    if tenant.is_none() {
        return Err(anyhow!("tenant {} vanished", source.tenant_id).into());
    }
    // NO KEY UPDATE still excludes other pushes and a delete, but not the foreign-key check of a
    // queued push recording its `index_runs` row.
    let locked: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT id, indexed_commit FROM sources WHERE id = $1 AND tenant_id = $2
         FOR NO KEY UPDATE",
    )
    .bind(source.id)
    .bind(source.tenant_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((_, locked_commit)) = locked else {
        return Err(anyhow!("source {} vanished", source.id).into());
    };
    // The diff above was computed from `source.indexed_commit`, read under the in-process lock
    // only. Another process (`lore index`) may have committed another commit since, and the row
    // lock is the first point where that is excluded. Applying the diff on top of it would keep
    // files only that commit had, so ask for a full index instead; nothing is written yet.
    if !full_mode && locked_commit != source.indexed_commit {
        tx.rollback().await?;
        return Err(IngestError::NeedFull);
    }
    let mut job = Job {
        state,
        tx,
        pending: PendingEmbeddings::default(),
        source,
        paths,
        commit,
        report: IndexReport {
            indexed_commit: commit.to_string(),
            files_indexed: 0,
            files_unchanged: 0,
            files_skipped: changes.skipped_paths,
            files_deleted: 0,
            chunks: 0,
        },
        kept: HashSet::new(),
        totals: IndexStats::default(),
    };
    push.indexing(changes.upserts.len() + changes.deleted.len(), full_mode);
    for entry in &changes.upserts {
        job.index_entry(entry).await?;
        push.file_done(&job.totals);
    }
    for path in &changes.deleted {
        if job.delete_doc(path).await? {
            job.report.files_deleted += 1;
        }
        push.file_done(&job.totals);
    }
    if full_mode {
        job.delete_vanished().await?;
    }

    set_indexed_commit(&mut *job.tx, source.id, commit).await?;
    job.tx.commit().await?;
    job.pending.publish(&state.pool).await;
    Ok(job.report)
}

/// The longest header line accepted: a ref line is a sha and a ref name.
const MAX_HEADER_LINE: u64 = 4096;

/// Rejects a file that does not start with a complete bundle header: the signature line, then
/// prerequisite, capability and ref lines, then an empty line (the pack follows).
async fn check_bundle_header(bundle: &Path) -> Result<(), IngestError> {
    let not_a_bundle = || IngestError::BadRequest("the upload is not a git bundle".to_string());
    let file = tokio::fs::File::open(bundle)
        .await
        .context("opening the uploaded bundle")?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut first = true;
    loop {
        line.clear();
        let n = (&mut reader)
            .take(MAX_HEADER_LINE)
            .read_until(b'\n', &mut line)
            .await
            .context("reading the uploaded bundle")?;
        // End of file, or a line too long to be a header line, before the empty line.
        if n == 0 || line.last() != Some(&b'\n') {
            return Err(not_a_bundle());
        }
        if first {
            if !BUNDLE_HEADERS.contains(&line.as_slice()) {
                return Err(not_a_bundle());
            }
            first = false;
        } else if line == b"\n" {
            return Ok(());
        }
    }
}

/// Creates the bare mirror when missing and (re)applies its settings.
async fn ensure_mirror(paths: &RepoPaths) -> Result<(), IngestError> {
    let mirror = Path::new(&paths.mirror);
    if !tokio::fs::try_exists(mirror.join("HEAD"))
        .await
        .context("checking the mirror")?
    {
        tokio::fs::create_dir_all(&paths.dir)
            .await
            .context("creating the repo directory")?;
        git(&["init", "--bare", "-q", &paths.mirror]).await?;
    }
    paths
        .mirror_git(&["config", "core.symlinks", "false"])
        .await?;
    paths
        .mirror_git(&["config", "transfer.fsckObjects", "true"])
        .await?;
    Ok(())
}

/// Fetches the bundle's `HEAD` into the mirror and checks that it is `expected_ref`.
async fn fetch_bundle(
    paths: &RepoPaths,
    bundle: &str,
    expected_ref: &str,
) -> Result<(), IngestError> {
    if let Err(e) = paths
        .mirror_git(&["fetch", "--no-tags", "-q", "--", bundle, "HEAD"])
        .await
    {
        return Err(classify_fetch_error(e));
    }
    let fetched = paths
        .mirror_git(&["rev-parse", "--verify", "FETCH_HEAD^{commit}"])
        .await
        .map_err(|_| IngestError::BadRequest("the bundle's HEAD is not a commit".to_string()))?;
    if String::from_utf8_lossy(&fetched).trim() != expected_ref {
        return Err(IngestError::BadRequest(
            "ref does not match the bundle's HEAD".to_string(),
        ));
    }
    Ok(())
}

/// A bundle that needs commits the mirror lacks is `NeedFull`. A fetch that fails because the
/// pack is broken is the client's fault (400). Anything else is a server problem.
fn classify_fetch_error(e: GitError) -> IngestError {
    let stderr = e.stderr.to_ascii_lowercase();
    if stderr.contains("prerequisite") {
        return IngestError::NeedFull;
    }
    const CLIENT_FAULT: [&str; 6] = [
        "index-pack",
        "unpack-objects",
        "early eof",
        "fsck",
        "did not receive expected object",
        "couldn't find remote ref",
    ];
    if CLIENT_FAULT.iter().any(|m| stderr.contains(m)) {
        tracing::warn!(error = %e, "rejected a bundle that git could not fetch");
        return IngestError::BadRequest("the upload is not a valid git bundle".to_string());
    }
    IngestError::Other(e.into())
}

/// Brings the checkout to `commit`. The checkout exists for deep mode (grep and read_file), never
/// for indexing. `core.symlinks=false` makes git write a symlink as a plain file holding the link
/// text.
async fn refresh_checkout(paths: &RepoPaths, commit: &str) -> Result<(), IngestError> {
    let checkout = Path::new(&paths.checkout);
    if !tokio::fs::try_exists(checkout)
        .await
        .context("checking the checkout")?
    {
        tokio::fs::create_dir_all(checkout)
            .await
            .context("creating the checkout directory")?;
        // A stale index would make git think the missing files are already there.
        match tokio::fs::remove_file(Path::new(&paths.mirror).join("index")).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::Error::from(e).context("removing the index").into()),
        }
    }
    paths
        .checkout_git(&["read-tree", "--reset", "-u", commit])
        .await?;
    paths.checkout_git(&["clean", "-fdq"]).await?;
    Ok(())
}

/// A path and its git file mode in the new commit.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: String,
    mode: String,
}

/// What the new commit changes, as far as the index is concerned.
#[derive(Debug, Default, PartialEq, Eq)]
struct Changes {
    /// Added or modified paths (in full mode: every path of the tree).
    upserts: Vec<Entry>,
    /// Paths removed since the last indexed commit (incremental mode only).
    deleted: Vec<String>,
    /// Paths that are not valid UTF-8 and so can never be documents.
    skipped_paths: usize,
}

impl Changes {
    /// Every file of the tree at `commit`.
    async fn from_tree(paths: &RepoPaths, commit: &str) -> Result<Self, IngestError> {
        let out = paths.mirror_git(&["ls-tree", "-r", "-z", commit]).await?;
        Ok(Self::from_records(parse_ls_tree(&out)?))
    }

    /// The changes between the last indexed commit and `commit`. The mirror must still have the
    /// old commit, otherwise it was lost or reset and the client has to send a full bundle.
    async fn from_diff(
        paths: &RepoPaths,
        indexed: &str,
        commit: &str,
    ) -> Result<Self, IngestError> {
        if !is_full_sha(indexed)
            || paths
                .mirror_git(&["cat-file", "-e", &format!("{indexed}^{{commit}}")])
                .await
                .is_err()
        {
            return Err(IngestError::NeedFull);
        }
        // `--raw` gives the new file mode too, so a file that became a symlink is seen as one.
        let out = paths
            .mirror_git(&[
                "diff",
                "--raw",
                "-z",
                "--no-renames",
                "--no-abbrev",
                indexed,
                commit,
            ])
            .await?;
        let mut upserts = Vec::new();
        let mut deleted = Vec::new();
        for change in parse_raw_diff(&out)? {
            match change {
                RawChange::Deleted(path) => deleted.push(path),
                RawChange::Upsert(path, mode) => upserts.push((path, mode)),
            }
        }
        let mut changes = Self::from_records(upserts);
        // A path that is not UTF-8 never had a document, so a deletion of it has nothing to do.
        changes.deleted = deleted
            .into_iter()
            .filter_map(|p| String::from_utf8(p).ok())
            .collect();
        Ok(changes)
    }

    fn from_records(records: Vec<(Vec<u8>, String)>) -> Self {
        let mut changes = Changes::default();
        for (path, mode) in records {
            match String::from_utf8(path) {
                Ok(path) => changes.upserts.push(Entry { path, mode }),
                Err(_) => changes.skipped_paths += 1,
            }
        }
        changes
    }
}

/// Parses `git ls-tree -r -z` output: `<mode> SP <type> SP <sha> TAB <path> NUL` per entry.
fn parse_ls_tree(out: &[u8]) -> anyhow::Result<Vec<(Vec<u8>, String)>> {
    let mut entries = Vec::new();
    for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| anyhow!("unexpected ls-tree record"))?;
        let (meta, path) = record.split_at(tab);
        let meta = std::str::from_utf8(meta).context("ls-tree metadata is not UTF-8")?;
        let mode = meta
            .split(' ')
            .next()
            .filter(|m| !m.is_empty())
            .ok_or_else(|| anyhow!("ls-tree record without a mode"))?;
        entries.push((path.get(1..).unwrap_or_default().to_vec(), mode.to_string()));
    }
    Ok(entries)
}

#[derive(Debug, PartialEq, Eq)]
enum RawChange {
    Deleted(Vec<u8>),
    /// Added, modified or type changed, with the new mode.
    Upsert(Vec<u8>, String),
}

/// Parses `git diff --raw -z --no-renames` output: `:<old mode> <new mode> <old sha> <new sha>
/// <status> NUL <path> NUL` per change.
fn parse_raw_diff(out: &[u8]) -> anyhow::Result<Vec<RawChange>> {
    let mut changes = Vec::new();
    let mut tokens = out.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(meta) = tokens.next() {
        let path = tokens
            .next()
            .ok_or_else(|| anyhow!("diff record without a path"))?
            .to_vec();
        let meta = std::str::from_utf8(meta).context("diff metadata is not UTF-8")?;
        let fields: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [_old_mode, new_mode, _old_sha, _new_sha, status] = fields[..] else {
            return Err(anyhow!("unexpected diff record {meta:?}"));
        };
        match status {
            "D" => changes.push(RawChange::Deleted(path)),
            "A" | "M" | "T" => changes.push(RawChange::Upsert(path, new_mode.to_string())),
            other => return Err(anyhow!("unexpected diff status {other:?}")),
        }
    }
    Ok(changes)
}

/// True when the path is never indexed: build output, vendored code, lock files, minified or
/// binary assets.
fn path_is_skipped(path: &str) -> bool {
    if path.split('/').any(|seg| SKIP_DIRS.contains(&seg)) {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if SKIP_NAMES.contains(&name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    SKIP_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

/// The stored hash of a repo file: of its content and the chunker version, so that a file
/// chunked by an older version does not count as unchanged.
fn file_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("chunker {CHUNKER_VERSION}\n"));
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

fn language_name(lang: Language) -> &'static str {
    match lang {
        Language::Rust => "rust",
        Language::Go => "go",
        Language::TypeScript => "typescript",
        Language::Tsx => "tsx",
        Language::JavaScript => "javascript",
        Language::Python => "python",
        Language::Sql => "sql",
        Language::Markdown => "markdown",
        Language::Other => "text",
    }
}

#[derive(Serialize)]
struct DocMetadata<'a> {
    path: &'a str,
    language: &'static str,
    commit: &'a str,
}

/// One ingest run: the counters and the paths seen so far.
struct Job<'a> {
    state: &'a IndexCtx,
    tx: Transaction<'static, Postgres>,
    pending: PendingEmbeddings,
    source: &'a Source,
    paths: &'a RepoPaths,
    commit: &'a str,
    report: IndexReport,
    /// Paths whose document is up to date (full mode only uses it, to find vanished documents).
    kept: HashSet<String>,
    /// Chunks written and embedded so far, and the time spent embedding, for the push's progress.
    totals: IndexStats,
}

impl Job<'_> {
    /// Indexes one added or modified path, or skips it (deleting its old document).
    async fn index_entry(&mut self, entry: &Entry) -> Result<(), IngestError> {
        let path = entry.path.as_str();
        // Only regular files: not symlinks (120000), submodules (160000) or anything odd.
        let regular = matches!(entry.mode.as_str(), "100644" | "100755");
        if !regular || path_is_skipped(path) {
            return self.skip(path).await;
        }
        // The `<commit>:<path>` form reads the object from the tree. It cannot be taken for an
        // option and never follows a symlink.
        let spec = format!("{}:{path}", self.commit);
        let size = self.paths.mirror_git(&["cat-file", "-s", &spec]).await?;
        let size: u64 = String::from_utf8_lossy(&size)
            .trim()
            .parse()
            .context("cat-file -s printed a bad size")?;
        if size > MAX_FILE_BYTES {
            return self.skip(path).await;
        }
        let blob = self.paths.mirror_git(&["cat-file", "blob", &spec]).await?;
        // Postgres text cannot hold a NUL, and binary content is not worth indexing.
        if blob.contains(&0) {
            return self.skip(path).await;
        }
        let Ok(content) = String::from_utf8(blob) else {
            return self.skip(path).await;
        };

        let content_hash = file_hash(&content);
        let known = document_hash(&mut *self.tx, self.source.id, path).await?;
        if known.as_deref() == Some(content_hash.as_str()) {
            self.report.files_unchanged += 1;
            self.kept.insert(path.to_string());
            return Ok(());
        }

        let lang = language_for(path);
        let metadata = serde_json::to_value(DocMetadata {
            path,
            language: language_name(lang),
            commit: self.commit,
        })
        .context("building document metadata")?;
        let doc = NewDocument {
            tenant_id: self.source.tenant_id,
            source_id: self.source.id,
            external_id: path.to_string(),
            kind: if lang == Language::Markdown {
                "doc"
            } else {
                "code"
            }
            .to_string(),
            title: path.to_string(),
            uri: None,
            metadata,
            content_hash,
            body: None,
        };
        let chunks = chunk_file(path, &content);
        let stats =
            index_repo_text(self.state, &mut self.tx, &mut self.pending, doc, chunks).await?;
        self.report.files_indexed += 1;
        self.report.chunks += stats.chunks;
        self.totals.chunks += stats.chunks;
        self.totals.embedded += stats.embedded;
        self.totals.embed_time += stats.embed_time;
        self.kept.insert(path.to_string());
        Ok(())
    }

    /// Counts the path as skipped and drops its document, if it has one.
    async fn skip(&mut self, path: &str) -> Result<(), IngestError> {
        self.report.files_skipped += 1;
        if self.delete_doc(path).await? {
            self.report.files_deleted += 1;
        }
        Ok(())
    }

    async fn delete_doc(&mut self, path: &str) -> Result<bool, IngestError> {
        let deleted = delete_document(&mut self.tx, self.source.id, path).await?;
        Ok(deleted)
    }

    /// Full mode: deletes the documents of paths that are not in the new tree.
    async fn delete_vanished(&mut self) -> Result<(), IngestError> {
        for id in list_external_ids(&mut *self.tx, self.source.id).await? {
            if !self.kept.contains(&id) && self.delete_doc(&id).await? {
                self.report.files_deleted += 1;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_sha_is_40_lowercase_hex() {
        assert!(is_full_sha(&"a".repeat(40)));
        assert!(is_full_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_sha(&"A".repeat(40)));
        assert!(!is_full_sha(&"a".repeat(39)));
        assert!(!is_full_sha(&"a".repeat(41)));
        assert!(!is_full_sha("--upload-pack=touch"));
        assert!(!is_full_sha("HEAD"));
        assert!(!is_full_sha(""));
    }

    #[test]
    fn skip_rules() {
        for p in [
            "node_modules/x/index.js",
            "a/vendor/b.go",
            "dist/app.js",
            "build",
            "crates/x/target/debug/a.rs",
            ".git/config",
            ".venv/lib/a.py",
            "pkg/__pycache__/a.py",
            "package-lock.json",
            "web/pnpm-lock.yaml",
            "yarn.lock",
            "Cargo.lock",
            "go.sum",
            "a.min.js",
            "a.js.map",
            "logo.svg",
            "LOGO.PNG",
            "a.jpg",
            "a.gif",
            "a.ico",
            "a.pdf",
            "a.zip",
            "a.tar.gz",
            "a.woff",
            "a.woff2",
            "a.ttf",
        ] {
            assert!(path_is_skipped(p), "{p} should be skipped");
        }
        for p in [
            "src/main.rs",
            "README.md",
            "docs/vendors.md",
            "src/builder.rs",
            "my-package-lock.json.md",
            "a.min.json",
            "-rf",
        ] {
            assert!(!path_is_skipped(p), "{p} should be indexed");
        }
    }

    #[test]
    fn ls_tree_records_keep_mode_and_odd_paths() {
        let out = b"100644 blob aaaa\tsrc/a b.rs\x00120000 blob bbbb\tleak\x00160000 commit cccc\tsub\x00100755 blob dddd\t-weird\xff\ntab\tname\0";
        let entries = parse_ls_tree(out).unwrap();
        let modes: Vec<&str> = entries.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(modes, ["100644", "120000", "160000", "100755"]);
        assert_eq!(entries[0].0, b"src/a b.rs");
        assert_eq!(entries[3].0, b"-weird\xff\ntab\tname");
        assert!(parse_ls_tree(b"").unwrap().is_empty());
        assert!(parse_ls_tree(b"garbage\0").is_err());
    }

    #[test]
    fn raw_diff_records() {
        let z = "0".repeat(40);
        let a = "a".repeat(40);
        let out = format!(
            ":100644 100644 {a} {a} M\0src/a.rs\0:100644 000000 {a} {z} D\0gone.rs\0:000000 120000 {z} {a} A\0leak\0:100644 120000 {a} {a} T\0now-link\0"
        );
        let changes = parse_raw_diff(out.as_bytes()).unwrap();
        assert_eq!(
            changes,
            [
                RawChange::Upsert(b"src/a.rs".to_vec(), "100644".into()),
                RawChange::Deleted(b"gone.rs".to_vec()),
                RawChange::Upsert(b"leak".to_vec(), "120000".into()),
                RawChange::Upsert(b"now-link".to_vec(), "120000".into()),
            ]
        );
        assert!(parse_raw_diff(b"").unwrap().is_empty());
        assert!(parse_raw_diff(b":100644 100644 a b M\0").is_err());
        assert!(parse_raw_diff(b"not a record\0path\0").is_err());
        let rename = format!(":100644 100644 {a} {a} R100\0old\0new\0");
        assert!(parse_raw_diff(rename.as_bytes()).is_err());
    }

    #[test]
    fn non_utf8_paths_are_counted_as_skipped() {
        let changes = Changes::from_records(vec![
            (b"ok.rs".to_vec(), "100644".into()),
            (b"bad\xff.rs".to_vec(), "100644".into()),
        ]);
        assert_eq!(changes.upserts.len(), 1);
        assert_eq!(changes.skipped_paths, 1);
    }

    /// Runs git against `dir` with the host configuration ignored. Test helper only.
    fn test_git(dir: &Path, args: &[&std::ffi::OsStr], stdin: &[u8]) -> String {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git runs");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin)
            .expect("write stdin");
        let out = child.wait_with_output().expect("git finishes");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf-8 output")
            .trim()
            .to_string()
    }

    /// Commits `files` (path bytes and content) into the bare repo `dir` without a work tree, so
    /// the path bytes are whatever the test says, on every file system.
    fn test_commit(dir: &Path, parent: Option<&str>, files: &[(&[u8], &str)]) -> String {
        use std::ffi::OsStr;
        // `mktree -z` builds the tree straight from records, no index and no work tree needed.
        let mut records = Vec::new();
        for (path, content) in files {
            let blob = test_git(
                dir,
                &[
                    OsStr::new("hash-object"),
                    OsStr::new("-w"),
                    OsStr::new("--stdin"),
                ],
                content.as_bytes(),
            );
            records.extend_from_slice(format!("100644 blob {blob}\t").as_bytes());
            records.extend_from_slice(path);
            records.push(0);
        }
        let tree = test_git(dir, &[OsStr::new("mktree"), OsStr::new("-z")], &records);
        let mut args = vec![
            OsStr::new("commit-tree"),
            OsStr::new(&tree),
            OsStr::new("-m"),
            OsStr::new("c"),
        ];
        if let Some(p) = parent {
            args.push(OsStr::new("-p"));
            args.push(OsStr::new(p));
        }
        test_git(dir, &args, b"")
    }

    #[tokio::test]
    async fn a_path_that_is_not_utf8_is_skipped_by_the_tree_and_the_diff() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mirror = dir.path().join("mirror.git");
        let mirror_arg = mirror.to_str().expect("utf-8 path").to_string();
        let init = std::process::Command::new("git")
            .args(["init", "--bare", "-q", &mirror_arg])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git init runs");
        assert!(init.success());

        let first = test_commit(&mirror, None, &[(b"ok.rs", "fn a() {}\n")]);
        let second = test_commit(
            &mirror,
            Some(&first),
            &[
                (b"ok.rs", "fn a() {}\n"),
                (b"bad\xffname.rs", "fn b() {}\n"),
                (b"zz.rs", "fn z() {}\n"),
            ],
        );
        let paths = RepoPaths {
            dir: dir.path().to_path_buf(),
            mirror: mirror_arg,
            checkout: dir
                .path()
                .join("checkout")
                .to_str()
                .expect("utf-8")
                .to_string(),
        };

        let tree = Changes::from_tree(&paths, &second).await.expect("tree");
        let mut names: Vec<_> = tree.upserts.iter().map(|e| e.path.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["ok.rs", "zz.rs"]);
        assert_eq!(tree.skipped_paths, 1);

        let diff = Changes::from_diff(&paths, &first, &second)
            .await
            .expect("diff");
        let mut names: Vec<_> = diff.upserts.iter().map(|e| e.path.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["zz.rs"]);
        assert_eq!(diff.skipped_paths, 1);
        assert!(diff.deleted.is_empty());

        // Removing the odd path again deletes nothing: it never had a document.
        let third = test_commit(&mirror, Some(&second), &[(b"ok.rs", "fn a() {}\n")]);
        let gone = Changes::from_diff(&paths, &second, &third)
            .await
            .expect("diff");
        assert_eq!(gone.deleted, ["zz.rs"]);
        assert_eq!(gone.skipped_paths, 0);
    }

    #[test]
    fn disk_usage_sums_each_repo_directory() {
        let data = tempfile::tempdir().expect("temp dir");
        assert!(disk_usage(data.path()).is_empty());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let repos = data.path().join("repos");
        std::fs::create_dir_all(repos.join(a.to_string()).join("checkout/src")).unwrap();
        std::fs::write(repos.join(a.to_string()).join("HEAD"), "abc").unwrap();
        std::fs::write(repos.join(a.to_string()).join("checkout/src/x.rs"), "12345").unwrap();
        std::fs::create_dir_all(repos.join(b.to_string())).unwrap();
        std::fs::create_dir_all(repos.join("not-a-source")).unwrap();
        std::fs::write(repos.join("not-a-source/big"), "x".repeat(10_000)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            repos.join("not-a-source"),
            repos.join(a.to_string()).join("link"),
        )
        .unwrap();

        let usage = disk_usage(data.path());
        assert_eq!(usage.len(), 2);
        // The symlink counts as itself, not as the 10,000 bytes it points at.
        let a_bytes = usage[&a];
        assert!((8..1000).contains(&a_bytes), "{a_bytes}");
        assert_eq!(usage[&b], 0);
    }

    #[test]
    fn locks_are_per_source_and_reused() {
        let locks = SourceLocks::default();
        assert!(locks.is_empty());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let first = locks.get(a);
        assert!(Arc::ptr_eq(&first, &locks.get(a)));
        assert!(!Arc::ptr_eq(&first, &locks.get(b)));
        assert_eq!(locks.len(), 2);
    }
}
