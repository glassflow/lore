//! A deep answer on a repo the server adopted: `lore index` (a Job with its own data dir)
//! indexed the repo, then the server, whose data dir was empty, synced it. `grep` and
//! `read_file` read the checkout the sync built.

mod common;

use std::collections::VecDeque;
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use common::{commit_all, create_repo, create_tenant_key, fixture_repo, send, test_router};
use common::{head, test_state_with};
use lore::llm::{Block, Llm, LlmRequest, LlmResponse, Message, Usage};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// A word only the file `src/billing/plans.rs` of the fixture has.
const NEEDLE: &str = "spans_per_day";
/// A line only the file `NOTES.md`, added after the Job indexed, has.
const MARKER: &str = "Adopted_marker_only_in_the_checkout";
const PLANS: &str = "src/billing/plans.rs";

/// A scripted model that records the requests it gets.
struct Script {
    replies: Mutex<VecDeque<Vec<Block>>>,
    calls: Mutex<Vec<LlmRequest>>,
}

#[async_trait::async_trait]
impl Llm for Script {
    fn name(&self) -> &str {
        "script"
    }

    fn supports_tools(&self) -> bool {
        true
    }

    async fn complete(&self, req: LlmRequest) -> anyhow::Result<LlmResponse> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(req);
        let content = self
            .replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .expect("the script has a reply left");
        let tools = content.iter().any(|b| matches!(b, Block::ToolUse { .. }));
        Ok(LlmResponse {
            content,
            stop_reason: if tools { "tool_use" } else { "end_turn" }.to_string(),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            model: "script".to_string(),
        })
    }
}

fn dsn(pool: &PgPool) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL is set for database tests");
    let mut url = reqwest::Url::parse(&base).expect("DATABASE_URL parses");
    url.set_path(&format!(
        "/{}",
        pool.connect_options().get_database().expect("a database")
    ));
    url.to_string()
}

/// Runs the real `lore index` with its own data dir against the test database.
async fn lore_index(
    pool: &PgPool,
    job_data: &std::path::Path,
    tenant: Uuid,
    source: Uuid,
    remote: &str,
) {
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
    .env("LORE_DATABASE_URL", dsn(pool))
    .env("LORE_DATA_DIR", job_data)
    .env("LORE_EMBEDDER", "hash")
    .env("LORE_SYNC_ALLOW_FILE_REMOTES", "true")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_CONFIG_NOSYSTEM", "1");
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await
        .unwrap()
        .expect("the lore binary runs");
    assert!(
        out.status.success(),
        "lore index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tool_results(m: &Message) -> Vec<(&str, bool)> {
    m.content
        .iter()
        .map(|b| match b {
            Block::ToolResult {
                content, is_error, ..
            } => (content.as_str(), *is_error),
            other => panic!("expected a tool result, got {other:?}"),
        })
        .collect()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_deep_answer_reads_the_checkout_the_sync_adopted(pool: PgPool) {
    let server_data = tempfile::tempdir().unwrap();
    let job_data = tempfile::tempdir().unwrap();
    let script = Arc::new(Script {
        replies: Mutex::new(
            vec![
                vec![Block::ToolUse {
                    id: "g1".into(),
                    name: "grep".into(),
                    input: json!({ "pattern": format!("{NEEDLE}|{MARKER}"), "source": "sample" }),
                }],
                vec![Block::ToolUse {
                    id: "r1".into(),
                    name: "read_file".into(),
                    input: json!({ "source": "sample", "path": PLANS, "start_line": 1, "end_line": 3 }),
                }],
                vec![Block::Text {
                    text: format!("Plans cap seats [sample/{PLANS}:1-3]."),
                }],
            ]
            .into(),
        ),
        calls: Mutex::new(Vec::new()),
    });
    let mut state = test_state_with(pool.clone(), server_data.path(), |c| {
        c.sync_allow_file_remotes = true;
    });
    state.llm = script.clone();
    let router = test_router(state);
    let (tenant, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    let (status, repo) = create_repo(&router, &key, "sample").await;
    assert_eq!(status, 200, "{repo}");
    let source: Uuid = repo["id"].as_str().unwrap().parse().unwrap();

    // The Job indexes the first commit; the remote then moves on, so the sync adopts the repo
    // and indexes the diff, and the checkout holds the newer file too.
    let remote = fixture_repo();
    let url = format!("file://{}", remote.path().display());
    lore_index(&pool, job_data.path(), tenant, source, &url).await;
    std::fs::write(
        remote.path().join("NOTES.md"),
        format!("# Notes\n\n{MARKER}.\n"),
    )
    .unwrap();
    commit_all(&remote, "notes");
    let body = json!({ "name": "sample", "remote_url": url }).to_string();
    let (status, _) = send(&router, "POST", "/v1/repos", &key, body.into_bytes()).await;
    assert_eq!(status, 200);
    let (status, report) = send(
        &router,
        "POST",
        &format!("/v1/repos/{source}/sync"),
        &key,
        Vec::new(),
    )
    .await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["adopted"], true, "{report}");
    assert_eq!(report["indexed_commit"], head(&remote).as_str());
    assert!(
        server_data
            .path()
            .join(format!("repos/{source}/checkout/{PLANS}"))
            .is_file()
    );

    let ask = json!({ "question": "which plans cap seats?", "mode": "deep" });
    let (status, body) = send(
        &router,
        "POST",
        "/v1/ask",
        &key,
        ask.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tool_calls"], 2, "{body}");

    // The tools returned the checkout's text, not errors.
    let calls = script.calls.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(calls.len(), 3);
    let grep = tool_results(&calls[1].messages[2]);
    assert!(!grep[0].1, "grep failed: {}", grep[0].0);
    assert!(
        grep[0].0.contains(&format!("sample/{PLANS}:"))
            && grep[0].0.contains(NEEDLE)
            && grep[0].0.contains("sample/NOTES.md:3:")
            && grep[0].0.contains(MARKER),
        "{}",
        grep[0].0
    );
    let read = tool_results(&calls[2].messages[4]);
    assert!(!read[0].1, "read_file failed: {}", read[0].0);
    let on_disk = std::fs::read_to_string(
        server_data
            .path()
            .join(format!("repos/{source}/checkout/{PLANS}")),
    )
    .unwrap();
    let first = on_disk.lines().next().unwrap();
    assert!(read[0].0.contains(first), "{}", read[0].0);

    // The answer cites the file of the adopted checkout.
    let cited = body["citations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["path"] == PLANS)
        .unwrap_or_else(|| panic!("no citation for {PLANS}: {body}"));
    assert_eq!(cited["start_line"], 1);
    assert_eq!(cited["end_line"], 3);
    assert!(cited["document_id"].is_string(), "{cited}");
}
