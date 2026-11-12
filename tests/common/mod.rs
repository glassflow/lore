#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use lore::config::Config;
use lore::embed::HashEmbedder;
use lore::http::{AppState, router};
use lore::llm::{FakeLlm, ModeModel, Models};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub fn test_config(data_dir: &Path) -> Config {
    test_config_with_limit(data_dir, None)
}

/// Like `test_config`, with `LORE_MAX_BUNDLE_BYTES` set when `max_bundle_bytes` is given.
pub fn test_config_with_limit(data_dir: &Path, max_bundle_bytes: Option<u64>) -> Config {
    Config::from_lookup(|k| match k {
        "LORE_DATA_DIR" => Some(data_dir.to_string_lossy().into_owned()),
        "LORE_ADMIN_TOKEN" => Some("t".to_string()),
        "LORE_MAX_BUNDLE_BYTES" => max_bundle_bytes.map(|n| n.to_string()),
        _ => None,
    })
    .expect("test config is valid")
}

pub fn test_state(pool: PgPool, data_dir: &Path) -> AppState {
    test_state_with_limit(pool, data_dir, None)
}

/// A state whose bundle upload limit is `max_bundle_bytes` (the default when `None`).
pub fn test_state_with_limit(
    pool: PgPool,
    data_dir: &Path,
    max_bundle_bytes: Option<u64>,
) -> AppState {
    build_state(pool, test_config_with_limit(data_dir, max_bundle_bytes))
}

/// A state whose config is the test config after `change` has edited it.
pub fn test_state_with(
    pool: PgPool,
    data_dir: &Path,
    change: impl FnOnce(&mut Config),
) -> AppState {
    let mut config = test_config(data_dir);
    change(&mut config);
    build_state(pool, config)
}

fn build_state(pool: PgPool, config: Config) -> AppState {
    AppState {
        pool,
        config: Arc::new(config),
        embedder: Arc::new(HashEmbedder),
        llm: Arc::new(FakeLlm::new()),
        models: Models {
            fast: ModeModel {
                model: "fake".into(),
                effort: None,
            },
            deep: ModeModel {
                model: "fake".into(),
                effort: None,
            },
        },
        source_locks: Arc::default(),
        deep_guard: Arc::default(),
    }
}

pub fn test_router(state: AppState) -> Router {
    router(state)
}

/// Serves `router` on a random local port (`127.0.0.1:0`) and returns its base URL, for tests
/// that talk to it over real HTTP, such as the CLI tests.
pub async fn serve_router(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let addr = listener.local_addr().expect("local address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    format!("http://{addr}")
}

/// Creates a tenant and one key with the given scopes through the admin routes (admin token
/// `"t"`). Returns the tenant id and the plaintext key.
pub async fn create_tenant_key(router: &Router, name: &str, scopes: &[&str]) -> (Uuid, String) {
    let req = Request::post("/v1/admin/tenants")
        .header("authorization", "Bearer t")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": name }).to_string()))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "create tenant {name}");
    let tenant: Value = body_json(res).await;
    let tenant_id: Uuid = tenant["id"].as_str().unwrap().parse().unwrap();

    let req = Request::post(format!("/v1/admin/tenants/{tenant_id}/keys"))
        .header("authorization", "Bearer t")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "scopes": scopes }).to_string()))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "create key for {name}");
    let key: Value = body_json(res).await;
    (tenant_id, key["key"].as_str().unwrap().to_string())
}

/// Runs `git` in `dir` and returns its stdout. The host's git configuration (signing, hooks,
/// templates, `init.defaultBranch`) is ignored, so the helpers behave the same on every machine.
pub fn git(dir: impl AsRef<Path>, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir.as_ref())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("git output is utf-8")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create fixture dir");
    for entry in std::fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let dest = to.join(entry.file_name());
        if entry.file_type().expect("fixture entry type").is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).expect("copy fixture file");
        }
    }
}

/// The sample repo (`tests/fixtures/sample-repo`) copied into a temp dir and committed on `main`
/// as `init`.
pub fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-repo");
    copy_dir(&src, dir.path());
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    commit_all(&dir, "init");
    dir
}

/// Stages every change in `dir` and commits it.
pub fn commit_all(dir: impl AsRef<Path>, msg: &str) {
    let dir = dir.as_ref();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// `git bundle create` with `range` (for example `["main"]` or `["<sha>..main"]`); returns the
/// bundle bytes.
pub fn bundle(dir: impl AsRef<Path>, range: &[&str]) -> Vec<u8> {
    let out = tempfile::tempdir().expect("temp dir");
    let file = out.path().join("repo.bundle");
    let file_arg = file.to_string_lossy().into_owned();
    let mut args = vec!["bundle", "create", "-q", file_arg.as_str()];
    args.extend_from_slice(range);
    git(dir, &args);
    std::fs::read(&file).expect("read bundle")
}

/// The full sha of `HEAD`.
pub fn head(dir: impl AsRef<Path>) -> String {
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

pub async fn body_json(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Sends a request with a bearer key and returns the status and the JSON body (`Null` when the
/// body is empty or not JSON).
pub async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    key: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    (status, body_json(res).await)
}

/// `POST /v1/repos` with the given name. Returns the status and the JSON body.
pub async fn create_repo(router: &Router, key: &str, name: &str) -> (StatusCode, Value) {
    let body = json!({ "name": name }).to_string().into_bytes();
    send(router, "POST", "/v1/repos", key, body).await
}

/// `PUT /v1/repos/{id}/bundle` with the given bytes. Returns the status and the JSON body.
pub async fn put_bundle(
    router: &Router,
    key: &str,
    source_id: Uuid,
    bytes: Vec<u8>,
    sha: &str,
    full: bool,
) -> (StatusCode, Value) {
    let uri = format!("/v1/repos/{source_id}/bundle?ref={sha}&full={full}");
    let req = Request::put(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/octet-stream")
        .body(Body::from(bytes))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    (status, body_json(res).await)
}

/// Creates the source `sample` and pushes a full bundle of a fresh `fixture_repo()` with
/// `ref = head`. Returns the source id and the repo (so a test can commit more and push again).
pub async fn push_fixture(router: &Router, key: &str) -> (Uuid, tempfile::TempDir) {
    let (status, repo_json) = create_repo(router, key, "sample").await;
    assert_eq!(status, StatusCode::OK, "create repo: {repo_json}");
    let source_id: Uuid = repo_json["id"].as_str().unwrap().parse().unwrap();
    let repo = fixture_repo();
    let sha = head(&repo);
    let bytes = bundle(&repo, &["HEAD"]);
    let (status, report) = put_bundle(router, key, source_id, bytes, &sha, true).await;
    assert_eq!(status, StatusCode::OK, "push fixture: {report}");
    assert_eq!(report["indexed_commit"], sha.as_str());
    (source_id, repo)
}
