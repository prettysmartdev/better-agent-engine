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
/// The `provider` sub-object is resolved from the profile's `primary_provider`
/// registry name — never the registry name's raw config (which carries the
/// auth token template).
fn public_profile(state: &AppState, p: &ProfileRecord) -> Value {
    let provider = p
        .provider_config
        .as_str()
        .and_then(|name| state.provider_registry.get(name))
        .map(|cfg| json!({ "provider": cfg.provider.as_str(), "model": cfg.model }))
        .unwrap_or_else(|| json!({ "provider": Value::Null, "model": Value::Null }));
    json!({
        "id": p.id,
        "name": p.name,
        "allowed_tools": p.allowed_tools,
        "mcp_servers": p.mcp_servers,
        "provider": provider,
    })
}

/// Enforce a profile's tool allowlist against a client's declared tools. An
/// empty allowlist permits no tools. Shared by `create` and `join` — each
/// client's declaration is validated independently against the same profile.
fn enforce_tool_allowlist(
    profile: &ProfileRecord,
    tools: &[ClientToolDef],
) -> Result<(), ApiError> {
    let allowed: HashSet<&str> = profile
        .allowed_tools
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for tool in tools {
        if !allowed.contains(tool.name.as_str()) {
            return Err(ApiError::forbidden(
                "tool_not_allowed",
                format!("tool {:?} is not in the profile's allowlist", tool.name),
            ));
        }
    }
    Ok(())
}

/// The declared tools as the JSON array the engine advertises to the LLM.
fn declared_tools_json(tools: &[ClientToolDef]) -> Value {
    json!(tools
        .iter()
        .map(|t| json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.input_schema.clone().unwrap_or_else(|| json!({})),
        }))
        .collect::<Vec<_>>())
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

    // The profile's primary provider must resolve against the startup registry
    // before a session can exist at all — a missing primary is fatal for every
    // client on this profile (unlike missing fallbacks/MCP servers, which are
    // logged and skipped at use time).
    let primary_provider = profile.provider_config.as_str().unwrap_or_default();
    if !state.provider_registry.contains_key(primary_provider) {
        return primary_provider_unavailable_at_open(
            &state,
            &client_key,
            &profile,
            primary_provider,
            &body,
        );
    }

    // Enforce the tool allowlist. An empty allowlist permits no tools.
    enforce_tool_allowlist(&profile, &body.tools)?;

    // Persist the declared tools as-is for the engine to advertise to the LLM
    // (stored under this client's own key in the per-client tools object).
    let client_tools = declared_tools_json(&body.tools);

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
            "profile": public_profile(&state, &profile),
        })),
    ))
}

// ---------------------------------------------------------------------------
// POST /api/v1/sessions/{id}/join
// ---------------------------------------------------------------------------

/// Mint an additional session key for an existing open session, so a second
/// (third, …) client key can drive/observe it. Body: the same shape as
/// [`CreateSession`].
///
/// The core guard is the **profile match**: the joining client key must be
/// bound to the exact profile the session was opened with — a client on a
/// different profile is rejected with `403 profile_mismatch` before any event
/// is logged or session key minted (an authorization failure at the client-key
/// level, same posture as `tool_not_allowed`). The joiner declares its own,
/// independent tool set (validated against the shared profile's allowlist and
/// stored under its own key in the per-client `client_tools` object — never
/// merged with any other driver's list), and existing participants see the
/// join live via a broadcast `session.join` event.
pub async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CreateSession>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let client_key = auth_client(&state, &headers)?;

    let session = state
        .store
        .with_conn(|c| sessions::get_session(c, &id))
        .map_err(ApiError::from_db)?;
    let session = session.ok_or_else(|| ApiError::not_found(format!("no session {id}")))?;
    // A joiner cannot resurrect a terminal session.
    if session.state != STATE_OPEN {
        return Err(ApiError::conflict(
            "session_closed",
            format!("session is already {}", session.state),
        ));
    }

    // The client → profile → session mapping: a client key on a different
    // profile must never attach to this session.
    if client_key.profile_id.as_deref() != Some(session.profile_id.as_str()) {
        return Err(ApiError::forbidden(
            "profile_mismatch",
            "the client key's profile does not match this session's profile",
        ));
    }

    // Load the shared profile; it may have been deleted since the session
    // opened. Same rejection posture as `create`.
    let profile = state
        .store
        .with_conn(|c| profiles::get(c, &session.profile_id))
        .map_err(ApiError::from_db)?;
    let profile = match profile {
        Some(p) => p,
        None => {
            return profile_unavailable_at_open(&state, &client_key, &session.profile_id, &body)
        }
    };

    // The primary provider must still resolve — the same fatal-for-the-profile
    // check `create` performs, re-run on every join attempt.
    let primary_provider = profile.provider_config.as_str().unwrap_or_default();
    if !state.provider_registry.contains_key(primary_provider) {
        return primary_provider_unavailable_at_open(
            &state,
            &client_key,
            &profile,
            primary_provider,
            &body,
        );
    }

    // The joiner's own tool declaration, against the same profile's allowlist.
    enforce_tool_allowlist(&profile, &body.tools)?;
    let client_tools = declared_tools_json(&body.tools);
    let tool_names: Vec<&str> = body.tools.iter().map(|t| t.name.as_str()).collect();

    // Upsert this client's tools entry, mint its session key, and log the join
    // under one lock; publish the join event to live watchers afterwards.
    let session_key = keys::generate_session_key();
    let join_event = state.store.with_conn(|c| {
        sessions::set_client_tools(c, &session.id, &client_key.id, &client_tools)
            .map_err(ApiError::from_db)?;
        keys::insert_session_key(
            c,
            &session.id,
            &client_key.id,
            &session.profile_id,
            &session_key,
        )
        .map_err(|e| match e {
            keys::InsertError::Sqlite(e) => ApiError::from_db(e),
            keys::InsertError::Key(e) => {
                tracing::error!("session key hashing failed: {e}");
                ApiError::internal("failed to hash session key")
            }
        })?;
        sessions::insert_event(
            c,
            &session.id,
            Some(&client_key.id),
            EventType::SessionJoin,
            &json!({ "client_version": body.client_version, "tools": tool_names }),
        )
        .map_err(ApiError::from_db)
    })?;
    state.broadcaster.publish(&join_event);

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "session_id": session.id,
            "session_key": session_key.plaintext,
            "profile": public_profile(&state, &profile),
        })),
    ))
}

// ---------------------------------------------------------------------------
// GET /api/v1/sessions/{id}/participants
// ---------------------------------------------------------------------------

/// The session's currently-registered drivers, from the in-memory registry.
/// Live-only by design (lost on restart, like the MCP/broadcast state): the
/// durable "who ever joined" record is the event log's `session.open` /
/// `session.join` / `session.driver.register` events.
pub async fn participants(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (session, _key) = auth_session(&state, &headers, &id)?;
    Ok(Json(
        json!({ "drivers": state.registered_drivers(&session.id) }),
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

/// Handle a profile whose `primary_provider` name does not resolve against the
/// startup registry: log the operator error (on **every** attempt, never
/// deduplicated — same posture as the MCP "not found" logging), record an
/// error session and a `session.error` event (matching
/// [`profile_unavailable_at_open`]'s pattern), and reject with
/// `422 primary_provider_unavailable`. No open session is created and no
/// session key is issued.
fn primary_provider_unavailable_at_open(
    state: &AppState,
    client_key: &KeyRecord,
    profile: &ProfileRecord,
    primary_provider: &str,
    body: &CreateSession,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    tracing::error!(
        profile_id = %profile.id,
        profile_name = %profile.name,
        primary_provider = %primary_provider,
        "profile's primary provider not found in bae-config.toml; refusing to open a session"
    );
    let client_tools = json!(body
        .tools
        .iter()
        .map(|t| json!({"name": t.name}))
        .collect::<Vec<_>>());
    let _ = state.store.with_conn(|c| -> rusqlite::Result<()> {
        let session = sessions::create_session(
            c,
            &client_key.id,
            &profile.id,
            STATE_ERROR,
            body.client_version.as_deref(),
            &client_tools,
        )?;
        sessions::insert_event(
            c,
            &session.id,
            Some(&client_key.id),
            EventType::SessionError,
            &json!({
                "reason": "primary_provider_unavailable",
                "profile_id": profile.id,
                "primary_provider": primary_provider,
            }),
        )?;
        Ok(())
    });
    Err(ApiError::unprocessable(
        "primary_provider_unavailable",
        format!(
            "the profile's primary provider {primary_provider:?} is not configured in bae-config.toml"
        ),
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
    // stream cleanly), tear down any live MCP connections (kill spawned stdio
    // subprocesses and drop the registry entry), then clear the session's
    // multi-client runtime state (driver registrations, turn gate, and any
    // parked paused turn — whose dropped guard frees queued waiters).
    state.broadcaster.remove(&session.id);
    if let Some(mcp) = state.take_mcp_session(&session.id) {
        mcp.lock().await.shutdown().await;
    }
    state.remove_session_runtime(&session.id);

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
        let names = vec!["fs".to_string(), "ghost".to_string(), "search".to_string()];
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
