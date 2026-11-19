mod common;

use std::path::Path;

use common::{
    commit_all, create_tenant_key, fixture_repo, git, head, serve_router, test_router, test_state,
};
use serde_json::Value;
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::process::Command;

const PLANS: &str = "src/billing/plans.rs";

/// A server on a local port, with one tenant key, and a place for the CLI's temp files.
struct Env {
    base: String,
    key: String,
    /// The server's data dir (mirrors live in `data/repos/<source id>`).
    data: TempDir,
    /// `TMPDIR` of the CLI process, so a test can see whether it left a bundle behind.
    cli_tmp: TempDir,
}

/// What a `lore` run printed and whether it exited zero.
struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The stdout of a run that must have succeeded, as JSON.
    fn json(&self) -> Value {
        assert!(self.ok, "lore failed: {}", self.stderr);
        serde_json::from_str(self.stdout.trim()).expect("stdout is JSON")
    }
}

impl Env {
    async fn new(pool: PgPool) -> Env {
        Env::with_scopes(pool, &["ingest", "read"]).await
    }

    async fn with_scopes(pool: PgPool, scopes: &[&str]) -> Env {
        let data = tempfile::tempdir().unwrap();
        let router = test_router(test_state(pool, data.path()));
        let (_, key) = create_tenant_key(&router, "acme", scopes).await;
        let base = serve_router(router).await;
        Env {
            base,
            key,
            data,
            cli_tmp: tempfile::tempdir().unwrap(),
        }
    }

    async fn lore(&self, args: &[&str]) -> Run {
        self.lore_as(&self.key, args, &[]).await
    }

    /// Runs the built binary with `--url` and `--key` and the given arguments. `envs` are extra
    /// environment variables for the process.
    async fn lore_as(&self, key: &str, args: &[&str], envs: &[(&str, &Path)]) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        cmd.args(["--url", &self.base, "--key", key])
            .args(args)
            .env_remove("LORE_URL")
            .env_remove("LORE_KEY")
            .env_remove("LORE_ADMIN_TOKEN")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("TMPDIR", self.cli_tmp.path());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().await.expect("the lore binary runs");
        Run {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// `lore repo push <repo> --name sample --json` plus `extra`.
    async fn push(&self, repo: &TempDir, extra: &[&str]) -> Run {
        let path = repo.path().to_str().unwrap();
        let mut args = vec!["repo", "push", path, "--name", "sample", "--json"];
        args.extend_from_slice(extra);
        self.lore(&args).await
    }

    /// The CLI must remove its bundle files on every path.
    fn assert_no_leftover_bundles(&self) {
        let left: Vec<_> = std::fs::read_dir(self.cli_tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            left.is_empty(),
            "the CLI left files in its temp dir: {left:?}"
        );
    }

    /// Deletes every mirror and checkout of the server.
    fn lose_server_mirrors(&self) {
        let repos = self.data.path().join("repos");
        assert!(repos.exists(), "the server has no repos dir yet");
        std::fs::remove_dir_all(repos).unwrap();
    }
}

/// Appends a line to `plans.rs` and commits it.
fn change_plans(repo: &TempDir, line: &str) {
    let file = repo.path().join(PLANS);
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str(&format!("\n// {line}\n"));
    std::fs::write(file, text).unwrap();
    commit_all(repo, line);
}

async fn chunk_count(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM chunks c
         JOIN documents d ON d.id = c.document_id
         JOIN sources s ON s.id = d.source_id
         WHERE s.name = $1",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn push_indexes_full_then_incremental_then_nothing(pool: PgPool) {
    let env = Env::new(pool).await;
    let repo = fixture_repo();
    let first = head(&repo);

    // The first push sends the whole repo.
    let report = env.push(&repo, &[]).await.json();
    assert_eq!(report["files_indexed"], 6);
    assert_eq!(report["files_unchanged"], 0);
    assert_eq!(report["files_skipped"], 1);
    assert_eq!(report["indexed_commit"], first.as_str());
    assert_eq!(report["files_in_index"], 6);
    assert!(report["chunks"].as_u64().unwrap() > 0);

    // A change to one file sends only the new commit: the other files are not even looked at
    // (a full bundle would report them as unchanged).
    change_plans(&repo, "add a plan");
    let second = head(&repo);
    let report = env.push(&repo, &[]).await.json();
    assert_eq!(report["files_indexed"], 1);
    assert_eq!(report["files_unchanged"], 0);
    assert_eq!(report["files_deleted"], 0);
    assert_eq!(report["indexed_commit"], second.as_str());
    // An edit adds no file: the index holds the same 6.
    assert_eq!(report["files_in_index"], 6);

    // Nothing new: no bundle at all, a report of zeros, and the same commit.
    let out = env.push(&repo, &[]).await;
    assert!(out.ok, "{}", out.stderr);
    let report = out.json();
    assert_eq!(report["files_indexed"], 0);
    assert_eq!(report["files_unchanged"], 0);
    assert_eq!(report["files_skipped"], 0);
    assert_eq!(report["files_deleted"], 0);
    assert_eq!(report["chunks"], 0);
    assert_eq!(report["indexed_commit"], second.as_str());
    // The report has no bundle to read it from: it says what the repo list says.
    assert_eq!(report["files_in_index"], 6);

    env.assert_no_leftover_bundles();
}

#[sqlx::test(migrations = "./migrations")]
async fn push_up_to_date_needs_no_server_mirror(pool: PgPool) {
    let env = Env::new(pool).await;
    let repo = fixture_repo();
    env.push(&repo, &[]).await.json();

    // If the CLI sent anything, the server would have to rebuild its mirror. A no-op does not.
    env.lose_server_mirrors();
    let report = env.push(&repo, &[]).await.json();
    assert_eq!(report["files_indexed"], 0);
    assert_eq!(report["indexed_commit"], head(&repo).as_str());
    assert!(!env.data.path().join("repos").exists());
}

#[sqlx::test(migrations = "./migrations")]
async fn push_recovers_when_the_server_lost_its_mirror(pool: PgPool) {
    let env = Env::new(pool).await;
    let repo = fixture_repo();
    env.push(&repo, &[]).await.json();

    env.lose_server_mirrors();
    change_plans(&repo, "after the mirror was lost");
    let out = env.push(&repo, &[]).await;
    assert!(out.ok, "the push should recover: {}", out.stderr);
    let report = out.json();
    assert_eq!(report["indexed_commit"], head(&repo).as_str());
    assert_eq!(report["files_indexed"], 1);
    // The retry was a full bundle: the unchanged files were looked at again.
    assert_eq!(report["files_unchanged"], 5);
    env.assert_no_leftover_bundles();
}

#[sqlx::test(migrations = "./migrations")]
async fn push_sends_a_full_bundle_when_history_was_rewritten(pool: PgPool) {
    let env = Env::new(pool).await;
    let repo = fixture_repo();
    change_plans(&repo, "before the rewrite");
    let old = head(&repo);
    assert_eq!(env.push(&repo, &[]).await.json()["files_indexed"], 6);

    // Rewrite the pushed commit. The old commit stays in the object store but is no longer an
    // ancestor of HEAD, so `HEAD ^old` is not a valid increment.
    std::fs::write(repo.path().join(PLANS), "// rewritten\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let new = head(&repo);
    assert_ne!(old, new);
    git(&repo, &["cat-file", "-e", &format!("{old}^{{commit}}")]);

    let out = env.push(&repo, &[]).await;
    assert!(out.ok, "{}", out.stderr);
    let report = out.json();
    assert_eq!(report["indexed_commit"], new.as_str());
    assert_eq!(report["files_indexed"], 1);
    // Full mode again: every other file was compared and found unchanged.
    assert_eq!(report["files_unchanged"], 5);
    env.assert_no_leftover_bundles();
}

#[sqlx::test(migrations = "./migrations")]
async fn full_flag_sends_the_whole_history_even_when_up_to_date(pool: PgPool) {
    let env = Env::new(pool).await;
    let repo = fixture_repo();
    env.push(&repo, &[]).await.json();

    let report = env.push(&repo, &["--full"]).await.json();
    assert_eq!(report["indexed_commit"], head(&repo).as_str());
    assert_eq!(report["files_indexed"], 0);
    assert_eq!(report["files_unchanged"], 6);
    env.assert_no_leftover_bundles();
}

#[sqlx::test(migrations = "./migrations")]
async fn plain_output_is_one_summary_line(pool: PgPool) {
    let env = Env::new(pool.clone()).await;
    let repo = fixture_repo();
    let path = repo.path().to_str().unwrap();

    let out = env.lore(&["repo", "push", path, "--name", "sample"]).await;
    assert!(out.ok, "{}", out.stderr);
    let chunks = chunk_count(&pool, "sample").await;
    assert!(chunks > 0);
    let short = &head(&repo)[..7];
    assert_eq!(
        out.stdout,
        format!(
            "indexed sample at {short}: 6 indexed, 0 unchanged, 0 deleted, 1 skipped, {chunks} chunks\n"
        )
    );

    // --json is one compact line.
    let json = env.push(&repo, &["--full"]).await;
    assert_eq!(json.stdout.trim().lines().count(), 1, "{}", json.stdout);
    assert!(
        json.stdout.starts_with("{\"indexed_commit\":\""),
        "{}",
        json.stdout
    );
    assert!(!json.stdout.contains(": "), "{}", json.stdout);
}

#[sqlx::test(migrations = "./migrations")]
async fn uncommitted_changes_are_noted_on_stderr_and_not_indexed(pool: PgPool) {
    let env = Env::new(pool).await;
    let repo = fixture_repo();
    let note = "note: uncommitted changes are not indexed";

    let clean = env.push(&repo, &[]).await;
    assert!(!clean.stderr.contains(note), "{}", clean.stderr);

    // An edit to a tracked file and a new file, neither committed.
    std::fs::write(repo.path().join(PLANS), "// not committed\n").unwrap();
    std::fs::write(repo.path().join("scratch.txt"), "scratch\n").unwrap();
    let dirty = env.push(&repo, &[]).await;
    assert!(dirty.stderr.contains(note), "{}", dirty.stderr);
    assert!(!dirty.stdout.contains(note), "the note belongs on stderr");
    // The commit is already indexed, so this is a no-op that ignores the edits.
    let report = dirty.json();
    assert_eq!(report["files_indexed"], 0);
    assert_eq!(report["indexed_commit"], head(&repo).as_str());
}

#[sqlx::test(migrations = "./migrations")]
async fn push_outside_a_git_repo_fails_with_a_clear_message(pool: PgPool) {
    let env = Env::new(pool).await;
    let plain = tempfile::tempdir().unwrap();
    std::fs::write(plain.path().join("a.txt"), "a\n").unwrap();
    // Keep git from finding a repo in a parent of the temp dir.
    let ceiling = plain.path().parent().unwrap();

    let path = plain.path().to_str().unwrap();
    let out = env
        .lore_as(
            &env.key,
            &["repo", "push", path, "--name", "plain"],
            &[("GIT_CEILING_DIRECTORIES", ceiling)],
        )
        .await;
    assert!(!out.ok);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(
        out.stderr.contains("is not inside a git work tree"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains(path), "{}", out.stderr);

    // Nothing was created on the server.
    let list = env.lore(&["repo", "list"]).await;
    assert!(list.ok, "{}", list.stderr);
    assert_eq!(list.stdout, "");
}

#[sqlx::test(migrations = "./migrations")]
async fn push_of_a_repo_without_commits_fails(pool: PgPool) {
    let env = Env::new(pool).await;
    let empty = tempfile::tempdir().unwrap();
    git(&empty, &["init", "-q", "-b", "main"]);

    let out = env
        .lore(&[
            "repo",
            "push",
            empty.path().to_str().unwrap(),
            "--name",
            "e",
        ])
        .await;
    assert!(!out.ok);
    assert!(out.stderr.contains("no commit to index"), "{}", out.stderr);
}

#[sqlx::test(migrations = "./migrations")]
async fn repo_list_prints_name_short_commit_and_template(pool: PgPool) {
    let env = Env::new(pool).await;

    // No repos, no output.
    let out = env.lore(&["repo", "list"]).await;
    assert!(out.ok, "{}", out.stderr);
    assert_eq!(out.stdout, "");

    let repo = fixture_repo();
    env.push(&repo, &[]).await.json();
    let sha = head(&repo);

    let out = env.lore(&["repo", "list"]).await;
    assert!(out.ok, "{}", out.stderr);
    assert_eq!(out.stdout, format!("sample  {}  -\n", &sha[..7]));

    let json = env.lore(&["repo", "list", "--json"]).await;
    assert_eq!(json.stdout.trim().lines().count(), 1, "{}", json.stdout);
    let list = json.json();
    let repos = list["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["name"], "sample");
    assert_eq!(repos[0]["indexed_commit"], sha.as_str());
    assert_eq!(repos[0]["web_url_template"], Value::Null);
    assert!(repos[0]["id"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn web_url_is_stored_and_listed(pool: PgPool) {
    let env = Env::new(pool).await;
    let repo = fixture_repo();
    let template = "https://example.com/acme/sample/blob/{commit}/{path}";
    env.push(&repo, &["--web-url", template]).await.json();

    let out = env.lore(&["repo", "list"]).await;
    assert_eq!(
        out.stdout,
        format!("sample  {}  {template}\n", &head(&repo)[..7])
    );
    let json = env.lore(&["repo", "list", "--json"]).await.json();
    assert_eq!(json["repos"][0]["web_url_template"], template);
}

#[sqlx::test(migrations = "./migrations")]
async fn name_defaults_to_the_repo_directory_not_the_given_path(pool: PgPool) {
    let env = Env::new(pool).await;
    let repo = fixture_repo();
    let dir = repo
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // A path inside the repo is resolved to the top of the work tree.
    let inside = repo.path().join("src/billing");
    let out = env
        .lore(&["repo", "push", inside.to_str().unwrap(), "--json"])
        .await;
    assert_eq!(out.json()["files_indexed"], 6);

    let list = env.lore(&["repo", "list", "--json"]).await.json();
    assert_eq!(list["repos"][0]["name"], dir.as_str());
}

#[sqlx::test(migrations = "./migrations")]
async fn push_prints_the_server_error_and_never_the_key(pool: PgPool) {
    // A key without the ingest scope.
    let env = Env::with_scopes(pool, &["read"]).await;
    let repo = fixture_repo();

    let out = env.push(&repo, &[]).await;
    assert!(!out.ok);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(out.stderr.contains("server returned 403"), "{}", out.stderr);
    assert!(!out.stderr.contains(&env.key), "the key was printed");

    // A key the server does not know.
    let bad = "lk_0000000000000000000000000000000000000000";
    let path = repo.path().to_str().unwrap();
    let out = env
        .lore_as(bad, &["repo", "push", path, "--name", "sample"], &[])
        .await;
    assert!(!out.ok);
    assert!(
        out.stderr.contains("server returned 401: unauthorized"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains(bad), "the key was printed");
    env.assert_no_leftover_bundles();
}
