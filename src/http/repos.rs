//! Repo sources: create, list, push a git bundle and sync from a remote.

use std::error::Error as StdError;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::auth::{Caller, Scope};
use crate::error::AppError;
use crate::http::admin::remove_source;
use crate::http::{AppJson, AppQuery, AppState};
use crate::repo::progress::{IndexRun, latest_run};
use crate::repo::sync::{SyncReport, check_remote_url, remote_urls, set_remote_url};
use crate::repo::{IndexReport, ingest_bundle, is_full_sha};
use crate::store::{Source, ensure_source, get_source, list_sources};

#[derive(Deserialize)]
pub struct CreateRepo {
    name: String,
    web_url_template: Option<String>,
    /// Missing or null keeps the stored remote, `""` removes it.
    remote_url: Option<String>,
}

#[derive(Serialize)]
pub struct RepoItem {
    id: Uuid,
    name: String,
    indexed_commit: Option<String>,
    web_url_template: Option<String>,
    remote_url: Option<String>,
}

impl RepoItem {
    fn new(s: Source, remote_url: Option<String>) -> Self {
        Self {
            id: s.id,
            name: s.name,
            indexed_commit: s.indexed_commit,
            web_url_template: s.web_url_template,
            remote_url,
        }
    }
}

#[derive(Serialize)]
pub struct RepoList {
    repos: Vec<RepoItem>,
}

#[derive(Deserialize)]
pub struct BundleQuery {
    #[serde(rename = "ref")]
    sha: String,
    #[serde(default)]
    full: bool,
}

/// 1 to 100 characters of `[A-Za-z0-9._-]`.
pub(crate) fn valid_repo_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub async fn create_repo(
    caller: Caller,
    State(state): State<AppState>,
    AppJson(req): AppJson<CreateRepo>,
) -> Result<Json<RepoItem>, AppError> {
    caller.require(Scope::Ingest)?;
    if !valid_repo_name(&req.name) {
        return Err(AppError::BadRequest(
            "repo name must be 1 to 100 characters of A-Z, a-z, 0-9, . _ and -".to_string(),
        ));
    }
    let remote = req.remote_url.as_deref().map(str::trim);
    if let Some(url) = remote.filter(|u| !u.is_empty()) {
        check_remote_url(url, state.config.sync_allow_file_remotes)
            .map_err(AppError::BadRequest)?;
    }
    let source = ensure_source(
        &state.pool,
        caller.tenant_id,
        "repo",
        &req.name,
        req.web_url_template.as_deref(),
    )
    .await?;
    if let Some(url) = remote {
        set_remote_url(&state.pool, source.id, Some(url).filter(|u| !u.is_empty())).await?;
    }
    let mut remotes = remote_urls(&state.pool, caller.tenant_id).await?;
    let remote_url = remotes.remove(&source.id);
    Ok(Json(RepoItem::new(source, remote_url)))
}

pub async fn list_repos(
    caller: Caller,
    State(state): State<AppState>,
) -> Result<Json<RepoList>, AppError> {
    caller.require(Scope::Read)?;
    let sources = list_sources(&state.pool, caller.tenant_id, Some("repo")).await?;
    let mut remotes = remote_urls(&state.pool, caller.tenant_id).await?;
    Ok(Json(RepoList {
        repos: sources
            .into_iter()
            .map(|s| {
                let remote = remotes.remove(&s.id);
                RepoItem::new(s, remote)
            })
            .collect(),
    }))
}

/// True when the error is, or is caused by, the body limit layer's "length limit exceeded".
pub(crate) fn is_length_limit(e: &(dyn StdError + 'static)) -> bool {
    let mut cur: Option<&(dyn StdError + 'static)> = Some(e);
    while let Some(err) = cur {
        if err.to_string().contains("length limit exceeded") {
            return true;
        }
        cur = err.source();
    }
    false
}

/// Streams the request body into a new temp file in `<data_dir>/tmp`. The returned path deletes
/// the file when dropped, so every exit path cleans up.
async fn spool_body(state: &AppState, body: Body) -> Result<tempfile::TempPath, AppError> {
    let dir = state.config.data_dir.join("tmp");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::from(e).context("creating the tmp dir")))?;
    let (file, path) = tempfile::Builder::new()
        .prefix("upload-")
        .tempfile_in(&dir)
        .map_err(|e| AppError::Internal(anyhow::Error::from(e).context("creating a temp file")))?
        .into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            if is_length_limit(&e) {
                AppError::PayloadTooLarge
            } else {
                AppError::BadRequest("could not read the request body".to_string())
            }
        })?;
        file.write_all(&chunk).await.map_err(|e| {
            AppError::Internal(anyhow::Error::from(e).context("writing the upload"))
        })?;
    }
    file.flush()
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::from(e).context("flushing the upload")))?;
    Ok(path)
}

pub async fn put_bundle(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    AppQuery(query): AppQuery<BundleQuery>,
    body: Body,
) -> Result<Json<IndexReport>, AppError> {
    caller.require(Scope::Ingest)?;
    // Another tenant's source, or a source that is not a repo, looks like a missing one.
    let source = get_source(&state.pool, caller.tenant_id, id)
        .await?
        .filter(|s| s.kind == "repo")
        .ok_or(AppError::NotFound)?;
    if !is_full_sha(&query.sha) {
        return Err(AppError::BadRequest(
            "ref must be a full 40 character lowercase hex commit sha".to_string(),
        ));
    }

    let upload = spool_body(&state, body).await?;

    // The ingest runs in its own task: if the client disconnects, it still finishes instead of
    // stopping halfway. The temp file goes with the task.
    let task = tokio::spawn(async move {
        let report =
            ingest_bundle(&state.index_ctx(), &source, &upload, &query.sha, query.full).await;
        drop(upload);
        report
    });
    let report = task
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::from(e).context("ingest task failed")))??;
    Ok(Json(report))
}

/// The optional body of a sync. `token` is a GitHub token for this fetch only: it is never
/// stored or logged, so this has no `Debug`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    token: Option<String>,
}

/// The longest accepted token (GitHub's are well under 300 characters).
const MAX_TOKEN_CHARS: usize = 1024;

/// An empty body is valid (no token). Any other body must be `{"token": "..."}`; the parse error
/// is not echoed, so a token in a malformed body cannot come back.
fn sync_token(body: &[u8]) -> Result<Option<String>, AppError> {
    if body.trim_ascii().is_empty() {
        return Ok(None);
    }
    let req: SyncRequest = serde_json::from_slice(body).map_err(|_| {
        AppError::BadRequest("the body must be empty or JSON like {\"token\": \"...\"}".to_string())
    })?;
    match req.token {
        Some(t) if t.chars().count() > MAX_TOKEN_CHARS || t.chars().any(char::is_control) => {
            Err(AppError::BadRequest(format!(
                "token must be at most {MAX_TOKEN_CHARS} characters, without control characters"
            )))
        }
        token => Ok(token),
    }
}

/// Fetches the repo's remote and indexes what changed since the last indexed commit. A `token`
/// in the body is used for this fetch only.
pub async fn sync_repo(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Result<Json<SyncReport>, AppError> {
    caller.require(Scope::Ingest)?;
    let source = get_source(&state.pool, caller.tenant_id, id)
        .await?
        .filter(|s| s.kind == "repo")
        .ok_or(AppError::NotFound)?;
    let token = sync_token(&body)?;
    // Like a bundle push, the sync finishes even if the client disconnects.
    let task =
        tokio::spawn(async move { crate::repo::sync::sync_repo(&state, &source, token).await });
    let report = task
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::from(e).context("sync task failed")))??;
    Ok(Json(report))
}

/// Deletes one of the caller's repos with its documents, chunks, mirror and checkout, exactly as
/// the admin delete of a source does. A 409 while a run of the repo is live.
pub async fn delete_repo(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    caller.require(Scope::Ingest)?;
    // Another tenant's source, or a source that is not a repo, looks like a missing one.
    let source = get_source(&state.pool, caller.tenant_id, id)
        .await?
        .filter(|s| s.kind == "repo")
        .ok_or(AppError::NotFound)?;
    remove_source(&state, source.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The latest index run of the repo, whichever process ran it, with `stale` set when the run
/// has not ended and has not been written for 90 seconds.
pub async fn repo_progress(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<IndexRun>, AppError> {
    caller.require(Scope::Read)?;
    let source = get_source(&state.pool, caller.tenant_id, id)
        .await?
        .filter(|s| s.kind == "repo")
        .ok_or(AppError::NotFound)?;
    let run = latest_run(&state.pool, source.id)
        .await?
        .ok_or(AppError::NotFoundCode("no_index_run"))?;
    Ok(Json(run))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names() {
        assert!(valid_repo_name("sample"));
        assert!(valid_repo_name("My_Repo-1.git"));
        assert!(valid_repo_name(&"a".repeat(100)));
        assert!(!valid_repo_name(""));
        assert!(!valid_repo_name(&"a".repeat(101)));
        assert!(!valid_repo_name("a/b"));
        assert!(!valid_repo_name("a b"));
        assert!(!valid_repo_name("repo\u{e9}"));
        assert!(!valid_repo_name("../x"));
    }
}
