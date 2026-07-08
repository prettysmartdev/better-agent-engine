//! Admin provider-registry endpoint (`/admin/v1/providers`).
//!
//! Read-only, mirroring `/admin/v1/mcp-servers`: the provider registry is
//! config-file-driven (`bae-config.toml` `[providers]`) rather than DB-driven,
//! so this endpoint lets an operator confirm what a *running* server actually
//! has available. It exposes each entry's `name`, `provider` (wire-format
//! kind), `model`, and **effective** `base_url` (the explicit value, or the
//! kind's default when omitted) — never `auth_token`. Served on the loopback
//! admin listener, plain HTTP/REST like every other admin endpoint.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::api::AppState;

/// `GET /admin/v1/providers`
///
/// Lists configured providers, sorted by name for stable output. `base_url` is
/// always the effective (resolved-default-or-explicit) endpoint so an operator
/// can confirm which URL is actually in effect. Returns `{"items": [...]}`;
/// the list is empty when no config file was provided.
pub async fn list(State(state): State<AppState>) -> Json<Value> {
    let mut entries: Vec<(&String, &crate::engine::provider::ProviderConfig)> =
        state.provider_registry.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let items: Vec<Value> = entries
        .into_iter()
        .map(|(name, cfg)| {
            json!({
                "name": name,
                "provider": cfg.provider.as_str(),
                "model": cfg.model,
                "base_url": cfg.effective_base_url(),
            })
        })
        .collect();
    Json(json!({ "items": items }))
}
