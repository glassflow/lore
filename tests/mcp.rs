mod common;

use std::sync::Arc;

use common::{create_tenant_key, push_fixture, serve_router, test_router, test_state};
use lore::config::Config;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use sqlx::PgPool;

const PLANS: &str = "src/billing/plans.rs";

type Client = RunningService<RoleClient, ClientConfig>;

/// A running lore with the fixture indexed for the tenant `acme`. Returns the base URL, a key
/// with scopes `ingest` and `read`, and the router (for making more tenants).
async fn start(pool: PgPool) -> (String, String, axum::Router) {
    let data = tempfile::tempdir().unwrap();
    let router = test_router(test_state(pool, data.path()));
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    push_fixture(&router, &key).await;
    let url = serve_router(router.clone()).await;
    // The data dir only matters while ingesting, which has finished.
    drop(data);
    (url, key, router)
}

async fn connect(url: &str, key: &str) -> Client {
    // The transport adds the `Bearer ` prefix itself.
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{url}/mcp")).auth_header(key),
    );
    ClientConfig::default()
        .serve(transport)
        .await
        .expect("initialize against /mcp")
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("tool arguments must be a JSON object");
    };
    client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .unwrap_or_else(|e| panic!("calling {tool}: {e}"))
}

fn texts(res: &CallToolResult) -> Vec<String> {
    res.content
        .iter()
        .map(|c| c.as_text().expect("text content").text.clone())
        .collect()
}

fn is_error(res: &CallToolResult) -> bool {
    res.is_error == Some(true)
}

/// One hit of a `search` result, from its header `[n] <source> <place> (<about>) id=<uuid>` and
/// the excerpt lines under it.
#[derive(Debug)]
struct TextHit {
    source: String,
    place: String,
    kind: String,
    id: String,
    lines: Vec<String>,
}

/// The hits in the single text item of a `search` result; `No hits.` is none.
fn search_hits(res: &CallToolResult) -> Vec<TextHit> {
    assert!(!is_error(res), "{:?}", texts(res));
    let texts = texts(res);
    assert_eq!(texts.len(), 1);
    if texts[0] == "No hits." {
        return vec![];
    }
    let mut hits: Vec<TextHit> = Vec::new();
    let mut previous_blank = true;
    for line in texts[0].lines() {
        let header = format!("[{}] ", hits.len() + 1);
        if previous_blank && line.starts_with(&header) {
            let (rest, id) = line[header.len()..].rsplit_once(" id=").expect("an id");
            let (source, rest) = rest.split_once(' ').expect("a source");
            let (place, about) = rest.rsplit_once(" (").expect("an (about)");
            let kind = about.trim_end_matches(')').split(':').next().unwrap();
            hits.push(TextHit {
                source: source.to_string(),
                place: place.to_string(),
                kind: kind.to_string(),
                id: id.to_string(),
                lines: vec![],
            });
        } else if let Some(hit) = hits.last_mut() {
            hit.lines.push(line.to_string());
        } else {
            panic!("text before the first hit: {}", texts[0]);
        }
        previous_blank = line.is_empty();
    }
    hits
}

/// The JSON array in the single text item of a `list_sources` result.
fn json_items(res: &CallToolResult) -> Vec<Value> {
    assert!(!is_error(res), "{:?}", texts(res));
    let texts = texts(res);
    assert_eq!(texts.len(), 1);
    serde_json::from_str(&texts[0]).expect("a JSON array")
}

/// A raw `initialize` POST to `/mcp`, with the given extra headers.
async fn raw_initialize(url: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "raw", "version": "0" },
        },
    });
    let mut req = reqwest::Client::new()
        .post(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_string());
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    req.send().await.expect("request to /mcp")
}

#[sqlx::test(migrations = "./migrations")]
async fn lists_the_four_tools_and_describes_the_server(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["ask", "get_document", "list_sources", "search"]);

    let info = client.peer_info().expect("server info");
    let server = info.server_info.as_ref().expect("server name and version");
    assert_eq!(server.name, "lore");
    assert_eq!(server.version, lore::VERSION);
    assert_eq!(
        info.instructions.as_deref(),
        Some(
            "lore indexes code, documents and agent sessions. Start with `search`: it is fast, runs no language model, and returns the matching lines with their paths and line numbers. `get_document` reads one document in full. `ask` has lore's own model write an answer, which takes seconds to a minute: prefer `search` when you can read the code yourself. `list_sources` names what is indexed."
        )
    );
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn search_finds_the_plans_file_and_get_document_reads_it(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    let res = call(&client, "search", json!({ "query": "plan limits" })).await;
    let hits = search_hits(&res);
    assert!(!hits.is_empty());
    let hit = hits
        .iter()
        .find(|h| h.place.starts_with(&format!("{PLANS}:")))
        .unwrap_or_else(|| panic!("no hit for {PLANS}: {hits:?}"));
    assert_eq!(hit.kind, "code");
    assert_eq!(hit.source, "sample");
    // `path:start-end`, and the excerpt is numbered from `start`.
    let (start, end) = hit.place[PLANS.len() + 1..].split_once('-').unwrap();
    let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
    assert!(start >= 1 && end >= start, "{}", hit.place);
    assert!(!hit.lines.is_empty() && hit.lines.len() <= 15, "{hit:?}");
    assert!(hit.lines[0].starts_with(&format!("{start}: ")), "{hit:?}");

    let res = call(&client, "get_document", json!({ "document_id": hit.id })).await;
    assert!(!is_error(&res), "{:?}", texts(&res));
    let texts = texts(&res);
    assert_eq!(texts.len(), 1);
    let doc = &texts[0];
    assert!(doc.starts_with("# "), "{doc}");
    assert!(doc.contains(&format!("\ncode · {PLANS}\n\n")), "{doc}");
    assert!(doc.contains("enum Plan"), "{doc}");
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn get_document_returns_a_note_exactly_as_it_was_sent(pool: PgPool) {
    let (url, key, router) = start(pool).await;
    // CRLF, blank lines and a trailing newline are lost when chunks are joined.
    let sent = "first\r\n\r\n\r\nsecond line\n\n";
    let body = json!({
        "source": "notes",
        "documents": [{ "external_id": "n1", "title": "Exact", "text": sent }]
    });
    let (status, reply) = common::send(
        &router,
        "POST",
        "/v1/documents",
        &key,
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{reply}");

    let client = connect(&url, &key).await;
    let hits = search_hits(&call(&client, "search", json!({ "query": "second line" })).await);
    let id = hits.iter().find(|h| h.place == "Exact").unwrap().id.clone();
    let res = call(&client, "get_document", json!({ "document_id": id })).await;
    assert!(!is_error(&res), "{:?}", texts(&res));
    let doc = &texts(&res)[0];
    assert!(doc.ends_with(&format!("\n\n{sent}")), "{doc:?}");
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn search_clamps_k_and_filters_by_kind(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    let one = search_hits(&call(&client, "search", json!({ "query": "plan", "k": 0 })).await);
    assert_eq!(one.len(), 1, "k below 1 is raised to 1");
    let negative = search_hits(&call(&client, "search", json!({ "query": "plan", "k": -3 })).await);
    assert_eq!(negative.len(), 1);
    let two = search_hits(&call(&client, "search", json!({ "query": "plan", "k": 2 })).await);
    assert_eq!(two.len(), 2);
    let many = search_hits(&call(&client, "search", json!({ "query": "plan", "k": 1000 })).await);
    assert!(many.len() > 2 && many.len() <= 50, "{}", many.len());

    let docs = search_hits(
        &call(
            &client,
            "search",
            json!({ "query": "plan", "kind": ["doc"] }),
        )
        .await,
    );
    assert!(!docs.is_empty());
    assert!(docs.iter().all(|h| h.kind == "doc"), "{docs:?}");

    let sample = search_hits(
        &call(
            &client,
            "search",
            json!({ "query": "plan", "source": "sample" }),
        )
        .await,
    );
    assert!(!sample.is_empty());
    assert!(sample.iter().all(|h| h.source == "sample"), "{sample:?}");
    let elsewhere = call(
        &client,
        "search",
        json!({ "query": "plan", "source": "nope" }),
    )
    .await;
    assert_eq!(texts(&elsewhere), ["No hits."]);

    let bad = call(
        &client,
        "search",
        json!({ "query": "plan", "kind": ["nonsense"] }),
    )
    .await;
    assert!(is_error(&bad), "an unknown kind is a tool error");
    let blank = call(&client, "search", json!({ "query": "  " })).await;
    assert!(is_error(&blank), "a blank query is a tool error");
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn another_tenant_sees_nothing(pool: PgPool) {
    let (url, key, router) = start(pool).await;
    let client = connect(&url, &key).await;
    let hits = search_hits(&call(&client, "search", json!({ "query": "plan limits" })).await);
    let id = hits
        .iter()
        .find(|h| h.place.starts_with(PLANS))
        .unwrap()
        .id
        .clone();

    let (_, other_key) = create_tenant_key(&router, "globex", &["read"]).await;
    let other = connect(&url, &other_key).await;
    assert!(
        search_hits(&call(&other, "search", json!({ "query": "plan limits" })).await).is_empty()
    );
    assert!(json_items(&call(&other, "list_sources", json!({})).await).is_empty());

    let res = call(&other, "get_document", json!({ "document_id": id })).await;
    assert!(is_error(&res));
    assert_eq!(texts(&res), ["document not found"]);
    client.cancel().await.unwrap();
    other.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn get_document_errors_are_tool_errors(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    let unknown = call(
        &client,
        "get_document",
        json!({ "document_id": "00000000-0000-0000-0000-000000000001" }),
    )
    .await;
    assert!(is_error(&unknown));
    assert_eq!(texts(&unknown), ["document not found"]);

    let not_uuid = call(&client, "get_document", json!({ "document_id": "plans" })).await;
    assert!(is_error(&not_uuid));
    assert_eq!(texts(&not_uuid), ["document_id must be a UUID"]);
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn list_sources_has_name_kind_commit_and_count(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    let sources = json_items(&call(&client, "list_sources", json!({})).await);
    assert_eq!(sources.len(), 1);
    let s = &sources[0];
    assert_eq!(s["name"], "sample");
    assert_eq!(s["kind"], "repo");
    assert_eq!(s["indexed_commit"].as_str().unwrap().len(), 40);
    assert!(s["documents"].as_i64().unwrap() >= 4, "{s}");
    let mut keys: Vec<&String> = s.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["documents", "indexed_commit", "kind", "name"]);
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn ask_returns_the_answer_with_sources_and_a_footer(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    let res = call(
        &client,
        "ask",
        json!({ "question": "how many plans are there and what are their limits?" }),
    )
    .await;
    assert!(!is_error(&res), "{:?}", texts(&res));
    let texts = texts(&res);
    assert_eq!(texts.len(), 1, "one text item: no JSON copy of the answer");

    let (answer, rest) = texts[0]
        .split_once("\n\nSources:\n")
        .expect("a Sources: block");
    assert!(answer.starts_with("Fake answer from"), "{answer}");
    let (sources, footer) = rest.rsplit_once("\n\n").expect("a footer");
    assert!(footer.starts_with("(fast mode, "), "{footer}");
    assert!(footer.ends_with(" s)"), "{footer}");
    let lines: Vec<&str> = sources.lines().collect();
    assert!(lines[0].starts_with("[1] "), "{sources}");
    assert!(sources.contains(PLANS), "{sources}");
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn ask_deep_runs_the_agent_loop_and_a_blank_question_is_an_error(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    let res = call(
        &client,
        "ask",
        json!({ "question": "list every plan and its seat limit", "mode": "deep" }),
    )
    .await;
    assert!(!is_error(&res), "{:?}", texts(&res));
    let texts = texts(&res);
    assert_eq!(texts.len(), 1);
    let (_, sources) = texts[0].split_once("\n\nSources:\n").expect("sources");
    assert!(sources.contains(PLANS), "{}", texts[0]);
    let footer = texts[0].rsplit("\n\n").next().unwrap();
    let calls: u32 = footer
        .strip_prefix("(deep mode, ")
        .and_then(|f| f.split_once(" tool calls"))
        .map(|(n, _)| n.parse().unwrap())
        .unwrap_or_else(|| panic!("a deep footer: {footer}"));
    assert!(calls >= 1, "{footer}");

    let blank = call(&client, "ask", json!({ "question": " " })).await;
    assert!(is_error(&blank));
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn requests_without_a_good_read_key_are_refused_before_rmcp(pool: PgPool) {
    let (url, _, router) = start(pool).await;

    let res = raw_initialize(&url, &[]).await;
    assert_eq!(res.status(), 401);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body, json!({ "error": "unauthorized" }));

    let res = raw_initialize(&url, &[("authorization", "Bearer lk_nope")]).await;
    assert_eq!(res.status(), 401);

    // A key that cannot read gets 403 with the standard body.
    let (_, ingest_only) = create_tenant_key(&router, "writers", &["ingest"]).await;
    let res = raw_initialize(&url, &[("authorization", &format!("Bearer {ingest_only}"))]).await;
    assert_eq!(res.status(), 403);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body, json!({ "error": "missing scope read" }));

    // Other methods are refused too, not only POST.
    let res = reqwest::Client::new()
        .get(format!("{url}/mcp"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_host_header_is_unchecked_unless_allowed_hosts_are_set(pool: PgPool) {
    let (url, key, _) = start(pool.clone()).await;
    let auth = format!("Bearer {key}");

    // The default accepts any Host, as it is when lore runs behind a proxy or a Service.
    let res = raw_initialize(
        &url,
        &[("authorization", &auth), ("host", "lore.example.com")],
    )
    .await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());

    // With LORE_MCP_ALLOWED_HOSTS only those hosts pass.
    let data = tempfile::tempdir().unwrap();
    let mut state = test_state(pool, data.path());
    state.config = Arc::new(
        Config::from_lookup(|k| match k {
            "LORE_DATA_DIR" => Some(data.path().to_string_lossy().into_owned()),
            "LORE_ADMIN_TOKEN" => Some("t".to_string()),
            "LORE_MCP_ALLOWED_HOSTS" => Some("lore.example.com, other.example.com".to_string()),
            _ => None,
        })
        .unwrap(),
    );
    let url = serve_router(test_router(state)).await;
    let res = raw_initialize(
        &url,
        &[("authorization", &auth), ("host", "evil.example.org")],
    )
    .await;
    assert_eq!(res.status(), 403);
    let res = raw_initialize(
        &url,
        &[("authorization", &auth), ("host", "other.example.com")],
    )
    .await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn ask_in_sources_mode_lists_the_hits_and_names_no_model(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    let res = call(
        &client,
        "ask",
        json!({ "question": "how many plans are there?", "mode": "sources" }),
    )
    .await;
    assert!(!is_error(&res), "{:?}", texts(&res));
    let texts = texts(&res);
    assert_eq!(texts.len(), 1);
    let (answer, rest) = texts[0].split_once("\n\nSources:\n").expect("sources");
    assert!(answer.lines().all(|l| l.starts_with("- [")), "{answer}");
    assert!(answer.contains(PLANS), "{answer}");
    let footer = rest.rsplit("\n\n").next().unwrap();
    assert!(footer.starts_with("(sources mode, no model, "), "{footer}");
    client.cancel().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn ask_refuses_a_question_over_2000_characters_in_every_mode(pool: PgPool) {
    let (url, key, _) = start(pool).await;
    let client = connect(&url, &key).await;

    for mode in ["fast", "sources", "deep"] {
        let long = call(
            &client,
            "ask",
            json!({ "question": "\u{e9}".repeat(2001), "mode": mode }),
        )
        .await;
        assert!(is_error(&long), "{mode}");
        let text = texts(&long).join("\n");
        assert!(
            text.contains("question must be at most 2000 characters"),
            "{text}"
        );
    }
    let ok = call(
        &client,
        "ask",
        json!({ "question": "\u{e9}".repeat(2000), "mode": "deep" }),
    )
    .await;
    assert!(!is_error(&ok), "{:?}", texts(&ok));
    client.cancel().await.unwrap();
}
