//! Client session endpoints (`/api/v1/sessions`).
//!
//! The full client-facing surface: exchange a client key for a session, drive
//! the message loop, replay the event log, and close the session. Auth is a
//! bearer key on every request — the client key on session creation, the
//! session key on everything else — verified with Argon2id in constant time,
//! filtered by `role` and `deleted_at IS NULL` in the lookup query.

use std::collections::HashSet;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::error::ApiError;
use crate::api::pagination::{next_cursor, PageQuery};
use crate::api::AppState;
use crate::engine::session::{self, Outcome};
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
fn auth_session(
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
    // one lock.
    let session = state.store.with_conn(|c| {
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
        sessions::insert_event(
            c,
            &session.id,
            Some(&client_key.id),
            EventType::SessionOpen,
            &json!({ "client_version": body.client_version, "tools": tool_names }),
        )
        .map_err(ApiError::from_db)?;
        Ok::<_, ApiError>(session)
    })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "session_id": session.id,
            "session_key": session_key.plaintext,
            "profile": public_profile(&profile),
        })),
    ))
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
// POST /api/v1/sessions/{id}/messages
// ---------------------------------------------------------------------------

/// `POST /api/v1/sessions/{id}/messages` body.
#[derive(Debug, Deserialize)]
pub struct PostMessage {
    pub message: MessageBody,
}

#[derive(Debug, Deserialize)]
pub struct MessageBody {
    #[serde(default = "default_role")]
    pub role: String,
    pub content: Value,
}

fn default_role() -> String {
    "user".to_string()
}

pub async fn post_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PostMessage>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (session, _key) = auth_session(&state, &headers, &id)?;
    if session.state != STATE_OPEN {
        return Err(ApiError::conflict(
            "session_closed",
            format!("session is {}, not open", session.state),
        ));
    }

    let mut events: Vec<EventRecord> = Vec::new();

    // Record the incoming client turn. If it carries tool_result blocks (the
    // client returning output for a prior tool.call), log those explicitly too.
    let msg_payload = json!({ "role": body.message.role, "content": body.message.content });
    events.push(
        state
            .store
            .with_conn(|c| {
                sessions::insert_event(
                    c,
                    &session.id,
                    Some(&session.client_key_id),
                    EventType::ClientMessageSend,
                    &msg_payload,
                )
            })
            .map_err(ApiError::from_db)?,
    );
    for block in tool_result_blocks(&body.message.content) {
        events.push(
            state
                .store
                .with_conn(|c| {
                    sessions::insert_event(
                        c,
                        &session.id,
                        Some(&session.client_key_id),
                        EventType::ToolResult,
                        &block,
                    )
                })
                .map_err(ApiError::from_db)?,
        );
    }

    // The profile could have been deleted mid-session.
    let profile = state
        .store
        .with_conn(|c| profiles::get(c, &session.profile_id))
        .map_err(ApiError::from_db)?;
    let profile = match profile {
        Some(p) => p,
        None => {
            let ev = state
                .store
                .with_conn(|c| {
                    sessions::insert_event(
                        c,
                        &session.id,
                        Some(&session.client_key_id),
                        EventType::SessionError,
                        &json!({ "reason": "profile_unavailable", "profile_id": session.profile_id }),
                    )
                })
                .map_err(ApiError::from_db)?;
            events.push(ev);
            let _ = state
                .store
                .with_conn(|c| sessions::close_session(c, &session.id, STATE_ERROR));
            return Err(ApiError::unprocessable(
                "profile_unavailable",
                "the profile bound to this session is no longer available",
            ));
        }
    };

    // Run the loop and merge its events after the client turn.
    let turn = session::run_turn(&state.store, &state.http, &session, &profile)
        .await
        .map_err(|e| {
            tracing::error!("session loop failed: {e}");
            ApiError::internal("session loop failed")
        })?;
    events.extend(turn.events);

    let events_json: Vec<Value> = events.iter().map(event_view).collect();
    let status = match turn.outcome {
        Outcome::Completed | Outcome::Paused => StatusCode::OK,
        Outcome::ProvidersFailed => StatusCode::BAD_GATEWAY,
    };
    Ok((
        status,
        Json(json!({
            "message": turn.message,
            "events": events_json,
        })),
    ))
}

/// Extract `tool_result` blocks from a message content value, as `tool.result`
/// event payloads. A plain string or a block-less array yields nothing.
fn tool_result_blocks(content: &Value) -> Vec<Value> {
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
fn event_view(e: &EventRecord) -> Value {
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
    if !closed {
        return Err(ApiError::conflict(
            "session_closed",
            format!("session is already {}", session.state),
        ));
    }
    state
        .store
        .with_conn(|c| {
            sessions::insert_event(
                c,
                &session.id,
                Some(&session.client_key_id),
                EventType::SessionClose,
                &json!({ "reason": "client_close" }),
            )
        })
        .map_err(ApiError::from_db)?;
    Ok(Json(
        json!({ "session_id": session.id, "state": STATE_CLOSED }),
    ))
}
