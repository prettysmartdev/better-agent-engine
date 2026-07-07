//! Client session endpoints (`/api/v1/sessions`).
//!
//! The full client-facing surface: exchange a client key for a session, drive
//! the message loop, replay the event log, and close the session. Auth is a
//! bearer key on every request — the client key on session creation, the
//! session key on everything else — verified with Argon2id in constant time,
//! filtered by `role` and `deleted_at IS NULL` in the lookup query.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::error::ApiError;
use crate::api::pagination::{next_cursor, PageQuery};
use crate::api::AppState;
use crate::config_file::McpServerConfig;
use crate::engine::broadcast;
use crate::engine::mcp::McpSession;
use crate::events::EventType;
use crate::store::keys::{self, KeyRecord};
use crate::store::profiles::{self, ProfileRecord};
use crate::store::sessions::{
    self, EventRecord, SessionRecord, STATE_CLOSED, STATE_ERROR, STATE_OPEN,
};

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

/// Extract the `Authorization: Bearer <token>` value, or 401.
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

/// Authenticate the request as a client key (role = 'client').
fn auth_client(state: &AppState, headers: &HeaderMap) -> Result<KeyRecord, ApiError> {
    let token = bearer_token(headers)?;
    let record = state
        .store
        .with_conn(|c| keys::authenticate_client(c, &token))
        .map_err(auth_key_err)?;
    record.ok_or_else(|| ApiError::unauthorized("invalid client key"))
}

/// Authenticate the request as the session key for `session_id`, returning the
/// session and the key. Rejects a session key presented on the wrong session
/// (the lookup is scoped to `session_id`, so a mismatched key finds no row).
pub(crate) fn auth_session(
    state: &AppState,
    headers: &HeaderMap,
    session_id: &str,
) -> Result<(SessionRecord, KeyRecord), ApiError> {
    let token = bearer_token(headers)?;
    let outcome = state.store.with_conn(|c| {
        let key = keys::authenticate_session(c, &token, session_id).map_err(auth_key_err)?;
        let key = match key {
            Some(k) => k,
            None => return Ok(None),
        };
        // The session row must exist for a valid key to be meaningful.
        let session = sessions::get_session(c, session_id).map_err(ApiError::from_db)?;
        Ok::<_, ApiError>(session.map(|s| (s, key)))
    })?;
    outcome.ok_or_else(|| ApiError::unauthorized("invalid session key"))
}

/// Map a key-store error to an API error: a DB failure is a 500, anything else
/// (malformed stored hash, etc.) is treated as an auth failure.
fn auth_key_err(e: keys::KeyError) -> ApiError {
    match e {
        keys::KeyError::Db(e) => ApiError::from_db(e),
        other => {
            tracing::warn!("key verification error: {other}");
            ApiError::unauthorized("invalid key")
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/sessions
// ---------------------------------------------------------------------------

/// A tool the client declares it can execute.
#[derive(Debug, Deserialize)]
pub struct ClientToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
}

/// `POST /api/v1/sessions` body.
#[derive(Debug, Deserialize)]
pub struct CreateSession {
    #[serde(default)]
    pub client_version: Option<String>,
    #[serde(default)]
    pub tools: Vec<ClientToolDef>,
}

/// Client-safe projection of a profile (no `auth_token`, no env var names).
fn public_profile(p: &ProfileRecord) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "allowed_tools": p.allowed_tools,
        "mcp_servers": p.mcp_servers,
        "provider": {
            "provider": p.provider_config.get("provider").cloned().unwrap_or(Value::Null),
            "model": p.provider_config.get("model").cloned().unwrap_or(Value::Null),
        },
    })
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateSession>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let client_key = auth_client(&state, &headers)?;
    let profile_id = client_key
        .profile_id
        .clone()
        .ok_or_else(|| ApiError::internal("client key has no profile"))?;

    // Load the profile. It may have been deleted between key creation and now.
    let profile = state
        .store
        .with_conn(|c| profiles::get(c, &profile_id))
        .map_err(ApiError::from_db)?;
    let profile = match profile {
        Some(p) => p,
        None => return profile_unavailable_at_open(&state, &client_key, &profile_id, &body),
    };

    // Enforce the tool allowlist. An empty allowlist permits no tools.
    let allowed: HashSet<&str> = profile
        .allowed_tools
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for tool in &body.tools {
        if !allowed.contains(tool.name.as_str()) {
            return Err(ApiError::forbidden(
                "tool_not_allowed",
                format!("tool {:?} is not in the profile's allowlist", tool.name),
            ));
        }
    }

    // Persist the declared tools as-is for the engine to advertise to the LLM.
    let client_tools = json!(body
        .tools
        .iter()
        .map(|t| json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.input_schema.clone().unwrap_or_else(|| json!({})),
        }))
        .collect::<Vec<_>>());

    let session_key = keys::generate_session_key();
    let tool_names: Vec<&str> = body.tools.iter().map(|t| t.name.as_str()).collect();

    // Create the session row, its session key, and the session.open event under
    // one lock. The session.open event is inserted inside this transaction (it is
    // atomic with row creation) and published afterwards through the broadcast
    // choke point; no watcher can exist yet, so the publish is a harmless no-op,
    // but it keeps every event insert funnelled through one place.
    let (session, open_event) = state.store.with_conn(|c| {
        let session = sessions::create_session(
            c,
            &client_key.id,
            &profile_id,
            STATE_OPEN,
            body.client_version.as_deref(),
            &client_tools,
        )
        .map_err(ApiError::from_db)?;
        keys::insert_session_key(c, &session.id, &client_key.id, &profile_id, &session_key)
            .map_err(|e| match e {
                keys::InsertError::Sqlite(e) => ApiError::from_db(e),
                keys::InsertError::Key(e) => {
                    tracing::error!("session key hashing failed: {e}");
                    ApiError::internal("failed to hash session key")
                }
            })?;
        let open_event = sessions::insert_event(
            c,
            &session.id,
            Some(&client_key.id),
            EventType::SessionOpen,
            &json!({ "client_version": body.client_version, "tools": tool_names }),
        )
        .map_err(ApiError::from_db)?;
        Ok::<_, ApiError>((session, open_event))
    })?;
    state.broadcaster.publish(&open_event);

    // Resolve the profile's configured MCP servers against the startup registry
    // and connect to each. Resolution is deliberately at session-creation time
    // (not profile-create time): the registry is server-config-driven and may
    // differ from what existed when the profile was written.
    resolve_mcp_servers(&state, &profile, &session.id).await;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "session_id": session.id,
            "session_key": session_key.plaintext,
            "profile": public_profile(&profile),
        })),
    ))
}

/// Connect the MCP servers a profile opts into, and retain the live connections
/// on `state` for the life of the session.
///
/// For each name in `profile.mcp_servers`:
/// - not found in the registry → `tracing::error!` and skip (non-fatal, logged
///   **every** session creation — never deduplicated/cached);
/// - found but fails to connect (missing stdio binary, unreachable endpoint,
///   unset auth env var) → treated exactly like "not found": log and skip.
///
/// This never fails session creation; a fully-unresolvable profile just yields a
/// session with no MCP tools.
async fn resolve_mcp_servers(state: &AppState, profile: &ProfileRecord, session_id: &str) {
    // `mcp_servers` is a JSON array of server-name strings (validated as such at
    // profile-write time). Anything else yields no servers.
    let names: Vec<String> = profile
        .mcp_servers
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return;
    }

    // Resolve names against the registry first (never short-circuiting on a
    // miss), then log every miss individually and connect every hit.
    let (resolved, missing) = resolve_registry_names(&state.mcp_registry, &names);
    for name in &missing {
        tracing::error!(
            profile_id = %profile.id,
            profile_name = %profile.name,
            mcp_server_name = %name,
            session_id = %session_id,
            "configured MCP server not found in bae-config.toml; skipping"
        );
    }

    let mut mcp = McpSession::new();
    for cfg in resolved {
        if let Err(e) = mcp.connect(cfg).await {
            // A resolves-by-name-but-fails-to-connect server is handled the same
            // as "not found": log and skip, non-fatal to session creation.
            tracing::error!(
                profile_id = %profile.id,
                profile_name = %profile.name,
                mcp_server_name = %cfg.name,
                session_id = %session_id,
                error = %e,
                "configured MCP server failed to connect; skipping"
            );
        }
    }

    if mcp.has_servers() {
        state.insert_mcp_session(session_id, mcp);
    }
}

/// Split a profile's configured MCP server names into `(resolved, missing)`
/// against the startup registry, preserving order and **never** short-circuiting
/// on a miss — every absent name is returned so the caller logs and skips each
/// one individually. The pure resolution step behind [`resolve_mcp_servers`],
/// separated out so the valid-resolves / invalid-skipped / empty-registry matrix
/// is unit-testable without spinning up a live MCP connection.
pub(crate) fn resolve_registry_names<'a>(
    registry: &'a HashMap<String, McpServerConfig>,
    names: &[String],
) -> (Vec<&'a McpServerConfig>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for name in names {
        match registry.get(name) {
            Some(cfg) => resolved.push(cfg),
            None => missing.push(name.clone()),
        }
    }
    (resolved, missing)
}

/// Handle a profile that has been deleted before session open: record an error
/// session and a `session.error` event, then return `profile_unavailable`.
fn profile_unavailable_at_open(
    state: &AppState,
    client_key: &KeyRecord,
    profile_id: &str,
    body: &CreateSession,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let client_tools = json!(body
        .tools
        .iter()
        .map(|t| json!({"name": t.name}))
        .collect::<Vec<_>>());
    let _ = state.store.with_conn(|c| -> rusqlite::Result<()> {
        let session = sessions::create_session(
            c,
            &client_key.id,
            profile_id,
            STATE_ERROR,
            body.client_version.as_deref(),
            &client_tools,
        )?;
        sessions::insert_event(
            c,
            &session.id,
            Some(&client_key.id),
            EventType::SessionError,
            &json!({ "reason": "profile_unavailable", "profile_id": profile_id }),
        )?;
        Ok(())
    });
    Err(ApiError::unprocessable(
        "profile_unavailable",
        "the profile bound to this client key is no longer available",
    ))
}

// ---------------------------------------------------------------------------
// Message body (shared with the JSON-RPC `session.sendMessage` method)
// ---------------------------------------------------------------------------

/// The `message` object carried by `session.sendMessage`'s `params` (and, before
/// this work item, by `POST /api/v1/sessions/{id}/messages`). The message-send
/// loop now lives on the JSON-RPC `/rpc` endpoint (see [`super::rpc`]); this type
/// and the helpers below are shared with it.
#[derive(Debug, Deserialize)]
pub(crate) struct MessageBody {
    #[serde(default = "default_role")]
    pub role: String,
    pub content: Value,
}

fn default_role() -> String {
    "user".to_string()
}

/// Extract `tool_result` blocks from a message content value, as `tool.result`
/// event payloads. A plain string or a block-less array yields nothing.
pub(crate) fn tool_result_blocks(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                .map(|b| {
                    json!({
                        "tool_use_id": b.get("tool_use_id").cloned().unwrap_or(Value::Null),
                        "dispatch": "client",
                        "content": b.get("content").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GET /api/v1/sessions/{id}/events
// ---------------------------------------------------------------------------

/// JSON view of one `session_events` row.
pub(crate) fn event_view(e: &EventRecord) -> Value {
    json!({
        "id": e.id,
        "session_id": e.session_id,
        "client_key_id": e.client_key_id,
        "event_type": e.event_type,
        "payload": e.payload,
        "created_at": e.created_at,
    })
}

pub async fn get_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let (session, _key) = auth_session(&state, &headers, &id)?;
    let (after, limit) = page.resolve()?;
    let (rows, has_more) = state
        .store
        .with_conn(|c| sessions::list_events(c, &session.id, after, limit))
        .map_err(ApiError::from_db)?;
    let last_rowid = rows.last().map(|(rid, _)| *rid);
    let items: Vec<Value> = rows.iter().map(|(_, e)| event_view(e)).collect();
    Ok(Json(json!({
        "items": items,
        "next_cursor": next_cursor(last_rowid, has_more),
    })))
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/sessions/{id}
// ---------------------------------------------------------------------------

pub async fn close(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (session, _key) = auth_session(&state, &headers, &id)?;
    let closed = state
        .store
        .with_conn(|c| sessions::close_session(c, &session.id, STATE_CLOSED))
        .map_err(ApiError::from_db)?;

    // A `session.close` event is only meaningful for a real open->closed
    // transition; a session that was already terminal (e.g. moved to `error` by a
    // provider failure) does not get one.
    if closed {
        // Log + publish the close event through the broadcast choke point so any
        // live `session.subscribe` watcher sees the session end before its stream
        // closes.
        broadcast::insert_and_publish(
            &state.store,
            &state.broadcaster,
            &session.id,
            Some(&session.client_key_id),
            EventType::SessionClose,
            &json!({ "reason": "client_close" }),
        )
        .map_err(ApiError::from_db)?;
    }

    // Free the session's in-memory resources unconditionally — including when the
    // session was already terminal. A session that ended in `error` can never
    // transition through `close_session`, so if cleanup were gated on the
    // transition it would leak its MCP subprocess and its broadcast-channel entry
    // until server restart. Both operations are idempotent no-ops when the
    // resources are already gone (e.g. a double DELETE).
    //
    // Drop the broadcast channel (dropping the sender ends every live watcher's
    // stream cleanly), then tear down any live MCP connections (kill spawned
    // stdio subprocesses and drop the registry entry).
    state.broadcaster.remove(&session.id);
    if let Some(mcp) = state.take_mcp_session(&session.id) {
        mcp.lock().await.shutdown().await;
    }

    if !closed {
        return Err(ApiError::conflict(
            "session_closed",
            format!("session is already {}", session.state),
        ));
    }

    Ok(Json(
        json!({ "session_id": session.id, "state": STATE_CLOSED }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_file::McpTransport;

    fn stdio_cfg(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some("true".into()),
            args: vec![],
            url: None,
            headers: HashMap::new(),
        }
    }

    fn registry(names: &[&str]) -> HashMap<String, McpServerConfig> {
        names
            .iter()
            .map(|n| (n.to_string(), stdio_cfg(n)))
            .collect()
    }

    #[test]
    fn valid_names_resolve_and_a_middle_miss_does_not_short_circuit() {
        let reg = registry(&["fs", "search"]);
        let names = vec![
            "fs".to_string(),
            "ghost".to_string(),
            "search".to_string(),
        ];
        let (resolved, missing) = resolve_registry_names(&reg, &names);
        // The miss between two valid names must not drop the trailing valid one.
        let resolved_names: Vec<&str> = resolved.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(resolved_names, vec!["fs", "search"]);
        assert_eq!(missing, vec!["ghost".to_string()]);
    }

    #[test]
    fn names_absent_from_a_populated_registry_are_all_missing() {
        let reg = registry(&["fs"]);
        let names = vec!["ghost".to_string(), "phantom".to_string()];
        let (resolved, missing) = resolve_registry_names(&reg, &names);
        assert!(resolved.is_empty());
        assert_eq!(missing, names);
    }

    #[test]
    fn empty_registry_makes_every_name_missing() {
        let reg: HashMap<String, McpServerConfig> = HashMap::new();
        let names = vec!["fs".to_string(), "search".to_string()];
        let (resolved, missing) = resolve_registry_names(&reg, &names);
        assert!(resolved.is_empty());
        assert_eq!(missing, names);
    }
}
