mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{test_router, test_state};
use sqlx::PgPool;
use tower::ServiceExt;

/// The page, as the server embeds it.
const PAGE: &str = include_str!("../src/web/index.html");
/// The admin page, as the server embeds it.
const ADMIN: &str = include_str!("../src/web/admin.html");

#[sqlx::test(migrations = "./migrations")]
async fn root_serves_the_page_without_a_key(pool: PgPool) {
    let app = test_router(test_state(pool, &std::env::temp_dir()));
    let res = app
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains(r#"id="ask-form""#));
    assert_eq!(body, PAGE);
}

#[test]
fn page_has_the_form_and_its_wiring() {
    for needle in [
        r#"<form id="ask-form""#,
        r#"<input type="password" id="key""#,
        r#"<textarea id="question""#,
        r#"name="mode" value="fast""#,
        r#"name="mode" value="sources""#,
        r#"name="mode" value="deep""#,
        r#"<button type="submit""#,
        "Ask your code, docs and agent sessions",
        "lore.key",
        "fetch('/v1/info')",
        "fetch('/v1/ask'",
        "'Authorization': 'Bearer ' + key",
        ".rel = 'noopener'",
        ".target = '_blank'",
        "Deep mode is not enabled on this server",
        "@media (prefers-color-scheme: dark)",
        r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#,
        "max-width: 860px",
        "font-size: 18px",
        r#"<script id="lore-pure">"#,
    ] {
        assert!(PAGE.contains(needle), "the page is missing {needle}");
    }
}

#[test]
fn pages_load_nothing_from_outside() {
    // No scheme-qualified URL appears anywhere, so nothing is fetched from a CDN or font host.
    for (name, page) in [("ask", PAGE), ("admin", ADMIN)] {
        assert!(!page.contains("http://"), "http:// URL in the {name} page");
        assert!(
            !page.contains("https://"),
            "https:// URL in the {name} page"
        );
        for needle in ["<link", "<script src", "src=\"//", "href=\"//", "@import"] {
            assert!(!page.contains(needle), "the {name} page has {needle}");
        }
    }
}

#[test]
fn pages_have_no_em_dashes() {
    assert!(!PAGE.contains('\u{2014}'));
    assert!(!ADMIN.contains('\u{2014}'));
}

/// Runs the page's pure functions under node (tests/web_pure.js). Skipped without node.
#[test]
fn pure_functions_pass_under_node() {
    let node = std::process::Command::new("node").arg("--version").output();
    if !node.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("node is not installed: skipping the page script checks");
        return;
    }
    let root = env!("CARGO_MANIFEST_DIR");
    let out = std::process::Command::new("node")
        .arg(format!("{root}/tests/web_pure.js"))
        .arg(format!("{root}/src/web/index.html"))
        .output()
        .expect("run node");
    assert!(
        out.status.success(),
        "node checks failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_serves_its_page_without_a_token(pool: PgPool) {
    let app = test_router(test_state(pool, &std::env::temp_dir()));
    let res = app
        .oneshot(Request::get("/admin").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers().clone();
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert_eq!(csp, lore::web::ADMIN_CSP);
    for part in [
        "default-src 'none'",
        "connect-src 'self'",
        "form-action 'none'",
        "frame-ancestors 'none'",
    ] {
        assert!(csp.contains(part), "the policy is missing {part}");
    }
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), ADMIN);
}

#[test]
fn admin_page_has_its_sections_and_wiring() {
    for needle in [
        r#"<form id="token-form""#,
        r#"<input type="password" id="token""#,
        r#"id="facts""#,
        r#"id="pushes""#,
        r#"id="tenants""#,
        r#"<script id="lore-admin-pure">"#,
        "var TOKEN_NAME = 'lore.admin'",
        "sessionStorage.setItem(TOKEN_NAME",
        "'Authorization': 'Bearer ' + token",
        "api('GET', '/v1/admin/overview')",
        "api('GET', '/v1/admin/pushes')",
        "api('POST', keysUrl(t.id)",
        "api('DELETE', revokeUrl(k.id))",
        "navigator.clipboard.writeText",
        "document.execCommand('copy')",
        "It is shown once",
        "Clients using it stop at once.",
        "@media (prefers-color-scheme: dark)",
        "@media (max-width: 640px)",
        r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#,
    ] {
        assert!(ADMIN.contains(needle), "the admin page is missing {needle}");
    }
    // The token never lands in local storage, and server text never becomes markup.
    assert!(!ADMIN.contains("localStorage"));
    assert!(!ADMIN.contains("innerHTML"));
}

/// Runs the admin page's pure functions under node (tests/web_admin_pure.js), in UTC so local
/// times are fixed. Skipped without node.
#[test]
fn admin_pure_functions_pass_under_node() {
    let node = std::process::Command::new("node").arg("--version").output();
    if !node.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("node is not installed: skipping the admin page script checks");
        return;
    }
    let root = env!("CARGO_MANIFEST_DIR");
    let out = std::process::Command::new("node")
        .env("TZ", "UTC")
        .arg(format!("{root}/tests/web_admin_pure.js"))
        .arg(format!("{root}/src/web/admin.html"))
        .output()
        .expect("run node");
    assert!(
        out.status.success(),
        "node checks failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
