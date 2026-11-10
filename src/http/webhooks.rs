//! The GitHub webhook: a `push` to a repository's default branch syncs every repo source whose
//! `remote_url` is that repository. GitHub signs each delivery with the shared secret
//! (`LORE_GITHUB_WEBHOOK_SECRET`); a delivery without a valid `X-Hub-Signature-256` is a 401.
//! The endpoint needs no API key: a valid delivery can only make the server fetch remotes that
//! tenants configured, never send it content.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::error::AppError;
use crate::http::AppState;
use crate::repo::sync::{sources_for_remote, spawn_syncs};

/// GitHub caps a delivery at 25 MB.
pub const BODY_LIMIT: usize = 25 * 1024 * 1024;

#[derive(Serialize)]
pub struct WebhookReply {
    /// How many repo sources a sync was started for.
    queued: usize,
}

#[derive(Deserialize)]
struct PushEvent {
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(default)]
    deleted: bool,
    repository: Repository,
}

#[derive(Deserialize)]
struct Repository {
    default_branch: Option<String>,
    clone_url: Option<String>,
    html_url: Option<String>,
}

/// True when `header` is `sha256=<hex>` and the hex is the HMAC-SHA256 of `body` under
/// `secret`. The comparison takes the same time wherever the first difference is.
pub fn signature_ok(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(sig) = header.trim().strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig) = hex::decode(sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

pub async fn github(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<WebhookReply>), AppError> {
    let Some(secret) = state.config.github_webhook_secret.as_deref() else {
        return Err(AppError::NotFound);
    };
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    if !signature_ok(secret.as_bytes(), &body, signature) {
        return Err(AppError::Unauthorized);
    }
    let none = |status| Ok((status, Json(WebhookReply { queued: 0 })));
    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    match event {
        "ping" => return none(StatusCode::OK),
        "push" => {}
        _ => return none(StatusCode::ACCEPTED),
    }
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        return Err(AppError::Unsupported(
            "set the webhook's content type to application/json".to_string(),
        ));
    }
    let push: PushEvent = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid push event: {e}")))?;
    let default_ref = push
        .repository
        .default_branch
        .as_deref()
        .map(|b| format!("refs/heads/{b}"));
    if push.deleted || default_ref.as_deref() != Some(push.git_ref.as_str()) {
        return none(StatusCode::ACCEPTED);
    }
    let urls: Vec<&str> = [&push.repository.clone_url, &push.repository.html_url]
        .into_iter()
        .filter_map(|u| u.as_deref())
        .collect();
    let sources = sources_for_remote(&state.pool, &urls).await?;
    let queued = sources.len();
    spawn_syncs(&state, sources);
    Ok((StatusCode::ACCEPTED, Json(WebhookReply { queued })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn a_valid_signature_is_accepted() {
        let body = br#"{"zen":"Keep it logically awesome."}"#;
        let header = sign(b"s3cret", body);
        assert!(signature_ok(b"s3cret", body, &header));
        assert!(signature_ok(
            b"s3cret",
            body,
            &header.to_uppercase().replace("SHA256=", "sha256=")
        ));
    }

    #[test]
    fn github_documentation_example_verifies() {
        // From GitHub's "Validating webhook deliveries" page.
        assert!(signature_ok(
            b"It's a Secret to Everybody",
            b"Hello, World!",
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        ));
    }

    #[test]
    fn a_wrong_or_malformed_signature_is_refused() {
        let body = b"{}";
        let good = sign(b"s3cret", body);
        assert!(!signature_ok(b"other", body, &good));
        assert!(!signature_ok(b"s3cret", b"{} ", &good));
        let mut flipped = good.clone().into_bytes();
        let last = flipped.len() - 1;
        flipped[last] = if flipped[last] == b'0' { b'1' } else { b'0' };
        assert!(!signature_ok(
            b"s3cret",
            body,
            &String::from_utf8(flipped).unwrap()
        ));
        for bad in [
            "",
            "sha256=",
            "sha256=zz",
            "sha1=0123",
            good.trim_start_matches("sha256="),
            &good[..good.len() - 2],
        ] {
            assert!(!signature_ok(b"s3cret", body, bad), "{bad:?}");
        }
    }
}
