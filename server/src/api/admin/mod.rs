//! Admin-only router (`BAE_ADMIN_ADDR`).
//!
//! A **separate** [`Router`] from the client one, bound strictly to a loopback
//! listener (enforced in config validation), so the admin surface is never
//! reachable off-host. Like the client listener it speaks plain HTTP.
//!
//! Every `/admin/v1/*` route requires an `Authorization: Bearer <admin_key>`
//! header (an active `role='admin'` key, Argon2id-verified in constant time)
//! unless the operator explicitly opts out with
//! `--dangerously-disable-admin-auth`, which restores the historical zero-auth
//! behavior. See [`crate::admin_auth`] for the bootstrap that provisions the
//! key material.
//!
//! Endpoints (all under `/admin/v1`):
//! - `profiles` — create, list (cursor-paginated), get, replace, soft-delete.
//! - `keys` — issue (plaintext once), list (never `key_hash`), revoke.
//! - `mcp-servers` — read-only list of the configured MCP registry (no secrets).
//! - `providers` — read-only list of the configured provider registry (no
//!   secrets; `base_url` is the effective value).
//! - `sandbox-status` — read-only, per-profile sandbox image provisioning
//!   status from the in-memory tracker.

pub mod keys;
pub mod mcp;
pub mod profiles;
pub mod providers;
pub mod sandbox;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::api::error::ApiError;
use crate::api::AppState;
use crate::store::keys as key_store;

/// Build the admin-only router.
///
/// When `auth_enabled` is set, an admin-key enforcement layer is installed on
/// every route; when it is not (the `--dangerously-disable-admin-auth` path) no
/// auth layer is added and the surface is open, exactly as it was before admin
/// auth shipped.
///
/// axum 0.8 path captures use `{id}` syntax.
pub fn router(state: AppState, auth_enabled: bool) -> Router {
    let mut router = Router::new()
        .route(
            "/admin/v1/profiles",
            post(profiles::create).get(profiles::list),
        )
        .route(
            "/admin/v1/profiles/{id}",
            get(profiles::get)
                .put(profiles::replace)
                .delete(profiles::delete),
        )
        .route("/admin/v1/keys", post(keys::create).get(keys::list))
        .route("/admin/v1/keys/{id}", axum::routing::delete(keys::delete))
        .route("/admin/v1/mcp-servers", get(mcp::list))
        .route("/admin/v1/providers", get(providers::list))
        .route("/admin/v1/sandbox-status", get(sandbox::list));

    // Layer auth *below* request logging (added last, so it runs outermost) so
    // that rejected requests are still logged with their 401 status.
    if auth_enabled {
        router = router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_admin_auth,
        ));
    }

    router
        .layer(axum::middleware::from_fn(crate::api::log_requests))
        .with_state(state)
}

/// Enforce `Authorization: Bearer <admin_key>` on every admin route.
///
/// Extracts the bearer token and verifies it, in constant time, against every
/// active `role='admin'` key (see [`key_store::authenticate_admin`]). A missing
/// or malformed header, or a token that matches no active admin key (including
/// any client- or session-role key, which is never selected), is a `401`.
async fn require_admin_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let token = match bearer_token(&headers) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    match state
        .store
        .with_conn(|c| key_store::authenticate_admin(c, &token))
    {
        Ok(Some(_)) => next.run(request).await,
        Ok(None) => ApiError::unauthorized("invalid admin key").into_response(),
        Err(e) => {
            tracing::error!("admin auth lookup failed: {e}");
            ApiError::internal("admin authentication failed").into_response()
        }
    }
}

/// Extract the `Authorization: Bearer <token>` value, or a 401 [`ApiError`].
/// Mirrors the client router's `bearer_token`, so both ports parse the header
/// identically (case-insensitive scheme, non-empty token).
fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing Authorization header"))?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .ok_or_else(|| ApiError::unauthorized("Authorization header must be a Bearer token"))?;
    if token.is_empty() {
        return Err(ApiError::unauthorized("empty bearer token"));
    }
    Ok(token.to_owned())
}
