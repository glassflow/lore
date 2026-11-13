//! `lore index`: clones or fetches one repo and indexes it into Postgres, then exits. The one
//! command that does not talk to a server: it connects to the database itself, for a Kubernetes
//! Job that indexes a repo too big or too slow for a request.
//!
//! Inputs: `LORE_DATABASE_URL` and the embedder settings as the server reads them
//! (`Config::from_env`), and `LORE_INDEX_TOKEN`, the git token for a private remote. The token
//! comes from the environment only: it is never a flag, never in a URL or in git's arguments,
//! and never printed, logged or stored (see `repo::sync`). Git does get it, as a basic-auth
//! `http.extraHeader` in its environment, so it is readable from that git process's
//! `/proc/<pid>/environ` while the fetch runs; `LORE_INDEX_TOKEN` and `LORE_DATABASE_URL`
//! themselves are removed from git's environment.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use uuid::Uuid;

use crate::config::Config;
use crate::repo::IndexCtx;
use crate::repo::lock::SourceAdvisoryLock;
use crate::repo::sync::{RemoteIndex, check_remote_url, index_remote, scrub};
use crate::store::get_source;

/// Exit code of a run that did not index.
pub const EXIT_FAILED: i32 = 1;
/// Exit code when another index of the same source is running.
pub const EXIT_BUSY: i32 = 3;

/// The environment variable holding the git token of a private remote.
pub const TOKEN_VAR: &str = "LORE_INDEX_TOKEN";

#[derive(Args)]
pub struct IndexArgs {
    /// Tenant id: the source must belong to this tenant
    #[arg(long)]
    pub tenant: Uuid,
    /// Repo source id
    #[arg(long)]
    pub repo: Uuid,
    /// https URL of the git remote, without credentials (a token goes in LORE_INDEX_TOKEN)
    #[arg(long)]
    pub remote: String,
}

/// The line printed on success.
#[derive(Debug, Serialize)]
pub struct IndexOutput {
    pub commit: String,
    pub files_indexed: usize,
    pub files_unchanged: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub chunks: usize,
    pub chunks_embedded: usize,
    pub embed_ms: u64,
    pub full: bool,
}

impl From<RemoteIndex> for IndexOutput {
    fn from(r: RemoteIndex) -> Self {
        Self {
            commit: r.report.indexed_commit,
            files_indexed: r.report.files_indexed,
            files_unchanged: r.report.files_unchanged,
            files_skipped: r.report.files_skipped,
            files_deleted: r.report.files_deleted,
            chunks: r.report.chunks,
            chunks_embedded: r.chunks_embedded,
            embed_ms: r.embed_ms,
            // Always the whole tree, like `lore repo push --full`.
            full: true,
        }
    }
}

/// Runs the command and returns its exit code: 0 done, 1 failed, 3 another index of the source
/// is running. The result goes to stdout as one JSON line, an error to stderr. Both are free of
/// the token.
pub async fn run_index(config: Config, args: IndexArgs) -> i32 {
    let token = std::env::var(TOKEN_VAR).ok().filter(|t| !t.is_empty());
    match index(config, args, token.clone()).await {
        Ok(Outcome::Done(out)) => match serde_json::to_string(&out) {
            Ok(line) => {
                println!("{line}");
                0
            }
            Err(e) => fail(&format!("{e}"), token.as_deref()),
        },
        Ok(Outcome::Busy(source)) => {
            eprintln!(
                "lore index: another index of source {source} is running; not waiting for it"
            );
            EXIT_BUSY
        }
        Err(e) => fail(&format!("{e:#}"), token.as_deref()),
    }
}

fn fail(message: &str, token: Option<&str>) -> i32 {
    let message = match token {
        Some(t) => scrub(message, t),
        None => message.to_string(),
    };
    eprintln!("lore index: {message}");
    EXIT_FAILED
}

enum Outcome {
    Done(IndexOutput),
    Busy(Uuid),
}

async fn index(config: Config, args: IndexArgs, token: Option<String>) -> Result<Outcome> {
    check_remote_url(&args.remote, config.sync_allow_file_remotes).map_err(anyhow::Error::msg)?;
    let pool = crate::db::connect(&config.database_url).await?;
    crate::db::migrate(&pool).await?;

    // The tenant check: another tenant's source looks like a missing one.
    let source = get_source(&pool, args.tenant, args.repo)
        .await?
        .filter(|s| s.kind == "repo")
        .with_context(|| {
            format!(
                "repo source {} not found for tenant {}",
                args.repo, args.tenant
            )
        })?;

    let Some(lock) = SourceAdvisoryLock::try_acquire(&config.database_url, source.id).await? else {
        return Ok(Outcome::Busy(source.id));
    };
    let result = run_locked(&config, &pool, &source, &args.remote, token).await;
    lock.release().await;
    pool.close().await;
    result.map(|r| Outcome::Done(r.into()))
}

async fn run_locked(
    config: &Config,
    pool: &sqlx::PgPool,
    source: &crate::store::Source,
    remote: &str,
    token: Option<String>,
) -> Result<RemoteIndex> {
    // The model loads on the blocking pool, as `serve` does it, once the lock is ours.
    let started = std::time::Instant::now();
    let embedder = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || crate::embed::from_config(&config))
            .await
            .context("embedder load task failed")??
    };
    tracing::info!(
        embedder = embedder.name(),
        load_ms = started.elapsed().as_millis() as u64,
        source_id = %source.id,
        "embedder ready"
    );
    let ctx = IndexCtx {
        pool: pool.clone(),
        config: Arc::new(config.clone()),
        embedder,
        source_locks: Default::default(),
    };
    Ok(index_remote(&ctx, source, remote, token).await?)
}
