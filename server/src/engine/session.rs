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
//!
//! [`run_compaction`] is the second entrypoint: one provider call that replaces
//! the session's provider-facing history with a single self-contained summary,
//! per `aspec/work-items/0016-session-compaction.md`. It is driven either by the
//! `session.compact` RPC (`mode: client`) or, for a session created with
//! `compaction: {"mode":"auto","size":N}`, by [`run_turn`] itself once the
//! provider's own reported token usage for the turn it just finished crosses
//! `N`. Nothing is rewritten: a synthetic `user` preamble and the summary are
//! appended as ordinary `server.message.send` events and
//! `session.compaction.completed` points at both, so a later
//! [`sessions::stream_history`] starts its scan at the preamble while the full
//! pre-compaction log stays intact for replay.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::broadcast::{self, EventBroadcaster};
use super::mcp::McpSession;
use super::provider::{self, ProviderConfig};
use super::sandbox::{CommandRunner, ExecResult, SandboxDriver, SandboxHandle};
use super::subagent::{self, SubagentStatus, SubagentTask, SubagentToolDef};
use crate::api::client::sessions::CompactionConfig;
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
    /// An auto-compaction threshold crossing observed immediately before this
    /// turn paused for a client tool. The compaction cannot run against an
    /// unresolved `tool_use`, so the RPC layer parks this tiny trigger with the
    /// turn and passes it back when the client resumes. It is then honored at
    /// the first safe, completed boundary even if that final provider response
    /// has lower or unavailable usage.
    pub pending_auto_compaction: Option<CompactionTrigger>,
}

/// An engine call failed for a reason the caller must distinguish.
///
/// [`TurnError::Store`] is a persistence-layer failure (distinct from a
/// provider failure, which for an ordinary turn is a normal
/// [`Outcome::ProvidersFailed`]). [`TurnError::CompactionFailed`] is the
/// compaction-only logical failure: a [`run_compaction`] attempt that could not
/// produce a summary because every provider was exhausted. It never closes or
/// errors the session; the pre-compaction history stays active and the attempt
/// is retryable. A successful summary with unavailable usage still completes,
/// recording nullable accounting fields.
#[derive(Debug)]
pub enum TurnError {
    Store(rusqlite::Error),
    CompactionFailed(String),
}

impl From<rusqlite::Error> for TurnError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Store(value)
    }
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnError::Store(e) => write!(f, "session store error: {e}"),
            TurnError::CompactionFailed(detail) => write!(f, "compaction failed: {detail}"),
        }
    }
}
impl std::error::Error for TurnError {}

/// What caused a compaction, for the `session.compaction.started` payload.
///
/// [`CompactionTrigger::Auto`] carries the just-completed turn's last successful
/// provider call usage sum that crossed the session's configured threshold;
/// [`CompactionTrigger::Client`] carries the best-effort figure read from the
/// session's newest persisted `provider.response` (`None` when the session has
/// made no provider call with usable usage yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    Auto {
        token_count: u64,
        threshold_tokens: u64,
        /// The first auto attempt after a failed one (backoff satisfied):
        /// `started.reason` is `retry_after_failure` instead of
        /// `token_threshold`.
        retry_after_failure: bool,
    },
    Client {
        token_count: Option<u64>,
    },
}

/// The persisted synthetic user message that precedes every compaction summary.
/// Byte-exact and never templated, so prompt-cache prefixes stay stable across
/// turns. Also the text the Anthropic request normalizer prepends to a history
/// that does not start with `user` ([`provider::prepare_messages`]).
pub const COMPACTION_PREAMBLE_TEXT: &str =
    "The earlier part of this conversation was compacted. A summary follows.";
/// `payload.synthetic` value on the compaction preamble event.
pub const SYNTHETIC_COMPACTION_PREAMBLE: &str = "compaction_preamble";
/// `payload.synthetic` value on the user message the server writes when it
/// retires an expired paused turn on behalf of `session.compact`.
pub const SYNTHETIC_ABANDONED_TOOL_RESULTS: &str = "abandoned_tool_results";
/// Output budget floor for the compaction call: it uses the larger of this and
/// the provider entry's own `max_tokens`, so a profile tuned for short turns
/// cannot truncate the summary.
pub const MIN_COMPACTION_MAX_TOKENS: u32 = 4096;

/// A failed auto-compaction attempt, remembered so the next attempt waits until
/// the history has meaningfully grown (see [`CompactionBackoff`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailedAutoCompaction {
    /// The trigger's token count at the failed attempt.
    pub token_count: u64,
    /// Turns completed since that attempt.
    pub completed_turns_since: u32,
}

/// Per-session auto-compaction backoff state, keyed by session id. In-memory
/// only (a restart clears it, which merely allows one early retry). After a
/// failed auto attempt, auto compaction is suppressed until at least one more
/// turn has completed **and** the trigger's token count exceeds the failed
/// attempt's; that retry's `started` event says `reason: "retry_after_failure"`.
/// A manual `session.compact` ignores it.
pub type CompactionBackoff = Arc<std::sync::Mutex<HashMap<String, FailedAutoCompaction>>>;

/// The built-in compaction instruction, used for every auto-triggered
/// compaction and for a `session.compact` call that supplies no prompt of its
/// own. It is appended as an ordinary final `user` message — the server has no
/// system-role concept and this work item deliberately does not add one.
const DEFAULT_COMPACTION_PROMPT: &str = "Produce a single self-contained compacted summary of this session. Preserve the key facts, decisions, constraints, completed work, open tasks, and any state needed to continue the session. Do not include commentary outside the summary.";

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
    .map_err(TurnError::Store)
}

/// The explicit `usage` member every **successful** `provider.response` payload
/// carries: the provider's own reported token pair, or JSON `null` when it
/// omitted usage (or returned a malformed/partial object). Consumers — the
/// auto-compaction check and `sessions::last_provider_token_count` — read this
/// small field instead of re-parsing the full raw `body`.
fn usage_payload(usage: Option<(u64, u64)>) -> Value {
    match usage {
        Some((input, output)) => json!({ "input_tokens": input, "output_tokens": output }),
        None => Value::Null,
    }
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
    pending_auto_compaction: Option<CompactionTrigger>,
    compaction_backoff: &CompactionBackoff,
    metrics: &crate::telemetry::Metrics,
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
    let (configs, config_names) = match resolve_provider_chain(profile, provider_registry) {
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

    // History streamed from the log; extended in-memory across MCP round-trips.
    let mut history: Vec<Value> = store
        .with_conn(|c| sessions::stream_history(c, sid))
        .map_err(TurnError::Store)?;

    // The most recent **successful** provider call's reported token usage,
    // carried across the iteration loop so the auto-compaction check at the
    // turn boundary can compare it against the session's configured threshold
    // without any SQL aggregate. A later success replaces it (including with
    // `None`, when that provider omitted usage — the check then skips rather
    // than reading a stale number); failed attempts leave it untouched.
    // Deliberately uninitialized: every path that reads it has gone through a
    // successful provider call first (an exhausted walk returns early), so there
    // is no "no call yet" state to invent a value for.
    let mut last_usage: Option<(u64, u64)>;

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
        let success = provider_attempts(
            store,
            http,
            broadcaster,
            sid,
            cid,
            &configs,
            &config_names,
            &history_value,
            &tools_value,
            metrics,
            &mut events,
            AttemptPurpose::Turn,
        )
        .await?;

        let body = match success {
            Some(ok) => {
                last_usage = ok.usage;
                ok.canonical
            }
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
            // The one auto-compaction check point: a true turn boundary, with
            // this turn's own assistant message already persisted, and before
            // the turn is handed back — so the *next* `session.sendMessage` is
            // the first to see the compacted history. Never mid-loop (an
            // unresolved tool exchange only exists in the in-memory `history`
            // extension above) and never on a `Paused` outcome, whose assistant
            // `tool_use` message must stay the active history tail until the
            // client answers it.
            //
            // Every compaction event (started, the provider exchange, preamble,
            // summary, completed — or, on failure, just the attempt's audit
            // trail) is appended to this turn's `events`, so the terminal
            // `result.events` is the turn's full log.
            let auto_trigger =
                pending_auto_compaction.or_else(|| auto_trigger(session, last_usage));
            maybe_auto_compact(
                store,
                http,
                broadcaster,
                session,
                profile,
                provider_registry,
                auto_trigger,
                compaction_backoff,
                acting_client_key_id,
                metrics,
                &mut events,
            )
            .await?;
            return Ok(Turn {
                message,
                events,
                outcome: Outcome::Completed,
                pending_tool_results: Vec::new(),
                pending_auto_compaction: None,
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
            let name = tu.name.as_str();
            // One `bae.tool.dispatch` child span per dispatched block, all four
            // buckets. `input.bytes` is the serialized size — never the payload
            // (contract §4). Held across the dispatch so its duration covers the
            // work; is_error/output.bytes set below for server-executed buckets.
            let input_bytes = serde_json::to_vec(&tu.input).map(|v| v.len()).unwrap_or(0) as i64;
            let dispatch_span = crate::telemetry::tool_dispatch_span(name, dispatch, input_bytes);
            metrics.record_tool_call(dispatch);

            if *dispatch == "client" {
                // The server records the span for the client-dispatched block
                // but does not execute it (no is_error/output.bytes); the client
                // runs it and returns results on resume.
                continue;
            }
            // Only server-owned dispatches have a server execution latency.
            let dispatch_started = Instant::now();

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
                        // `bae.sandbox.exec` grandchild span, parented to this
                        // tool-dispatch span (contract §1.1).
                        let exec_span = crate::telemetry::sandbox_exec_span(&dispatch_span, name);
                        match tracing::Instrument::instrument(
                            sandbox_driver.exec(&handle, cmd),
                            exec_span.clone(),
                        )
                        .await
                        {
                            Ok(r) => {
                                let is_err = r.exit_code != 0;
                                crate::telemetry::set_i64(
                                    &exec_span,
                                    crate::telemetry::ATTR_SANDBOX_EXIT_CODE,
                                    r.exit_code as i64,
                                );
                                if is_err {
                                    crate::telemetry::set_error(&exec_span, "non-zero exit code");
                                }
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
                                // Generic span status only — a sandbox driver
                                // error string can carry forwarded command
                                // output/secrets; never export it (telemetry
                                // contract §4). Full text goes to the event log.
                                crate::telemetry::set_error(&exec_span, "sandbox exec failed");
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
                record_dispatch_outcome(
                    metrics,
                    &dispatch_span,
                    dispatch,
                    dispatch_started.elapsed(),
                    is_error,
                    &result_content,
                );
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
                    // The subagent's own `bae.subagent` span is a separate root
                    // opened in the detached task; it Links back to this
                    // dispatch span. Capture the link target now (§2.1).
                    let launch_link = crate::telemetry::span_context(&dispatch_span);
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
                        launch_link,
                    )
                    .await
                };
                events.push(log_event(
                    store, broadcaster, sid, cid, EventType::ToolResult,
                    json!({ "tool_use_id": tu.id, "dispatch": "subagent", "is_error": is_error, "content": result_content }),
                )?);
                record_dispatch_outcome(
                    metrics,
                    &dispatch_span,
                    dispatch,
                    dispatch_started.elapsed(),
                    is_error,
                    &result_content,
                );
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
                (Some(m), Some(srv)) => {
                    // `bae.mcp.call` grandchild span around the `tools/call`
                    // round trip, parented to this tool-dispatch span (§1.1).
                    let mcp_span =
                        crate::telemetry::mcp_call_span(&dispatch_span, Some(srv.as_str()), name);
                    let call = async { m.lock().await.call_tool(name, &tu.input).await };
                    match tracing::Instrument::instrument(call, mcp_span.clone()).await {
                        Ok(result) => {
                            let is_err = result
                                .get("isError")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            if is_err {
                                crate::telemetry::set_error(&mcp_span, "MCP tool returned isError");
                            }
                            let content =
                                result.get("content").cloned().unwrap_or_else(|| json!([]));
                            (
                                json!({ "server_name": srv, "ok": !is_err, "result": result }),
                                content,
                                is_err,
                            )
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            // Generic span status only — the MCP error string is
                            // an MCP server's arbitrary `error.message` and may
                            // echo forwarded input/secrets; never export it as a
                            // span attribute/status (telemetry contract §4). The
                            // full text still goes to the event log below.
                            crate::telemetry::set_error(&mcp_span, "MCP call failed");
                            (
                                json!({ "server_name": srv, "ok": false, "error": msg }),
                                mcp_error_content(&e.to_string()),
                                true,
                            )
                        }
                    }
                }
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
            record_dispatch_outcome(
                metrics,
                &dispatch_span,
                dispatch,
                dispatch_started.elapsed(),
                is_error,
                &result_content,
            );
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
                // A tool-result turn must remain the active history tail until
                // the client answers it. Preserve a threshold crossing rather
                // than appending an invalid compaction instruction here.
                pending_auto_compaction: pending_auto_compaction
                    .or_else(|| auto_trigger(session, last_usage)),
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

/// Resolve a profile's provider name references against the startup registry:
/// the primary config followed by every fallback that resolved, plus the
/// registry names aligned with that list for the `bae.provider.name` span
/// attribute. Missing fallback names are logged and skipped inside the resolver;
/// only an unusable primary is an error.
///
/// Shared by [`run_turn`] and [`run_compaction`] so a compaction walks exactly
/// the same provider chain, in the same order, as an ordinary turn.
fn resolve_provider_chain(
    profile: &ProfileRecord,
    provider_registry: &HashMap<String, ProviderConfig>,
) -> Result<(Vec<ProviderConfig>, Vec<String>), provider::ProviderConfigError> {
    let fallback_names: Vec<String> = profile
        .fallback_configs
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let (primary, fallbacks) = match profile.provider_config.as_str() {
        Some(name) => provider::resolve_from_profile(provider_registry, name, &fallback_names)?,
        None => {
            return Err(provider::ProviderConfigError::Malformed(
                "primary_provider is not a string".to_string(),
            ))
        }
    };
    let configs: Vec<ProviderConfig> = std::iter::once(primary).chain(fallbacks).collect();
    let config_names: Vec<String> = std::iter::once(
        profile
            .provider_config
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
    .chain(
        fallback_names
            .iter()
            .filter(|n| provider_registry.contains_key(*n))
            .cloned(),
    )
    .collect();
    Ok((configs, config_names))
}

/// Why a [`provider_attempts`] walk is running. The single behavioural
/// differences between the two callers: an ordinary turn records a
/// `session.error` context event when the primary fails (that failure is what
/// starts the fallback walk), while a compaction attempt writes no
/// session-level error events at all — a failed compaction is not a session
/// failure — and tags both provider payloads `purpose: "compaction"` so token
/// accounting can tell the summary call apart from real turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptPurpose {
    Turn,
    Compaction,
}

/// The first successful attempt of a [`provider_attempts`] walk.
struct AttemptSuccess {
    /// The canonical-shape response body.
    canonical: Value,
    /// The attempt's reported `(input_tokens, output_tokens)`; `None` when the
    /// provider omitted usage.
    usage: Option<(u64, u64)>,
    /// Set when the response was cut off by the output budget
    /// ([`provider::truncation_reason`]).
    truncated: Option<&'static str>,
}

/// One provider call's fallback walk: try `configs` in order, inserting a
/// `provider.request` before and a `provider.response` after **every** attempt
/// (success or failure), and return the first success. `None` means every
/// provider failed; the caller decides what that means.
///
/// Each attempt's messages are prepared for that attempt's provider kind
/// ([`provider::prepare_messages`]) — the primary and fallbacks may differ in
/// kind — and the logged `provider.request.messages` is exactly the list sent.
/// When the Anthropic normalizer had to change the sequence, the request payload
/// also carries `normalized` describing what it did.
#[allow(clippy::too_many_arguments)]
async fn provider_attempts(
    store: &Store,
    http: &reqwest::Client,
    broadcaster: &EventBroadcaster,
    sid: &str,
    cid: &str,
    configs: &[ProviderConfig],
    config_names: &[String],
    messages: &Value,
    tools: &Value,
    metrics: &crate::telemetry::Metrics,
    events: &mut Vec<EventRecord>,
    purpose: AttemptPurpose,
) -> Result<Option<AttemptSuccess>, TurnError> {
    for (i, cfg) in configs.iter().enumerate() {
        let kind = if i == 0 { "primary" } else { "fallback" };
        // One `bae.provider.attempt` child span per fallback-walk iteration,
        // wrapping the `provider::call` await (contract §1.1).
        let attempt_span = crate::telemetry::provider_attempt_span(
            config_names.get(i).map(String::as_str).unwrap_or_default(),
            cfg.provider.as_str(),
            &cfg.model,
            i,
            kind,
        );
        let (sent_messages, normalized) = provider::prepare_messages(cfg.provider, messages);
        let mut request_payload = json!({
            "attempt": i,
            "kind": kind,
            "provider": cfg.provider.as_str(),
            "base_url": cfg.effective_base_url(),
            "model": cfg.model,
            "max_tokens": cfg.max_tokens,
            "messages": sent_messages,
            "tools": tools,
        });
        if let Some(normalized) = normalized {
            request_payload["normalized"] = normalized;
        }
        if purpose == AttemptPurpose::Compaction {
            request_payload["purpose"] = json!("compaction");
        }
        events.push(log_event(
            store,
            broadcaster,
            sid,
            cid,
            EventType::ProviderRequest,
            request_payload,
        )?);

        let provider_started = Instant::now();
        match tracing::Instrument::instrument(
            provider::call(http, cfg, &sent_messages, tools),
            attempt_span.clone(),
        )
        .await
        {
            Ok(resp) => {
                metrics.record_provider_attempt(
                    cfg.provider.as_str(),
                    "ok",
                    provider_started.elapsed(),
                );
                crate::telemetry::set_i64(&attempt_span, crate::telemetry::ATTR_HTTP_STATUS, 200);
                // Read the provider's own token accounting off the raw body
                // before it is moved into the event: `usage` is not part of the
                // canonical content translation, and recording it as an explicit
                // small field means no consumer ever re-parses `body`.
                let usage = provider::usage_tokens(cfg.provider, &resp.raw);
                let truncated = provider::truncation_reason(cfg.provider, &resp.raw);
                // The event records the raw, untranslated wire body; the
                // loop consumes only the canonical translation.
                let mut response_payload = json!({ "attempt": i, "kind": kind, "provider": cfg.provider.as_str(), "ok": true, "status": 200, "body": resp.raw, "usage": usage_payload(usage) });
                if purpose == AttemptPurpose::Compaction {
                    response_payload["purpose"] = json!("compaction");
                }
                events.push(log_event(
                    store,
                    broadcaster,
                    sid,
                    cid,
                    EventType::ProviderResponse,
                    response_payload,
                )?);
                return Ok(Some(AttemptSuccess {
                    canonical: resp.canonical,
                    usage,
                    truncated,
                }));
            }
            Err(e) => {
                metrics.record_provider_attempt(
                    cfg.provider.as_str(),
                    "error",
                    provider_started.elapsed(),
                );
                if let Some(status) = e.status() {
                    crate::telemetry::set_i64(
                        &attempt_span,
                        crate::telemetry::ATTR_HTTP_STATUS,
                        status as i64,
                    );
                }
                crate::telemetry::set_error(&attempt_span, e.detail());
                let mut response_payload = json!({
                    "attempt": i, "kind": kind, "provider": cfg.provider.as_str(), "ok": false,
                    "status": e.status(), "error": e.detail(), "body": e.body(),
                });
                if purpose == AttemptPurpose::Compaction {
                    response_payload["purpose"] = json!("compaction");
                }
                events.push(log_event(
                    store,
                    broadcaster,
                    sid,
                    cid,
                    EventType::ProviderResponse,
                    response_payload,
                )?);
                // The primary failing is the trigger for the fallback walk;
                // record a session.error context event once, then continue.
                if purpose == AttemptPurpose::Turn && i == 0 {
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
    Ok(None)
}

/// The auto-mode compaction step, run once per turn at a true turn boundary.
///
/// Eligible only for a session created with
/// `compaction: {"mode":"auto","size":N}` — `Client` mode and a legacy NULL
/// config (`None`) never produce a trigger, so the server only ever rewrites
/// what it sends to the model for a session that explicitly asked for it (the
/// bounded exception to `aspec/architecture/design.md` Principle 2).
///
/// `trigger` is either the just-completed turn's threshold crossing or a
/// crossing preserved while an earlier client-tool exchange was paused.
///
/// **Backoff.** When the session's previous auto attempt failed, this turn is
/// counted toward the [`CompactionBackoff`] and a new attempt runs only once at
/// least one more turn has completed **and** the trigger's token count exceeds
/// the failed attempt's; it then runs with `reason: "retry_after_failure"`. A
/// failure (re-)arms the backoff; a success clears it.
///
/// Every event the attempt logs is appended to `events` (the turn's own log).
/// A logical [`TurnError::CompactionFailed`] is swallowed — the ordinary turn
/// that just succeeded stays successful and the session stays open. A store
/// failure still propagates.
#[allow(clippy::too_many_arguments)]
async fn maybe_auto_compact(
    store: &Store,
    http: &reqwest::Client,
    broadcaster: &EventBroadcaster,
    session: &SessionRecord,
    profile: &ProfileRecord,
    provider_registry: &HashMap<String, ProviderConfig>,
    trigger: Option<CompactionTrigger>,
    backoff: &CompactionBackoff,
    acting_client_key_id: &str,
    metrics: &crate::telemetry::Metrics,
    events: &mut Vec<EventRecord>,
) -> Result<(), TurnError> {
    // Count this completed turn against an armed backoff before deciding.
    let failed = {
        let mut map = backoff.lock().expect("compaction backoff mutex poisoned");
        map.get_mut(&session.id).map(|f| {
            f.completed_turns_since = f.completed_turns_since.saturating_add(1);
            *f
        })
    };
    let Some(mut trigger) = trigger else {
        return Ok(());
    };
    let CompactionTrigger::Auto {
        token_count,
        retry_after_failure,
        ..
    } = &mut trigger
    else {
        return Ok(());
    };
    let token_count = *token_count;
    if let Some(failed) = failed {
        if failed.completed_turns_since < 1 || token_count <= failed.token_count {
            tracing::debug!(
                session_id = %session.id,
                token_count,
                failed_token_count = failed.token_count,
                "auto compaction suppressed: backing off after a failed attempt"
            );
            return Ok(());
        }
        *retry_after_failure = true;
    }
    let result = run_compaction(
        store,
        http,
        broadcaster,
        session,
        profile,
        provider_registry,
        // Auto mode has no custom-prompt field: always the built-in prompt.
        None,
        trigger,
        acting_client_key_id,
        metrics,
        events,
    )
    .await;
    let mut map = backoff.lock().expect("compaction backoff mutex poisoned");
    match result {
        Ok(_) => {
            map.remove(&session.id);
            Ok(())
        }
        Err(TurnError::CompactionFailed(detail)) => {
            tracing::warn!(
                session_id = %session.id,
                "auto compaction attempt failed, backing off until the history grows: {detail}"
            );
            map.insert(
                session.id.clone(),
                FailedAutoCompaction {
                    token_count,
                    completed_turns_since: 0,
                },
            );
            Ok(())
        }
        // A store failure inside the compaction's own writes (e.g. its
        // transaction rolled back). The turn itself already completed and was
        // persisted, so it must not be reported as failed: record the problem
        // (best effort — the store may still be failing), back off exactly as
        // for a logical failure, and let the turn return normally.
        Err(e) => {
            tracing::error!(
                session_id = %session.id,
                "auto compaction attempt hit a store error, backing off: {e}"
            );
            map.insert(
                session.id.clone(),
                FailedAutoCompaction {
                    token_count,
                    completed_turns_since: 0,
                },
            );
            drop(map);
            match log_event(
                store,
                broadcaster,
                &session.id,
                acting_client_key_id,
                EventType::SessionError,
                json!({ "reason": "compaction_store_failed", "detail": e.to_string() }),
            ) {
                Ok(ev) => events.push(ev),
                Err(log_err) => {
                    tracing::error!("failed to log compaction_store_failed: {log_err}")
                }
            }
            Ok(())
        }
    }
}

/// Build an auto-compaction trigger from a completed provider call. The strict
/// comparison lives here so a paused turn can park the exact same decision for
/// its later safe boundary without re-reading provider events or tokenizing.
fn auto_trigger(
    session: &SessionRecord,
    last_usage: Option<(u64, u64)>,
) -> Option<CompactionTrigger> {
    let size = match &session.compaction {
        Some(CompactionConfig::Auto { size }) => *size,
        _ => return None,
    };
    let Some((input, output)) = last_usage else {
        tracing::debug!(
            session_id = %session.id,
            "auto compaction check skipped: the provider reported no usage for this turn"
        );
        return None;
    };
    let token_count = input.saturating_add(output);
    (token_count > size).then_some(CompactionTrigger::Auto {
        token_count,
        threshold_tokens: size,
        retry_after_failure: false,
    })
}

/// Compact a session: ask the model for a single self-contained summary of the
/// current effective history and make that summary the new starting point for
/// everything the server sends the model next.
///
/// Nothing is ever rewritten or deleted — `session_events` stays append-only.
/// Success persists a synthetic `user` preamble
/// (`{"role":"user","content":[…COMPACTION_PREAMBLE_TEXT…],"synthetic":"compaction_preamble"}`),
/// the summary as an `assistant` message (both `server.message.send`), and the
/// `session.compaction.completed` marker pointing at both. A later
/// [`sessions::stream_history`] scopes its scan to start at the preamble, so the
/// provider-facing history becomes `user(preamble) → assistant(summary) → …` —
/// a valid sequence for every provider — while the pre-compaction log remains
/// fully intact for replay.
///
/// Event order on success:
/// `session.compaction.started` → one or more (`provider.request` →
/// `provider.response`) attempts → preamble → summary →
/// `session.compaction.completed` (returned). The last three are inserted in
/// one SQLite transaction and broadcast only after it commits.
///
/// The call gets its own output budget: every provider entry's `max_tokens` is
/// raised to at least [`MIN_COMPACTION_MAX_TOKENS`]. It fails with
/// [`TurnError::CompactionFailed`], leaving `started` and the provider events as
/// the attempt's only trace and the session's effective history unchanged, when
/// every provider fails, when the summary was truncated by the output budget
/// (`stop_reason: "max_tokens"` / `finish_reason: "length"` — no fallback is
/// tried, it is a budget problem, not an outage), or when it contains no text.
/// A successful response without usage still commits its summary; its
/// completion fields are null because accounting is unavailable.
///
/// Every event logged is pushed onto `events`, success or failure. `prompt` is
/// the already-resolved compaction instruction (a `session.compact` caller's own
/// prompt, else the session's stored client-mode prompt); `None` uses
/// [`DEFAULT_COMPACTION_PROMPT`].
#[allow(clippy::too_many_arguments)]
pub async fn run_compaction(
    store: &Store,
    http: &reqwest::Client,
    broadcaster: &EventBroadcaster,
    session: &SessionRecord,
    profile: &ProfileRecord,
    provider_registry: &HashMap<String, ProviderConfig>,
    prompt: Option<&str>,
    trigger: CompactionTrigger,
    acting_client_key_id: &str,
    metrics: &crate::telemetry::Metrics,
    events: &mut Vec<EventRecord>,
) -> Result<EventRecord, TurnError> {
    let sid = session.id.as_str();
    let cid = acting_client_key_id;

    // Resolved before anything is logged: a profile whose primary provider no
    // longer resolves cannot start a compaction at all, so no `started` event is
    // written for an attempt that never reaches the model.
    let (mut configs, config_names) = match resolve_provider_chain(profile, provider_registry) {
        Ok(v) => v,
        Err(e) => return Err(TurnError::CompactionFailed(format!("provider_config: {e}"))),
    };
    for cfg in &mut configs {
        cfg.max_tokens = cfg.max_tokens.max(MIN_COMPACTION_MAX_TOKENS);
    }

    // The current effective history — already scoped to the most recent
    // completed compaction (starting at its preamble), so a second compaction
    // never re-summarizes already-compacted content.
    let history: Vec<Value> = store
        .with_conn(|c| sessions::stream_history(c, sid))
        .map_err(TurnError::Store)?;
    let compacted_message_count = history.len() as u64;

    let started_payload = match trigger {
        CompactionTrigger::Auto {
            token_count,
            threshold_tokens,
            retry_after_failure,
        } => json!({
            "trigger": "auto",
            "reason": if retry_after_failure { "retry_after_failure" } else { "token_threshold" },
            "token_count": token_count,
            "threshold_tokens": threshold_tokens,
        }),
        // A manual call is governed by no threshold, even on an auto session;
        // its `token_count` is the caller-supplied best-effort figure.
        CompactionTrigger::Client { token_count } => json!({
            "trigger": "client",
            "reason": "manual",
            "token_count": token_count,
            "threshold_tokens": Value::Null,
        }),
    };
    events.push(log_event(
        store,
        broadcaster,
        sid,
        cid,
        EventType::SessionCompactionStarted,
        started_payload,
    )?);

    // History plus the instruction as a final `user` turn. The server has no
    // system-role concept and this work item deliberately does not add one, so
    // compaction uses the same message shapes as every other provider call. If
    // the history already ends with a `user` message (a paused turn retired by
    // `session.compact`), the instruction is appended to it as a text block
    // rather than sent as a second consecutive user message. No tools are
    // advertised: the model's only job here is to write the summary.
    let instruction = prompt.unwrap_or(DEFAULT_COMPACTION_PROMPT);
    let mut messages = history;
    match messages.last_mut() {
        Some(last) if last.get("role").and_then(Value::as_str) == Some("user") => {
            let mut blocks = match last.get("content") {
                Some(Value::Array(blocks)) => blocks.clone(),
                Some(Value::String(text)) => vec![json!({ "type": "text", "text": text })],
                _ => Vec::new(),
            };
            blocks.push(json!({ "type": "text", "text": instruction }));
            last["content"] = Value::Array(blocks);
        }
        _ => messages.push(json!({ "role": "user", "content": instruction })),
    }
    let messages_value = Value::Array(messages);
    let tools_value = Value::Array(Vec::new());

    let success = provider_attempts(
        store,
        http,
        broadcaster,
        sid,
        cid,
        &configs,
        &config_names,
        &messages_value,
        &tools_value,
        metrics,
        events,
        AttemptPurpose::Compaction,
    )
    .await?;
    let Some(success) = success else {
        return Err(TurnError::CompactionFailed(format!(
            "all {} providers failed",
            configs.len()
        )));
    };
    if let Some(reason) = success.truncated {
        return Err(TurnError::CompactionFailed(format!(
            "summary truncated: {reason}"
        )));
    }
    let content = success
        .canonical
        .get("content")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let has_text = content.as_array().is_some_and(|blocks| {
        blocks.iter().any(|b| {
            b.get("type").and_then(Value::as_str) == Some("text")
                && b.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.trim().is_empty())
        })
    });
    if !has_text {
        return Err(TurnError::CompactionFailed("empty summary".to_string()));
    }

    // The preamble makes the replayed history start with `user`, and the
    // summary — the single message containing the fully compacted session — is
    // the assistant reply to it. The summary text is not duplicated in the
    // completed marker; it lives in the event the marker points at.
    let preamble = json!({
        "role": "user",
        "content": [{ "type": "text", "text": COMPACTION_PREAMBLE_TEXT }],
        "synthetic": SYNTHETIC_COMPACTION_PREAMBLE,
    });
    let summary = json!({ "role": "assistant", "content": content });
    let completed = json!({
        "compacted_message_count": compacted_message_count,
        "input_tokens": success.usage.map(|(input_tokens, _)| input_tokens),
        "summary_tokens": success.usage.map(|(_, summary_tokens)| summary_tokens),
    });
    let records = store
        .with_conn(|c| {
            sessions::insert_compaction_records(c, sid, Some(cid), &preamble, &summary, &completed)
        })
        .map_err(TurnError::Store)?;
    // Published only now that the transaction has committed, in write order.
    for record in [&records.preamble, &records.summary, &records.completed] {
        broadcaster.publish(record);
    }
    events.push(records.preamble);
    events.push(records.summary);
    events.push(records.completed.clone());
    Ok(records.completed)
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
    launch_link: Option<opentelemetry::trace::SpanContext>,
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
    // Construct the root span before spawning rather than inside the async
    // task. A caller can issue `session.cancelSubagent` immediately after the
    // launch response; if the scheduler has not polled the task yet, a guard
    // created inside that future would not exist for `abort()` to drop. Moving
    // construction here means every launched task owns a real guard from the
    // instant its JoinHandle is stored, and aborting it always records the
    // terminal `cancelled` outcome.
    let subagent_span_guard =
        crate::telemetry::SubagentSpanGuard::new(&session_id, &task_id, launch_link.as_ref());
    let join = tokio::spawn(async move {
        background_running_gate.notified().await;
        // The `bae.subagent` span is its OWN ROOT (never a child — the launching
        // turn's span has already ended), Linked back to the launching
        // tool-dispatch span (contract §2.1). Its guard was constructed before
        // spawning, so an abort before this task's first poll still ends the
        // span with `outcome="cancelled"`; normal paths below finish it with
        // their natural terminal outcome.
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
        // End the subagent span with its terminal outcome (Error on failure).
        // `finish` marks the guard finalized, so its `Drop` will not overwrite
        // this with `cancelled`.
        let completed = event_type == EventType::SubagentCompleted;
        subagent_span_guard.finish(
            if completed {
                crate::telemetry::SUBAGENT_OUTCOME_COMPLETED
            } else {
                crate::telemetry::SUBAGENT_OUTCOME_FAILED
            },
            !completed,
        );
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
        .map_err(TurnError::Store)?;
    Ok(Turn {
        message: json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "The provider is currently unavailable." }],
        }),
        events,
        outcome: Outcome::ProvidersFailed,
        pending_tool_results: Vec::new(),
        pending_auto_compaction: None,
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

/// Record a server-executed tool's result shape on its `bae.tool.dispatch`
/// span: `is_error`, the output byte size (size metadata only — never the
/// payload, per contract §4), and Error status when the result is an error.
/// No-op when telemetry is disabled.
fn record_dispatch_outcome(
    metrics: &crate::telemetry::Metrics,
    span: &tracing::Span,
    dispatch: &'static str,
    latency: std::time::Duration,
    is_error: bool,
    result_content: &Value,
) {
    metrics.record_tool_latency(dispatch, latency);
    let output_bytes = serde_json::to_vec(result_content)
        .map(|v| v.len())
        .unwrap_or(0) as i64;
    crate::telemetry::set_bool(span, crate::telemetry::ATTR_TOOL_IS_ERROR, is_error);
    crate::telemetry::set_i64(span, crate::telemetry::ATTR_TOOL_OUTPUT_BYTES, output_bytes);
    if is_error {
        crate::telemetry::set_error(span, "tool result is_error");
    }
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
                None,
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
