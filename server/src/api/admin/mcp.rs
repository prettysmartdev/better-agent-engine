//! Admin MCP-server-registry endpoint (`/admin/v1/mcp-servers`).
//!
//! Read-only. The MCP server registry is config-file-driven (`bae-config.toml`)
//! rather than DB-driven like profiles and keys, so this endpoint lets an
//! operator confirm what a *running* server actually has available. It exposes
//! only each server's `name` and `transport` — never `command`, `args`, `url`,
//! or `headers` (which can carry secrets). Served on the loopback admin
//! listener, plain HTTP/REST like every other admin endpoint.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::api::AppState;

/// `GET /admin/v1/mcp-servers`
///
/// Lists configured MCP servers (name + transport), sorted by name for stable
/// output. Returns `{"items": [...]}`; the list is empty when no config file
/// was provided.
pub async fn list(State(state): State<AppState>) -> Json<Value> {
    let mut items: Vec<(&str, &str)> = state
        .mcp_registry
        .values()
        .map(|s| (s.name.as_str(), s.transport.as_str()))
        .collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    let items: Vec<Value> = items
        .into_iter()
        .map(|(name, transport)| json!({ "name": name, "transport": transport }))
        .collect();
    Json(json!({ "items": items }))
}
