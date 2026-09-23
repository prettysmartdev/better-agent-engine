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
use crate::api::client::sessions::CompactionConfig;
use crate::events::EventType;

/// Prefix on every session id.
pub const SESSION_ID_PREFIX: &str = "ses_";
/// Prefix on every event id.
pub const EVENT_ID_PREFIX: &str = "evt_";
/// Prefix on every remotely tracked subagent id.
pub const SUBAGENT_ID_PREFIX: &str = "sba_";

/// Lifecycle state of a session (mirrors the `sessions.state` CHECK constraint).
pub const STATE_OPEN: &str = "open";
pub const STATE_CLOSED: &str = "closed";
pub const STATE_ERROR: &str = "error";

/// A session row, with `client_tools` parsed from its JSON blob.
///
/// `client_tools` is a JSON **object keyed by client key id** — each
/// participating client's declared tool list lives under its own
/// `client_key_id` key (`{"key_abc": [{tool def}, …], "key_def": […]}`), so
/// per-client tool sets are never merged. `client_key_id` remains the key
/// that *created* the session; further participants are recorded by their
/// session keys and `session.join` events.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub client_key_id: String,
    pub profile_id: String,
    pub state: String,
    pub client_version: Option<String>,
    pub client_tools: Value,
    /// Per-client Auto-mode sandbox tool declarations, keyed by client key id
    /// exactly like [`SessionRecord::client_tools`] but kept in a sibling
    /// column so the two tool kinds are never confused. These tools are
    /// dispatched **server-side** against the session's remote sandbox by
    /// `run_turn`, never returned to the client.
    pub sandbox_tools: Value,
    /// Per-client remote-subagent declarations. Unlike `client_tools`, these
    /// are executed by the server in the session's existing remote sandbox.
    pub subagent_tools: Value,
    /// Session-level compaction config, fixed at creation. A newly created row
    /// is always `Some`; a pre-migration or hand-seeded NULL/malformed value
    /// reads as `None` and is semantically equivalent to client mode with no
    /// stored prompt.
    pub compaction: Option<CompactionConfig>,
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

const SESSION_COLS: &str = "id, client_key_id, profile_id, state, client_version, client_tools, \
     sandbox_tools, subagent_tools, compaction, created_at, closed_at";

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
        sandbox_tools: row
            .get::<_, String>("sandbox_tools")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null),
        subagent_tools: row
            .get::<_, String>("subagent_tools")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null),
        // NULL (pre-migration/hand-seeded) or malformed JSON reads back as
        // `None` — semantically client mode with no stored prompt.
        compaction: row
            .get::<_, Option<String>>("compaction")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        created_at: row.get("created_at")?,
        closed_at: row.get("closed_at")?,
    })
}

/// Create a session row. `client_tools` is the creating client's declared tool
/// list; it is stored under the creator's own `client_key_id` key in the
/// per-client `client_tools` object (see [`SessionRecord`]), never as a bare
/// array — further participants add their own entries via
/// [`set_client_tools`]. `sandbox_tools` is the creator's Auto-mode sandbox
/// tool list, stored the same per-client way in its own sibling column.
#[allow(clippy::too_many_arguments)]
pub fn create_session(
    conn: &Connection,
    client_key_id: &str,
    profile_id: &str,
    state: &str,
    client_version: Option<&str>,
    client_tools: &Value,
    sandbox_tools: &Value,
    subagent_tools: &Value,
    compaction: &CompactionConfig,
) -> rusqlite::Result<SessionRecord> {
    let id = generate_id(SESSION_ID_PREFIX);
    let tools_by_client = serde_json::json!({ client_key_id: client_tools });
    let sandbox_by_client = serde_json::json!({ client_key_id: sandbox_tools });
    let subagent_by_client = serde_json::json!({ client_key_id: subagent_tools });
    // Serialize the normalized compaction config to JSON TEXT. Serialization of
    // this small tagged enum cannot fail; fall back to the client-mode default
    // rather than panicking if it somehow did.
    let compaction_json = serde_json::to_string(compaction)
        .unwrap_or_else(|_| serde_json::to_string(&CompactionConfig::default()).unwrap());
    let sql = format!(
        "INSERT INTO sessions \
           (id, client_key_id, profile_id, state, client_version, client_tools, \
            sandbox_tools, subagent_tools, compaction, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, {NOW_SQL}) \
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
            tools_by_client.to_string(),
            sandbox_by_client.to_string(),
            subagent_by_client.to_string(),
            compaction_json,
        ],
        row_to_session,
    )
}

/// Upsert **one** client's entry in a session's per-client `client_tools`
/// object. Only `client_key_id`'s own entry is written: other clients' entries
/// are never read into the new value, merged, or overwritten, and a second
/// call for the same client *replaces* (does not merge with) that client's
/// prior list. A stored value that is not an object (pre-multi-client rows)
/// is replaced by a fresh object holding only this client's entry.
pub fn set_client_tools(
    conn: &Connection,
    session_id: &str,
    client_key_id: &str,
    tools: &Value,
) -> rusqlite::Result<()> {
    set_per_client_column(conn, session_id, "client_tools", client_key_id, tools)
}

/// The `sandbox_tools` twin of [`set_client_tools`]: upsert one client's
/// Auto-mode sandbox tool declarations in the sibling per-client object.
/// Identical semantics — only the target client's entry is written, and a
/// repeat call replaces (never merges) that client's own list.
pub fn set_client_sandbox_tools(
    conn: &Connection,
    session_id: &str,
    client_key_id: &str,
    tools: &Value,
) -> rusqlite::Result<()> {
    set_per_client_column(conn, session_id, "sandbox_tools", client_key_id, tools)
}

/// The `subagent_tools` twin: upsert one client's remote-subagent
/// declarations without affecting any other driver's entry.
pub fn set_client_subagent_tools(
    conn: &Connection,
    session_id: &str,
    client_key_id: &str,
    tools: &Value,
) -> rusqlite::Result<()> {
    set_per_client_column(conn, session_id, "subagent_tools", client_key_id, tools)
}

/// Shared upsert behind [`set_client_tools`] / [`set_client_sandbox_tools`].
/// `column` is a compile-time constant from those two callers only — never
/// caller-supplied input — so the `format!` cannot inject.
fn set_per_client_column(
    conn: &Connection,
    session_id: &str,
    column: &'static str,
    client_key_id: &str,
    tools: &Value,
) -> rusqlite::Result<()> {
    let current: Option<String> = conn.query_row(
        &format!("SELECT {column} FROM sessions WHERE id = ?1"),
        params![session_id],
        |r| r.get(0),
    )?;
    let mut by_client = current
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    by_client.insert(client_key_id.to_owned(), tools.clone());
    conn.execute(
        &format!("UPDATE sessions SET {column} = ?2 WHERE id = ?1"),
        params![session_id, Value::Object(by_client).to_string()],
    )?;
    Ok(())
}

/// Fetch a session by id (any state).
pub fn get_session(conn: &Connection, id: &str) -> rusqlite::Result<Option<SessionRecord>> {
    let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1");
    conn.query_row(&sql, params![id], row_to_session).optional()
}

/// One page of sessions ordered by insertion (rowid), optionally filtered to a
/// single lifecycle `state` (`open`/`closed`/`error`). `after` is the exclusive
/// rowid cursor; `limit` rows are returned. The returned bool is true when more
/// rows remain after this page. Mirrors [`crate::store::profiles::list`]'s
/// cursor-pagination shape (fetch `limit + 1` to detect a further page).
pub fn list_sessions(
    conn: &Connection,
    after: i64,
    limit: i64,
    state: Option<&str>,
) -> rusqlite::Result<(Vec<(i64, SessionRecord)>, bool)> {
    use rusqlite::types::ToSql;

    let state_clause = if state.is_some() {
        "AND state = ?3"
    } else {
        ""
    };
    let sql = format!(
        "SELECT rowid, {SESSION_COLS} FROM sessions \
         WHERE rowid > ?1 {state_clause} \
         ORDER BY rowid LIMIT ?2"
    );
    let fetch = limit + 1;
    let mut sql_params: Vec<&dyn ToSql> = vec![&after, &fetch];
    if let Some(ref s) = state {
        sql_params.push(s);
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(sql_params.as_slice(), |row| {
        Ok((row.get::<_, i64>(0)?, row_to_session(row)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    let has_more = out.len() as i64 > limit;
    out.truncate(limit as usize);
    Ok((out, has_more))
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
/// Each row becomes a provider-shaped `{role, content}` message whose role is
/// taken from the payload: a `client.message.send` defaults to `user`, a
/// `server.message.send` to `assistant` — but a server message may carry
/// `role: "user"` (the compaction preamble, or the synthetic tool results that
/// retire an abandoned pause), and it replays as a `user` message. Any other
/// payload field (e.g. `synthetic`) never reaches the provider.
pub fn stream_history(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<Value>> {
    // Scope the message scan to the most recent *completed* compaction, if any.
    // The lower bound is the compaction preamble's own rowid (not the completed
    // marker's): the preamble and the summary are inserted just *before* the
    // completed marker, so replay starts `user(preamble) → assistant(summary)`
    // followed by everything sent after it.
    let lower_bound = history_lower_bound(conn, session_id)?;
    let sql = "SELECT event_type, payload FROM session_events \
               WHERE session_id = ?1 \
                 AND event_type IN ('client.message.send', 'server.message.send') \
                 AND rowid >= ?2 \
               ORDER BY rowid";
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params![session_id, lower_bound])?;
    let mut history = Vec::new();
    while let Some(row) = rows.next()? {
        let event_type: String = row.get(0)?;
        let payload: String = row.get(1)?;
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let content = payload.get("content").cloned().unwrap_or(Value::Null);
        let default_role = if event_type == EventType::ServerMessageSend.as_str() {
            "assistant"
        } else {
            "user"
        };
        let role = payload
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(default_role)
            .to_string();
        history.push(serde_json::json!({ "role": role, "content": content }));
    }
    Ok(history)
}

/// The `tool_use` blocks of the session's most recent **assistant**
/// `server.message.send` (the paused assistant turn), in their original order,
/// each retaining its `id`/`name`/`dispatch` fields. Server messages with
/// `role: "user"` (synthetic ones such as the compaction preamble) are skipped.
/// Empty when there is no such event or it carried no `tool_use` blocks.
///
/// Used at mixed-turn resume to know exactly which `tool_use` ids the merged
/// `user` turn must answer, and in what order, without loading full history.
pub fn last_assistant_tool_uses(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<Value>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM session_events \
             WHERE session_id = ?1 AND event_type = 'server.message.send' \
               AND coalesce(json_extract(payload, '$.role'), 'assistant') = 'assistant' \
             ORDER BY rowid DESC LIMIT 1",
            params![session_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(Vec::new());
    };
    let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
    let blocks = payload
        .get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Ok(blocks)
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

/// Best-effort accumulated context token count for a session: the `input_tokens
/// + output_tokens` recorded on the `usage` field (added by the engine) of the
/// session's newest **successful** (`ok: true`) `provider.response` that is not
/// a compaction call's own response (`purpose: "compaction"`) — a failed
/// attempt carries no usage, and a summary call's usage measures the old
/// history plus the instruction, not the session's current context. Returns
/// `None` when there is no such response yet, or the newest one has no usable
/// `usage` (`usage: null`, or a malformed/partial object) — it does **not**
/// search further back for an older usable response. Used only for the
/// informational `token_count` on a manually-triggered
/// `session.compaction.started` event.
pub fn last_provider_token_count(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<u64>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM session_events \
             WHERE session_id = ?1 AND event_type = 'provider.response' \
               AND json_extract(payload, '$.ok') = 1 \
               AND json_extract(payload, '$.purpose') IS NULL \
             ORDER BY rowid DESC LIMIT 1",
            params![session_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
    let usage = payload.get("usage");
    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64);
    match (input, output) {
        (Some(i), Some(o)) => Ok(Some(i.saturating_add(o))),
        _ => Ok(None),
    }
}

/// The inclusive rowid lower bound for [`stream_history`]'s provider-facing
/// message scan, from the most recent `session.compaction.completed` marker:
///
/// 1. `payload.preamble_event_id` resolves in this session to a
///    `server.message.send` → that rowid (replay starts at the preamble).
/// 2. Else `payload.summary_event_id` resolves to a message event → that rowid
///    (sessions compacted before the preamble existed; the Anthropic request
///    normalizer keeps their assistant-first replay valid).
/// 3. Else `0` (full history).
///
/// With no completed marker, a malformed payload, or a missing/foreign
/// reference the result is the full history — never dropping context on a
/// broken reference.
fn history_lower_bound(conn: &Connection, session_id: &str) -> rusqlite::Result<i64> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM session_events \
             WHERE session_id = ?1 AND event_type = 'session.compaction.completed' \
             ORDER BY rowid DESC LIMIT 1",
            params![session_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(0);
    };
    let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
    // Resolve an id to its rowid and event type within this session.
    let resolve = |id: &str| -> rusqlite::Result<Option<(i64, String)>> {
        conn.query_row(
            "SELECT rowid, event_type FROM session_events \
             WHERE session_id = ?1 AND id = ?2",
            params![session_id, id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
    };
    if let Some(preamble_id) = payload.get("preamble_event_id").and_then(Value::as_str) {
        if let Some((rowid, event_type)) = resolve(preamble_id)? {
            if event_type == EventType::ServerMessageSend.as_str() {
                return Ok(rowid);
            }
        }
    }
    let Some(summary_id) = payload.get("summary_event_id").and_then(Value::as_str) else {
        return Ok(0);
    };
    match resolve(summary_id)? {
        Some((rowid, event_type))
            if event_type == EventType::ClientMessageSend.as_str()
                || event_type == EventType::ServerMessageSend.as_str() =>
        {
            Ok(rowid)
        }
        _ => Ok(0),
    }
}

/// The three records a successful compaction commits, in insertion order.
#[derive(Debug, Clone)]
pub struct CompactionRecords {
    pub preamble: EventRecord,
    pub summary: EventRecord,
    pub completed: EventRecord,
}

/// Insert a successful compaction's preamble, summary, and
/// `session.compaction.completed` marker in **one** `IMMEDIATE` transaction, in
/// that order. `completed` is the marker's payload minus the two pointers;
/// `preamble_event_id` and `summary_event_id` are filled in here from the rows
/// just inserted. On any error the transaction rolls back, so no orphan
/// preamble or summary is ever visible to [`stream_history`]. The caller
/// publishes the returned records only after this returns `Ok` (i.e. after
/// commit).
pub fn insert_compaction_records(
    conn: &Connection,
    session_id: &str,
    client_key_id: Option<&str>,
    preamble: &Value,
    summary: &Value,
    completed: &Value,
) -> rusqlite::Result<CompactionRecords> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let preamble = insert_event(
        &tx,
        session_id,
        client_key_id,
        EventType::ServerMessageSend,
        preamble,
    )?;
    let summary = insert_event(
        &tx,
        session_id,
        client_key_id,
        EventType::ServerMessageSend,
        summary,
    )?;
    let mut completed = completed.clone();
    if let Some(obj) = completed.as_object_mut() {
        obj.insert("preamble_event_id".into(), Value::from(preamble.id.clone()));
        obj.insert("summary_event_id".into(), Value::from(summary.id.clone()));
    }
    let completed = insert_event(
        &tx,
        session_id,
        client_key_id,
        EventType::SessionCompactionCompleted,
        &completed,
    )?;
    tx.commit()?;
    Ok(CompactionRecords {
        preamble,
        summary,
        completed,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use serde_json::json;

    /// Insert the `profiles`/`keys` parent rows the `sessions` foreign keys
    /// require (`profile_id → profiles(id)`, `client_key_id → keys(id)`).
    fn seed_parents(c: &Connection) {
        c.execute_batch(
            "INSERT INTO profiles (id, name) VALUES ('pro_1', 'p');\n\
             INSERT INTO keys (id, role) VALUES ('key_a', 'client');",
        )
        .unwrap();
    }

    fn test_session(c: &Connection) -> SessionRecord {
        seed_parents(c);
        create_session(
            c,
            "key_a",
            "pro_1",
            STATE_OPEN,
            None,
            &json!([]),
            &json!([]),
            &json!([]),
            &CompactionConfig::default(),
        )
        .unwrap()
    }

    fn message(
        c: &Connection,
        session_id: &str,
        event_type: EventType,
        content: &str,
    ) -> EventRecord {
        let role = if event_type == EventType::ServerMessageSend {
            "assistant"
        } else {
            "user"
        };
        insert_event(
            c,
            session_id,
            Some("key_a"),
            event_type,
            &json!({ "role": role, "content": content }),
        )
        .unwrap()
    }

    fn completed(c: &Connection, session_id: &str, summary_event_id: &str) -> EventRecord {
        insert_event(
            c,
            session_id,
            Some("key_a"),
            EventType::SessionCompactionCompleted,
            &json!({
                "summary_event_id": summary_event_id,
                "compacted_message_count": 1,
                "input_tokens": 100,
                "summary_tokens": 10,
            }),
        )
        .unwrap()
    }

    #[test]
    fn stream_history_returns_full_message_history_without_compaction() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            message(c, &session.id, EventType::ClientMessageSend, "first");
            message(c, &session.id, EventType::ServerMessageSend, "answer");

            assert_eq!(
                stream_history(c, &session.id).unwrap(),
                vec![
                    json!({ "role": "user", "content": "first" }),
                    json!({ "role": "assistant", "content": "answer" }),
                ]
            );
        });
    }

    #[test]
    fn stream_history_starts_at_compacted_summary_and_keeps_later_messages() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            message(c, &session.id, EventType::ClientMessageSend, "before");
            let summary = message(c, &session.id, EventType::ServerMessageSend, "summary");
            completed(c, &session.id, &summary.id);
            message(c, &session.id, EventType::ClientMessageSend, "after");

            assert_eq!(
                stream_history(c, &session.id).unwrap(),
                vec![
                    json!({ "role": "assistant", "content": "summary" }),
                    json!({ "role": "user", "content": "after" }),
                ]
            );
        });
    }

    #[test]
    fn stream_history_uses_only_the_most_recent_compaction_summary() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            message(c, &session.id, EventType::ClientMessageSend, "before-1");
            let summary_one = message(c, &session.id, EventType::ServerMessageSend, "summary-1");
            completed(c, &session.id, &summary_one.id);
            message(c, &session.id, EventType::ClientMessageSend, "between");
            let summary_two = message(c, &session.id, EventType::ServerMessageSend, "summary-2");
            completed(c, &session.id, &summary_two.id);
            message(c, &session.id, EventType::ClientMessageSend, "after-2");

            assert_eq!(
                stream_history(c, &session.id).unwrap(),
                vec![
                    json!({ "role": "assistant", "content": "summary-2" }),
                    json!({ "role": "user", "content": "after-2" }),
                ]
            );
        });
    }

    /// A `server.message.send` carrying `role: "user"` (the compaction
    /// preamble) replays as a `user` message, and history starts at the
    /// preamble named by the newest completed marker.
    #[test]
    fn stream_history_starts_at_the_compaction_preamble() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            message(c, &session.id, EventType::ClientMessageSend, "before");
            message(c, &session.id, EventType::ServerMessageSend, "old answer");
            insert_compaction_records(
                c,
                &session.id,
                Some("key_a"),
                &json!({ "role": "user", "content": "preamble", "synthetic": "compaction_preamble" }),
                &json!({ "role": "assistant", "content": "summary" }),
                &json!({ "compacted_message_count": 2 }),
            )
            .unwrap();
            message(c, &session.id, EventType::ClientMessageSend, "after");

            assert_eq!(
                stream_history(c, &session.id).unwrap(),
                vec![
                    json!({ "role": "user", "content": "preamble" }),
                    json!({ "role": "assistant", "content": "summary" }),
                    json!({ "role": "user", "content": "after" }),
                ]
            );
        });
    }

    /// A preamble reference that does not resolve falls back to the summary
    /// bound; a summary reference that does not resolve either means the
    /// full history.
    #[test]
    fn history_bound_falls_back_from_preamble_to_summary_to_full() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            message(c, &session.id, EventType::ClientMessageSend, "before");
            let summary = message(c, &session.id, EventType::ServerMessageSend, "summary");
            insert_event(
                c,
                &session.id,
                Some("key_a"),
                EventType::SessionCompactionCompleted,
                &json!({ "preamble_event_id": "evt_missing", "summary_event_id": summary.id }),
            )
            .unwrap();
            assert_eq!(
                stream_history(c, &session.id).unwrap(),
                vec![json!({ "role": "assistant", "content": "summary" })]
            );
            insert_event(
                c,
                &session.id,
                Some("key_a"),
                EventType::SessionCompactionCompleted,
                &json!({ "preamble_event_id": "evt_missing", "summary_event_id": "evt_gone" }),
            )
            .unwrap();
            assert_eq!(stream_history(c, &session.id).unwrap().len(), 2);
        });
    }

    /// `stream_history` takes `role` from the payload and defaults it by event
    /// type when absent; `synthetic` never reaches the replayed message.
    #[test]
    fn stream_history_defaults_role_by_event_type_and_drops_synthetic() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            for (event_type, payload) in [
                (EventType::ClientMessageSend, json!({ "content": "q" })),
                (EventType::ServerMessageSend, json!({ "content": "a" })),
                (
                    EventType::ServerMessageSend,
                    json!({ "role": "user", "content": "u", "synthetic": "abandoned_tool_results" }),
                ),
            ] {
                insert_event(c, &session.id, Some("key_a"), event_type, &payload).unwrap();
            }
            assert_eq!(
                stream_history(c, &session.id).unwrap(),
                vec![
                    json!({ "role": "user", "content": "q" }),
                    json!({ "role": "assistant", "content": "a" }),
                    json!({ "role": "user", "content": "u" }),
                ]
            );
        });
    }

    /// The token count behind the auto trigger and the manual `started` event
    /// comes from the newest *successful, non-compaction* provider response.
    #[test]
    fn last_provider_token_count_skips_failed_and_compaction_responses() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            let response = |payload: Value| {
                insert_event(
                    c,
                    &session.id,
                    Some("key_a"),
                    EventType::ProviderResponse,
                    &payload,
                )
                .unwrap();
            };
            assert_eq!(last_provider_token_count(c, &session.id).unwrap(), None);
            response(json!({ "ok": true, "usage": { "input_tokens": 900, "output_tokens": 200 } }));
            response(json!({ "ok": false, "usage": { "input_tokens": 7, "output_tokens": 7 } }));
            response(json!({ "ok": true, "purpose": "compaction",
                             "usage": { "input_tokens": 50000, "output_tokens": 100 } }));
            response(json!({ "ok": false, "purpose": "compaction" }));
            assert_eq!(
                last_provider_token_count(c, &session.id).unwrap(),
                Some(1100)
            );
            response(json!({ "ok": true }));
            assert_eq!(
                last_provider_token_count(c, &session.id).unwrap(),
                None,
                "the newest ordinary response carried no usage"
            );
        });
    }

    /// A session row written before migration 0009 (NULL `compaction`) and a
    /// hand-seeded malformed value both read back as `None`.
    #[test]
    fn pre_0009_null_compaction_column_reads_as_none() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            assert!(get_session(c, &session.id)
                .unwrap()
                .unwrap()
                .compaction
                .is_some());
            c.execute(
                "UPDATE sessions SET compaction = NULL WHERE id = ?1",
                [&session.id],
            )
            .unwrap();
            let read = get_session(c, &session.id).unwrap().unwrap();
            assert!(read.compaction.is_none());
            assert_eq!(read.id, session.id);
            c.execute(
                "UPDATE sessions SET compaction = 'not json' WHERE id = ?1",
                [&session.id],
            )
            .unwrap();
            assert!(get_session(c, &session.id)
                .unwrap()
                .unwrap()
                .compaction
                .is_none());
        });
    }

    /// A failure while inserting the completed marker rolls back the preamble
    /// and summary written before it in the same transaction.
    #[test]
    fn insert_compaction_records_is_atomic() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            let session = test_session(c);
            message(c, &session.id, EventType::ClientMessageSend, "before");
            c.execute_batch(
                "CREATE TEMP TRIGGER fail_completed BEFORE INSERT ON session_events \
                 WHEN NEW.event_type = 'session.compaction.completed' \
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
            let err = insert_compaction_records(
                c,
                &session.id,
                Some("key_a"),
                &json!({ "role": "user", "content": "preamble" }),
                &json!({ "role": "assistant", "content": "summary" }),
                &json!({}),
            );
            assert!(err.is_err());
            assert_eq!(
                stream_history(c, &session.id).unwrap(),
                vec![json!({ "role": "user", "content": "before" })],
                "no orphan preamble or summary survives the rollback"
            );
            assert!(c.is_autocommit(), "the transaction was closed");
        });
    }

    /// `set_client_tools` writes exactly one client's entry in the per-client
    /// `client_tools` object: other clients' entries are left byte-for-byte
    /// untouched, and a second call for the same client *replaces* (does not
    /// merge with) that client's own prior list.
    #[test]
    fn set_client_tools_upserts_only_the_target_client_and_replaces_on_repeat() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            seed_parents(c);
            // The creator ("key_a") declares one tool; create_session stores it
            // under key_a inside the per-client object.
            let session = create_session(
                c,
                "key_a",
                "pro_1",
                STATE_OPEN,
                Some("1.0.0"),
                &json!([{ "name": "only_a" }]),
                &json!([]),
                &json!([]),
                &CompactionConfig::default(),
            )
            .unwrap();

            // A joiner ("key_b") upserts its own, independent entry.
            set_client_tools(c, &session.id, "key_b", &json!([{ "name": "only_b" }])).unwrap();
            let row = get_session(c, &session.id).unwrap().unwrap();
            // Both entries are present, each under its own key; nothing merged.
            assert_eq!(row.client_tools["key_a"], json!([{ "name": "only_a" }]));
            assert_eq!(row.client_tools["key_b"], json!([{ "name": "only_b" }]));

            // Snapshot key_a's raw JSON to prove the next call leaves it identical.
            let key_a_before = row.client_tools["key_a"].clone();

            // A second call for key_b REPLACES its list — no merge with the prior
            // ["only_b"] — and never touches key_a.
            set_client_tools(c, &session.id, "key_b", &json!([{ "name": "replaced_b" }])).unwrap();
            let row = get_session(c, &session.id).unwrap().unwrap();
            assert_eq!(
                row.client_tools["key_b"],
                json!([{ "name": "replaced_b" }]),
                "a repeat call replaces, does not merge, the client's own list"
            );
            assert_eq!(
                row.client_tools["key_a"], key_a_before,
                "another client's entry is left byte-for-byte untouched"
            );
        });
    }

    /// `list_sessions` paginates by rowid (insertion order) and honours the
    /// optional lifecycle-state filter, reusing `SESSION_COLS`/`row_to_session`.
    #[test]
    fn list_sessions_paginates_and_filters_by_state() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            seed_parents(c);
            // Three sessions in insertion order: open, closed, error.
            let s_open = create_session(
                c,
                "key_a",
                "pro_1",
                STATE_OPEN,
                Some("1.0.0"),
                &json!([]),
                &json!([]),
                &json!([]),
                &CompactionConfig::default(),
            )
            .unwrap();
            let s_closed = create_session(
                c,
                "key_a",
                "pro_1",
                STATE_CLOSED,
                None,
                &json!([]),
                &json!([]),
                &json!([]),
                &CompactionConfig::default(),
            )
            .unwrap();
            let s_error = create_session(
                c,
                "key_a",
                "pro_1",
                STATE_ERROR,
                None,
                &json!([]),
                &json!([]),
                &json!([]),
                &CompactionConfig::default(),
            )
            .unwrap();

            // --- Pagination: first page of 2 signals has_more, then the tail. ---
            let (page1, has_more) = list_sessions(c, 0, 2, None).unwrap();
            assert_eq!(page1.len(), 2);
            assert!(has_more, "a third row remains after a page of 2");
            assert_eq!(page1[0].1.id, s_open.id);
            assert_eq!(page1[1].1.id, s_closed.id);

            let cursor = page1.last().unwrap().0;
            let (page2, has_more) = list_sessions(c, cursor, 2, None).unwrap();
            assert_eq!(page2.len(), 1, "one row left on the second page");
            assert!(!has_more, "no rows remain after the last page");
            assert_eq!(page2[0].1.id, s_error.id);
            // The row parses back through row_to_session with its fields intact.
            assert_eq!(page2[0].1.state, STATE_ERROR);
            assert_eq!(page2[0].1.profile_id, "pro_1");

            // --- State filter isolates a single lifecycle state. ---
            let (open_only, has_more) = list_sessions(c, 0, 50, Some(STATE_OPEN)).unwrap();
            assert_eq!(open_only.len(), 1);
            assert!(!has_more);
            assert_eq!(open_only[0].1.id, s_open.id);
            assert_eq!(open_only[0].1.client_version.as_deref(), Some("1.0.0"));

            let (closed_only, _) = list_sessions(c, 0, 50, Some(STATE_CLOSED)).unwrap();
            assert_eq!(closed_only.len(), 1);
            assert_eq!(closed_only[0].1.id, s_closed.id);

            // Scanning past the final row yields an empty page with no further
            // rows (the admin endpoint's empty-result path).
            let last_rowid = page2[0].0;
            let (empty, has_more) = list_sessions(c, last_rowid, 50, None).unwrap();
            assert!(empty.is_empty(), "no rows past the final cursor");
            assert!(!has_more);
        });
    }

    /// A stored non-object `client_tools` value (a pre-multi-client row) is
    /// replaced by a fresh object holding only this client's entry.
    #[test]
    fn set_client_tools_replaces_a_legacy_non_object_blob() {
        let store = Store::open_in_memory().unwrap();
        store.with_conn(|c| {
            seed_parents(c);
            let id = generate_id(SESSION_ID_PREFIX);
            // Simulate a legacy row whose client_tools is a bare array.
            c.execute(
                &format!(
                    "INSERT INTO sessions (id, client_key_id, profile_id, state, client_tools, created_at) \
                     VALUES (?1, 'key_a', 'pro_1', '{STATE_OPEN}', ?2, {NOW_SQL})"
                ),
                params![id, json!([{ "name": "legacy" }]).to_string()],
            )
            .unwrap();

            set_client_tools(c, &id, "key_b", &json!([{ "name": "only_b" }])).unwrap();
            let row = get_session(c, &id).unwrap().unwrap();
            assert!(row.client_tools.is_object(), "legacy blob is replaced by an object");
            assert_eq!(row.client_tools["key_b"], json!([{ "name": "only_b" }]));
            // The legacy bare array did not survive as a stray key.
            assert_eq!(row.client_tools.as_object().unwrap().len(), 1);
        });
    }
}
