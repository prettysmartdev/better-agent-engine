//! Admin combined-config endpoint (`/admin/v1/config`).
//!
//! Read-only, mirroring `/admin/v1/mcp-servers` and `/admin/v1/providers` but
//! richer: it exposes the full startup snapshot of every config-file-driven
//! (`bae-config.toml`) section — `[mcp]`, `[providers]`, and `[telemetry]` — in
//! one response, so an operator can confirm what a *running* server actually
//! has available without `docker exec`-ing in to read the file on disk.
//!
//! Unlike the two minimal sibling endpoints (which *omit* `command`/`args`/
//! `url`/`headers`/`auth_token` entirely), this endpoint exposes those fields —
//! with every **secret-bearing** value replaced by a fixed [`REDACTED`] marker
//! so the UI can render a placeholder rather than nothing. The three
//! secret-bearing values are MCP server `headers` values, provider `auth_token`,
//! and telemetry `otlp_headers` values; every one is masked **unconditionally**,
//! never conditioned on the value's shape (a literal secret typed straight into
//! the TOML is masked identically to an unresolved `${ENV_VAR}` token), and
//! never partially (a fixed-length marker, so the secret's length never leaks).
//! Non-secret fields — `command`/`args`/`url`, effective `base_url`, the OTLP
//! collector endpoint, sampling, service name, trace/metric toggles — are shown
//! in full. Served on the loopback admin listener, plain HTTP/REST like every
//! other admin endpoint.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::api::AppState;
use crate::config_file::TelemetryConfig;

/// The fixed placeholder every secret-bearing value is replaced with. A single
/// fixed-length marker (not one dot per character of the real value) is
/// deliberate: it avoids leaking the secret's length as a side channel.
pub const REDACTED: &str = "••••••••";

/// `GET /admin/v1/config`
///
/// Returns the full startup config snapshot with all secrets redacted:
///
/// ```json
/// {
///   "mcp": { "servers": [ … ] },
///   "providers": { "entries": [ … ] },
///   "telemetry": { … }
/// }
/// ```
///
/// `mcp.servers` and `providers.entries` are sorted by `name` for stable
/// output. A missing config file, or a file with no `[mcp]`/`[providers]`/
/// `[telemetry]` table, yields empty lists and a default-disabled `telemetry`
/// object with `200 OK` — never an error.
pub async fn get(State(state): State<AppState>) -> Json<Value> {
    let mut servers: Vec<&crate::config_file::McpServerConfig> =
        state.mcp_registry.values().collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    let servers: Vec<Value> = servers
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "transport": s.transport.as_str(),
                "command": s.command,
                "args": s.args,
                "url": s.url,
                "headers": redact_map(&s.headers),
            })
        })
        .collect();

    let mut entries: Vec<(&String, &crate::engine::provider::ProviderConfig)> =
        state.provider_registry.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(name, cfg)| {
            json!({
                "name": name,
                "provider": cfg.provider.as_str(),
                "model": cfg.model,
                "base_url": cfg.effective_base_url(),
                "auth_token": REDACTED,
            })
        })
        .collect();

    Json(json!({
        "mcp": { "servers": servers },
        "providers": { "entries": entries },
        "telemetry": telemetry_view(&state.telemetry_config),
    }))
}

/// Redact every value of a header-style map, preserving keys, and serialize in
/// a deterministic (sorted-key) order so the response body is stable for
/// snapshot tests. An empty map serializes to `{}`.
fn redact_map<S: std::borrow::Borrow<str> + Ord>(
    map: &std::collections::HashMap<S, String>,
) -> Value {
    let sorted: BTreeMap<&str, &str> = map.keys().map(|k| (k.borrow(), REDACTED)).collect();
    let mut out = Map::new();
    for (k, v) in sorted {
        out.insert(k.to_owned(), Value::String(v.to_owned()));
    }
    Value::Object(out)
}

/// Build the redacted `telemetry` view mirroring [`TelemetryConfig`], with only
/// `otlp_headers` values masked. `service_name` is emitted as the *effective*
/// name (`"baesrv"` when unset), matching how `base_url` emits its effective
/// value; `otlp_headers` is `{}` when absent. `metrics.disabled` preserves the
/// configured order.
fn telemetry_view(cfg: &TelemetryConfig) -> Value {
    let otlp_headers = match &cfg.otlp_headers {
        Some(headers) => redact_map(headers),
        None => Value::Object(Map::new()),
    };
    json!({
        "enabled": cfg.enabled,
        "otlp_endpoint": cfg.otlp_endpoint,
        "otlp_headers": otlp_headers,
        "sample_ratio": cfg.sample_ratio,
        "service_name": cfg.service_name.as_deref().unwrap_or("baesrv"),
        "traces": { "enabled": cfg.traces.enabled },
        "metrics": {
            "enabled": cfg.metrics.enabled,
            "disabled": cfg.metrics.disabled,
        },
    })
}
