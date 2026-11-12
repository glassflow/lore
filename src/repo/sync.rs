//! Remote sync: the server fetches a repo source's `remote_url` by itself and indexes what
//! changed, so an index stays fresh without a CI step. A sync is started by
//! `POST /v1/repos/{id}/sync`, by the poller (`LORE_SYNC_INTERVAL_SECS`) or by a GitHub `push`
//! webhook. It fetches the remote's default branch (its `HEAD`) into the same bare mirror a
//! bundle push uses, then runs the same incremental index under the same per-source lock.
//!
//! Security rules, on top of the ones in the parent module:
//! - A remote URL is `https://` without credentials. `file://` is accepted only with
//!   `LORE_SYNC_ALLOW_FILE_REMOTES`, and git is told the same (`GIT_ALLOW_PROTOCOL`), so neither a
//!   redirect nor a crafted URL makes it use another transport.
//! - A private remote's token is passed per call, in the body of `POST /v1/repos/{id}/sync`, and
//!   used for that fetch only. Without one, the token comes from the environment, from
//!   `LORE_GIT_TOKEN_<TENANT>` (the tenant name upper-cased, `-` as `_`), so a tenant can only
//!   ever send its own token. Either way it is passed to git as an `http.extraHeader` through
//!   the environment: it is never stored, and never appears in a URL, in git's arguments, in the
//!   mirror's config, in a log or in an error (`fetch_remote` scrubs what git says). It stays
//!   visible in the git child's `/proc/<pid>/environ` while the fetch runs.
//! - What git says when a fetch fails stays on the server (log and admin page); the client is
//!   told only that the remote could not be fetched.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, anyhow};
use base64::Engine;
use serde::Serialize;
use uuid::Uuid;

use super::progress::RunWriter;
use super::{
    GitError, IndexReport, IngestError, RepoPaths, ensure_mirror, git_env, index_commit,
    is_full_sha, push_error, refresh_checkout,
};
use crate::error::AppError;
use crate::http::AppState;
use crate::store::{Source, get_source};

/// A fetch that takes longer than this is stopped (git is killed with it).
const FETCH_TIMEOUT: Duration = Duration::from_secs(600);

/// The longest error kept for the admin page.
const MAX_ERROR_CHARS: usize = 500;

/// The longest accepted remote URL.
const MAX_REMOTE_URL: usize = 2048;

/// What one sync did. An up to date sync indexed nothing and reports zeros.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    #[serde(flatten)]
    pub report: IndexReport,
    /// The remote's head was already indexed.
    pub up_to_date: bool,
    /// The whole tree was indexed: the source's first index, a history rewrite, or the last
    /// indexed commit was gone from the mirror.
    pub full: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("the repo has no remote_url")]
    NoRemote,
    /// The fetch failed; the text is git's message, for the server only.
    #[error("could not fetch the remote: {0}")]
    Fetch(String),
    /// The remote refused the credentials, or asked for some and got none; for the server only.
    #[error("the remote refused authentication: {0}")]
    Auth(String),
    #[error(transparent)]
    Ingest(#[from] IngestError),
}

impl From<sqlx::Error> for SyncError {
    fn from(e: sqlx::Error) -> Self {
        SyncError::Ingest(e.into())
    }
}

impl From<anyhow::Error> for SyncError {
    fn from(e: anyhow::Error) -> Self {
        SyncError::Ingest(e.into())
    }
}

impl From<GitError> for SyncError {
    fn from(e: GitError) -> Self {
        SyncError::Ingest(e.into())
    }
}

impl From<SyncError> for AppError {
    fn from(e: SyncError) -> Self {
        match e {
            SyncError::NoRemote => AppError::Conflict("the repo has no remote_url".to_string()),
            SyncError::Fetch(m) => {
                tracing::warn!(error = %m, "remote fetch failed");
                AppError::BadGateway("could not fetch the remote".to_string())
            }
            SyncError::Auth(m) => {
                tracing::warn!(error = %m, "remote refused authentication");
                AppError::BadGateway(
                    "the remote refused authentication: send a token that can read the repo as {\"token\": \"...\"} in the sync request body"
                        .to_string(),
                )
            }
            // A full index is done here, never asked of the client.
            SyncError::Ingest(IngestError::NeedFull) => {
                AppError::Internal(anyhow!("a full index still needed a full bundle"))
            }
            SyncError::Ingest(e) => e.into(),
        }
    }
}

/// How a failed sync reads on the admin page.
fn admin_error(e: &SyncError) -> String {
    let text = match e {
        SyncError::NoRemote => "no remote_url".to_string(),
        SyncError::Fetch(m) => format!("fetch failed: {m}"),
        SyncError::Auth(m) => format!("remote refused authentication: {m}"),
        SyncError::Ingest(e) => push_error(e),
    };
    text.chars().take(MAX_ERROR_CHARS).collect()
}

/// Checks a remote URL: `https://` with a host and no user or password, or `file://` when
/// `allow_file`. The error is the client-facing reason.
pub fn check_remote_url(url: &str, allow_file: bool) -> Result<(), String> {
    let https_only = "remote_url must be an https:// URL without credentials";
    if url.is_empty() || url.len() > MAX_REMOTE_URL {
        return Err(format!(
            "remote_url must be 1 to {MAX_REMOTE_URL} characters"
        ));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("remote_url must not contain spaces or control characters".to_string());
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| https_only.to_string())?;
    match parsed.scheme() {
        "https" => {
            let has_host = parsed.host_str().is_some_and(|h| !h.is_empty());
            if !has_host || !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(https_only.to_string());
            }
            Ok(())
        }
        "file" if allow_file => Ok(()),
        _ => Err(https_only.to_string()),
    }
}

/// The form two remote URLs are compared in, to match a webhook's repository to sources:
/// lower case, without a trailing `/` or `.git`, query or fragment. `None` when it does not
/// parse.
pub fn remote_key(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    let mut path = parsed.path().to_ascii_lowercase();
    while path.ends_with('/') {
        path.pop();
    }
    let path = path.strip_suffix(".git").unwrap_or(&path);
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!(
        "{}://{}{port}{path}",
        parsed.scheme(),
        parsed.host_str().unwrap_or_default().to_ascii_lowercase()
    ))
}

/// The environment variable holding the token of a tenant's private remotes:
/// `LORE_GIT_TOKEN_` and the tenant name upper-cased with `-` as `_`. Tenant names are
/// `[a-z0-9-]`, so two tenants never share a variable.
pub fn token_var(tenant_name: &str) -> String {
    format!(
        "LORE_GIT_TOKEN_{}",
        tenant_name.to_ascii_uppercase().replace('-', "_")
    )
}

/// The `http.extraHeader` value that sends `token` as HTTP basic auth, which GitHub, GitLab
/// and Gitea accept for a personal or app token.
fn auth_header(token: &str) -> String {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    format!("Authorization: Basic {basic}")
}

/// `text` with the token, its basic-auth form and the whole header value replaced, so what git
/// says can be logged and stored. The header goes first: it contains the basic-auth form.
fn scrub(text: &str, token: &str) -> String {
    let header = auth_header(token);
    let basic = header.trim_start_matches("Authorization: Basic ");
    text.replace(&header, "[token]")
        .replace(basic, "[token]")
        .replace(token, "[token]")
}

/// Whether git's stderr says the remote wanted credentials and got none or bad ones.
fn is_auth_failure(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    [
        "authentication failed",
        "terminal prompts disabled",
        "could not read username",
        "error: 401",
        "error: 403",
    ]
    .iter()
    .any(|m| s.contains(m))
}

/// Sets, or with `None` clears, the remote URL of a repo source. The caller checked the URL and
/// that the source belongs to its tenant.
pub async fn set_remote_url(
    pool: &sqlx::PgPool,
    source_id: Uuid,
    url: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE sources SET remote_url = $2 WHERE id = $1")
        .bind(source_id)
        .bind(url)
        .execute(pool)
        .await?;
    Ok(())
}

/// The remote URLs of a tenant's repo sources that have one, by source id.
pub async fn remote_urls(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
) -> sqlx::Result<HashMap<Uuid, String>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, remote_url FROM sources
         WHERE tenant_id = $1 AND kind = 'repo' AND remote_url IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

#[derive(sqlx::FromRow)]
struct RemoteRow {
    #[sqlx(flatten)]
    source: Source,
    remote_url: String,
}

const REMOTE_SOURCES_SQL: &str = "
    SELECT id, tenant_id, kind, name, web_url_template, indexed_commit, remote_url
    FROM sources WHERE kind = 'repo' AND remote_url IS NOT NULL
    ORDER BY tenant_id, name";

/// Every repo source of every tenant that has a remote.
pub async fn remote_sources(pool: &sqlx::PgPool) -> sqlx::Result<Vec<Source>> {
    let rows: Vec<RemoteRow> = sqlx::query_as(REMOTE_SOURCES_SQL).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| r.source).collect())
}

/// The repo sources, of any tenant, whose remote is one of `urls` (compared by [`remote_key`]).
pub async fn sources_for_remote(pool: &sqlx::PgPool, urls: &[&str]) -> sqlx::Result<Vec<Source>> {
    let keys: Vec<String> = urls.iter().filter_map(|u| remote_key(u)).collect();
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<RemoteRow> = sqlx::query_as(REMOTE_SOURCES_SQL).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter(|r| remote_key(&r.remote_url).is_some_and(|k| keys.contains(&k)))
        .map(|r| r.source)
        .collect())
}

/// Records how a sync ended. A failed one keeps the last synced commit.
async fn record_sync(
    pool: &sqlx::PgPool,
    source_id: Uuid,
    commit: Option<&str>,
    error: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE sources SET last_sync_at = now(),
           last_sync_commit = COALESCE($2, last_sync_commit), last_sync_error = $3
         WHERE id = $1",
    )
    .bind(source_id)
    .bind(commit)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetches the remote of `source` (a repo source the caller read for its tenant) and indexes
/// its head when it moved. The outcome is recorded on the source for the admin page; a run that
/// indexes is also recorded in `index_runs`. A `token` wins over the tenant's environment
/// token for this call; it is not kept.
pub async fn sync_repo(
    state: &AppState,
    source: &Source,
    token: Option<String>,
) -> Result<SyncReport, SyncError> {
    if source.kind != "repo" {
        return Err(IngestError::BadRequest("source is not a repo".to_string()).into());
    }
    let lock = state.source_locks.get(source.id);
    let _guard = lock.lock().await;
    sync_under_lock(state, source, token).await
}

/// The caller owns this source's lock, including while recording the outcome.
async fn sync_under_lock(
    state: &AppState,
    source: &Source,
    token: Option<String>,
) -> Result<SyncReport, SyncError> {
    let result = sync_locked(state, source, token).await;
    if matches!(result, Err(SyncError::NoRemote)) {
        return result;
    }
    let commit = result
        .as_ref()
        .ok()
        .map(|r| r.report.indexed_commit.as_str());
    let error = result.as_ref().err().map(admin_error);
    if let Err(e) = record_sync(&state.pool, source.id, commit, error.as_deref()).await {
        tracing::error!(error = %e, source_id = %source.id, "recording a sync failed");
    }
    result
}

/// Poll each configured remote independently. At most one poll per source can run, and a busy
/// source is skipped rather than accumulating waiting tasks. A slow fetch or push therefore
/// cannot delay other sources. Shutdown stops scheduling and drains the polls already running.
pub async fn poll_repos(
    state: AppState,
    interval: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut jobs = tokio::task::JoinSet::<()>::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                if let Err(e) = result {
                    tracing::error!(error = %e, "repo poll task failed");
                }
                continue;
            }
            _ = tick.tick() => {}
        }
        let sources = match remote_sources(&state.pool).await {
            Ok(sources) => sources,
            Err(e) => {
                tracing::error!(error = %e, "listing repos for polling failed");
                continue;
            }
        };
        for source in sources {
            let Ok(guard) = state.source_locks.get(source.id).try_lock_owned() else {
                continue;
            };
            let state = state.clone();
            jobs.spawn(async move {
                let _guard = guard;
                match sync_under_lock(&state, &source, None).await {
                    Ok(r) if !r.up_to_date => {
                        tracing::info!(source_id = %source.id, commit = %r.report.indexed_commit, "repo synced by poller");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(source_id = %source.id, error = %e, "repo poll failed"),
                }
            });
        }
    }
    while let Some(result) = jobs.join_next().await {
        if let Err(e) = result {
            tracing::error!(error = %e, "repo poll task failed");
        }
    }
}

async fn sync_locked(
    state: &AppState,
    source: &Source,
    token: Option<String>,
) -> Result<SyncReport, SyncError> {
    // Read again under the lock: a push may have moved the indexed commit while this waited.
    let source = get_source(&state.pool, source.tenant_id, source.id)
        .await?
        .ok_or_else(|| anyhow!("source {} vanished", source.id))?;
    let (url, tenant): (Option<String>, String) = sqlx::query_as(
        "SELECT s.remote_url, t.name FROM sources s JOIN tenants t ON t.id = s.tenant_id
         WHERE s.id = $1",
    )
    .bind(source.id)
    .fetch_one(&state.pool)
    .await?;
    let url = url.ok_or(SyncError::NoRemote)?;
    let allow_file = state.config.sync_allow_file_remotes;
    // Checked again: the URL was stored under the configuration of that time.
    check_remote_url(&url, allow_file).map_err(SyncError::Fetch)?;
    let token = token.filter(|t| !t.is_empty()).or_else(|| {
        std::env::var(token_var(&tenant))
            .ok()
            .filter(|t| !t.is_empty())
    });

    let paths = RepoPaths::new(&state.config.data_dir, source.id)?;
    ensure_mirror(&paths).await?;
    let head = fetch_remote(&paths, &url, token.as_deref(), allow_file, false).await?;

    if source.indexed_commit.as_deref() == Some(head.as_str()) {
        // A lost data volume leaves the index intact but no checkout for deep mode.
        if !tokio::fs::try_exists(&paths.checkout)
            .await
            .context("checking the checkout")?
        {
            refresh_checkout(&paths, &head).await?;
        }
        return Ok(SyncReport {
            report: IndexReport {
                indexed_commit: head,
                files_indexed: 0,
                files_unchanged: 0,
                files_skipped: 0,
                files_deleted: 0,
                chunks: 0,
            },
            up_to_date: true,
            full: false,
        });
    }

    let mut full = source.indexed_commit.is_none();
    if let Some(indexed) = source.indexed_commit.as_deref()
        && paths
            .mirror_git(&["merge-base", "--is-ancestor", indexed, &head])
            .await
            .is_err()
    {
        // A force push or a lost mirror: ask for all objects again, without negotiation
        // based on the old history, then reconcile the entire indexed tree.
        fetch_remote(&paths, &url, token.as_deref(), allow_file, true).await?;
        full = true;
    }
    let push = RunWriter::new(state.pool.clone())
        .start(source.id, source.tenant_id, &head, full)
        .await;
    push.cloning();
    let mut result = index_commit(state, &source, &paths, &head, full, &push).await;
    if matches!(result, Err(IngestError::NeedFull)) {
        // The last indexed commit is gone from the mirror (the mirror was lost, or history was
        // rewritten and pruned): index the whole tree, which deletes what is no longer there.
        full = true;
        result = index_commit(state, &source, &paths, &head, true, &push).await;
    }
    match result {
        Ok(report) => {
            push.done(&report).await;
            Ok(SyncReport {
                report,
                up_to_date: false,
                full,
            })
        }
        Err(e) => {
            push.failed(&push_error(&e)).await;
            Err(e.into())
        }
    }
}

/// Fetches the remote's `HEAD` (its default branch) into the mirror and returns its commit.
async fn fetch_remote(
    paths: &RepoPaths,
    url: &str,
    token: Option<&str>,
    allow_file: bool,
    refetch: bool,
) -> Result<String, SyncError> {
    let git_dir = format!("--git-dir={}", paths.mirror);
    let protocols = if allow_file { "https:file" } else { "https" };
    let header = token
        .filter(|_| url.starts_with("https://"))
        .map(auth_header);
    let mut envs = vec![("GIT_ALLOW_PROTOCOL", protocols)];
    if let Some(h) = &header {
        envs.extend([
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "http.extraHeader"),
            ("GIT_CONFIG_VALUE_0", h.as_str()),
        ]);
    }
    let mut args = vec![git_dir.as_str(), "fetch", "--no-tags", "-q"];
    if refetch {
        args.push("--refetch");
    }
    args.extend(["--", url, "HEAD"]);
    let fetch = git_env(&args, &envs);
    match tokio::time::timeout(FETCH_TIMEOUT, fetch).await {
        Err(_) => return Err(SyncError::Fetch("timed out".to_string())),
        Ok(Err(e)) => {
            let stderr = if e.stderr.is_empty() {
                e.to_string()
            } else {
                e.stderr
            };
            // The one place git's text leaves with the token possibly in it.
            let stderr = match token {
                Some(t) => scrub(&stderr, t),
                None => stderr,
            };
            return Err(if is_auth_failure(&stderr) {
                SyncError::Auth(stderr)
            } else {
                SyncError::Fetch(stderr)
            });
        }
        Ok(Ok(_)) => {}
    }
    let out = paths
        .mirror_git(&["rev-parse", "--verify", "FETCH_HEAD^{commit}"])
        .await?;
    let head = String::from_utf8_lossy(&out).trim().to_string();
    if !is_full_sha(&head) {
        return Err(anyhow!("rev-parse printed an unexpected commit {head:?}").into());
    }
    Ok(head)
}

/// Syncs every repo source that has a remote, one after another. Failures are recorded and
/// logged, and do not stop the others. Returns how many sources indexed a new commit.
pub async fn sync_all(state: &AppState) -> anyhow::Result<usize> {
    let mut indexed = 0;
    for source in remote_sources(&state.pool).await? {
        match sync_repo(state, &source, None).await {
            Ok(r) if !r.up_to_date => {
                indexed += 1;
                tracing::info!(source_id = %source.id, commit = %r.report.indexed_commit, "repo synced");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(source_id = %source.id, error = %e, "repo sync failed"),
        }
    }
    Ok(indexed)
}

/// Starts a sync of each source in the background, for the webhook, which answers before the
/// fetch is done.
pub fn spawn_syncs(state: &AppState, sources: Vec<Source>) {
    for source in sources {
        let state = state.clone();
        tokio::spawn(async move {
            match sync_repo(&state, &source, None).await {
                Ok(r) => tracing::info!(
                    source_id = %source.id,
                    commit = %r.report.indexed_commit,
                    up_to_date = r.up_to_date,
                    "repo synced by webhook"
                ),
                Err(e) => {
                    tracing::warn!(source_id = %source.id, error = %e, "repo sync by webhook failed")
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_are_https_without_credentials() {
        for ok in [
            "https://github.com/acme/app.git",
            "https://github.com/acme/app",
            "https://git.example.com:8443/team/app.git",
        ] {
            assert_eq!(check_remote_url(ok, false), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "http://github.com/acme/app.git",
            "git@github.com:acme/app.git",
            "ssh://git@github.com/acme/app.git",
            "file:///etc",
            "/srv/repos/app",
            "ext::sh -c touch% /tmp/pwned",
            "https://user:pass@github.com/acme/app.git",
            "https://token@github.com/acme/app.git",
            "https://github.com/acme/app .git",
            "https://github.com/acme/app\n.git",
            "--upload-pack=touch",
        ] {
            assert!(
                check_remote_url(bad, false).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(check_remote_url(&format!("https://x/{}", "a".repeat(3000)), false).is_err());
        assert_eq!(check_remote_url("file:///srv/repos/app", true), Ok(()));
        assert!(check_remote_url("http://x/a", true).is_err());
    }

    #[test]
    fn remote_keys_ignore_case_suffix_and_slash() {
        let key = remote_key("https://github.com/Acme/App.git").unwrap();
        assert_eq!(key, "https://github.com/acme/app");
        for same in [
            "https://github.com/acme/app",
            "https://GitHub.com/acme/app/",
            "https://github.com/acme/app.git/",
            " https://github.com/ACME/APP ",
        ] {
            assert_eq!(remote_key(same).as_deref(), Some(key.as_str()), "{same}");
        }
        assert_ne!(
            remote_key("https://github.com/acme/app2"),
            Some(key.clone())
        );
        assert_ne!(remote_key("https://gitlab.com/acme/app"), Some(key));
        assert_eq!(
            remote_key("https://h:8443/a.git").as_deref(),
            Some("https://h:8443/a")
        );
        assert_eq!(remote_key("not a url"), None);
    }

    #[test]
    fn token_variables_are_per_tenant() {
        assert_eq!(token_var("acme"), "LORE_GIT_TOKEN_ACME");
        assert_eq!(token_var("glass-flow-2"), "LORE_GIT_TOKEN_GLASS_FLOW_2");
        assert_ne!(token_var("a-b"), token_var("ab"));
    }

    #[test]
    fn the_token_is_sent_as_basic_auth() {
        assert_eq!(
            auth_header("t0k"),
            "Authorization: Basic eC1hY2Nlc3MtdG9rZW46dDBr"
        );
    }

    #[test]
    fn scrub_removes_every_form_of_the_token() {
        let token = "ghp_fake123";
        let header = auth_header(token);
        let basic = header.trim_start_matches("Authorization: Basic ");
        let text = format!("a {token} b {basic} c {header} d");
        let out = scrub(&text, token);
        assert_eq!(out, "a [token] b [token] c [token] d");
    }

    #[test]
    fn auth_failures_are_told_from_other_errors() {
        assert!(is_auth_failure(
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
        ));
        assert!(is_auth_failure(
            "fatal: Authentication failed for 'https://x/'"
        ));
        assert!(!is_auth_failure(
            "fatal: unable to access: Could not resolve host"
        ));
    }

    #[test]
    fn admin_errors_are_capped() {
        let long = SyncError::Fetch("x".repeat(2000));
        assert_eq!(admin_error(&long).chars().count(), MAX_ERROR_CHARS);
        assert_eq!(admin_error(&SyncError::NoRemote), "no remote_url");
    }
}
