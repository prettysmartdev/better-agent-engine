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
//! 5. If the response contains tool calls, insert a `tool.call` per call, each
//!    tagged with its `dispatch` (`client` / `sandbox` / `mcp`), then
//!    **partition** the calls on that tag. The server-dispatched tools
//!    (`sandbox` + `mcp`) are **always** dispatched first, server-side: sandbox
//!    tools against the session's remote sandbox and MCP tools against the
//!    session's live MCP connections (`mcp.request` / `mcp.response` with the
//!    real `tools/call` exchange, then `tool.result`). A tool the session has
//!    no MCP server for, or a server that fails mid-turn, yields an
//!    error-shaped `tool.result` so the model can adjust — the turn is never
//!    aborted for a tool failure. Then, on the `client` bucket:
//!    - empty (all-server turn): append the collected results to history and
//!      loop — the common MCP path, no pause, no persistence;
//!    - non-empty (mixed or all-client turn): persist the whole assistant
//!      message (every `tool_use` block, each carrying its `dispatch` tag) as
//!      `server.message.send`, return [`Outcome::Paused`], and carry the
//!      already-dispatched server results out via [`Turn::pending_tool_results`]
//!      so the caller can merge them with the client's results on resume.
//! 6. On a plain (no-tool) response, insert `server.message.send` and finish.
//!
//! The auth token is resolved inside [`super::provider::call`] and never reaches
//! this module, an event payload, or a log line.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::broadcast::{self, EventBroadcaster};
use super::mcp::McpSession;
use super::provider::{self, ProviderConfig};
use super::sandbox::{CommandRunner, ExecResult, SandboxDriver, SandboxHandle};
use super::subagent::{self, SubagentStatus, SubagentTask, SubagentToolDef};
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
    /// Server-dispatched `tool_result` blocks that were executed before the
    /// turn paused, to be merged with the client's own results on resume.
    /// Non-empty **only** on a mixed-turn [`Outcome::Paused`] (a turn that
    /// contains at least one client tool alongside sandbox/MCP tools); empty on
    /// every other outcome, including an all-client `Paused`.
    pub pending_tool_results: Vec<Value>,
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
/// **Partitioned dispatch.** When an assistant turn contains tool calls, they
/// are split on their `dispatch` tag: `sandbox`/`mcp` calls are always
/// dispatched server-side first (collecting their `tool_result` blocks), and
/// only then does the `client` bucket decide the outcome. An empty client
/// bucket (all-server turn) loops in-process with the results appended to
/// history; a non-empty client bucket (mixed or all-client turn) persists the
/// full assistant message and returns [`Outcome::Paused`] with the collected
/// server results carried out in [`Turn::pending_tool_results`], so the caller
/// can reassemble both result sets into the single following `user` turn on
/// resume.
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
    subagents: Arc<std::sync::Mutex<HashMap<String, HashMap<String, SubagentTask>>>>,
    command_runner: Arc<dyn CommandRunner>,
    subagent_timeout: std::time::Duration,
    max_subagents_per_session: usize,
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

    // Remote declarations retain config in storage but expose only ordinary
    // provider tool fields. They are a fourth, server-side dispatch bucket.
    let subagent_declarations: Vec<SubagentToolDef> = session
        .subagent_tools
        .get(acting_client_key_id)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let subagent_tool_names: HashSet<String> = subagent_declarations
        .iter()
        .map(|t| t.name.clone())
        .collect();
    let subagent_tools: Vec<Value> = subagent_declarations
        .iter()
        .map(|t| {
            json!({
                "name": t.name, "description": t.description,
                "input_schema": t.input_schema.clone().unwrap_or_else(|| json!({})),
            })
        })
        .collect();

    // Merge the session's MCP tool definitions (from `tools/list` at connect
    // time) into what we advertise to the provider, and snapshot the
    // `tool_name -> server_name` routes for dispatch and event tagging.
    let mut advertised_tools = client_tools;
    advertised_tools.extend(sandbox_tools);
    advertised_tools.extend(subagent_tools);
    let mcp_routes: std::collections::HashMap<String, String> = match &mcp {
        Some(m) => {
            let guard = m.lock().await;
            advertised_tools.extend(guard.tools().iter().cloned());
            guard.routes_snapshot()
        }
        None => std::collections::HashMap::new(),
    };
    let advertised_tools = advertised_tools;

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
        // Recomputed for every provider iteration: status visibility is live
        // state, never a persisted declaration.
        let mut iteration_tools = advertised_tools.clone();
        if subagents
            .lock()
            .expect("subagents mutex poisoned")
            .get(sid)
            .is_some_and(|m| !m.is_empty())
        {
            iteration_tools.push(subagent::status_tool_definition());
        }
        let tools_value = Value::Array(iteration_tools);
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
                pending_tool_results: Vec::new(),
            });
        }

        // Classify each tool call by how it will be dispatched: "client" (the
        // acting client's own tools), "sandbox" (Auto-mode sandbox tools,
        // server-dispatched), or "mcp" (everything else). This dispatch tag is
        // the partition key below; it is echoed onto every `tool.call` event
        // and — for a mixed/all-client turn — onto the persisted assistant
        // `tool_use` blocks so the client can tell its own blocks apart.
        let dispatches: Vec<&'static str> = tool_uses
            .iter()
            .map(|tu| {
                let name = tu.name.as_str();
                if client_tool_names.contains(name) {
                    "client"
                } else if sandbox_tool_names.contains(name) {
                    "sandbox"
                } else if subagent_tool_names.contains(name)
                    || name == subagent::REMOTE_STATUS_TOOL_NAME
                {
                    "subagent"
                } else {
                    "mcp"
                }
            })
            .collect();
        // Dispatch tag by `tool_use` id, for annotating the persisted assistant
        // message on a mixed/all-client pause.
        let dispatch_by_id: HashMap<String, &'static str> = tool_uses
            .iter()
            .zip(&dispatches)
            .filter_map(|(tu, d)| tu.id.as_str().map(|id| (id.to_owned(), *d)))
            .collect();

        // Record every tool call, tagged with how it will be dispatched. MCP
        // calls also carry the resolved `server_name` (null if unroutable).
        for (tu, dispatch) in tool_uses.iter().zip(&dispatches) {
            let name = tu.name.as_str();
            let mut payload = json!({
                "id": tu.id,
                "name": name,
                "input": tu.input,
                "dispatch": dispatch,
            });
            if *dispatch == "mcp" {
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

        let has_client = dispatches.contains(&"client");

        // Always dispatch the server-side tools (sandbox + subagent + MCP) first, in
        // tool_use order, collecting each one's `tool_result` block. This runs
        // for every turn shape — all-server, mixed, and all-client (a no-op
        // then) — so observers see server-side work live even when the turn
        // later pauses for the client. Client-dispatched blocks are skipped
        // here; the client executes them.
        let mut server_tool_results: Vec<Value> = Vec::new();
        for (tu, dispatch) in tool_uses.iter().zip(&dispatches) {
            if *dispatch == "client" {
                continue;
            }
            let name = tu.name.as_str();

            // Auto-mode sandbox tools: dispatched server-side against the
            // session's remote sandbox, mirroring the MCP round trip below —
            // sandbox.request / sandbox.response bracket the driver call and
            // the result becomes an ordinary tool.result. A call with no
            // started sandbox is handled exactly like a tool with no MCP
            // server: an error-shaped tool.result, and the turn continues.
            if *dispatch == "sandbox" {
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
                server_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tu.id,
                    "content": result_content,
                    "is_error": is_error,
                }));
                continue;
            }

            // Remote subagents are intentionally fire-and-forget. This branch
            // returns the started acknowledgement in this turn; the detached
            // task emits terminal lifecycle events after the turn ends.
            if *dispatch == "subagent" {
                let (result_content, is_error) = if name == subagent::REMOTE_STATUS_TOOL_NAME {
                    remote_status_result(&subagents, sid, &tu.input)
                } else {
                    launch_remote_subagent(
                        store,
                        broadcaster,
                        sid,
                        cid,
                        name,
                        &tu.input,
                        &subagent_declarations,
                        &sandbox,
                        sandbox_driver.clone(),
                        subagents.clone(),
                        command_runner.clone(),
                        subagent_timeout,
                        max_subagents_per_session,
                    )
                    .await
                };
                events.push(log_event(
                    store, broadcaster, sid, cid, EventType::ToolResult,
                    json!({ "tool_use_id": tu.id, "dispatch": "subagent", "is_error": is_error, "content": result_content }),
                )?);
                server_tool_results.push(json!({
                    "type": "tool_result", "tool_use_id": tu.id,
                    "content": result_content, "is_error": is_error,
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
            server_tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": tu.id,
                "content": result_content,
                "is_error": is_error,
            }));
        }

        if has_client {
            // Mixed or all-client turn: hand the assistant turn to the client.
            // Persist it with a `dispatch` tag on every `tool_use` block so the
            // client executes only its own ("client") blocks and treats the
            // server-dispatched ones as informational. The persisted message
            // keeps ALL tool_use blocks because the following `user` turn must
            // answer every `tool_use` id. Carry the already-dispatched server
            // results out so the caller can merge them with the client's
            // results on resume (see rpc::drive_send_message).
            let content = annotate_dispatch(&content, &dispatch_by_id);
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
                pending_tool_results: server_tool_results,
            });
        }

        // All-server turn: append the assistant message and the merged tool
        // results to the in-memory history and continue the provider loop. This
        // assistant turn is internal (not sent to the client), so it is not
        // persisted as server.message.send; it is kept in the in-memory history
        // for the next provider call. This is the hot MCP path — no pause, no
        // persistence, no client.message.send.
        history.push(json!({ "role": "assistant", "content": content }));
        history.push(json!({ "role": "user", "content": Value::Array(server_tool_results) }));
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

/// Server-form tool-result content: a compact JSON object in one text block.
fn subagent_content(value: Value) -> Value {
    json!([{ "type": "text", "text": value.to_string() }])
}

/// Read remote task state and acknowledge terminal entries only after their
/// one permitted status response has been built.
fn remote_status_result(
    subagents: &Arc<std::sync::Mutex<HashMap<String, HashMap<String, SubagentTask>>>>,
    session_id: &str,
    input: &Value,
) -> (Value, bool) {
    let requested = input.get("subagent_id").and_then(Value::as_str);
    let mut map = subagents.lock().expect("subagents mutex poisoned");
    let Some(tasks) = map.get_mut(session_id) else {
        return if requested.is_some() {
            (
                subagent_content(json!({ "error": "unknown subagent_id" })),
                true,
            )
        } else {
            (subagent_content(json!({ "subagents": [] })), false)
        };
    };
    let mut ids: Vec<String> = match requested {
        Some(id) => {
            if !tasks.contains_key(id) {
                return (
                    subagent_content(json!({ "error": "unknown subagent_id" })),
                    true,
                );
            }
            vec![id.to_owned()]
        }
        None => tasks.keys().cloned().collect(),
    };
    if requested.is_none() {
        ids.sort_by_key(|id| tasks.get(id).map(|t| t.launch_sequence));
    }
    let entries: Vec<Value> = ids
        .iter()
        .filter_map(|id| tasks.get(id).map(|t| t.status_json(id)))
        .collect();
    let terminal: Vec<String> = ids
        .into_iter()
        .filter(|id| tasks.get(id).is_some_and(|t| t.status.terminal()))
        .collect();
    for id in terminal {
        tasks.remove(&id);
    }
    let empty = tasks.is_empty();
    if empty {
        map.remove(session_id);
    }
    (subagent_content(json!({ "subagents": entries })), false)
}

#[allow(clippy::too_many_arguments)]
async fn launch_remote_subagent(
    store: &Store,
    broadcaster: &EventBroadcaster,
    session_id: &str,
    client_key_id: &str,
    tool_name: &str,
    input: &Value,
    declarations: &[SubagentToolDef],
    sandbox: &Option<Arc<Mutex<SandboxHandle>>>,
    sandbox_driver: Arc<dyn SandboxDriver>,
    subagents: Arc<std::sync::Mutex<HashMap<String, HashMap<String, SubagentTask>>>>,
    runner: Arc<dyn CommandRunner>,
    default_timeout: std::time::Duration,
    max_subagents: usize,
) -> (Value, bool) {
    let (harness, model, prompt) = match (
        input.get("harness").and_then(Value::as_str),
        input.get("model").and_then(Value::as_str),
        input.get("prompt").and_then(Value::as_str),
    ) {
        (Some(h), Some(m), Some(p))
            if !h.trim().is_empty() && !m.trim().is_empty() && !p.trim().is_empty() =>
        {
            (h.to_owned(), m.to_owned(), p.to_owned())
        }
        _ => {
            return (
                subagent_content(
                    json!({ "error": "launch_subagent requires string \"harness\", \"model\", and \"prompt\"" }),
                ),
                true,
            )
        }
    };
    let Some(tool) = declarations.iter().find(|d| d.name == tool_name) else {
        return (
            subagent_content(json!({ "error": format!("unknown harness {:?}", harness) })),
            true,
        );
    };
    let Some(def) = tool
        .subagents
        .iter()
        .find(|d| d.harness == harness)
        .cloned()
    else {
        return (
            subagent_content(json!({ "error": format!("unknown harness {:?}", harness) })),
            true,
        );
    };
    if subagents
        .lock()
        .expect("subagents mutex poisoned")
        .get(session_id)
        .map(|m| {
            m.values()
                .filter(|t| t.status == SubagentStatus::Running)
                .count()
        })
        .unwrap_or(0)
        >= max_subagents
    {
        return (
            subagent_content(
                json!({ "error": format!("subagent limit reached (max {max_subagents} per session)") }),
            ),
            true,
        );
    }
    let Some(sandbox) = sandbox else {
        let msg = format!("no remote sandbox is running for tool '{tool_name}'; call session.startRemoteSandbox first");
        return (subagent_content(json!({ "error": msg })), true);
    };
    let handle = sandbox.lock().await.clone();
    if handle.image != tool.image {
        return (
            subagent_content(
                json!({ "error": format!("remote sandbox image mismatch: subagent declared {:?} but the running sandbox uses {:?}", tool.image, handle.image) }),
            ),
            true,
        );
    }

    let subagent_id = crate::store::generate_id(subagent::SUBAGENT_ID_PREFIX);
    let common = |detail: Value| json!({ "dispatch": "remote", "subagent_id": subagent_id, "harness": harness, "model": model, "detail": detail });
    if let Err(e) = broadcast::insert_and_publish(
        store,
        broadcaster,
        session_id,
        Some(client_key_id),
        EventType::SubagentStart,
        &common(Value::Null),
    ) {
        tracing::error!("failed to log subagent start: {e}");
    }
    subagents
        .lock()
        .expect("subagents mutex poisoned")
        .entry(session_id.to_owned())
        .or_default()
        .insert(
            subagent_id.clone(),
            SubagentTask::running(harness.clone(), model.clone()),
        );

    let task_id = subagent_id.clone();
    let session_id = session_id.to_owned();
    let client_key_id = client_key_id.to_owned();
    let store = store.clone();
    let broadcaster = broadcaster.clone();
    let command = subagent::interpolate(&def.command_template, &model, &prompt, &def.prompt_via);
    let stdin = (def.prompt_via == "stdin").then(|| prompt.into_bytes());
    let args = vec![
        "exec".to_owned(),
        "-i".to_owned(),
        handle.id,
        "sh".to_owned(),
        "-c".to_owned(),
        command,
    ];
    let program = sandbox_driver.cli_program().to_owned();
    let timeout = subagent::timeout_for(&def, default_timeout);
    let background_subagents = subagents.clone();
    let background_harness = harness.clone();
    let background_model = model.clone();
    let background_session_id = session_id.clone();
    let background_client_key_id = client_key_id.clone();
    let background_store = store.clone();
    let background_broadcaster = broadcaster.clone();
    // A command can resolve immediately (especially in tests, or for a very
    // short CLI). Hold the detached task until the synchronous `running`
    // lifecycle event has been persisted so the canonical start -> running ->
    // terminal ordering cannot race.
    let running_gate = Arc::new(tokio::sync::Notify::new());
    let background_running_gate = running_gate.clone();
    let join = tokio::spawn(async move {
        background_running_gate.notified().await;
        let outcome = tokio::time::timeout(
            timeout,
            runner.run_with_stdin(&program, &args, stdin.as_deref()),
        )
        .await;
        let (event_type, reason, exit_code, detail, stdout, stderr, truncated, status) =
            match outcome {
                Err(_) => (
                    EventType::SubagentFailed,
                    Some("timeout".to_owned()),
                    None,
                    None,
                    None,
                    None,
                    false,
                    SubagentStatus::TimedOut,
                ),
                Ok(Err(e)) => (
                    EventType::SubagentFailed,
                    Some("spawn_failed".to_owned()),
                    None,
                    Some(e.to_string()),
                    None,
                    None,
                    false,
                    SubagentStatus::Failed,
                ),
                Ok(Ok(out)) => {
                    let code = out.status.code().unwrap_or(-1);
                    let (stdout, a) = subagent::truncate_output(&out.stdout);
                    let (stderr, b) = subagent::truncate_output(&out.stderr);
                    if code == 0 {
                        (
                            EventType::SubagentCompleted,
                            None,
                            Some(0),
                            None,
                            Some(stdout),
                            Some(stderr),
                            a || b,
                            SubagentStatus::Completed,
                        )
                    } else {
                        (
                            EventType::SubagentFailed,
                            Some("nonzero_exit".to_owned()),
                            Some(code),
                            None,
                            Some(stdout),
                            Some(stderr),
                            a || b,
                            SubagentStatus::Failed,
                        )
                    }
                }
            };
        let updated = {
            let mut all = background_subagents
                .lock()
                .expect("subagents mutex poisoned");
            let Some(task) = all
                .get_mut(&background_session_id)
                .and_then(|m| m.get_mut(&task_id))
            else {
                return;
            };
            if task.status != SubagentStatus::Running {
                false
            } else {
                task.status = status;
                task.reason = reason.clone();
                task.exit_code = exit_code;
                task.detail = detail.clone();
                task.stdout = stdout;
                task.stderr = stderr;
                task.truncated = truncated;
                task.task = None;
                true
            }
        };
        if !updated {
            return;
        }
        let mut payload = json!({ "dispatch": "remote", "subagent_id": task_id, "harness": background_harness, "model": background_model, "detail": detail });
        if event_type == EventType::SubagentCompleted {
            payload["exit_code"] = json!(0);
        }
        if event_type == EventType::SubagentFailed {
            payload["reason"] = json!(reason);
            payload["exit_code"] = json!(exit_code);
        }
        if let Err(e) = broadcast::insert_and_publish(
            &background_store,
            &background_broadcaster,
            &background_session_id,
            Some(&background_client_key_id),
            event_type,
            &payload,
        ) {
            tracing::error!("failed to log remote subagent terminal event: {e}");
        }
    });
    if let Some(task) = subagents
        .lock()
        .expect("subagents mutex poisoned")
        .get_mut(session_id.as_str())
        .and_then(|m| m.get_mut(&subagent_id))
    {
        task.task = Some(join);
    }
    if let Err(e) = broadcast::insert_and_publish(
        &store,
        &broadcaster,
        session_id.as_str(),
        Some(client_key_id.as_str()),
        EventType::SubagentRunning,
        &common(Value::Null),
    ) {
        tracing::error!("failed to log subagent running: {e}");
    }
    running_gate.notify_one();
    (
        subagent_content(
            json!({ "subagent_id": subagent_id, "harness": harness, "model": model, "status": "started" }),
        ),
        false,
    )
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
        pending_tool_results: Vec::new(),
    })
}

/// Return `content` with a `dispatch` field added to every `tool_use` block,
/// looked up by the block's `id`. Blocks without a known id (or non-array
/// content) pass through unchanged. Applied only to the mixed/all-client
/// assistant message persisted as `server.message.send`, so the client can
/// route each block to the right executor. The tag is a non-standard field and
/// is stripped before the message is ever replayed to a provider (see
/// [`super::provider::call`]).
fn annotate_dispatch(content: &Value, dispatch_by_id: &HashMap<String, &'static str>) -> Value {
    let Some(arr) = content.as_array() else {
        return content.clone();
    };
    Value::Array(
        arr.iter()
            .map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    if let (Some(id), Some(obj)) =
                        (b.get("id").and_then(Value::as_str), b.as_object())
                    {
                        if let Some(d) = dispatch_by_id.get(id) {
                            let mut o = obj.clone();
                            o.insert("dispatch".to_string(), json!(d));
                            return Value::Object(o);
                        }
                    }
                }
                b.clone()
            })
            .collect(),
    )
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
    use std::process::Output;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    struct UnitRunner {
        started: Notify,
        release: Notify,
        called: AtomicBool,
    }

    impl UnitRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                started: Notify::new(),
                release: Notify::new(),
                called: AtomicBool::new(false),
            })
        }
    }

    impl CommandRunner for UnitRunner {
        fn run<'a>(
            &'a self,
            program: &'a str,
            args: &'a [String],
        ) -> super::super::sandbox::BoxFuture<'a, std::io::Result<Output>> {
            self.run_with_stdin(program, args, None)
        }

        fn run_with_stdin<'a>(
            &'a self,
            _program: &'a str,
            _args: &'a [String],
            _stdin: Option<&'a [u8]>,
        ) -> super::super::sandbox::BoxFuture<'a, std::io::Result<Output>> {
            self.called.store(true, Ordering::SeqCst);
            self.started.notify_one();
            Box::pin(async move {
                self.release.notified().await;
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    Ok(Output {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: b"done".to_vec(),
                        stderr: Vec::new(),
                    })
                }
                #[cfg(not(unix))]
                {
                    unreachable!("server CI is Unix-like");
                }
            })
        }
    }

    struct UnitSandboxDriver;

    impl super::super::sandbox::SandboxDriver for UnitSandboxDriver {
        fn cli_program(&self) -> &'static str {
            "mock-engine"
        }

        fn ensure_image<'a>(
            &'a self,
            _image: &'a str,
        ) -> super::super::sandbox::BoxFuture<
            'a,
            Result<super::super::sandbox::EnsureOutcome, super::super::sandbox::SandboxError>,
        > {
            Box::pin(async {
                Err(super::super::sandbox::SandboxError::Runtime {
                    detail: "not used".into(),
                })
            })
        }

        fn start<'a>(
            &'a self,
            _image: &'a str,
        ) -> super::super::sandbox::BoxFuture<
            'a,
            Result<super::super::sandbox::SandboxHandle, super::super::sandbox::SandboxError>,
        > {
            Box::pin(async {
                Err(super::super::sandbox::SandboxError::Runtime {
                    detail: "not used".into(),
                })
            })
        }

        fn exec<'a>(
            &'a self,
            _handle: &'a super::super::sandbox::SandboxHandle,
            _command: &'a str,
        ) -> super::super::sandbox::BoxFuture<
            'a,
            Result<super::super::sandbox::ExecResult, super::super::sandbox::SandboxError>,
        > {
            Box::pin(async {
                Err(super::super::sandbox::SandboxError::Runtime {
                    detail: "not used".into(),
                })
            })
        }

        fn stop<'a>(
            &'a self,
            _handle: &'a super::super::sandbox::SandboxHandle,
        ) -> super::super::sandbox::BoxFuture<'a, Result<(), super::super::sandbox::SandboxError>>
        {
            Box::pin(async {
                Err(super::super::sandbox::SandboxError::Runtime {
                    detail: "not used".into(),
                })
            })
        }
    }

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

    #[tokio::test]
    async fn remote_launch_dispatch_is_non_blocking_unit() {
        let runner = UnitRunner::new();
        let store = Store::open_in_memory().unwrap();
        let broadcaster = EventBroadcaster::new();
        let subagents = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let declaration = SubagentToolDef {
            name: "launch_subagent".into(),
            description: None,
            input_schema: None,
            image: "image".into(),
            subagents: vec![super::super::subagent::SubagentDef {
                harness: "mock".into(),
                command_template: "mock --model {model}".into(),
                prompt_via: "stdin".into(),
                timeout_secs: Some(60),
            }],
        };
        let sandbox = Arc::new(tokio::sync::Mutex::new(SandboxHandle {
            id: "container".into(),
            image: "image".into(),
        }));
        let started = tokio::time::Instant::now();
        let (content, is_error) = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            launch_remote_subagent(
                &store,
                &broadcaster,
                "ses_unit",
                "key_unit",
                "launch_subagent",
                &json!({ "harness": "mock", "model": "m", "prompt": "p" }),
                &[declaration],
                &Some(sandbox),
                Arc::new(UnitSandboxDriver),
                subagents,
                runner.clone(),
                std::time::Duration::from_secs(60),
                8,
            ),
        )
        .await
        .expect("dispatch must not await the runner");
        assert!(!is_error);
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        assert!(content[0]["text"].as_str().unwrap().contains("started"));
        runner.started.notified().await;
        assert!(runner.called.load(Ordering::SeqCst));
        runner.release.notify_one();
    }
}
