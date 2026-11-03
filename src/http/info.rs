//! `GET /v1/info`: what this server is running. No key needed.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::http::AppState;
use crate::llm::Llm;

#[derive(Serialize)]
pub struct Info {
    version: String,
    llm: String,
    embedder: String,
    modes: Vec<&'static str>,
}

/// The answer modes the provider supports. Deep mode drives tools, so it is on only when the
/// provider can use them.
pub fn modes(llm: &dyn Llm) -> Vec<&'static str> {
    if llm.supports_tools() {
        vec!["fast", "deep"]
    } else {
        vec!["fast"]
    }
}

pub async fn info(State(state): State<AppState>) -> Json<Info> {
    Json(Info {
        version: state.config.version.clone(),
        llm: state.llm.name().to_string(),
        embedder: state.embedder.name().to_string(),
        modes: modes(state.llm.as_ref()),
    })
}
