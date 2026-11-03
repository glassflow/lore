mod common;

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{create_tenant_key, push_fixture, send, test_router, test_state_with};
use lore::answer::tools::{MAX_OUTPUT_CHARS, ToolOutput, run_tool, run_tool_scoped, tool_defs};
use lore::http::AppState;
use lore::sessions::process_due_sessions;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const CLAUDE_CODE: &str = include_str!("fixtures/otlp/claude_code_session.json");
const SESSION_ID: &str = "sess-fixture-1";
const TRUNCATED: &str = "\n…[output truncated]";

/// A tenant `acme` with the sample repo pushed (source `sample`) and the session fixture
/// ingested and processed.
struct Env {
    state: AppState,
    router: Router,
    tenant: Uuid,
    key: String,
    source_id: Uuid,
    data: tempfile::TempDir,
}

impl Env {
    async fn new(pool: PgPool) -> Env {
        let data = tempfile::tempdir().unwrap();
        let state = test_state_with(pool, data.path(), |cfg| cfg.session_quiet_secs = 0);
        let router = test_router(state.clone());
        let (tenant, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        let (source_id, _repo) = push_fixture(&router, &key).await;
        let req = Request::post("/v1/traces")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(CLAUDE_CODE.to_string()))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(process_due_sessions(&state, 20).await.unwrap(), 1);
        Env {
            state,
            router,
            tenant,
            key,
            source_id,
            data,
        }
    }

    async fn run(&self, name: &str, input: Value) -> ToolOutput {
        run_tool(&self.state, self.tenant, name, &input).await
    }

    /// `<data_dir>/repos/<source_id>/checkout`
    fn checkout(&self) -> PathBuf {
        self.data
            .path()
            .join("repos")
            .join(self.source_id.to_string())
            .join("checkout")
    }

    /// A second tenant that has pushed nothing.
    async fn other_tenant(&self) -> Uuid {
        create_tenant_key(&self.router, "globex", &["ingest", "read"])
            .await
            .0
    }
}

fn ok(out: ToolOutput) -> String {
    assert!(!out.is_error, "unexpected tool error: {}", out.text);
    out.text
}

fn err(out: ToolOutput) -> String {
    assert!(out.is_error, "expected a tool error, got: {}", out.text);
    out.text
}

// --- the ticket's checks ---------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn grep_finds_the_plan_enum(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = ok(env.run("grep", json!({ "pattern": "enum Plan" })).await);
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("sample/src/billing/plans.rs:"), "{text}");
    assert!(first.contains("pub enum Plan"), "{text}");
    assert!(text.ends_with("(1 matches)"), "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn grep_path_glob_keeps_only_matching_paths(pool: PgPool) {
    let env = Env::new(pool).await;
    // The ticket's check: nothing but docs/ paths. ("Plan" has no capital-P match in docs.)
    let text = ok(env
        .run("grep", json!({ "pattern": "Plan", "path_glob": "docs/**" }))
        .await);
    for line in text.lines().filter(|l| !l.starts_with('(')) {
        assert!(line.starts_with("sample/docs/"), "{text}");
    }
    assert_eq!(text, "(0 matches)");

    // The same glob finds a word that is in docs/pricing.md, and only there.
    let text = ok(env
        .run(
            "grep",
            json!({ "pattern": "seats", "path_glob": "docs/**" }),
        )
        .await);
    assert!(text.starts_with("sample/docs/pricing.md:"), "{text}");
    for line in text.lines().filter(|l| !l.starts_with('(')) {
        assert!(line.starts_with("sample/docs/"), "{text}");
    }
    // Without the glob it also finds the Rust source.
    let all = ok(env.run("grep", json!({ "pattern": "seats" })).await);
    assert!(all.contains("sample/src/billing/plans.rs:"), "{all}");

    // A glob without a directory matches by extension at any depth.
    let rs = ok(env
        .run("grep", json!({ "pattern": "seats", "path_glob": "*.rs" }))
        .await);
    assert!(
        rs.lines()
            .filter(|l| !l.starts_with('('))
            .all(|l| l.contains(".rs:")),
        "{rs}"
    );
    assert!(rs.contains("plans.rs"), "{rs}");
}

#[sqlx::test(migrations = "./migrations")]
async fn grep_with_a_bad_regex_or_glob_is_an_error(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = err(env.run("grep", json!({ "pattern": "(" })).await);
    assert!(text.contains("regex"), "{text}");
    assert!(text.contains("unclosed group"), "{text}");

    let text = err(env
        .run("grep", json!({ "pattern": "x", "path_glob": "[" }))
        .await);
    assert!(text.contains("path_glob"), "{text}");

    // A regex that compiles to more than the 1 MiB size limit.
    let text = err(env
        .run("grep", json!({ "pattern": "(\\w{100}){100}" }))
        .await);
    assert!(text.contains("regex"), "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn read_file_returns_a_header_and_numbered_lines(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = ok(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "src/billing/plans.rs",
                    "start_line": 1, "end_line": 5 }),
        )
        .await);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("sample/src/billing/plans.rs (lines 1-5 of "),
        "{text}"
    );
    assert!(lines[1].starts_with("1: "), "{text}");
    assert_eq!(
        lines[1],
        "1: /// Seat and span limits for each pricing plan."
    );
    assert!(lines[5].starts_with("5: "), "{text}");
    assert_eq!(lines.len(), 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn read_file_rejects_paths_that_leave_the_checkout(pool: PgPool) {
    let env = Env::new(pool).await;
    for path in [
        "../../etc/passwd",
        "src/../../../etc/passwd",
        "src/billing/../../../x",
        "/etc/passwd",
        "..",
        "",
    ] {
        let text = err(env
            .run("read_file", json!({ "source": "sample", "path": path }))
            .await);
        assert!(text.contains("path"), "{path}: {text}");
    }
    let text = err(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "no/such.rs" }),
        )
        .await);
    assert_eq!(text, "file not found");
    let text = err(env
        .run("read_file", json!({ "source": "sample", "path": "src" }))
        .await);
    assert_eq!(text, "not a regular file");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_tenant_without_repos_gets_zero_matches(pool: PgPool) {
    let env = Env::new(pool).await;
    let other = env.other_tenant().await;
    let out = run_tool(&env.state, other, "grep", &json!({ "pattern": "Plan" })).await;
    assert_eq!(ok(out), "(0 matches)");
}

#[sqlx::test(migrations = "./migrations")]
async fn get_session_returns_the_session_text(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = ok(env
        .run("get_session", json!({ "session_id": SESSION_ID }))
        .await);
    assert!(text.contains("How many plans"), "{text}");
    assert!(
        env.run("get_session", json!({ "session_id": "nope" }))
            .await
            .is_error
    );
    let text = err(env
        .run("get_session", json!({ "session_id": "nope" }))
        .await);
    assert_eq!(text, "session not found");
}

#[sqlx::test(migrations = "./migrations")]
async fn search_renders_a_session_hit(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = ok(env
        .run(
            "search",
            json!({ "query": "how many plans", "kind": ["session"] }),
        )
        .await);
    assert!(
        text.starts_with(&format!("session:{SESSION_ID} (")),
        "{text}"
    );
    assert!(text.lines().count() > 1, "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn search_renders_path_hits_and_other_documents(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = ok(env
        .run("search", json!({ "query": "pricing plan seat limits" }))
        .await);
    let header = text
        .lines()
        .find(|l| l.starts_with("sample/src/billing/plans.rs:"))
        .unwrap_or_else(|| panic!("no plans.rs hit: {text}"));
    // `<source>/<path>:<start>-<end> (<kind>) <symbols>`
    let rest = header.strip_prefix("sample/src/billing/plans.rs:").unwrap();
    let (range, after) = rest.split_once(' ').unwrap();
    let (start, end) = range.split_once('-').unwrap();
    assert!(
        start.parse::<u32>().is_ok() && end.parse::<u32>().is_ok(),
        "{header}"
    );
    assert!(after.starts_with("(code)"), "{header}");

    // A document of the documents API has no path: `doc:<document_id> (<title>)`.
    let res = send(
        &env.router,
        "POST",
        "/v1/documents",
        &env.key,
        json!({
            "source": "notes",
            "documents": [{ "external_id": "n1", "title": "Review", "text": "The zorblax review moved." }]
        })
        .to_string()
        .into_bytes(),
    )
    .await;
    assert_eq!(res.0, StatusCode::OK, "{}", res.1);
    let text = ok(env.run("search", json!({ "query": "zorblax" })).await);
    let id: String = sqlx::query_scalar(
        "SELECT id::text FROM documents WHERE tenant_id = $1 AND external_id = 'n1'",
    )
    .bind(env.tenant)
    .fetch_one(&env.state.pool)
    .await
    .unwrap();
    assert!(text.starts_with(&format!("doc:{id} (Review)")), "{text}");
    assert!(text.contains("The zorblax review moved."), "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn search_k_defaults_to_six_and_is_range_checked(pool: PgPool) {
    let env = Env::new(pool).await;
    // Many chunks match "the": count the hit headers.
    let headers = |text: &str| {
        text.lines()
            .filter(|l| {
                l.starts_with("sample/") || l.starts_with("session:") || l.starts_with("doc:")
            })
            .count()
    };
    let default = ok(env.run("search", json!({ "query": "plan seats" })).await);
    assert!(headers(&default) <= 6, "{default}");
    let one = ok(env
        .run("search", json!({ "query": "plan seats", "k": 1 }))
        .await);
    assert_eq!(headers(&one), 1, "{one}");
    ok(env
        .run("search", json!({ "query": "plan seats", "k": 10 }))
        .await);
    for k in [0, 11, -1] {
        let text = err(env.run("search", json!({ "query": "x", "k": k })).await);
        assert!(text.contains("k must be between 1 and 10"), "{text}");
    }
    // A bad kind is the search's own message.
    let text = err(env
        .run("search", json!({ "query": "x", "kind": ["bogus"] }))
        .await);
    assert!(text.contains("bogus"), "{text}");
    // Nothing found.
    let text = ok(env
        .run("search", json!({ "query": "x", "kind": ["note"] }))
        .await);
    assert_eq!(text, "(no results)");
}

// --- more ------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn grep_stops_at_max_results_and_says_so(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = ok(env
        .run("grep", json!({ "pattern": ".", "max_results": 3 }))
        .await);
    assert_eq!(text.lines().count(), 4, "{text}");
    assert!(text.ends_with("(stopped at 3 matches)"), "{text}");

    // Exactly as many matches as the limit is not a stop.
    let all = ok(env
        .run("grep", json!({ "pattern": ".", "max_results": 200 }))
        .await);
    let total: usize = all
        .lines()
        .last()
        .unwrap()
        .trim_start_matches('(')
        .trim_end_matches(" matches)")
        .parse()
        .unwrap_or_else(|_| {
            panic!("the fixture has more than 200 lines or the output changed: {all}")
        });
    let exact = ok(env
        .run("grep", json!({ "pattern": ".", "max_results": total }))
        .await);
    assert_eq!(exact, all);
    let one_less = ok(env
        .run("grep", json!({ "pattern": ".", "max_results": total - 1 }))
        .await);
    assert!(
        one_less.ends_with(&format!("(stopped at {} matches)", total - 1)),
        "{one_less}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn grep_output_is_in_a_stable_order_with_the_source_name(pool: PgPool) {
    let env = Env::new(pool).await;
    let a = ok(env.run("grep", json!({ "pattern": "." })).await);
    let b = ok(env.run("grep", json!({ "pattern": "." })).await);
    assert_eq!(a, b);
    let paths: Vec<&str> = a
        .lines()
        .filter(|l| !l.starts_with('('))
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert!(paths.iter().all(|p| p.starts_with("sample/")), "{a}");
    // Entries of a directory are visited by name, so equal paths are adjacent and the
    // top-level files come out in name order.
    let top: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| p.matches('/').count() == 1)
        .collect();
    let mut sorted = top.clone();
    sorted.sort();
    assert_eq!(top, sorted);
}

#[sqlx::test(migrations = "./migrations")]
async fn grep_by_source_name_and_unknown_source(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = ok(env
        .run(
            "grep",
            json!({ "pattern": "enum Plan", "source": "sample" }),
        )
        .await);
    assert!(text.starts_with("sample/src/billing/plans.rs:"), "{text}");
    let text = err(env
        .run("grep", json!({ "pattern": "x", "source": "nope" }))
        .await);
    assert_eq!(text, "unknown source");
    let text = err(env
        .run(
            "read_file",
            json!({ "source": "nope", "path": "src/billing/plans.rs" }),
        )
        .await);
    assert_eq!(text, "unknown source");
}

#[sqlx::test(migrations = "./migrations")]
async fn grep_includes_hidden_files_and_ignores_dot_git_and_gitignore(pool: PgPool) {
    let env = Env::new(pool).await;
    let root = env.checkout();
    std::fs::create_dir_all(root.join(".github")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".github/ci.yml"), "marker-hidden: yes\n").unwrap();
    std::fs::write(root.join(".git/config"), "marker-hidden in git\n").unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "marker-hidden ignored\n").unwrap();

    let text = ok(env.run("grep", json!({ "pattern": "marker-hidden" })).await);
    assert_eq!(
        text,
        "sample/.github/ci.yml:1: marker-hidden: yes\n\
         sample/ignored.txt:1: marker-hidden ignored\n\
         (2 matches)"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn grep_skips_binary_and_oversized_files_and_cuts_long_lines(pool: PgPool) {
    let env = Env::new(pool).await;
    let root = env.checkout();
    let mut binary = b"marker-skip\n".to_vec();
    binary.push(0);
    std::fs::write(root.join("blob.bin"), binary).unwrap();
    let big = format!("marker-skip\n{}", "z".repeat(256 * 1024));
    std::fs::write(root.join("big.txt"), big).unwrap();
    std::fs::write(
        root.join("wide.txt"),
        format!("marker-skip {}\n", "w".repeat(1000)),
    )
    .unwrap();
    // A NUL after the first 8 KiB does not make a file binary.
    let late_nul = format!("marker-skip\n{}\0\n", "n".repeat(9000));
    std::fs::write(root.join("late.txt"), late_nul).unwrap();

    let text = ok(env.run("grep", json!({ "pattern": "marker-skip" })).await);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(
        lines[0].starts_with("sample/late.txt:1: marker-skip"),
        "{text}"
    );
    assert_eq!(
        lines[1].chars().count(),
        "sample/wide.txt:1: ".len() + 200,
        "{text}"
    );
    assert_eq!(lines[2], "(2 matches)");
}

#[cfg(unix)]
#[sqlx::test(migrations = "./migrations")]
async fn symlinks_pointing_out_of_the_checkout_are_not_followed(pool: PgPool) {
    let env = Env::new(pool).await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "marker-secret\n").unwrap();
    let root = env.checkout();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("linkdir")).unwrap();

    let text = ok(env.run("grep", json!({ "pattern": "marker-secret" })).await);
    assert_eq!(text, "(0 matches)");

    for path in ["link.txt", "linkdir/secret.txt"] {
        let text = err(env
            .run("read_file", json!({ "source": "sample", "path": path }))
            .await);
        assert!(!text.contains("marker-secret"), "{text}");
        assert!(text.contains("outside"), "{path}: {text}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn read_file_clamps_to_400_lines_and_defaults_to_200(pool: PgPool) {
    let env = Env::new(pool).await;
    let long: String = (1..=1000).map(|i| format!("row {i}\n")).collect();
    std::fs::write(env.checkout().join("long.txt"), long).unwrap();
    let read = |input: Value| async { ok(env.run("read_file", input).await) };

    let text = read(json!({ "source": "sample", "path": "long.txt" })).await;
    assert!(
        text.starts_with("sample/long.txt (lines 1-200 of 1000)\n1: row 1\n"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 201);

    let text = read(json!({ "source": "sample", "path": "long.txt", "start_line": 10 })).await;
    assert!(
        text.starts_with("sample/long.txt (lines 10-209 of 1000)\n10: row 10\n"),
        "{text}"
    );

    let text =
        read(json!({ "source": "sample", "path": "long.txt", "start_line": 1, "end_line": 1000 }))
            .await;
    assert!(
        text.starts_with("sample/long.txt (lines 1-400 of 1000)\n"),
        "{text}"
    );
    assert!(text.ends_with("\n400: row 400"), "{text}");

    let text = read(
        json!({ "source": "sample", "path": "long.txt", "start_line": 990, "end_line": 1100 }),
    )
    .await;
    assert!(
        text.starts_with("sample/long.txt (lines 990-1000 of 1000)\n"),
        "{text}"
    );
    assert!(text.ends_with("\n1000: row 1000"), "{text}");

    let text = err(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "long.txt", "start_line": 1001 }),
        )
        .await);
    assert!(text.contains("past the end"), "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn read_file_refuses_binary_files_and_a_source_without_a_checkout(pool: PgPool) {
    let env = Env::new(pool).await;
    std::fs::write(env.checkout().join("blob.bin"), [0u8, 1, 2]).unwrap();
    let text = err(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "blob.bin" }),
        )
        .await);
    assert_eq!(text, "binary file");

    // A repo source that was created but never pushed has no checkout.
    let (status, body) = common::create_repo(&env.router, &env.key, "empty").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        ok(env
            .run("grep", json!({ "pattern": "x", "source": "empty" }))
            .await),
        "(0 matches)"
    );
    let text = err(env
        .run("read_file", json!({ "source": "empty", "path": "a.rs" }))
        .await);
    assert_eq!(text, "checkout not available");
}

#[sqlx::test(migrations = "./migrations")]
async fn output_is_cut_at_20000_chars_with_a_suffix(pool: PgPool) {
    let env = Env::new(pool).await;
    // 2000 lines of 101 bytes is about 200 KB (under the 256 KiB file limit); 200 matches of
    // about 120 characters are over 20,000.
    let line = format!("capme {}\n", "e".repeat(94));
    std::fs::write(env.checkout().join("many.txt"), line.repeat(2000)).unwrap();

    let text = ok(env
        .run("grep", json!({ "pattern": "capme", "max_results": 200 }))
        .await);
    assert!(text.ends_with(TRUNCATED), "{} chars", text.chars().count());
    assert_eq!(
        text.chars().count(),
        MAX_OUTPUT_CHARS + TRUNCATED.chars().count()
    );

    // read_file of 400 long lines is cut the same way.
    let wide: String = (1..=400)
        .map(|_| format!("{}\n", "ü".repeat(100)))
        .collect();
    std::fs::write(env.checkout().join("wide.txt"), wide).unwrap();
    let text = ok(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "wide.txt", "end_line": 400 }),
        )
        .await);
    assert!(text.ends_with(TRUNCATED));
    assert_eq!(
        text.chars().count(),
        MAX_OUTPUT_CHARS + TRUNCATED.chars().count()
    );

    // Error text is capped too: a regex error echoes the pattern.
    let out = env
        .run(
            "grep",
            json!({ "pattern": format!("({}", "a".repeat(30_000)) }),
        )
        .await;
    assert!(out.is_error);
    assert!(out.text.chars().count() <= MAX_OUTPUT_CHARS + TRUNCATED.chars().count());
}

#[sqlx::test(migrations = "./migrations")]
async fn get_session_text_is_cut_at_20000_chars(pool: PgPool) {
    let env = Env::new(pool).await;
    // Make the stored session text long: a body on the session document is returned as is.
    let long = "line of the session\n".repeat(2000);
    sqlx::query("UPDATE documents SET body = $2 WHERE tenant_id = $1 AND kind = 'session'")
        .bind(env.tenant)
        .bind(&long)
        .execute(&env.state.pool)
        .await
        .unwrap();
    let text = ok(env
        .run("get_session", json!({ "session_id": SESSION_ID }))
        .await);
    assert!(text.ends_with(TRUNCATED));
    assert_eq!(
        text.chars().count(),
        MAX_OUTPUT_CHARS + TRUNCATED.chars().count()
    );
    assert!(text.starts_with("line of the session\n"));
}

#[sqlx::test(migrations = "./migrations")]
async fn scoped_tools_see_only_the_named_sources(pool: PgPool) {
    let env = Env::new(pool).await;
    let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let run = |scope: Vec<String>, name: &'static str, input: Value| {
        let state = env.state.clone();
        let tenant = env.tenant;
        async move { run_tool_scoped(&state, tenant, Some(&scope), name, &input).await }
    };

    // Outside the scope, the repo is an unknown source and its sessions are not found.
    let elsewhere = names(&["elsewhere"]);
    let text = err(run(
        elsewhere.clone(),
        "read_file",
        json!({ "source": "sample", "path": "src/billing/plans.rs" }),
    )
    .await);
    assert_eq!(text, "unknown source");
    let text = err(run(
        elsewhere.clone(),
        "grep",
        json!({ "pattern": "Plan", "source": "sample" }),
    )
    .await);
    assert_eq!(text, "unknown source");
    let text = ok(run(elsewhere.clone(), "grep", json!({ "pattern": "Plan" })).await);
    assert_eq!(text, "(0 matches)");
    let text = ok(run(
        elsewhere.clone(),
        "search",
        json!({ "query": "how many plans" }),
    )
    .await);
    assert_eq!(text, "(no results)");
    let text = err(run(
        elsewhere,
        "get_session",
        json!({ "session_id": SESSION_ID }),
    )
    .await);
    assert_eq!(text, "session not found");

    // Scoped to the repo: its files, but not the session, which lives in the `otlp` source.
    let repo = names(&["sample"]);
    let text = ok(run(repo.clone(), "grep", json!({ "pattern": "enum Plan" })).await);
    assert!(text.starts_with("sample/src/billing/plans.rs:"), "{text}");
    let text = err(run(repo, "get_session", json!({ "session_id": SESSION_ID })).await);
    assert_eq!(text, "session not found");
    let text = ok(run(
        names(&["sample", "otlp"]),
        "get_session",
        json!({ "session_id": SESSION_ID }),
    )
    .await);
    assert!(!text.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn tools_never_cross_tenants(pool: PgPool) {
    let env = Env::new(pool).await;
    let other = env.other_tenant().await;
    let as_other = |name: &'static str, input: Value| {
        let state = env.state.clone();
        async move { run_tool(&state, other, name, &input).await }
    };

    // The other tenant has no source `sample` and no session.
    let text = err(as_other(
        "read_file",
        json!({ "source": "sample", "path": "src/billing/plans.rs" }),
    )
    .await);
    assert_eq!(text, "unknown source");
    let text = err(as_other("grep", json!({ "pattern": "Plan", "source": "sample" })).await);
    assert_eq!(text, "unknown source");
    let text = err(as_other("get_session", json!({ "session_id": SESSION_ID })).await);
    assert_eq!(text, "session not found");
    let text = ok(as_other("search", json!({ "query": "how many plans" })).await);
    assert_eq!(text, "(no results)");

    // With its own source of the same name, the other tenant sees its own checkout only.
    let key = create_tenant_key(&env.router, "initech", &["ingest", "read"]).await;
    let (tenant_b, key_b) = key;
    let (source_b, _repo) = push_fixture(&env.router, &key_b).await;
    assert_ne!(source_b, env.source_id);
    std::fs::write(env.checkout().join("only-in-a.txt"), "marker-tenant-a\n").unwrap();
    let a = ok(env
        .run("grep", json!({ "pattern": "marker-tenant-a" }))
        .await);
    assert!(a.starts_with("sample/only-in-a.txt:1:"), "{a}");
    let b = ok(run_tool(
        &env.state,
        tenant_b,
        "grep",
        &json!({ "pattern": "marker-tenant-a" }),
    )
    .await);
    assert_eq!(b, "(0 matches)");
    let b = err(run_tool(
        &env.state,
        tenant_b,
        "read_file",
        &json!({ "source": "sample", "path": "only-in-a.txt" }),
    )
    .await);
    assert_eq!(b, "file not found");
}

#[sqlx::test(migrations = "./migrations")]
async fn bad_tools_and_bad_input_are_short_errors(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = err(env.run("rm_rf", json!({})).await);
    assert!(text.starts_with("unknown tool"), "{text}");

    let bad: Vec<(&str, Value)> = vec![
        ("search", json!({})),
        ("search", json!({ "query": 5 })),
        ("search", json!({ "query": "x", "k": 1.5 })),
        ("search", json!({ "query": "x", "k": "3" })),
        ("search", json!({ "query": "x", "kind": "code" })),
        ("search", json!({ "query": "x", "extra": 1 })),
        ("grep", json!({})),
        ("grep", json!({ "pattern": 1 })),
        ("grep", json!({ "pattern": "x", "max_results": 0 })),
        ("grep", json!({ "pattern": "x", "max_results": 201 })),
        ("grep", json!({ "pattern": "x", "max_results": 2.5 })),
        ("grep", json!({ "pattern": "x", "source": 1 })),
        ("grep", json!({ "pattern": "x", "path_glob": ["a"] })),
        ("grep", json!({ "pattern": "x", "unknown": true })),
        ("read_file", json!({})),
        ("read_file", json!({ "source": "sample" })),
        ("read_file", json!({ "path": "a" })),
        ("read_file", json!({ "source": "sample", "path": 3 })),
        (
            "read_file",
            json!({ "source": "sample", "path": "a", "start_line": 0 }),
        ),
        (
            "read_file",
            json!({ "source": "sample", "path": "a", "start_line": -4 }),
        ),
        (
            "read_file",
            json!({ "source": "sample", "path": "a", "start_line": "1" }),
        ),
        (
            "read_file",
            json!({ "source": "sample", "path": "a", "end_line": 0 }),
        ),
        (
            "read_file",
            json!({ "source": "sample", "path": "a", "start_line": 5, "end_line": 4 }),
        ),
        (
            "read_file",
            json!({ "source": "sample", "path": "a", "extra": null }),
        ),
        ("get_session", json!({})),
        ("get_session", json!({ "session_id": 7 })),
        ("get_session", json!({ "session_id": "a", "x": 1 })),
        ("get_session", Value::Null),
        ("grep", json!([1, 2])),
        ("grep", json!("x")),
    ];
    for (name, input) in bad {
        let text = err(env.run(name, input.clone()).await);
        assert!(
            text.chars().count() < 300,
            "{name} {input}: error is not short: {text}"
        );
        assert!(text.starts_with("invalid input"), "{name} {input}: {text}");
    }
    // Huge numbers do not panic either.
    err(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "README.md", "start_line": u64::MAX }),
        )
        .await);
    let text = ok(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "README.md", "start_line": 1,
                    "end_line": i64::MAX }),
        )
        .await);
    assert!(text.starts_with("sample/README.md (lines 1-"), "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn tool_defs_match_what_run_tool_accepts(pool: PgPool) {
    let env = Env::new(pool).await;
    for def in tool_defs() {
        let schema = &def.input_schema;
        assert_eq!(schema["additionalProperties"], false, "{}", def.name);
        // An input with a property the schema does not list is refused by name.
        let text = err(env
            .run(&def.name, json!({ "not_in_the_schema": true }))
            .await);
        assert!(text.starts_with("invalid input"), "{}: {text}", def.name);
        for required in schema["required"].as_array().unwrap() {
            let name = required.as_str().unwrap();
            assert!(schema["properties"][name].is_object(), "{name}");
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn read_file_refuses_a_file_over_10_mib_and_not_one_at_the_limit(pool: PgPool) {
    let env = Env::new(pool).await;
    let limit = 10 * 1024 * 1024;
    // Sparse files: no real disk is used. Zero bytes would read as binary, so the file at the
    // limit is checked through the size error not firing, and the one above must fire it.
    let big = std::fs::File::create(env.checkout().join("big.txt")).unwrap();
    big.set_len(limit + 1).unwrap();
    let text = err(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "big.txt" }),
        )
        .await);
    assert_eq!(text, "file too large");

    let edge = std::fs::File::create(env.checkout().join("edge.txt")).unwrap();
    edge.set_len(limit).unwrap();
    let text = err(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "edge.txt" }),
        )
        .await);
    assert_eq!(
        text, "binary file",
        "a file at the limit is not 'too large'"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn read_file_strips_carriage_returns(pool: PgPool) {
    let env = Env::new(pool).await;
    std::fs::write(env.checkout().join("win.txt"), "one\r\ntwo\r\n").unwrap();
    let text = ok(env
        .run(
            "read_file",
            json!({ "source": "sample", "path": "win.txt" }),
        )
        .await);
    assert_eq!(text, "sample/win.txt (lines 1-2 of 2)\n1: one\n2: two");
    assert!(!text.contains('\r'), "{text:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_tool_name_is_cut_to_64_chars(pool: PgPool) {
    let env = Env::new(pool).await;
    let name = "x".repeat(200);
    let text = err(env.run(&name, json!({})).await);
    assert_eq!(text, format!("unknown tool: {}", "x".repeat(64)));
}

#[sqlx::test(migrations = "./migrations")]
async fn get_session_with_a_nul_in_the_id_is_session_not_found(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = err(env
        .run("get_session", json!({ "session_id": "abc\u{0}def" }))
        .await);
    assert_eq!(text, "session not found");
}
