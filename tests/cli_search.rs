mod common;

use common::{create_tenant_key, push_fixture, serve_router, test_router, test_state};
use serde_json::Value;
use sqlx::PgPool;
use tokio::process::Command;

/// A server on a local port with the fixture repo indexed and one tenant key.
struct Env {
    base: String,
    key: String,
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
        let data = tempfile::tempdir().unwrap();
        let router = test_router(test_state(pool, data.path()));
        let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        push_fixture(&router, &key).await;
        // Searching reads only the database, so the data dir (the repo mirrors) can go.
        drop(data);
        let base = serve_router(router).await;
        Env { base, key }
    }

    async fn lore(&self, args: &[&str]) -> Run {
        self.lore_with(Some(&self.key), args).await
    }

    /// Runs the built binary with `--url`, `--key` (when given) and the arguments.
    async fn lore_with(&self, key: Option<&str>, args: &[&str]) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        cmd.args(["--url", &self.base]);
        if let Some(key) = key {
            cmd.args(["--key", key]);
        }
        let out = cmd
            .args(args)
            .env_remove("LORE_URL")
            .env_remove("LORE_KEY")
            .env_remove("LORE_ADMIN_TOKEN")
            .output()
            .await
            .expect("the lore binary runs");
        Run {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn plain_output_is_path_lines_score_symbols_and_three_snippet_lines(pool: PgPool) {
    let env = Env::new(pool).await;

    let json = env.lore(&["search", "plan limits", "--json"]).await.json();
    let hits = json["hits"].as_array().unwrap();
    assert_eq!(hits[0]["path"], "src/billing/plans.rs");
    assert!(
        hits.iter()
            .any(|h| h["snippet"].as_str().unwrap().lines().count() > 3),
        "some snippet is longer than what is printed"
    );

    let out = env.lore(&["search", "plan limits"]).await;
    assert!(out.ok, "{}", out.stderr);
    assert!(out.stderr.is_empty(), "{}", out.stderr);
    assert!(out.stdout.ends_with('\n'));
    // Each hit, in order: `path:start-end  (score 0.032)  symbols`, then at most 3 snippet lines
    // indented by 4 spaces (a blank snippet line prints as an empty line).
    let mut lines = out.stdout.lines();
    for hit in hits {
        let symbols: Vec<&str> = hit["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        let mut header = format!(
            "{}:{}-{}  (score {:.3})",
            hit["path"].as_str().unwrap(),
            hit["start_line"],
            hit["end_line"],
            hit["score"].as_f64().unwrap()
        );
        if !symbols.is_empty() {
            header = format!("{header}  {}", symbols.join(", "));
        }
        assert_eq!(lines.next(), Some(header.as_str()));
        for want in hit["snippet"].as_str().unwrap().lines().take(3) {
            assert_eq!(lines.next(), Some(format!("    {want}").trim_end()));
        }
    }
    assert_eq!(lines.next(), None);
}

#[sqlx::test(migrations = "./migrations")]
async fn json_output_is_the_servers_reply_on_one_line(pool: PgPool) {
    let env = Env::new(pool).await;

    let out = env.lore(&["search", "plan limits", "--json"]).await;
    assert!(out.ok, "{}", out.stderr);
    assert_eq!(out.stdout.trim().lines().count(), 1, "{}", out.stdout);
    assert!(
        out.stdout.starts_with("{\"hits\":[{"),
        "compact JSON: {}",
        out.stdout
    );
    let json = out.json();
    let hits = json["hits"].as_array().unwrap();
    assert!(hits.len() > 1);
    // The full hit, not the plain output's subset.
    for field in [
        "chunk_id",
        "document_id",
        "source",
        "kind",
        "title",
        "uri",
        "path",
        "start_line",
        "end_line",
        "symbols",
        "snippet",
        "score",
    ] {
        assert!(hits[0].get(field).is_some(), "missing {field}: {}", hits[0]);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn flags_become_the_query_parameters(pool: PgPool) {
    let env = Env::new(pool).await;

    let json = env
        .lore(&["search", "plan limits", "--k", "2", "--json"])
        .await
        .json();
    assert_eq!(json["hits"].as_array().unwrap().len(), 2);

    let json = env
        .lore(&["search", "pricing plans", "--kind", "doc", "--json"])
        .await
        .json();
    assert_eq!(json["hits"][0]["path"], "docs/pricing.md");
    assert!(
        json["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["kind"] == "doc")
    );

    let json = env
        .lore(&[
            "search",
            "plan limits",
            "--kind",
            "code,doc",
            "--source",
            "sample",
            "--json",
        ])
        .await
        .json();
    let kinds: Vec<&str> = json["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"code") && kinds.contains(&"doc"),
        "{kinds:?}"
    );

    // A query with spaces, quotes and an ampersand reaches the server as one q.
    let out = env
        .lore(&["search", "plan & \"limits\" 100%", "--json"])
        .await;
    assert_eq!(out.json()["hits"][0]["path"], "src/billing/plans.rs");
}

#[sqlx::test(migrations = "./migrations")]
async fn no_hits_is_a_note_on_stderr_and_still_success(pool: PgPool) {
    let env = Env::new(pool).await;

    let out = env
        .lore(&["search", "plan limits", "--source", "nope"])
        .await;
    assert!(out.ok, "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr.trim(), "no results");

    let out = env
        .lore(&["search", "plan limits", "--source", "nope", "--json"])
        .await;
    assert_eq!(out.json()["hits"], serde_json::json!([]));
}

#[sqlx::test(migrations = "./migrations")]
async fn errors_are_printed_without_the_key(pool: PgPool) {
    let env = Env::new(pool).await;

    // The server's 400, with its message.
    let out = env.lore(&["search", "plan", "--kind", "docs"]).await;
    assert!(!out.ok);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(out.stderr.contains("server returned 400"), "{}", out.stderr);
    assert!(out.stderr.contains("unknown kind"), "{}", out.stderr);

    let out = env.lore(&["search", "   "]).await;
    assert!(!out.ok);
    assert!(out.stderr.contains("q is required"), "{}", out.stderr);

    // A key the server does not know.
    let bad = "lk_0000000000000000000000000000000000000000";
    let out = env.lore_with(Some(bad), &["search", "plan"]).await;
    assert!(!out.ok);
    assert!(out.stderr.contains("server returned 401"), "{}", out.stderr);
    assert!(!out.stderr.contains(bad), "the key was printed");

    // No key at all: the CLI says what is missing without calling the server.
    let out = env.lore_with(None, &["search", "plan"]).await;
    assert!(!out.ok);
    assert!(out.stderr.contains("--key"), "{}", out.stderr);
}
