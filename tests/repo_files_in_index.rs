//! `files_in_index`: how many files a repo's index holds after a run. It is in the report of a
//! first index, of a push and of a sync, and in each item of `GET /v1/repos`.

mod common;

use std::path::Path;

use axum::Router;
use axum::http::StatusCode;
use common::{
    bundle, commit_all, create_repo, create_tenant_key, git, head, put_bundle, send, test_router,
    test_state_with,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;

struct Env {
    router: Router,
    key: String,
    _data: TempDir,
}

async fn env(pool: PgPool) -> Env {
    let data = tempfile::tempdir().unwrap();
    let state = test_state_with(pool, data.path(), |c| {
        c.sync_allow_file_remotes = true;
        // A first index of 300 files commits in 3 batches: the count is taken after the last.
        c.first_index_batch = 100;
    });
    let router = test_router(state);
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    Env {
        router,
        key,
        _data: data,
    }
}

/// A git repo of `n` small text files, `f000.txt` and up.
fn repo_of(n: usize) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    for i in 0..n {
        write(dir.path(), &format!("f{i:03}.txt"), &format!("file {i}\n"));
    }
    commit_all(&dir, "init");
    dir
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

/// Adds two files and deletes one, in one commit.
fn add_two_delete_one(repo: &TempDir) {
    write(repo.path(), "new_a.txt", "a new file\n");
    write(repo.path(), "new_b.txt", "another new file\n");
    std::fs::remove_file(repo.path().join("f000.txt")).unwrap();
    commit_all(repo, "add two, delete one");
}

/// Edits one file and nothing else.
fn edit_one(repo: &TempDir) {
    write(repo.path(), "f001.txt", "file 1, edited\n");
    commit_all(repo, "edit one");
}

async fn repo_list(env: &Env) -> Vec<Value> {
    let (status, list) = send(&env.router, "GET", "/v1/repos", &env.key, Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    list["repos"].as_array().unwrap().clone()
}

/// `files_in_index` of the repo named `name` in `GET /v1/repos`.
async fn listed_in_index(env: &Env, name: &str) -> u64 {
    let repos = repo_list(env).await;
    let item = repos.iter().find(|r| r["name"] == name).unwrap();
    item["files_in_index"].as_u64().unwrap()
}

async fn sync(env: &Env, id: Uuid) -> Value {
    let uri = format!("/v1/repos/{id}/sync");
    let (status, report) = send(&env.router, "POST", &uri, &env.key, Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    report
}

#[sqlx::test(migrations = "./migrations")]
async fn a_sync_and_the_repo_list_count_the_files_the_index_holds(pool: PgPool) {
    let env = env(pool).await;
    let repo = repo_of(300);
    let url = format!("file://{}", repo.path().display());
    let (status, created) = send(
        &env.router,
        "POST",
        "/v1/repos",
        &env.key,
        json!({"name": "big", "remote_url": url})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    // Nothing is indexed yet.
    assert_eq!(created["files_in_index"], 0);
    assert_eq!(listed_in_index(&env, "big").await, 0);

    // Check 1: the first index of 300 files.
    let first = sync(&env, id).await;
    assert_eq!(first["files_indexed"], 300, "{first}");
    assert_eq!(first["files_in_index"], 300, "{first}");
    assert_eq!(listed_in_index(&env, "big").await, 300);

    // Check 2: a push that adds 2 files and deletes 1.
    add_two_delete_one(&repo);
    let second = sync(&env, id).await;
    assert_eq!(second["files_indexed"], 2, "{second}");
    assert_eq!(second["files_deleted"], 1, "{second}");
    assert_eq!(second["files_in_index"], 301, "{second}");
    assert_eq!(listed_in_index(&env, "big").await, 301);

    // Check 3: a push that only edits a file.
    edit_one(&repo);
    let third = sync(&env, id).await;
    assert_eq!(third["files_indexed"], 1, "{third}");
    assert_eq!(third["files_deleted"], 0, "{third}");
    assert_eq!(third["files_in_index"], 301, "{third}");
    assert_eq!(listed_in_index(&env, "big").await, 301);

    // A sync with nothing new indexes nothing and still says what the index holds.
    let again = sync(&env, id).await;
    assert_eq!(again["up_to_date"], true, "{again}");
    assert_eq!(again["files_indexed"], 0, "{again}");
    assert_eq!(again["files_in_index"], 301, "{again}");

    // Creating a repo that exists answers the repo as it is, with its count (the CLI reads it).
    let (status, existing) = send(
        &env.router,
        "POST",
        "/v1/repos",
        &env.key,
        json!({"name": "big"}).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{existing}");
    assert_eq!(existing["id"], created["id"]);
    assert_eq!(existing["files_in_index"], 301, "{existing}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_bundle_push_reports_the_files_in_index_too(pool: PgPool) {
    let env = env(pool).await;
    let (status, created) = create_repo(&env.router, &env.key, "pushed").await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let repo = repo_of(300);

    let sha = head(&repo);
    let (status, first) = put_bundle(
        &env.router,
        &env.key,
        id,
        bundle(&repo, &["HEAD"]),
        &sha,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["files_in_index"], 300, "{first}");
    assert_eq!(listed_in_index(&env, "pushed").await, 300);

    let prev = head(&repo);
    add_two_delete_one(&repo);
    let sha = head(&repo);
    let range = format!("^{prev}");
    let (status, second) = put_bundle(
        &env.router,
        &env.key,
        id,
        bundle(&repo, &["HEAD", &range]),
        &sha,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["files_indexed"], 2, "{second}");
    assert_eq!(second["files_deleted"], 1, "{second}");
    assert_eq!(second["files_in_index"], 301, "{second}");

    let prev = head(&repo);
    edit_one(&repo);
    let sha = head(&repo);
    let range = format!("^{prev}");
    let (status, third) = put_bundle(
        &env.router,
        &env.key,
        id,
        bundle(&repo, &["HEAD", &range]),
        &sha,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{third}");
    assert_eq!(third["files_in_index"], 301, "{third}");
    assert_eq!(listed_in_index(&env, "pushed").await, 301);
}

#[sqlx::test(migrations = "./migrations")]
async fn files_skipped_by_the_rules_are_not_counted(pool: PgPool) {
    let env = env(pool).await;
    let (_, created) = create_repo(&env.router, &env.key, "mixed").await;
    let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let repo = repo_of(3);
    // Lock files and images are never indexed.
    write(repo.path(), "Cargo.lock", "# lock\n");
    write(repo.path(), "logo.png", "not really a png\n");
    commit_all(&repo, "skipped files");

    let sha = head(&repo);
    let (status, report) = put_bundle(
        &env.router,
        &env.key,
        id,
        bundle(&repo, &["HEAD"]),
        &sha,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["files_skipped"], 2, "{report}");
    assert_eq!(report["files_in_index"], 3, "{report}");
    assert_eq!(listed_in_index(&env, "mixed").await, 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_repo_list_counts_each_repo_and_only_the_callers(pool: PgPool) {
    let env = env(pool).await;
    let (_, empty) = create_repo(&env.router, &env.key, "empty").await;
    assert_eq!(empty["files_in_index"], 0);

    for (name, n) in [("small", 2), ("medium", 5)] {
        let (_, created) = create_repo(&env.router, &env.key, name).await;
        let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
        let repo = repo_of(n);
        let sha = head(&repo);
        let (status, report) = put_bundle(
            &env.router,
            &env.key,
            id,
            bundle(&repo, &["HEAD"]),
            &sha,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{report}");
    }

    // Another tenant's repo of the same name must not leak into this list.
    let (_, other_key) = create_tenant_key(&env.router, "other", &["ingest", "read"]).await;
    let (_, theirs) = create_repo(&env.router, &other_key, "small").await;
    let their_id: Uuid = theirs["id"].as_str().unwrap().parse().unwrap();
    let repo = repo_of(9);
    let sha = head(&repo);
    let (status, _) = put_bundle(
        &env.router,
        &other_key,
        their_id,
        bundle(&repo, &["HEAD"]),
        &sha,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let totals: Vec<(String, u64)> = repo_list(&env)
        .await
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["files_in_index"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        totals,
        [
            ("empty".to_string(), 0),
            ("medium".to_string(), 5),
            ("small".to_string(), 2)
        ]
    );
}
