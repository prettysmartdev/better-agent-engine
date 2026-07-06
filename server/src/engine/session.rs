//! The session message loop.
//!
//! [`run_turn`] drives one client turn to completion, following the "Session
//! message loop" section of `aspec/work-items/0002-session-and-auth.md`:
//!
//! 1. Reconstruct conversation history by **streaming** `client.message.send` /
//!    `server.message.send` events (never loading the whole log at once).
//! 2. Before each provider call, insert a `provider.request` event (the full
//!    request payload, minus the resolved auth token).
//! 3. Call the primary provider. On failure, insert a `provider.response`
//!    failure event and a `session.error` context event, then walk
//!    `fallback_configs` in order — inserting a `provider.response` for **every**
//!    attempt, success or failure — until one succeeds.
//! 4. On success, insert a `provider.response` with the raw body.
//! 5. If the response contains tool calls, insert a `tool.call` per call.
//!    Client-side tools (declared by the client at session open) are returned to
//!    the client for execution and the turn pauses. MCP tools take the stub path
//!    (`mcp.request` / `mcp.response` with `{status:"stub"}`, then `tool.result`)
//!    and the loop continues with the stub result appended.
//! 6. On a plain (no-tool) response, insert `server.message.send` and finish.
//!
//! The auth token is resolved inside [`super::provider::call`] and never reaches
//! this module, an event payload, or a log line.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::provider::{self, ProviderConfig};
use crate::events::EventType;
use crate::store::sessions::{self, EventRecord, SessionRecord, STATE_ERROR};
use crate::store::{profiles::ProfileRecord, Store};

/// Upper bound on provider round-trips within a single turn, so a provider that
/// keeps emitting MCP tool calls cannot spin forever.
const MAX_ITERATIONS: usize = 8;

/// How the turn ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The provider returned a final, tool-free assistant message.
    Completed,
    /// The provider requested client-side tools; the turn is paused until the
    /// client sends tool results back on a subsequent request.
    Paused,
    /// No provider (primary or fallback) succeeded; the session is now `error`.
    ProvidersFailed,
}

/// The result of one turn: the assistant message to return to the client, every
/// event inserted during the turn, and how it ended.
#[derive(Debug)]
pub struct Turn {
    pub message: Value,
    pub events: Vec<EventRecord>,
    pub outcome: Outcome,
}

/// A turn failed at the persistence layer (distinct from a provider failure,
/// which is a normal [`Outcome::ProvidersFailed`]).
#[derive(Debug)]
pub struct TurnError(pub rusqlite::Error);

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session store error: {}", self.0)
    }
}
impl std::error::Error for TurnError {}

/// Append an event and return the record. Centralises the `with_conn` +
/// error-mapping boilerplate the loop repeats.
fn log_event(
    store: &Store,
    session_id: &str,
    client_key_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<EventRecord, TurnError> {
    store
        .with_conn(|c| {
            sessions::insert_event(c, session_id, Some(client_key_id), event_type, &payload)
        })
        .map_err(TurnError)
}

/// Run one client turn. The caller has already inserted the incoming
/// `client.message.send` (and any `tool.result` events for returned tool
/// output) before calling this.
pub async fn run_turn(
    store: &Store,
    http: &reqwest::Client,
    session: &SessionRecord,
    profile: &ProfileRecord,
) -> Result<Turn, TurnError> {
    let sid = session.id.as_str();
    let cid = session.client_key_id.as_str();
    let mut events: Vec<EventRecord> = Vec::new();

    // The client-side tools the LLM is allowed to call, and the tool-definition
    // array we advertise to the provider.
    let tools_value = match &session.client_tools {
        Value::Array(_) => session.client_tools.clone(),
        _ => json!([]),
    };
    let client_tool_names: HashSet<String> = tools_value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    // Parse provider configs. A malformed primary config is an operator error:
    // record it and end as ProvidersFailed rather than panicking.
    let (primary, fallbacks) =
        match provider::configs_from_profile(&profile.provider_config, &profile.fallback_configs) {
            Ok(v) => v,
            Err(e) => {
                events.push(log_event(
                    store,
                    sid,
                    cid,
                    EventType::SessionError,
                    json!({ "reason": "provider_config", "detail": e.to_string() }),
                )?);
                return finish_failed(store, sid, events);
            }
        };
    let configs: Vec<ProviderConfig> = std::iter::once(primary).chain(fallbacks).collect();

    // History streamed from the log; extended in-memory across MCP round-trips.
    let mut history: Vec<Value> = store
        .with_conn(|c| sessions::stream_history(c, sid))
        .map_err(TurnError)?;

    for _ in 0..MAX_ITERATIONS {
        let history_value = Value::Array(history.clone());

        // --- Provider attempt sequence: primary, then each fallback. ---
        let mut success: Option<Value> = None;
        for (i, cfg) in configs.iter().enumerate() {
            let kind = if i == 0 { "primary" } else { "fallback" };
            events.push(log_event(
                store,
                sid,
                cid,
                EventType::ProviderRequest,
                json!({
                    "attempt": i,
                    "kind": kind,
                    "provider": cfg.provider,
                    "base_url": cfg.base_url,
                    "model": cfg.model,
                    "max_tokens": cfg.max_tokens,
                    "messages": history_value,
                    "tools": tools_value,
                }),
            )?);

            match provider::call(http, cfg, &history_value, &tools_value).await {
                Ok(body) => {
                    events.push(log_event(
                        store,
                        sid,
                        cid,
                        EventType::ProviderResponse,
                        json!({ "attempt": i, "kind": kind, "provider": cfg.provider, "ok": true, "status": 200, "body": body }),
                    )?);
                    success = Some(body);
                    break;
                }
                Err(e) => {
                    events.push(log_event(
                        store,
                        sid,
                        cid,
                        EventType::ProviderResponse,
                        json!({
                            "attempt": i, "kind": kind, "provider": cfg.provider, "ok": false,
                            "status": e.status(), "error": e.detail(), "body": e.body(),
                        }),
                    )?);
                    // The primary failing is the trigger for the fallback walk;
                    // record a session.error context event once, then continue.
                    if i == 0 {
                        events.push(log_event(
                            store,
                            sid,
                            cid,
                            EventType::SessionError,
                            json!({ "reason": "provider_call_failed", "provider": cfg.provider, "detail": e.detail() }),
                        )?);
                    }
                }
            }
        }

        let body = match success {
            Some(b) => b,
            None => {
                events.push(log_event(
                    store,
                    sid,
                    cid,
                    EventType::SessionError,
                    json!({ "reason": "all_providers_failed", "attempts": configs.len() }),
                )?);
                return finish_failed(store, sid, events);
            }
        };

        // --- Interpret the assistant response. ---
        let content = body.get("content").cloned().unwrap_or_else(|| json!([]));
        let tool_uses = tool_use_blocks(&content);

        if tool_uses.is_empty() {
            // Final, tool-free assistant turn.
            let message = json!({ "role": "assistant", "content": content });
            events.push(log_event(
                store,
                sid,
                cid,
                EventType::ServerMessageSend,
                message.clone(),
            )?);
            return Ok(Turn {
                message,
                events,
                outcome: Outcome::Completed,
            });
        }

        // Record every tool call, tagged with how it will be dispatched.
        for tu in &tool_uses {
            let name = tu.name.as_str();
            let dispatch = if client_tool_names.contains(name) {
                "client"
            } else {
                "mcp"
            };
            events.push(log_event(
                store,
                sid,
                cid,
                EventType::ToolCall,
                json!({ "id": tu.id, "name": name, "input": tu.input, "dispatch": dispatch }),
            )?);
        }

        let has_client_tool = tool_uses
            .iter()
            .any(|tu| client_tool_names.contains(tu.name.as_str()));

        if has_client_tool {
            // Hand the assistant turn (with its tool_use blocks) to the client
            // for execution; persist it as the message we sent back and pause.
            let message = json!({ "role": "assistant", "content": content });
            events.push(log_event(
                store,
                sid,
                cid,
                EventType::ServerMessageSend,
                message.clone(),
            )?);
            return Ok(Turn {
                message,
                events,
                outcome: Outcome::Paused,
            });
        }

        // All tool calls are MCP: run the stub path and continue the loop with
        // the stub results appended. This assistant turn is internal (not sent
        // to the client) so it is not persisted as server.message.send; it is
        // kept in the in-memory history for the next provider call.
        history.push(json!({ "role": "assistant", "content": content }));
        let mut result_blocks: Vec<Value> = Vec::new();
        for tu in &tool_uses {
            events.push(log_event(
                store,
                sid,
                cid,
                EventType::McpRequest,
                json!({ "status": "stub", "tool": tu.name, "input": tu.input }),
            )?);
            events.push(log_event(
                store,
                sid,
                cid,
                EventType::McpResponse,
                json!({ "status": "stub", "tool": tu.name }),
            )?);
            let result_content = json!([{
                "type": "text",
                "text": format!("MCP stub: no result available for tool '{}'", tu.name),
            }]);
            events.push(log_event(
                store,
                sid,
                cid,
                EventType::ToolResult,
                json!({ "tool_use_id": tu.id, "dispatch": "mcp", "status": "stub", "content": result_content }),
            )?);
            result_blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": tu.id,
                "content": result_content,
            }));
        }
        history.push(json!({ "role": "user", "content": Value::Array(result_blocks) }));
        // ...and loop for the next provider call.
    }

    // Exceeded the round-trip budget.
    events.push(log_event(
        store,
        sid,
        cid,
        EventType::SessionError,
        json!({ "reason": "loop_limit", "max_iterations": MAX_ITERATIONS }),
    )?);
    finish_failed(store, sid, events)
}

/// Move the session to `error` and return a ProvidersFailed turn carrying the
/// events logged so far.
fn finish_failed(
    store: &Store,
    session_id: &str,
    events: Vec<EventRecord>,
) -> Result<Turn, TurnError> {
    store
        .with_conn(|c| sessions::close_session(c, session_id, STATE_ERROR))
        .map_err(TurnError)?;
    Ok(Turn {
        message: json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "The provider is currently unavailable." }],
        }),
        events,
        outcome: Outcome::ProvidersFailed,
    })
}

/// A `tool_use` block extracted from an assistant response.
struct ToolUse {
    id: Value,
    name: String,
    input: Value,
}

/// Pull the `tool_use` blocks out of an assistant `content` value. Content that
/// is a plain string, or an array without tool_use blocks, yields an empty list.
fn tool_use_blocks(content: &Value) -> Vec<ToolUse> {
    let Some(arr) = content.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| {
            let name = b.get("name").and_then(Value::as_str)?.to_owned();
            Some(ToolUse {
                id: b.get("id").cloned().unwrap_or(Value::Null),
                name,
                input: b.get("input").cloned().unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_use_blocks_ignores_text() {
        let content = json!([
            { "type": "text", "text": "hi" },
            { "type": "tool_use", "id": "t1", "name": "get_time", "input": {} },
        ]);
        let blocks = tool_use_blocks(&content);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "get_time");
    }

    #[test]
    fn tool_use_blocks_on_string_content() {
        assert!(tool_use_blocks(&json!("just text")).is_empty());
    }
}
