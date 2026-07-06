//! HTTP surface.
//!
//! The server exposes **two separate axum [`Router`](axum::Router) instances**,
//! each bound to its own listener — they are never merged and admin routes are
//! never gated behind middleware on a shared router:
//!
//! - [`client`] — the client-facing router (`BAE_ADDR`), serving `/healthz`,
//!   `/api/v1/meta`, and (in a later step) the `/api/v1/sessions` API.
//! - [`admin`] — the admin-only router (`BAE_ADMIN_ADDR`), bound strictly to
//!   loopback, serving (in a later step) the `/admin/v1` API.
//!
//! Both routers share the same [`AppState`] so they read/write one database.

pub mod admin;
pub mod client;
pub mod error;
pub mod pagination;

use crate::store::Store;

/// Shared state handed to both routers. Cloneable and cheap (the [`Store`] is an
/// `Arc` internally and the [`reqwest::Client`] is itself an `Arc` of a
/// connection pool).
#[derive(Clone)]
pub struct AppState {
    /// The SQLite-backed store.
    pub store: Store,
    /// Shared outbound HTTP client for provider (LLM) calls. Reusing one client
    /// keeps the connection pool warm across sessions.
    pub http: reqwest::Client,
}

impl AppState {
    /// Build application state from the store, with a default provider HTTP
    /// client.
    pub fn new(store: Store) -> Self {
        AppState {
            store,
            http: reqwest::Client::new(),
        }
    }
}
