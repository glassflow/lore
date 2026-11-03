//! `GET /v1/sources`: the tenant's sources with their document counts, for picking which ones a
//! search or question looks in.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::auth::{Caller, Scope};
use crate::error::AppError;
use crate::http::AppState;
use crate::store::{SourceSummary, list_source_summaries};

#[derive(Serialize)]
pub struct SourceList {
    sources: Vec<SourceSummary>,
}

/// Every source of the caller's tenant, ordered by kind and then name. Needs the read scope.
pub async fn list_sources(
    caller: Caller,
    State(state): State<AppState>,
) -> Result<Json<SourceList>, AppError> {
    caller.require(Scope::Read)?;
    let sources = list_source_summaries(&state.pool, caller.tenant_id).await?;
    Ok(Json(SourceList { sources }))
}
