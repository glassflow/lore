use std::sync::Arc;

use axum::extract::{FromRequest, FromRequestParts, Query, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::{from_fn_with_state, map_response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::answer::deep::DeepGuard;
use crate::config::Config;
use crate::embed::Embedder;
use crate::error::AppError;
use crate::llm::{Llm, Models};
use crate::mcp;
use crate::repo::SourceLocks;
use crate::repo::progress::PushRegistry;
use crate::web;

pub mod admin;
pub mod ask;
pub mod documents;
pub mod info;
pub mod me;
pub mod repos;
pub mod search;
pub mod traces;
pub mod webhooks;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub embedder: Arc<dyn Embedder>,
    pub llm: Arc<dyn Llm>,
    /// The model and effort used by each answer mode.
    pub models: Models,
    /// One lock per source, held while a bundle is ingested.
    pub source_locks: Arc<SourceLocks>,
    /// Running and recent repo pushes, with their progress, for the admin page.
    pub pushes: Arc<PushRegistry>,
    /// Per-tenant slots and the daily count of deep-mode questions.
    pub deep_guard: Arc<DeepGuard>,
}

pub fn router(state: AppState) -> Router {
    // The limit is on the bundle route only: other routes keep axum's own defaults.
    let bundle_limit = usize::try_from(state.config.max_bundle_bytes).unwrap_or(usize::MAX);
    // MCP clients get 401/403 from the middleware before rmcp sees the request. `route_layer`
    // wraps only the matched /mcp routes, so an unknown path stays a 404 whether or not it has
    // a key (`layer` would answer every unknown path with 401).
    let mcp = Router::new()
        .nest_service("/mcp", mcp::service(state.clone()))
        .route_layer(from_fn_with_state(state.clone(), mcp::require_caller));
    Router::new()
        .merge(mcp)
        .route("/", get(web::index))
        .route("/admin", get(web::admin))
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        .route("/v1/me", get(me::me))
        .route("/v1/admin/overview", get(admin::overview))
        .route("/v1/admin/pushes", get(admin::list_pushes))
        .route(
            "/v1/admin/tenants",
            post(admin::create_tenant).get(admin::list_tenants),
        )
        .route("/v1/admin/tenants/{id}", delete(admin::delete_tenant))
        .route("/v1/admin/sources/{id}", delete(admin::delete_source))
        .route(
            "/v1/admin/tenants/{id}/keys",
            post(admin::create_key).get(admin::list_keys),
        )
        .route("/v1/admin/keys/{id}", delete(admin::revoke_key))
        .route("/v1/repos", post(repos::create_repo).get(repos::list_repos))
        .route(
            "/v1/repos/{id}/bundle",
            put(repos::put_bundle).layer(RequestBodyLimitLayer::new(bundle_limit)),
        )
        .route("/v1/repos/{id}/sync", post(repos::sync_repo))
        .route(
            "/v1/webhooks/github",
            post(webhooks::github).layer(RequestBodyLimitLayer::new(webhooks::BODY_LIMIT)),
        )
        .route(
            "/v1/traces",
            post(traces::post_traces).layer(RequestBodyLimitLayer::new(16 * 1024 * 1024)),
        )
        .route(
            "/v1/documents",
            post(documents::post_documents)
                .layer(RequestBodyLimitLayer::new(documents::BODY_LIMIT))
                .layer(map_response(documents::json_payload_too_large)),
        )
        .route("/v1/documents/{id}", get(documents::get_document))
        .route(
            "/v1/documents/{source}/{*external_id}",
            delete(documents::delete_document),
        )
        .route("/v1/search", get(search::get_search))
        .route("/v1/ask", post(ask::post_ask))
        .route("/v1/info", get(info::info))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// `Json` with rejections turned into the standard 400 error body.
pub struct AppJson<T>(pub T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(AppJson(v)),
            Err(rejection) => Err(AppError::BadRequest(rejection.body_text())),
        }
    }
}

/// `Query` with rejections turned into the standard 400 error body.
pub struct AppQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for AppQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(AppQuery(v)),
            Err(rejection) => Err(AppError::BadRequest(rejection.body_text())),
        }
    }
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    db: &'static str,
}

#[derive(Serialize)]
struct Live {
    status: &'static str,
}

async fn healthz(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => (
            StatusCode::OK,
            Json(Health {
                status: "ok",
                db: "ok",
            }),
        ),
        Err(e) => {
            tracing::error!(error = %e, "health check: database error");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(Health {
                    status: "degraded",
                    db: "error",
                }),
            )
        }
    }
}

async fn livez() -> Json<Live> {
    Json(Live { status: "ok" })
}
