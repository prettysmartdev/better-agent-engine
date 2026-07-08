//! Client-facing router (`BAE_ADDR`).
//!
//! This listener speaks **plain HTTP**: TLS is terminated by an upstream proxy
//! (nginx/caddy/cloud LB) and the container is only ever reachable on an
//! internal network (see `aspec/architecture/security.md`). Never expose this
//! port directly to the internet.
//!
//! Implemented here: `GET /healthz` (unauthenticated liveness), `GET
//! /api/v1/meta` (version + supported API versions), the authenticated
//! `/api/v1/sessions` REST family (see [`sessions`]), and the one JSON-RPC 2.0
//! endpoint `POST /api/v1/sessions/{id}/rpc` carrying the session message/event
//! loop (see [`rpc`]).

pub mod rpc;
pub mod sessions;

use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::api::AppState;
use crate::{API_VERSIONS, VERSION};

/// Build the client-facing router. axum 0.8 path captures use `{id}` syntax.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/meta", get(meta))
        .route("/api/v1/sessions", post(sessions::create))
        .route(
            "/api/v1/sessions/{id}",
            axum::routing::delete(sessions::close),
        )
        .route("/api/v1/sessions/{id}/join", post(sessions::join))
        .route("/api/v1/sessions/{id}/rpc", post(rpc::rpc))
        .route("/api/v1/sessions/{id}/events", get(sessions::get_events))
        .route(
            "/api/v1/sessions/{id}/participants",
            get(sessions::participants),
        )
        .layer(axum::middleware::from_fn(crate::api::log_requests))
        .with_state(state)
}

/// Liveness probe. Always 200, no auth, no body — safe for load-balancer and
/// container health checks.
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Server identity: its version and the API versions it supports, so clients can
/// check compatibility at connect time.
#[derive(Debug, Serialize)]
struct Meta {
    version: &'static str,
    api_versions: &'static [&'static str],
}

async fn meta() -> Json<Meta> {
    Json(Meta {
        version: VERSION,
        api_versions: API_VERSIONS,
    })
}
