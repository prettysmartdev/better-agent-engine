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
//!    failure event and a `session.error` context event, then walk the
//!    profile's `fallback_providers` in order — inserting a `provider.response`
//!    for **every** attempt, success or failure — until one succeeds. Provider
//!    names are resolved against the startup registry: a missing primary ends
//!    the turn (defensive re-check — session creation already refuses an
//!    unresolvable primary), missing fallbacks are logged and skipped.
//! 4. On success, insert a `provider.response` with the raw wire body — for an
//!    OpenAI-kind attempt that is the untranslated Chat Completions response;
//!    the loop itself only ever consumes the canonical translation
//!    [`provider::call`] hands back, so this module stays wire-format-agnostic.
//! 5. If the response contains tool calls, insert a `tool.call` per call.
//!    Client-side tools (declared by the client at session open) are returned to
//!    the client for execution and the turn pauses. MCP tools are dispatched to
//!    the session's live MCP connections (`mcp.request` / `mcp.response` with the
//!    real `tools/call` exchange, then `tool.result`) and the loop continues with
//!    the real result appended. A tool the session has no MCP server for, or a
//!    server that fails mid-turn, yields an error-shaped `tool.result` so the
//!    model can adjust — the turn is never aborted for a tool failure.
//! 6. On a plain (no-tool) response, insert `server.message.send` and finish.
//!
//! The auth token is resolved inside [`super::provider::call`] and never reaches
//! this module, an event payload, or a log line.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::broadcast::{self, EventBroadcaster};
use super::mcp::McpSession;
use super::provider::{self, ProviderConfig};
use super::sandbox::{ExecResult, SandboxDriver, SandboxHandle};
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

/// Append an event, publish it live, and return the record. Routes through the
/// shared [`broadcast::insert_and_publish`] choke point so every event the turn
/// logs also reaches live `session.sendMessage`/`session.subscribe` watchers,
/// and centralises the error-mapping boilerplate the loop repeats.
fn log_event(
    store: &Store,
    broadcaster: &EventBroadcaster,
    session_id: &str,
    client_key_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<EventRecord, TurnError> {
    broadcast::insert_and_publish(
        store,
        broadcaster,
        session_id,
        Some(client_key_id),
        event_type,
        &payload,
    )
    .map_err(TurnError)
}

/// Run one client turn. The caller has already inserted the incoming
/// `client.message.send` (and any `tool.result` events for returned tool
/// output) before calling this.
///
/// `acting_client_key_id` is the client key driving this turn (the same id the
/// FIFO turn lock records as the turn's owner). It scopes the turn in two
/// ways: only the acting client's own entry in the session's per-client
/// `client_tools` object is advertised to the provider — another driver's
/// private tools are never sent during this turn, so the model cannot request
/// a tool the turn's owner doesn't implement — and every event the turn logs
/// is attributed to it, not to the session's original creator.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    store: &Store,
    http: &reqwest::Client,
    broadcaster: &EventBroadcaster,
    session: &SessionRecord,
    profile: &ProfileRecord,
    provider_registry: &std::collections::HashMap<String, ProviderConfig>,
    mcp: Option<Arc<Mutex<McpSession>>>,
    sandbox_driver: Arc<dyn SandboxDriver>,
    sandbox: Option<Arc<Mutex<SandboxHandle>>>,
    acting_client_key_id: &str,
) -> Result<Turn, TurnError> {
    let sid = session.id.as_str();
    let cid = acting_client_key_id;
    let mut events: Vec<EventRecord> = Vec::new();

    // The acting client's own client-side tools — only these count as "client
    // dispatch" and only these are advertised alongside the session-wide MCP
    // tools (merged below); other drivers' entries in the per-client object
    // are never read.
    let client_tools: Vec<Value> = session
        .client_tools
        .get(acting_client_key_id)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let client_tool_names: HashSet<String> = client_tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect();

    // The acting client's Auto-mode sandbox tools — the third dispatch bucket,
    // parallel to the client-tool/MCP-tool split: these are dispatched
    // server-side against the session's remote sandbox, exactly like an MCP
    // tool, without ever pausing the turn. Same per-client scoping rule as
    // `client_tools`.
    let sandbox_tools: Vec<Value> = session
        .sandbox_tools
        .get(acting_client_key_id)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sandbox_tool_names: HashSet<String> = sandbox_tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect();

    // Merge the session's MCP tool definitions (from `tools/list` at connect
    // time) into what we advertise to the provider, and snapshot the
    // `tool_name -> server_name` routes for dispatch and event tagging.
    let mut advertised_tools = client_tools;
    advertised_tools.extend(sandbox_tools);
    let mcp_routes: std::collections::HashMap<String, String> = match &mcp {
        Some(m) => {
            let guard = m.lock().await;
            advertised_tools.extend(guard.tools().iter().cloned());
            guard.routes_snapshot()
        }
        None => std::collections::HashMap::new(),
    };
    let tools_value = Value::Array(advertised_tools);

    // Resolve the profile's provider name references against the startup
    // registry. A non-string primary reference or a primary name absent from
    // the registry is an operator error — the latter is a defensive re-check
    // (session creation already refuses an unresolvable primary; the registry
    // or profile may have changed since): record it and end as ProvidersFailed
    // rather than panicking. Missing fallback names are logged and skipped
    // inside the resolver, never fatal.
    let fallback_names: Vec<String> = profile
        .fallback_configs
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let resolved = match profile.provider_config.as_str() {
        Some(name) => provider::resolve_from_profile(provider_registry, name, &fallback_names),
        None => Err(provider::ProviderConfigError::Malformed(
            "primary_provider is not a string".to_string(),
        )),
    };
    let (primary, fallbacks) = match resolved {
        Ok(v) => v,
        Err(e) => {
            events.push(log_event(
                store,
                broadcaster,
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
                broadcaster,
                sid,
                cid,
                EventType::ProviderRequest,
                json!({
                    "attempt": i,
                    "kind": kind,
                    "provider": cfg.provider.as_str(),
                    "base_url": cfg.effective_base_url(),
                    "model": cfg.model,
                    "max_tokens": cfg.max_tokens,
                    "messages": history_value,
                    "tools": tools_value,
                }),
            )?);

            match provider::call(http, cfg, &history_value, &tools_value).await {
                Ok(resp) => {
                    // The event records the raw, untranslated wire body; the
                    // loop consumes only the canonical translation.
                    events.push(log_event(
                        store,
                        broadcaster,
                        sid,
                        cid,
                        EventType::ProviderResponse,
                        json!({ "attempt": i, "kind": kind, "provider": cfg.provider.as_str(), "ok": true, "status": 200, "body": resp.raw }),
                    )?);
                    success = Some(resp.canonical);
                    break;
                }
                Err(e) => {
                    events.push(log_event(
                        store,
                        broadcaster,
                        sid,
                        cid,
                        EventType::ProviderResponse,
                        json!({
                            "attempt": i, "kind": kind, "provider": cfg.provider.as_str(), "ok": false,
                            "status": e.status(), "error": e.detail(), "body": e.body(),
                        }),
                    )?);
                    // The primary failing is the trigger for the fallback walk;
                    // record a session.error context event once, then continue.
                    if i == 0 {
                        events.push(log_event(
                            store,
                            broadcaster,
                            sid,
                            cid,
                            EventType::SessionError,
                            json!({ "reason": "provider_call_failed", "provider": cfg.provider.as_str(), "detail": e.detail() }),
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
                    broadcaster,
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
                broadcaster,
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

        // Record every tool call, tagged with how it will be dispatched:
        // "client" (the acting client's own tools), "sandbox" (Auto-mode
        // sandbox tools, server-dispatched), or "mcp" (everything else). MCP
        // calls also carry the resolved `server_name` (null if unroutable).
        for tu in &tool_uses {
            let name = tu.name.as_str();
            let dispatch = if client_tool_names.contains(name) {
                "client"
            } else if sandbox_tool_names.contains(name) {
                "sandbox"
            } else {
                "mcp"
            };
            let mut payload = json!({
                "id": tu.id,
                "name": name,
                "input": tu.input,
                "dispatch": dispatch,
            });
            if dispatch == "mcp" {
                payload["server_name"] = json!(mcp_routes.get(name));
            }
            events.push(log_event(
                store,
                broadcaster,
                sid,
                cid,
                EventType::ToolCall,
                payload,
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
                broadcaster,
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

        // All tool calls are server-dispatched (sandbox or MCP): dispatch each
        // and continue the loop with the real results appended. This assistant
        // turn is internal (not sent to the client) so it is not persisted as
        // server.message.send; it is kept in the in-memory history for the
        // next provider call.
        history.push(json!({ "role": "assistant", "content": content }));
        let mut result_blocks: Vec<Value> = Vec::new();
        for tu in &tool_uses {
            let name = tu.name.as_str();

            // Auto-mode sandbox tools: dispatched server-side against the
            // session's remote sandbox, mirroring the MCP round trip below —
            // sandbox.request / sandbox.response bracket the driver call and
            // the result becomes an ordinary tool.result. A call with no
            // started sandbox is handled exactly like a tool with no MCP
            // server: an error-shaped tool.result, and the turn continues.
            if sandbox_tool_names.contains(name) {
                let command = tu
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                events.push(log_event(
                    store,
                    broadcaster,
                    sid,
                    cid,
                    EventType::SandboxRequest,
                    json!({ "tool": name, "input": tu.input, "command": command }),
                )?);

                let (response_payload, result_content, is_error) = match (&sandbox, &command) {
                    (Some(sb), Some(cmd)) => {
                        // Held across the driver await, like an MCP dispatch.
                        let handle = sb.lock().await;
                        match sandbox_driver.exec(&handle, cmd).await {
                            Ok(r) => {
                                let is_err = r.exit_code != 0;
                                (
                                    json!({
                                        "sandbox_id": handle.id,
                                        "ok": !is_err,
                                        "result": {
                                            "stdout": r.stdout,
                                            "stderr": r.stderr,
                                            "exit_code": r.exit_code,
                                        },
                                    }),
                                    exec_result_content(&r),
                                    is_err,
                                )
                            }
                            Err(e) => {
                                let msg = e.to_string();
                                // Lifecycle visibility is identical regardless
                                // of dispatch mode: a failed exec also logs
                                // session.sandbox.error alongside the
                                // error-shaped response/tool.result.
                                events.push(log_event(
                                    store,
                                    broadcaster,
                                    sid,
                                    cid,
                                    EventType::SandboxError,
                                    json!({
                                        "image": handle.image,
                                        "sandbox_id": handle.id,
                                        "phase": "exec",
                                        "detail": msg,
                                        "dispatch": "remote",
                                        "unsandboxed": false,
                                    }),
                                )?);
                                (
                                    json!({ "sandbox_id": handle.id, "ok": false, "error": msg }),
                                    sandbox_error_content(&msg),
                                    true,
                                )
                            }
                        }
                    }
                    // No remote sandbox was ever started for this session.
                    (None, _) => {
                        let msg = format!(
                            "no remote sandbox is running for tool '{name}'; \
                             call session.startRemoteSandbox first"
                        );
                        (
                            json!({ "sandbox_id": Value::Null, "ok": false, "error": msg }),
                            sandbox_error_content(&msg),
                            true,
                        )
                    }
                    // The declared input_schema must require a string
                    // `command`; a model call without one cannot be executed.
                    (Some(_), None) => {
                        let msg = format!(
                            "sandbox tool '{name}' input is missing the required string \"command\""
                        );
                        (
                            json!({ "sandbox_id": Value::Null, "ok": false, "error": msg }),
                            sandbox_error_content(&msg),
                            true,
                        )
                    }
                };

                events.push(log_event(
                    store,
                    broadcaster,
                    sid,
                    cid,
                    EventType::SandboxResponse,
                    response_payload,
                )?);
                events.push(log_event(
                    store,
                    broadcaster,
                    sid,
                    cid,
                    EventType::ToolResult,
                    json!({
                        "tool_use_id": tu.id,
                        "dispatch": "sandbox",
                        "is_error": is_error,
                        "content": result_content,
                    }),
                )?);
                result_blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tu.id,
                    "content": result_content,
                    "is_error": is_error,
                }));
                continue;
            }

            let server = mcp_routes.get(name).cloned();

            events.push(log_event(
                store,
                broadcaster,
                sid,
                cid,
                EventType::McpRequest,
                json!({
                    "method": "tools/call",
                    "server_name": server,
                    "tool": name,
                    "input": tu.input,
                }),
            )?);

            // Dispatch, mapping every outcome (success, missing server, or a
            // connection that died mid-turn) to a (response payload, result
            // content, is_error) triple. A failure is never fatal to the turn:
            // the model sees an error result and can adjust. No reconnect.
            let (response_payload, result_content, is_error) = match (&mcp, &server) {
                (Some(m), Some(srv)) => match m.lock().await.call_tool(name, &tu.input).await {
                    Ok(result) => {
                        let is_err = result
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let content = result.get("content").cloned().unwrap_or_else(|| json!([]));
                        (
                            json!({ "server_name": srv, "ok": !is_err, "result": result }),
                            content,
                            is_err,
                        )
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        (
                            json!({ "server_name": srv, "ok": false, "error": msg }),
                            mcp_error_content(&e.to_string()),
                            true,
                        )
                    }
                },
                // No MCP server is configured for this tool: the profile
                // referenced an unconfigured/typo'd server, or the model invoked
                // a tool that was never advertised.
                _ => {
                    let msg = format!("no MCP server is configured for tool '{name}'");
                    (
                        json!({ "server_name": server, "ok": false, "error": msg.clone() }),
                        mcp_error_content(&msg),
                        true,
                    )
                }
            };

            events.push(log_event(
                store,
                broadcaster,
                sid,
                cid,
                EventType::McpResponse,
                response_payload,
            )?);
            events.push(log_event(
                store,
                broadcaster,
                sid,
                cid,
                EventType::ToolResult,
                json!({
                    "tool_use_id": tu.id,
                    "dispatch": "mcp",
                    "server_name": server,
                    "is_error": is_error,
                    "content": result_content,
                }),
            )?);
            result_blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": tu.id,
                "content": result_content,
                "is_error": is_error,
            }));
        }
        history.push(json!({ "role": "user", "content": Value::Array(result_blocks) }));
        // ...and loop for the next provider call.
    }

    // Exceeded the round-trip budget.
    events.push(log_event(
        store,
        broadcaster,
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

/// Build the error-shaped `tool_result` content for a failed MCP dispatch, so
/// the model sees the failure as tool output rather than the turn aborting.
fn mcp_error_content(msg: &str) -> Value {
    json!([{ "type": "text", "text": format!("MCP error: {msg}") }])
}

/// The sandbox twin of [`mcp_error_content`]: same error-shaped tool_result
/// posture (the model sees the failure as tool output; the turn continues).
fn sandbox_error_content(msg: &str) -> Value {
    json!([{ "type": "text", "text": format!("sandbox error: {msg}") }])
}

/// Render a sandbox exec's captured output as `tool_result` content: stdout,
/// then a `[stderr]` section when non-empty, then the exit code when non-zero
/// (a zero exit with clean stderr is just the stdout).
fn exec_result_content(r: &ExecResult) -> Value {
    let mut text = r.stdout.clone();
    if !r.stderr.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&r.stderr);
    }
    if r.exit_code != 0 {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("[exit_code: {}]", r.exit_code));
    }
    json!([{ "type": "text", "text": text }])
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
