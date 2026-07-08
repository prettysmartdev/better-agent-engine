//! Admin-only router (`BAE_ADMIN_ADDR`).
//!
//! A **separate** [`Router`] from the client one, bound strictly to a loopback
//! listener (enforced in config validation), so the admin surface is never
//! reachable off-host. Like the client listener it speaks plain HTTP; there is
//! no auth on this port initially because it is localhost-only.
//!
//! Endpoints (all under `/admin/v1`):
//! - `profiles` — create, list (cursor-paginated), get, replace, soft-delete.
//! - `keys` — issue (plaintext once), list (never `key_hash`), revoke.
//! - `mcp-servers` — read-only list of the configured MCP registry (no secrets).
//! - `providers` — read-only list of the configured provider registry (no
//!   secrets; `base_url` is the effective value).

pub mod keys;
pub mod mcp;
pub mod profiles;
pub mod providers;

use axum::routing::{get, post};
use axum::Router;

use crate::api::AppState;

/// Build the admin-only router.
///
/// axum 0.8 path captures use `{id}` syntax.
pub fn router(state: AppState) -> Router {
    Router::new()
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
        .layer(axum::middleware::from_fn(crate::api::log_requests))
        .with_state(state)
}
