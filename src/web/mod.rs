//! The built-in web pages, each one self-contained HTML file. No key is needed to load them: a
//! page asks for a key or the admin token and sends it to the API itself.
//!
//! - `GET /`: ask questions.
//! - `GET /admin`: see and manage tenants, sources, keys and pushes.

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

const INDEX_HTML: &str = include_str!("index.html");
const ADMIN_HTML: &str = include_str!("admin.html");

/// The admin page talks to this server only and is never framed. Inline script and style are
/// all it has; the policy keeps it from loading or sending anything anywhere else.
pub const ADMIN_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; \
    style-src 'unsafe-inline'; connect-src 'self'; img-src data:; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

fn html() -> (header::HeaderName, HeaderValue) {
    (
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    )
}

pub async fn index() -> Response {
    ([html()], INDEX_HTML).into_response()
}

pub async fn admin() -> Response {
    (
        [
            html(),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(ADMIN_CSP),
            ),
            (
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            ),
        ],
        ADMIN_HTML,
    )
        .into_response()
}
