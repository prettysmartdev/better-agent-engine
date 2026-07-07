//! Session and event persistence.
//!
//! Sessions bind a client key to a profile for the life of a conversation; the
//! `session_events` table is the **append-only** log of everything that happens
//! within one — client turns, provider requests/responses, tool calls/results,
//! MCP exchanges, and lifecycle events. There is no update or delete path for
//! events: history is only ever added to (per `aspec/architecture/design.md`).

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::{generate_id, NOW_SQL};
use crate::events::EventType;

/// Prefix on every session id.
pub const SESSION_ID_PREFIX: &str = "ses_";
/// Prefix on every event id.
pub const EVENT_ID_PREFIX: &str = "evt_";

/// Lifecycle state of a session (mirrors the `sessions.state` CHECK constraint).
pub const STATE_OPEN: &str = "open";
pub const STATE_CLOSED: &str = "closed";
pub const STATE_ERROR: &str = "error";

/// A session row, with `client_tools` parsed from its JSON blob.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub client_key_id: String,
    pub profile_id: String,
    pub state: String,
    pub client_version: Option<String>,
    pub client_tools: Value,
    pub created_at: String,
    pub closed_at: Option<String>,
}

/// A `session_events` row with its `payload` parsed back into JSON.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub id: String,
    pub session_id: String,
    pub client_key_id: Option<String>,
    pub event_type: String,
    pub payload: Value,
    pub created_at: String,
}

const SESSION_COLS: &str =
    "id, client_key_id, profile_id, state, client_version, client_tools, created_at, closed_at";

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get("id")?,
        client_key_id: row.get("client_key_id")?,
        profile_id: row.get("profile_id")?,
        state: row.get("state")?,
        client_version: row.get("client_version")?,
        client_tools: row
            .get::<_, String>("client_tools")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null),
        created_at: row.get("created_at")?,
        closed_at: row.get("closed_at")?,
    })
}

/// Create a session row.
#[allow(clippy::too_many_arguments)]
pub fn create_session(
    conn: &Connection,
    client_key_id: &str,
    profile_id: &str,
    state: &str,
    client_version: Option<&str>,
    client_tools: &Value,
) -> rusqlite::Result<SessionRecord> {
    let id = generate_id(SESSION_ID_PREFIX);
    let sql = format!(
        "INSERT INTO sessions \
           (id, client_key_id, profile_id, state, client_version, client_tools, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, {NOW_SQL}) \
         RETURNING {SESSION_COLS}"
    );
    conn.query_row(
        &sql,
        params![
            id,
            client_key_id,
            profile_id,
            state,
            client_version,
            client_tools.to_string(),
        ],
        row_to_session,
    )
}

/// Fetch a session by id (any state).
pub fn get_session(conn: &Connection, id: &str) -> rusqlite::Result<Option<SessionRecord>> {
    let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1");
    conn.query_row(&sql, params![id], row_to_session).optional()
}

/// Move a session to a terminal state and stamp `closed_at`. No-op on a session
/// that is already not `open`; returns whether the transition happened.
pub fn close_session(conn: &Connection, id: &str, state: &str) -> rusqlite::Result<bool> {
    let sql = format!(
        "UPDATE sessions SET state = ?2, closed_at = {NOW_SQL} \
         WHERE id = ?1 AND state = '{STATE_OPEN}'"
    );
    let n = conn.execute(&sql, params![id, state])?;
    Ok(n > 0)
}

/// Append an event to the log. This is the **only** write path for
/// `session_events`; there is deliberately no update or delete counterpart.
pub fn insert_event(
    conn: &Connection,
    session_id: &str,
    client_key_id: Option<&str>,
    event_type: EventType,
    payload: &Value,
) -> rusqlite::Result<EventRecord> {
    let id = generate_id(EVENT_ID_PREFIX);
    let payload_str = payload.to_string();
    let sql = format!(
        "INSERT INTO session_events \
           (id, session_id, client_key_id, event_type, payload, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, {NOW_SQL}) \
         RETURNING created_at"
    );
    let created_at: String = conn.query_row(
        &sql,
        params![
            id,
            session_id,
            client_key_id,
            event_type.as_str(),
            payload_str
        ],
        |r| r.get(0),
    )?;
    // The full event stream at TRACE: every persisted event with its complete
    // payload flows through here (this is the only session_events write path).
    tracing::trace!(
        session_id,
        event_id = %id,
        event_type = %event_type,
        payload = %payload_str,
        "session event"
    );
    Ok(EventRecord {
        id,
        session_id: session_id.to_owned(),
        client_key_id: client_key_id.map(str::to_owned),
        event_type: event_type.as_str().to_owned(),
        payload: payload.clone(),
        created_at,
    })
}

/// Reconstruct conversation history for a session by **streaming** the two
/// message event types in insertion order. Only `client.message.send` and
/// `server.message.send` rows are selected — large `provider.response` /
/// `tool.result` payloads are never pulled into memory — and rows are consumed
/// one at a time from the SQLite cursor rather than collected wholesale.
///
/// Each row becomes a provider-shaped `{role, content}` message: a
/// `client.message.send` keeps its stored role (defaulting to `user`), a
/// `server.message.send` is always `assistant`.
pub fn stream_history(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<Value>> {
    let sql = "SELECT event_type, payload FROM session_events \
               WHERE session_id = ?1 \
                 AND event_type IN ('client.message.send', 'server.message.send') \
               ORDER BY rowid";
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params![session_id])?;
    let mut history = Vec::new();
    while let Some(row) = rows.next()? {
        let event_type: String = row.get(0)?;
        let payload: String = row.get(1)?;
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let content = payload.get("content").cloned().unwrap_or(Value::Null);
        let role = if event_type == EventType::ServerMessageSend.as_str() {
            "assistant".to_string()
        } else {
            payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .to_string()
        };
        history.push(serde_json::json!({ "role": role, "content": content }));
    }
    Ok(history)
}

/// Resolve an event id to its rowid within a session, for `since_event_id`
/// replay. Returns `None` if no event with that id exists in the session (a
/// stale or foreign id), letting the caller fall back to replaying from the
/// start.
pub fn rowid_of_event(
    conn: &Connection,
    session_id: &str,
    event_id: &str,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT rowid FROM session_events WHERE session_id = ?1 AND id = ?2",
        params![session_id, event_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()
}

/// One page of a session's events, ordered by insertion (rowid), for replay.
/// `after` is the exclusive rowid cursor; `limit` rows are returned. The bool is
/// true when more rows remain after this page.
pub fn list_events(
    conn: &Connection,
    session_id: &str,
    after: i64,
    limit: i64,
) -> rusqlite::Result<(Vec<(i64, EventRecord)>, bool)> {
    let sql = "SELECT rowid, id, session_id, client_key_id, event_type, payload, created_at \
               FROM session_events \
               WHERE session_id = ?1 AND rowid > ?2 \
               ORDER BY rowid LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![session_id, after, limit + 1], |row| {
        let payload: String = row.get("payload")?;
        Ok((
            row.get::<_, i64>(0)?,
            EventRecord {
                id: row.get("id")?,
                session_id: row.get("session_id")?,
                client_key_id: row.get("client_key_id")?,
                event_type: row.get("event_type")?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                created_at: row.get("created_at")?,
            },
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    let has_more = out.len() as i64 > limit;
    out.truncate(limit as usize);
    Ok((out, has_more))
}
