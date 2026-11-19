//! `lore repo push` and `lore repo list`: index a local git repo on a lore server.
//!
//! `push` bundles the committed HEAD (only the commits the server lacks, when it already has an
//! earlier one) and uploads it to `PUT /v1/repos/{id}/bundle`. Every git call is an argument
//! array run with `git -C <root>`, never a shell.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use uuid::Uuid;

use super::client::{ApiError, Auth, Client};
use crate::repo::{IndexReport, is_full_sha};

#[derive(Subcommand)]
pub enum RepoCommand {
    /// Index the committed HEAD of a local git repo (only new commits when possible)
    Push(PushArgs),
    /// List the repos of the key's tenant
    List {
        /// Print the server's JSON on one line
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub struct PushArgs {
    /// A path inside the git repo
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Name of the repo on the server. Defaults to the name of the repo's directory
    #[arg(long)]
    name: Option<String>,
    /// URL template for links to files, for example https://github.com/acme/app/blob/{commit}/{path}
    #[arg(long)]
    web_url: Option<String>,
    /// Send the whole history even when the server already has an earlier commit
    #[arg(long)]
    full: bool,
    /// Print the report as JSON on one line
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct CreateRepoBody<'a> {
    name: &'a str,
    web_url_template: Option<&'a str>,
}

#[derive(Serialize, Deserialize)]
struct RepoItem {
    id: Uuid,
    name: String,
    indexed_commit: Option<String>,
    web_url_template: Option<String>,
    /// Missing from an older server.
    #[serde(default)]
    files_in_index: usize,
}

#[derive(Serialize, Deserialize)]
struct RepoList {
    repos: Vec<RepoItem>,
}

pub async fn run_repo(client: &Client, command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::Push(args) => push(client, args).await,
        RepoCommand::List { json } => list(client, json).await,
    }
}

async fn list(client: &Client, json: bool) -> Result<()> {
    let list: RepoList = client.get_json("/v1/repos", Auth::Key).await?;
    if json {
        println!("{}", serde_json::to_string(&list)?);
    } else {
        for repo in &list.repos {
            println!("{}", format_repo_line(repo));
        }
    }
    Ok(())
}

async fn push(client: &Client, args: PushArgs) -> Result<()> {
    let root = repo_root(&args.path).await?;
    let name = match args.name {
        Some(n) => n,
        None => root
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow!(
                    "cannot derive a repo name from {}: pass --name",
                    root.display()
                )
            })?,
    };
    let head = git_stdout(&root, ["rev-parse", "HEAD"])
        .await
        .with_context(|| format!("{} has no commit to index", root.display()))?
        .trim()
        .to_string();
    if !is_full_sha(&head) {
        bail!("git reported an unexpected HEAD: {head:?}");
    }
    if !git_stdout(&root, ["status", "--porcelain"])
        .await?
        .trim()
        .is_empty()
    {
        eprintln!("note: uncommitted changes are not indexed");
    }

    let repo: RepoItem = client
        .post_json(
            "/v1/repos",
            Auth::Key,
            &CreateRepoBody {
                name: &name,
                web_url_template: args.web_url.as_deref(),
            },
        )
        .await?;

    let report = if repo.indexed_commit.as_deref() == Some(head.as_str()) && !args.full {
        // Nothing new to send.
        IndexReport {
            indexed_commit: head.clone(),
            files_indexed: 0,
            files_unchanged: 0,
            files_skipped: 0,
            files_deleted: 0,
            files_in_index: repo.files_in_index,
            chunks: 0,
        }
    } else {
        let base = if args.full {
            None
        } else {
            incremental_base(&root, repo.indexed_commit.as_deref()).await?
        };
        upload_bundle(client, &root, repo.id, &head, base.as_deref()).await?
    };

    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("{}", format_report(&name, &report));
    }
    Ok(())
}

/// Bundles `head` and uploads it. With a `base` the bundle holds only the commits after it. When
/// the server asks for a full bundle (it lost its mirror), sends one and tries once more.
async fn upload_bundle(
    client: &Client,
    root: &Path,
    id: Uuid,
    head: &str,
    base: Option<&str>,
) -> Result<IndexReport> {
    // The directory and the bundle files in it are removed when `tmp` drops, on every path.
    let tmp = tempfile::tempdir().context("creating a temp dir for the bundle")?;
    let attempt = |file: PathBuf, full: bool| async move {
        let path = format!("/v1/repos/{id}/bundle?ref={head}&full={full}");
        client
            .put_file(&path, Auth::Key, &file, "application/octet-stream")
            .await
    };

    let first = tmp.path().join(if base.is_some() {
        "incremental.bundle"
    } else {
        "full.bundle"
    });
    make_bundle(root, &first, base).await?;
    match attempt(first, base.is_none()).await {
        Err(e) if base.is_some() && is_need_full_bundle(&e) => {
            let full = tmp.path().join("full.bundle");
            make_bundle(root, &full, None).await?;
            attempt(full, true).await
        }
        other => other,
    }
}

/// True when the server answered 409 `need_full_bundle`.
fn is_need_full_bundle(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ApiError>(),
        Some(api) if api.status == 409 && api.message == "need_full_bundle"
    )
}

/// The server's indexed commit when an incremental bundle on top of it is possible: the commit
/// is in this repo and is an ancestor of HEAD (a force-pushed history is neither).
async fn incremental_base(root: &Path, indexed: Option<&str>) -> Result<Option<String>> {
    let Some(indexed) = indexed else {
        return Ok(None);
    };
    // The sha goes into git's arguments, so anything else than 40 hex characters is not trusted.
    if !is_full_sha(indexed) {
        return Ok(None);
    }
    let known = git_succeeds(root, ["cat-file", "-e", &format!("{indexed}^{{commit}}")]).await?;
    if !known {
        return Ok(None);
    }
    let ancestor = git_succeeds(root, ["merge-base", "--is-ancestor", indexed, "HEAD"]).await?;
    Ok(ancestor.then(|| indexed.to_string()))
}

/// Writes a bundle of HEAD to `file`, without the history up to `base` when it is given.
async fn make_bundle(root: &Path, file: &Path, base: Option<&str>) -> Result<()> {
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("bundle"),
        OsStr::new("create"),
        OsStr::new("-q"),
        file.as_os_str(),
        OsStr::new("HEAD"),
    ];
    let exclude = base.map(|b| format!("^{b}"));
    if let Some(e) = &exclude {
        args.push(OsStr::new(e));
    }
    git_stdout(root, args)
        .await
        .context("creating the git bundle")?;
    Ok(())
}

/// The top level directory of the work tree that contains `path`.
async fn repo_root(path: &Path) -> Result<PathBuf> {
    let out = git_stdout(path, ["rev-parse", "--show-toplevel"])
        .await
        .with_context(|| format!("{} is not inside a git work tree", path.display()))?;
    let top = out.strip_suffix('\n').unwrap_or(&out);
    if top.is_empty() {
        bail!("{} is not inside a git work tree", path.display());
    }
    Ok(PathBuf::from(top))
}

/// Runs `git -C <root> <args>`. Fails to start, or exits non-zero, are errors with git's stderr.
async fn git_stdout<I, S>(root: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<S> = args.into_iter().collect();
    let out = run_git(root, &args).await?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            describe(&args),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("git printed output that is not UTF-8")
}

/// Runs `git -C <root> <args>` and tells whether it exited zero.
async fn git_succeeds<I, S>(root: &Path, args: I) -> Result<bool>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<S> = args.into_iter().collect();
    Ok(run_git(root, &args).await?.status.success())
}

async fn run_git<S: AsRef<OsStr>>(root: &Path, args: &[S]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .context("running git (is it installed and on the PATH?)")
}

fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The first 7 characters of a sha.
fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn format_report(name: &str, r: &IndexReport) -> String {
    format!(
        "indexed {name} at {}: {} indexed, {} unchanged, {} deleted, {} skipped, {} chunks",
        short_sha(&r.indexed_commit),
        r.files_indexed,
        r.files_unchanged,
        r.files_deleted,
        r.files_skipped,
        r.chunks
    )
}

/// `name  indexed_commit(short)  web_url_template`, with `-` for a value that is not set.
fn format_repo_line(r: &RepoItem) -> String {
    format!(
        "{}  {}  {}",
        r.name,
        r.indexed_commit.as_deref().map_or("-", short_sha),
        r.web_url_template.as_deref().unwrap_or("-")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_sha_takes_seven_characters() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
    }

    #[test]
    fn report_line_has_the_documented_format() {
        let r = IndexReport {
            indexed_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            files_indexed: 6,
            files_unchanged: 1,
            files_skipped: 3,
            files_deleted: 2,
            files_in_index: 7,
            chunks: 40,
        };
        assert_eq!(
            format_report("sample", &r),
            "indexed sample at 0123456: 6 indexed, 1 unchanged, 2 deleted, 3 skipped, 40 chunks"
        );
    }

    #[test]
    fn repo_line_marks_missing_values() {
        let mut r = RepoItem {
            id: Uuid::nil(),
            name: "sample".to_string(),
            indexed_commit: None,
            web_url_template: None,
            files_in_index: 0,
        };
        assert_eq!(format_repo_line(&r), "sample  -  -");
        r.indexed_commit = Some("0123456789abcdef0123456789abcdef01234567".to_string());
        r.web_url_template = Some("https://x/{path}".to_string());
        assert_eq!(format_repo_line(&r), "sample  0123456  https://x/{path}");
    }

    #[test]
    fn create_body_always_has_the_template_field() {
        let body = CreateRepoBody {
            name: "a",
            web_url_template: None,
        };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"name":"a","web_url_template":null}"#
        );
    }

    #[test]
    fn only_need_full_bundle_conflicts_trigger_the_retry() {
        let err = |status, message: &str| {
            anyhow::Error::from(ApiError {
                status,
                message: message.to_string(),
            })
        };
        assert!(is_need_full_bundle(&err(409, "need_full_bundle")));
        assert!(!is_need_full_bundle(&err(409, "other")));
        assert!(!is_need_full_bundle(&err(500, "need_full_bundle")));
        assert!(!is_need_full_bundle(&anyhow!("boom")));
    }
}
