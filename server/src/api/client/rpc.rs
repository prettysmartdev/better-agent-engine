//! The JSON-RPC 2.0 session loop (`POST /api/v1/sessions/{id}/rpc`).
//!
//! This is the **one** JSON-RPC endpoint on the client port; every other
//! client-port route stays plain REST/JSON with RFC 7807 errors. It carries
//! exactly the live, session-bound message/event loop:
//!
//! - `session.registerDriver` (`{}`) — registers the calling client key as a
//!   driver for this session. Registration is a prerequisite for
//!   `session.sendMessage` (an unregistered caller gets `-32001`) and logs a
//!   broadcast `session.driver.register` event so other participants see who
//!   is driving. SDK harnesses call this once during `connect()`/`join()`.
//! - `session.sendMessage` (`{message}`) — replaces the old
//!   `POST /api/v1/sessions/{id}/messages`. Streams a `session.event`
//!   notification for every event the turn produces, in order as it happens,
//!   then one terminal response carrying the same `{message, events}` body the
//!   old route returned. Concurrent turns on one session are serialized by a
//!   per-session FIFO gate (see [`drive_send_message`]): a queued driver's
//!   NDJSON response stays open with zero bytes written until it is dequeued.
//! - `session.compact` (`{prompt?}`) — the client-driven compaction trigger
//!   (`mode: client`): asks the model for a single self-contained summary of the
//!   session so far and makes it the new starting point for provider-facing
//!   history. Requires driver registration and takes the **same** per-session
//!   turn gate as `session.sendMessage`, so a compaction can never race a live
//!   turn; a paused turn fails it fast with `-32020` (`turn in progress`)
//!   instead of waiting. Returns the produced `session.compaction.completed` event; its
//!   `session.compaction.started`/`completed` pair reaches watchers through the
//!   ordinary broadcast path (see [`compact_rpc`]).
//! - `session.subscribe` (`{since_event_id?}`) — a non-driving observer feed
//!   (subscribing **is** the observer registration act; no driver registration
//!   needed): replay persisted events after `since_event_id`, then live
//!   notifications indefinitely. No terminal response while active.
//! - `session.unsubscribe` (`{}`) — ends any active `session.subscribe` streams
//!   for this session cleanly.
//!
//! **Framing.** The HTTP response is always a stream of newline-delimited
//! JSON-RPC objects (`Content-Type: application/x-ndjson`), even for a
//! single-object reply, so a harness's transport parses this endpoint uniformly.
//! An object with no `id` is a notification; the object carrying the request's
//! `id` is the terminal response. Batch requests (arrays) are rejected.
//!
//! **Auth** is the existing `Authorization: Bearer <session-key>` header, checked
//! exactly as for the other session-scoped routes; the session id is the `{id}`
//! path segment, so `params` never carry it. Auth failure is the only pre-stream
//! error and is returned as the usual RFC 7807 body (a 401). Once authenticated,
//! every JSON-RPC-level outcome — including `-32700`/`-32600`/`-32601`/`-32602`
//! envelope errors — is delivered in the NDJSON stream.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use super::sessions::{
    auth_session, enforce_tool_allowlist, event_view, stop_remote_sandbox, tool_result_blocks,
    ClientToolDef, CompactionConfig, MessageBody, SandboxStopOutcome,
};
use crate::api::AppState;
use crate::engine::{broadcast, session};
use crate::events::EventType;
use crate::store::profiles;
use crate::store::sessions::{self, rowid_of_event, SessionRecord, STATE_ERROR, STATE_OPEN};

/// How many persisted events to page at a time during `session.subscribe`
/// replay (matches the list endpoint's hard cap).
const REPLAY_CHUNK: i64 = 200;

/// Channel buffer between the driver task and the HTTP response stream.
const STREAM_BUFFER: usize = 64;

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /api/v1/sessions/{id}/rpc` — the JSON-RPC 2.0 dispatch entrypoint.
pub async fn rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    // Auth is a transport-level gate: a bad/missing session key is rejected as
    // the usual RFC 7807 401 *before* the NDJSON stream is opened, exactly as on
    // the other session-scoped routes.
    let (session, key) = match auth_session(&state, &headers, &id) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    // The acting client key behind this session key: a session key's
    // `client_id` records the client key it was minted for (creator or
    // joiner). This is what driver registration and per-turn ownership track.
    let acting_client_key_id = key.client_id.clone().unwrap_or_default();

    // Parse the JSON-RPC envelope. Everything from here on is delivered as a
    // JSON-RPC object in the NDJSON stream, never an RFC 7807 body.
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return single(error_obj(Value::Null, -32700, "Parse error")),
    };

    // Batch requests (arrays) are explicitly unsupported: streaming a method
    // like session.sendMessage incrementally is incompatible with the JSON-RPC
    // batch rule that a batch reply is a single array returned once every
    // sub-request completes.
    if value.is_array() {
        return single(error_obj(
            Value::Null,
            -32600,
            "Invalid Request: batch requests are not supported",
        ));
    }

    let obj = match value.as_object() {
        Some(o) => o,
        None => return single(error_obj(Value::Null, -32600, "Invalid Request")),
    };

    // Notification semantics: an object with no `id` field is a JSON-RPC
    // notification — the server processes it but MUST NOT send a response
    // (neither a result nor an error). A present-but-null `id` still counts as a
    // request and is echoed. `req_id` is the value echoed on any response;
    // `id_present` gates whether a terminal response is emitted at all.
    let id_present = obj.contains_key("id");
    let req_id = obj.get("id").cloned().unwrap_or(Value::Null);

    // Require `jsonrpc: "2.0"` and a string `method`.
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return single_or_empty(
            id_present,
            error_obj(
                req_id,
                -32600,
                "Invalid Request: missing or wrong \"jsonrpc\" version",
            ),
        );
    }
    let method = match obj.get("method").and_then(Value::as_str) {
        Some(m) => m.to_owned(),
        None => {
            return single_or_empty(
                id_present,
                error_obj(req_id, -32600, "Invalid Request: missing \"method\""),
            )
        }
    };
    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));

    // The id echoed on a terminal response, or `None` for a notification (which
    // gets no response). Streaming methods thread this through; single-response
    // arms use `single_or_empty`.
    let resp_id = if id_present {
        Some(req_id.clone())
    } else {
        None
    };

    match method.as_str() {
        "session.registerDriver" => {
            // Record the caller as a driver and log it for every participant.
            // Registration is idempotent: a repeat call succeeds without
            // logging a duplicate event. A terminal session cannot gain
            // drivers (mirroring sendMessage's state gate).
            if session.state != STATE_OPEN {
                let state_str = session.state.clone();
                return single_or_empty(
                    id_present,
                    error_obj(req_id, -32000, format!("session is {state_str}, not open")),
                );
            }
            if state.register_driver(&session.id, &acting_client_key_id) {
                if let Err(e) = broadcast::insert_and_publish(
                    &state.store,
                    &state.broadcaster,
                    &session.id,
                    Some(&acting_client_key_id),
                    EventType::SessionDriverRegistered,
                    &json!({}),
                ) {
                    tracing::error!("database error in /rpc: {e}");
                    return single_or_empty(
                        id_present,
                        error_obj(req_id, -32603, "Internal error"),
                    );
                }
                // Driver-connect sandbox notification, immediately after the
                // registration event, when the session's profile declares
                // sandbox images. Scoped strictly to THIS session's own
                // profile: the list is built by iterating the profile's own
                // `available_sandboxes` and looking up each name's status
                // under this profile's id — never by flattening the
                // server-wide status map, whose other entries belong to other
                // profiles and must not leak here.
                match state
                    .store
                    .with_conn(|c| profiles::get(c, &session.profile_id))
                {
                    Ok(Some(profile)) => {
                        let images = profile.sandbox_image_names();
                        if !images.is_empty() {
                            let listed: Vec<Value> = images
                                .iter()
                                .map(|name| {
                                    let status = state.sandbox_image_status(&profile.id, name);
                                    let mut entry =
                                        json!({ "name": name, "status": status.as_str() });
                                    if let Some(d) = status.detail() {
                                        entry["detail"] = json!(d);
                                    }
                                    entry
                                })
                                .collect();
                            if let Err(e) = broadcast::insert_and_publish(
                                &state.store,
                                &state.broadcaster,
                                &session.id,
                                Some(&acting_client_key_id),
                                EventType::SandboxAvailable,
                                &json!({ "images": listed }),
                            ) {
                                tracing::error!("database error in /rpc: {e}");
                                return single_or_empty(
                                    id_present,
                                    error_obj(req_id, -32603, "Internal error"),
                                );
                            }
                        }
                    }
                    // A deleted profile is surfaced by sendMessage's own
                    // profile check; registration itself still succeeds.
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("database error in /rpc: {e}");
                        return single_or_empty(
                            id_present,
                            error_obj(req_id, -32603, "Internal error"),
                        );
                    }
                }
            }
            // Link this request's `http.request` span to the session anchor
            // span (§2.3): a driver joining an existing session is linked to it,
            // never nested under it. No-op when telemetry is off.
            if let Some(ctx) = state.session_span_context(&session.id) {
                crate::telemetry::add_link(
                    &tracing::Span::current(),
                    &ctx,
                    crate::telemetry::LINK_SESSION,
                );
            }
            single_or_empty(
                id_present,
                result_obj(req_id, json!({ "registered": true })),
            )
        }
        "session.sendMessage" => {
            // Capture the active `http.request` span's context before spawning
            // the turn task: `tokio::spawn` does not carry the tracing context,
            // so the turn span (opened in the detached task) is parented back to
            // this request via the captured context — the client↔server join.
            let parent_cx = crate::telemetry::current_context();
            spawn_stream(move |tx| {
                drive_send_message(
                    state,
                    session,
                    acting_client_key_id,
                    resp_id,
                    params,
                    parent_cx,
                    tx,
                )
            })
        }
        "session.startRemoteSandbox" => {
            start_remote_sandbox_rpc(
                &state,
                &session,
                &acting_client_key_id,
                id_present,
                req_id,
                &params,
            )
            .await
        }
        "session.stopRemoteSandbox" => {
            // Explicit stop: the same internal helper session close uses, with
            // reason "explicit" instead of "session_close".
            if let Some(resp) =
                require_registered_driver(&state, &session, &acting_client_key_id, &method)
            {
                return single_or_empty(id_present, error_obj(req_id, resp.0, resp.1));
            }
            match stop_remote_sandbox(&state, &session.id, &acting_client_key_id, "explicit").await
            {
                SandboxStopOutcome::NotRunning => single_or_empty(
                    id_present,
                    error_obj(
                        req_id,
                        -32013,
                        "sandbox_not_running: no remote sandbox is running for this session",
                    ),
                ),
                SandboxStopOutcome::Stopped { image, sandbox_id } => single_or_empty(
                    id_present,
                    result_obj(
                        req_id,
                        json!({ "stopped": true, "image": image, "sandbox_id": sandbox_id }),
                    ),
                ),
                // The handle is already removed; report the driver failure.
                SandboxStopOutcome::Failed { detail } => single_or_empty(
                    id_present,
                    error_obj(req_id, -32000, format!("sandbox stop failed: {detail}")),
                ),
            }
        }
        "session.execRemoteSandbox" => {
            exec_remote_sandbox_rpc(
                &state,
                &session,
                &acting_client_key_id,
                id_present,
                req_id,
                &params,
            )
            .await
        }
        "session.compact" => {
            compact_rpc(
                &state,
                &session,
                &acting_client_key_id,
                id_present,
                req_id,
                &params,
            )
            .await
        }
        "session.reportLocalSandbox" => report_local_sandbox_rpc(
            &state,
            &session,
            &acting_client_key_id,
            id_present,
            req_id,
            &params,
        ),
        "session.reportLocalSubagent" => report_local_subagent_rpc(
            &state,
            &session,
            &acting_client_key_id,
            id_present,
            req_id,
            &params,
        ),
        "session.cancelSubagent" => cancel_subagent_rpc(
            &state,
            &session,
            &acting_client_key_id,
            id_present,
            req_id,
            &params,
        ),
        "session.updateClientTools" => update_client_tools_rpc(
            &state,
            &session,
            &acting_client_key_id,
            id_present,
            req_id,
            &params,
        ),
        "session.subscribe" => spawn_stream(move |tx| drive_subscribe(state, session, params, tx)),
        "session.unsubscribe" => {
            // Ends active session.subscribe streams for this session. The
            // cancellation side effect happens even for a notification; only the
            // terminal response is suppressed when there is no `id`.
            state.broadcaster.cancel_subscriptions(&session.id);
            single_or_empty(
                id_present,
                result_obj(req_id, json!({ "unsubscribed": true })),
            )
        }
        _ => single_or_empty(
            id_present,
            error_obj(req_id, -32601, format!("Method not found: {method}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// session.sendMessage
// ---------------------------------------------------------------------------

/// Drive one `session.sendMessage`: check driver registration, take the
/// session's FIFO turn gate, subscribe to the session feed, record the client
/// turn, run the loop while forwarding its events live, then write the
/// terminal `{message, events}` response.
///
/// **Turn ownership.** The gate serializes entire *logical turns*, not single
/// HTTP requests: a turn that ends [`session::Outcome::Paused`] (a client-side
/// tool call in flight) parks its gate guard in `pending_turns`, so only the
/// same client key can resume without queuing — any other driver's message
/// blocks on `lock_owned()` (its NDJSON response stays open with zero bytes
/// written) until the owner completes or abandons the turn. An owner that
/// stays away past `BAE_TURN_TIMEOUT` is treated as abandoned by the next
/// arrival: a `session.error` (`driver_turn_abandoned`) is logged and the gate
/// is released to the next FIFO waiter; the session stays `open`.
#[allow(clippy::too_many_arguments)]
async fn drive_send_message(
    state: AppState,
    session: SessionRecord,
    acting_client_key_id: String,
    resp_id: Option<Value>,
    params: Value,
    parent_cx: opentelemetry::Context,
    tx: mpsc::Sender<Bytes>,
) {
    // Explicit driver registration is the gate on sending — checked before the
    // turn lock or broadcast subscription is touched. Never auto-registers.
    if !state.is_registered_driver(&session.id, &acting_client_key_id) {
        emit_terminal(&tx, &resp_id, |id| {
            error_obj(
                id,
                -32001,
                "call session.registerDriver before session.sendMessage",
            )
        })
        .await;
        return;
    }

    if session.state != STATE_OPEN {
        let state_str = session.state.clone();
        emit_terminal(&tx, &resp_id, |id| {
            error_obj(id, -32000, format!("session is {state_str}, not open"))
        })
        .await;
        return;
    }

    // `params.message` is required and must carry `content`. Validated before
    // queuing on the gate so a malformed request fails fast instead of waiting
    // its turn.
    let message: MessageBody = match params.get("message").cloned() {
        Some(m) => match serde_json::from_value(m) {
            Ok(m) => m,
            Err(e) => {
                emit_terminal(&tx, &resp_id, |id| {
                    error_obj(id, -32602, format!("Invalid params: {e}"))
                })
                .await;
                return;
            }
        },
        None => {
            emit_terminal(&tx, &resp_id, |id| {
                error_obj(id, -32602, "Invalid params: missing \"message\"")
            })
            .await;
            return;
        }
    };

    // --- FIFO single-active-message mutex ---
    // (1) A paused turn owned by this caller is resumed by reclaiming its
    // parked guard — no queuing; ownership is checked by client key id, not by
    // the message's shape, so the owner may also abandon its pending tool call
    // voluntarily by sending a new message. (2) A paused turn whose owner
    // stayed away past the deadline is abandoned: the next arrival reclaims
    // the parked guard and completes the old exchange with cancellation
    // results before serving its message. (3) Otherwise queue on the gate;
    // `tokio::sync::Mutex` grants `lock_owned()` in request order.
    let mut abandoned_owner: Option<String> = None;
    let mut resumed_paused_turn = false;
    let mut timed_out_paused_turn = false;
    // The server-side tool results parked on this caller's paused turn (a mixed
    // turn); empty unless this request resumes such a turn. Merged with the
    // client's own results below, before the turn is recorded.
    let mut stashed_server_results: Vec<Value> = Vec::new();
    // A threshold crossing cannot compact an unresolved tool_use. Carry its
    // tiny trigger with the parked gate so a later final response cannot erase
    // the decision merely by reporting lower/no usage.
    let mut pending_auto_compaction = None;
    // The paused turn's span context, for the resume Link on the new turn span.
    let mut resumed_span_context: Option<opentelemetry::trace::SpanContext> = None;
    let reclaimed = match claim_pending_turn(&state, &session.id, &acting_client_key_id, true) {
        PendingClaim::Owned(pt) => {
            resumed_paused_turn = true;
            stashed_server_results = pt.server_tool_results;
            pending_auto_compaction = pt.pending_auto_compaction;
            resumed_span_context = pt.span_context;
            Some(pt.guard)
        }
        PendingClaim::Expired(pt) => {
            abandoned_owner = Some(pt.owner_client_key_id);
            resumed_paused_turn = true;
            timed_out_paused_turn = true;
            stashed_server_results = pt.server_tool_results;
            pending_auto_compaction = pt.pending_auto_compaction;
            resumed_span_context = pt.span_context;
            // Reclaim the parked guard for this request. This retires the
            // expired exchange before any queued message can interleave with
            // its synthetic cancellation results.
            Some(pt.guard)
        }
        PendingClaim::None | PendingClaim::Held => None,
    };
    if let Some(owner) = abandoned_owner {
        log_turn_abandoned(&state, &session.id, &owner);
    }
    let gate_guard = match reclaimed {
        Some(g) => g,
        // The point where a second driver's message genuinely waits its turn.
        None => state.turn_gate(&session.id).lock_owned().await,
    };
    // Held for the rest of this call; on a Paused outcome it is parked in
    // `pending_turns` instead of dropped. Every other exit path (completion,
    // error, client disconnect) drops it, releasing the next FIFO waiter.
    let mut gate_guard = Some(gate_guard);

    // Re-read the session now that the gate is held: while this message was
    // queued, the previous turn may have moved the session to `error`, or a
    // close/revocation may have ended it — a dequeued message must never drive
    // a terminal session. The fresh record also carries any `client_tools`
    // entries added by joins since this request authenticated.
    let session = match state
        .store
        .with_conn(|c| sessions::get_session(c, &session.id))
    {
        Ok(Some(s)) => s,
        Ok(None) => {
            emit_terminal(&tx, &resp_id, |id| {
                error_obj(id, -32000, "session no longer exists")
            })
            .await;
            return;
        }
        Err(e) => return emit_internal(&tx, &resp_id, e).await,
    };
    if session.state != STATE_OPEN {
        let state_str = session.state.clone();
        emit_terminal(&tx, &resp_id, |id| {
            error_obj(id, -32000, format!("session is {state_str}, not open"))
        })
        .await;
        return;
    }

    // Subscribe *before* running the turn so no event the turn produces slips
    // past us — but only after the gate is held, so a queued caller's stream
    // stays silent while another driver's turn is in flight. Every event is
    // broadcast to all watchers; the only events withheld from *this caller's*
    // own feed are the ones it literally sent in this request (its
    // `client.message.send` and any client-dispatched `tool.result`), tracked in
    // `self_event_ids` below so they are never echoed straight back to the
    // sender — while genuine observers on other connections still see them live.
    let (mut rx, _cancel) = state.broadcaster.subscribe(&session.id);

    // Record the incoming client turn (and any returned tool_result blocks),
    // attributed to the acting client key (the driver sending this message,
    // not necessarily the session's creator). These go through the publish
    // choke point and are broadcast to observers, but suppressed from the
    // sender's own stream via `self_event_ids`.
    // Move the client's content out so it can be merged in place. `message.role`
    // stays available (partial move).
    let mut content = message.content;

    // Resume of any paused turn: reassemble the single `user` turn the provider
    // requires from the server-side results dispatched before the pause (empty
    // for an all-client turn) and this client's own results — BEFORE recording
    // it as `client.message.send`, so `stream_history` always replays a
    // well-formed alternating transcript. The ids of the server-dispatched
    // blocks; their `tool.result` events were already logged at dispatch time,
    // so they are not re-logged below.
    let server_ids: HashSet<String> = stashed_server_results
        .iter()
        .filter_map(|b| {
            b.get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    if resumed_paused_turn {
        let assistant_tool_uses = match state
            .store
            .with_conn(|c| sessions::last_assistant_tool_uses(c, &session.id))
        {
            Ok(v) => v,
            Err(e) => return emit_internal(&tx, &resp_id, e).await,
        };
        let merged = if message.role != "user" {
            Err(format!(
                "paused tool continuation must have role \"user\", got {:?}",
                message.role
            ))
        } else if timed_out_paused_turn || is_voluntary_abandonment(&content) {
            cancel_abandoned_tool_uses(&assistant_tool_uses, &stashed_server_results, &content)
        } else {
            merge_tool_results(&assistant_tool_uses, &stashed_server_results, &content)
        };
        match merged {
            Ok(merged) => content = merged,
            Err(reason) => {
                // Fail loudly: the merged turn would not answer exactly the
                // paused assistant turn's tool_use ids (a missing, duplicate, or
                // unexpected id). Abandon this turn rather than forward a
                // malformed body upstream (Anthropic would 400).
                fail_tool_result_merge(&state, &session.id, &acting_client_key_id, &reason);
                emit_terminal(&tx, &resp_id, |id| {
                    error_obj(id, -32000, format!("tool result merge invalid: {reason}"))
                })
                .await;
                return;
            }
        }
    } else if !tool_result_blocks(&content).is_empty() {
        // No paused turn is waiting for these results — typically its owner
        // stayed away past `BAE_TURN_TIMEOUT` and another driver's message or
        // `session.compact` already retired it. Forwarding them would put an
        // orphan `tool_result` in the transcript (Anthropic answers 400 and the
        // session would be torn down), so reject this message and keep the
        // session open.
        emit_terminal(&tx, &resp_id, |id| {
            error_obj(
                id,
                -32000,
                "tool result merge invalid: no paused turn is awaiting tool results \
                 (it may have timed out and been retired)",
            )
        })
        .await;
        return;
    }

    let mut all_events = Vec::new();
    let msg_payload = json!({ "role": message.role, "content": content });
    match broadcast::insert_and_publish(
        &state.store,
        &state.broadcaster,
        &session.id,
        Some(&acting_client_key_id),
        EventType::ClientMessageSend,
        &msg_payload,
    ) {
        Ok(ev) => all_events.push(ev),
        Err(e) => return emit_internal(&tx, &resp_id, e).await,
    }
    for block in tool_result_blocks(&content) {
        // Skip the server-dispatched blocks merged into `content`; their
        // `tool.result` events were already logged when they were dispatched.
        if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
            if server_ids.contains(id) {
                continue;
            }
        }
        match broadcast::insert_and_publish(
            &state.store,
            &state.broadcaster,
            &session.id,
            Some(&acting_client_key_id),
            EventType::ToolResult,
            &block,
        ) {
            Ok(ev) => all_events.push(ev),
            Err(e) => return emit_internal(&tx, &resp_id, e).await,
        }
    }

    // The events this request's client literally sent — never echoed back to it
    // on its own stream (other observers still receive them live).
    let self_event_ids: HashSet<String> = all_events.iter().map(|e| e.id.clone()).collect();

    // The profile could have been deleted mid-session.
    let profile = match state
        .store
        .with_conn(|c| profiles::get(c, &session.profile_id))
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            let _ = broadcast::insert_and_publish(
                &state.store,
                &state.broadcaster,
                &session.id,
                Some(&acting_client_key_id),
                EventType::SessionError,
                &json!({ "reason": "profile_unavailable", "profile_id": session.profile_id }),
            );
            let _ = state
                .store
                .with_conn(|c| sessions::close_session(c, &session.id, STATE_ERROR));
            // The session moved to terminal `error`: end its anchor span now
            // (marked Error) so it is not left live until a later DELETE or
            // shutdown (contract §1.1).
            state.end_session_span(&session.id, Some("profile unavailable"));
            emit_terminal(&tx, &resp_id, |id| {
                error_obj(
                    id,
                    -32000,
                    "profile_unavailable: the profile bound to this session is no longer available",
                )
            })
            .await;
            return;
        }
        Err(e) => return emit_internal(&tx, &resp_id, e).await,
    };

    // Run the loop concurrently with forwarding its events as notifications.
    // The acting client key scopes the turn: only its declared tools (plus
    // session-wide MCP tools) are advertised, and every event the turn logs is
    // attributed to it.
    let mcp = state.mcp_session(&session.id);
    let sandbox = state.sandbox(&session.id);

    // Open the `bae.turn` span: parented to this request's `http.request` span
    // (via `parent_cx`, captured before the spawn), always Linked to the
    // session anchor span, and — on a resume — additionally Linked to the paused
    // turn's stored context. `run_turn`'s provider/tool/mcp/sandbox spans nest
    // under it via `.instrument(...)`. No-op span when telemetry is disabled.
    let turn_span = crate::telemetry::turn_span(
        &parent_cx,
        state.session_span_context(&session.id).as_ref(),
        resumed_span_context.as_ref(),
        &session.id,
        &acting_client_key_id,
    );

    let turn_fut = tracing::Instrument::instrument(
        session::run_turn(
            &state.store,
            &state.http,
            &state.broadcaster,
            &session,
            &profile,
            &state.provider_registry,
            mcp,
            state.sandbox_driver.clone(),
            sandbox,
            state.subagents.clone(),
            state.command_runner.clone(),
            state.subagent_timeout,
            state.max_subagents_per_session,
            &acting_client_key_id,
            pending_auto_compaction,
            &state.compaction_backoff,
            &state.telemetry_metrics,
        ),
        turn_span.clone(),
    );
    tokio::pin!(turn_fut);

    // `watching` guards the broadcast branch: once the channel closes (e.g. a
    // concurrent DELETE removed it) we stop polling it and just await the turn,
    // rather than spinning on a ready `Closed`.
    let mut watching = true;
    let turn_result = loop {
        tokio::select! {
            biased;
            recv = rx.recv(), if watching => match recv {
                Ok(record) => {
                    // Never echo the caller's own sent events back to it.
                    if self_event_ids.contains(&record.id) {
                        continue;
                    }
                    if !emit(&tx, &notification(&record)).await {
                        return; // client disconnected
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let _ = emit(&tx, &lagged_error()).await;
                    return;
                }
                Err(RecvError::Closed) => watching = false,
            },
            turn = &mut turn_fut => break turn,
        }
    };

    // Flush any events published between the last poll and the turn returning.
    while let Ok(record) = rx.try_recv() {
        if self_event_ids.contains(&record.id) {
            continue;
        }
        if !emit(&tx, &notification(&record)).await {
            return;
        }
    }

    match turn_result {
        Ok(turn) => {
            // Record the turn outcome on its span (and Error status on a
            // provider-exhausted turn), then end the session anchor span when
            // the turn moved the session to terminal `error`.
            let outcome_str = match turn.outcome {
                session::Outcome::Completed => "completed",
                session::Outcome::Paused => "paused",
                session::Outcome::ProvidersFailed => "providers_failed",
            };
            state.telemetry_metrics.record_turn_completed(outcome_str);
            crate::telemetry::set_str(&turn_span, crate::telemetry::ATTR_TURN_OUTCOME, outcome_str);
            if turn.outcome == session::Outcome::ProvidersFailed {
                crate::telemetry::set_error(&turn_span, "all providers failed");
                state.end_session_span(&session.id, Some("all providers failed"));
            }

            // A Paused turn (client-side tool call in flight) keeps holding the
            // FIFO gate across requests: park the guard so only this caller's
            // continuation resumes without queuing. Completed/ProvidersFailed
            // turns let the guard drop at the end of this call, releasing the
            // next FIFO waiter.
            if turn.outcome == session::Outcome::Paused {
                if let Some(guard) = gate_guard.take() {
                    // Capture the paused turn span's context so the resuming
                    // request's new turn span can Link back to it (§2.2).
                    let span_context = crate::telemetry::span_context(&turn_span);
                    state
                        .pending_turns
                        .lock()
                        .expect("pending_turns mutex poisoned")
                        .insert(
                            session.id.clone(),
                            crate::api::PendingTurn {
                                owner_client_key_id: acting_client_key_id.clone(),
                                guard,
                                deadline: tokio::time::Instant::now() + state.turn_timeout,
                                // The server-side results dispatched before this
                                // pause, to merge with the client's results when
                                // this owner resumes (empty on an all-client
                                // pause). Freed with the PendingTurn if abandoned.
                                server_tool_results: turn.pending_tool_results,
                                pending_auto_compaction: turn.pending_auto_compaction,
                                span_context,
                            },
                        );
                }
            }
            // The terminal result is the exact `{message, events}` body the old
            // `POST /messages` returned — a provider failure (session moved to
            // `error`) surfaces here too, in `result.message`, rather than as a
            // separate HTTP status.
            all_events.extend(turn.events);
            let events_json: Vec<Value> = all_events.iter().map(event_view).collect();
            let result = json!({ "message": turn.message, "events": events_json });
            emit_terminal(&tx, &resp_id, |id| result_obj(id, result)).await;
        }
        Err(e) => {
            tracing::error!("session loop failed: {e}");
            crate::telemetry::set_error(&turn_span, "session loop failed");
            emit_terminal(&tx, &resp_id, |id| {
                error_obj(id, -32000, "session loop failed")
            })
            .await;
        }
    }
    drop(turn_span);
    drop(gate_guard);
}

/// What [`claim_pending_turn`] found parked in `pending_turns` for a session.
enum PendingClaim {
    /// No paused turn.
    None,
    /// The caller's own paused turn, removed from `pending_turns` so the caller
    /// resumes it (its guard is the session's turn gate).
    Owned(crate::api::PendingTurn),
    /// A paused turn whose owner stayed away past `BAE_TURN_TIMEOUT`, removed
    /// so the caller can retire it (its guard is the session's turn gate).
    Expired(crate::api::PendingTurn),
    /// A live paused turn the caller may not take over; left parked.
    Held,
}

/// Inspect — and, for a turn the caller may take over, remove — the session's
/// parked paused turn. Shared by `session.sendMessage` (`owner_may_resume`:
/// the owner resumes its own pause) and `session.compact` (never resumes a
/// pause; only an expired one is reclaimed). An expired pause is reclaimed by
/// any caller, including its owner.
fn claim_pending_turn(
    state: &AppState,
    session_id: &str,
    caller: &str,
    owner_may_resume: bool,
) -> PendingClaim {
    let mut pending = state
        .pending_turns
        .lock()
        .expect("pending_turns mutex poisoned");
    let Some(pt) = pending.get(session_id) else {
        return PendingClaim::None;
    };
    if owner_may_resume && pt.owner_client_key_id == caller {
        return pending
            .remove(session_id)
            .map_or(PendingClaim::None, PendingClaim::Owned);
    }
    if tokio::time::Instant::now() > pt.deadline {
        return pending
            .remove(session_id)
            .map_or(PendingClaim::None, PendingClaim::Expired);
    }
    PendingClaim::Held
}

/// Log `session.error` `driver_turn_abandoned` for a reclaimed expired pause,
/// through the broadcast choke point so live watchers see the abandonment; the
/// session stays open (unlike a provider failure).
fn log_turn_abandoned(state: &AppState, session_id: &str, owner: &str) {
    if let Err(e) = broadcast::insert_and_publish(
        &state.store,
        &state.broadcaster,
        session_id,
        Some(owner),
        EventType::SessionError,
        &json!({ "reason": "driver_turn_abandoned", "owner_client_key_id": owner }),
    ) {
        tracing::error!("failed to log driver_turn_abandoned: {e}");
    }
}

/// Tear down a session whose paused turn cannot be answered with a valid
/// `user` turn: log `session.error` `tool_result_merge_invalid` and move the
/// session to terminal `error`. Its durable assistant `tool_use` message is
/// intentionally not replayable without a valid immediately-following result.
fn fail_tool_result_merge(state: &AppState, session_id: &str, acting: &str, reason: &str) {
    let _ = broadcast::insert_and_publish(
        &state.store,
        &state.broadcaster,
        session_id,
        Some(acting),
        EventType::SessionError,
        &json!({ "reason": "tool_result_merge_invalid", "detail": reason }),
    );
    let _ = state
        .store
        .with_conn(|c| sessions::close_session(c, session_id, STATE_ERROR));
    // The session moved to terminal `error`: end its anchor span now (marked
    // Error), matching the provider-exhaustion teardown, so it is not left live
    // with an inflated duration and Unset status (contract §1.1).
    state.end_session_span(session_id, Some("tool result merge invalid"));
}

/// Reassemble the single `user`-turn content that answers a paused mixed
/// assistant turn, from the server-side results dispatched before the pause and
/// the client's own results.
///
/// - `assistant_tool_uses`: the paused assistant turn's `tool_use` blocks, in
///   order (each with an `id`). Defines the exact id set the merged turn must
///   answer and the output ordering.
/// - `server_results`: server-dispatched `tool_result` blocks (sandbox/MCP).
///   These **win**: a client-supplied result for the same id is dropped.
/// - `client_content`: the incoming client message content — its own
///   `tool_result` blocks plus any other blocks (e.g. text), which are
///   preserved after the results.
///
/// Returns the ordered merged content array, or `Err(reason)` when the merged
/// results would not answer exactly the assistant turn's `tool_use` ids: a
/// missing id (would 400 upstream), an unexpected id (not in the turn), or a
/// duplicate client id.
fn merge_tool_results(
    assistant_tool_uses: &[Value],
    server_results: &[Value],
    client_content: &Value,
) -> Result<Value, String> {
    // Ordered ids the merged turn must answer.
    let expected: Vec<String> = assistant_tool_uses
        .iter()
        .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    if expected.is_empty() {
        return Err("paused assistant turn has no tool_use ids".to_string());
    }
    let expected_set: HashSet<&str> = expected.iter().map(String::as_str).collect();
    if expected_set.len() != expected.len() {
        return Err("paused assistant turn contains duplicate tool_use ids".to_string());
    }

    // Server results by id (server wins on collision below).
    let mut server_by_id: HashMap<String, Value> = HashMap::new();
    for b in server_results {
        let id = b
            .get("tool_use_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "server tool_result is missing a string tool_use_id".to_string())?;
        if server_by_id.insert(id.to_owned(), b.clone()).is_some() {
            return Err(format!(
                "server produced a duplicate tool_result for id {id:?}"
            ));
        }
    }

    // Client results by id, dropping any that collide with a server id; any
    // non-`tool_result` blocks are preserved as trailing extras.
    let mut client_by_id: HashMap<String, Value> = HashMap::new();
    let mut extras: Vec<Value> = Vec::new();
    for b in client_content.as_array().cloned().unwrap_or_default() {
        if b.get("type").and_then(Value::as_str) == Some("tool_result") {
            let id = b
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if server_by_id.contains_key(&id) {
                continue; // server result wins; drop the client's copy
            }
            if client_by_id.insert(id.clone(), b).is_some() {
                return Err(format!(
                    "client returned a duplicate tool_result for id {id:?}"
                ));
            }
        } else {
            extras.push(b);
        }
    }

    // Reject any id (from either side) not present in the assistant turn.
    for id in server_by_id.keys().chain(client_by_id.keys()) {
        if !expected_set.contains(id.as_str()) {
            return Err(format!(
                "tool_result id {id:?} is not in the paused assistant turn"
            ));
        }
    }

    // One result per assistant tool_use, in the assistant turn's order; server
    // wins, then any client result, else the id is unanswered.
    let mut merged: Vec<Value> = Vec::with_capacity(expected.len() + extras.len());
    for id in &expected {
        if let Some(b) = server_by_id.remove(id) {
            merged.push(b);
        } else if let Some(b) = client_by_id.remove(id) {
            merged.push(b);
        } else {
            return Err(format!("no tool_result for tool_use id {id:?}"));
        }
    }
    merged.extend(extras);
    Ok(Value::Array(merged))
}

/// A same-owner plain message, or the first new message after a pause timeout,
/// abandons the outstanding client calls. Complete the paused provider
/// exchange by synthesizing error results for every id the server did not
/// already answer, then retain the new message as trailing user content. This
/// keeps the session open without ever persisting an unanswered assistant
/// `tool_use` turn.
fn cancel_abandoned_tool_uses(
    assistant_tool_uses: &[Value],
    server_results: &[Value],
    incoming_content: &Value,
) -> Result<Value, String> {
    let server_ids: HashSet<&str> = server_results
        .iter()
        .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
        .collect();
    let mut client_content = Vec::new();
    for tool_use in assistant_tool_uses {
        let id = tool_use
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "paused tool_use is missing a string id".to_string())?;
        if !server_ids.contains(id) {
            client_content.push(json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": "Client tool call was abandoned before returning a result.",
                "is_error": true,
            }));
        }
    }

    match incoming_content {
        Value::String(text) if !text.is_empty() => {
            client_content.push(json!({ "type": "text", "text": text }));
        }
        Value::Array(blocks) => {
            if blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            {
                return Err(
                    "timed-out replacement message must not contain tool_result blocks".to_string(),
                );
            }
            client_content.extend(blocks.iter().cloned());
        }
        Value::String(_) => {}
        _ => {
            return Err(
                "abandonment message content must be a string or content-block array".to_string(),
            );
        }
    }

    merge_tool_results(
        assistant_tool_uses,
        server_results,
        &Value::Array(client_content),
    )
}

/// A non-empty message without any tool results is a deliberate replacement
/// for the paused tool continuation. Empty arrays remain malformed
/// continuations and are rejected by the exact-id validator.
fn is_voluntary_abandonment(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.is_empty(),
        Value::Array(blocks) => {
            !blocks.is_empty()
                && !blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// session.compact
// ---------------------------------------------------------------------------

/// How often a queued `session.compact` re-checks for a paused turn while it
/// waits on the turn gate.
const COMPACT_PAUSE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// `session.compact` params. `{}`, `{"prompt":null}`, and `{"prompt":"..."}` are
/// all accepted; anything else in `prompt` is `-32602`.
#[derive(Debug, Default, serde::Deserialize)]
struct CompactParams {
    #[serde(default)]
    prompt: Option<String>,
}

/// `session.compact` (`params: {"prompt"?: string}`) — the client-driven
/// compaction trigger, the `mode: client` counterpart to auto mode's in-turn
/// check.
///
/// Gated exactly like the other driver methods (`-32001` without
/// `session.registerDriver`) and, unlike them, serialized through the **same**
/// per-session `turn_gates` mutex `session.sendMessage` uses: compaction and a
/// live turn both decide what "current history" means, so they must never run
/// concurrently on one session. Waiting on that gate is also why the session and
/// profile are re-read after it is acquired, exactly as [`drive_send_message`]
/// does — a queued caller must not compact a session that has since gone
/// terminal.
///
/// Permitted on either stored mode: a harness may force an early compaction on
/// an `auto` session, and a `prompt` given here applies to that one call without
/// touching the session's persisted config. Prompt precedence is this call's
/// `prompt`, then the session's stored `mode: client` prompt, then the engine's
/// built-in default.
///
/// **Paused turns.** A compaction cannot run against an unanswered `tool_use`,
/// and a paused turn parks the gate until its owner resumes it — so waiting
/// would deadlock a harness that pauses on a tool call and then compacts. If
/// the session has a live paused turn (any owner, including the caller) the
/// call fails immediately with `-32020` (`turn in progress: resolve the paused
/// turn before compacting`), including when a turn that was running while this
/// call queued pauses before releasing the gate. A pause past
/// `BAE_TURN_TIMEOUT` is reclaimed exactly as `session.sendMessage` reclaims it
/// (`session.error` `driver_turn_abandoned`), retired with one synthetic
/// `role: "user"` `server.message.send` (`synthetic: "abandoned_tool_results"`)
/// answering every outstanding `tool_use`, and the compaction proceeds. A
/// running (not paused) turn is still waited for on the gate.
///
/// Manual compaction ignores the auto-compaction backoff; a success clears it.
///
/// The terminal result is the produced `session.compaction.completed` event
/// record. Like `session.sendMessage`, the response is an NDJSON stream: once
/// the gate is held the caller is subscribed to the session feed, and every
/// event the compaction logs (an expired pause's retirement, `started`, the
/// provider exchange, the preamble, the summary, `completed`) is forwarded as a
/// `session.event` notification frame before the terminal frame. Failures that
/// happen before the gate is taken (`-32001`, a non-open session, `-32602`)
/// answer with a single frame.
async fn compact_rpc(
    state: &AppState,
    session: &SessionRecord,
    acting_client_key_id: &str,
    id_present: bool,
    req_id: Value,
    params: &Value,
) -> Response {
    if let Some((code, msg)) =
        require_registered_driver(state, session, acting_client_key_id, "session.compact")
    {
        return single_or_empty(id_present, error_obj(req_id, code, msg));
    }
    if session.state != STATE_OPEN {
        let state_str = session.state.clone();
        return single_or_empty(
            id_present,
            error_obj(req_id, -32000, format!("session is {state_str}, not open")),
        );
    }
    // Validated before queuing on the gate, so a malformed request fails fast
    // instead of waiting out another driver's turn.
    let call_params: CompactParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return single_or_empty(
                id_present,
                error_obj(req_id, -32602, format!("Invalid params: {e}")),
            )
        }
    };
    let state = state.clone();
    let session_id = session.id.clone();
    let acting = acting_client_key_id.to_string();
    let resp_id = id_present.then_some(req_id);
    spawn_stream(move |tx| drive_compact(state, session_id, acting, resp_id, call_params, tx))
}

/// Drive one `session.compact` once its params are valid: queue on the turn
/// gate (failing fast on a live pause), subscribe to the session feed, run the
/// compaction while forwarding its events live, then write the terminal frame.
async fn drive_compact(
    state: AppState,
    session_id: String,
    acting_client_key_id: String,
    resp_id: Option<Value>,
    call_params: CompactParams,
    tx: mpsc::Sender<Bytes>,
) {
    // The point where a compaction genuinely waits for an in-flight turn (and
    // vice versa). Held across the history read, the provider call, and every
    // event insert below. While queued, keep checking for a pause: a paused
    // turn never releases the gate on its own, so fail fast (or reclaim an
    // expired one) instead of waiting. The lock future stays pinned across
    // checks so this call keeps its FIFO position.
    let gate = state.turn_gate(&session_id);
    let lock = gate.lock_owned();
    tokio::pin!(lock);
    let mut pause_check = tokio::time::interval(COMPACT_PAUSE_CHECK_INTERVAL);
    let (_gate, expired_pause) = loop {
        match claim_pending_turn(&state, &session_id, &acting_client_key_id, false) {
            PendingClaim::None => {}
            PendingClaim::Expired(pt) => {
                break (
                    pt.guard,
                    Some((pt.owner_client_key_id, pt.server_tool_results)),
                )
            }
            // `owner_may_resume = false` never yields `Owned`; dropping its
            // guard here would silently release the owner's paused turn.
            PendingClaim::Owned(_) => unreachable!("session.compact never resumes a pause"),
            PendingClaim::Held => {
                emit_terminal(&tx, &resp_id, |id| {
                    error_obj(
                        id,
                        -32020,
                        "turn in progress: resolve the paused turn before compacting",
                    )
                })
                .await;
                return;
            }
        }
        tokio::select! {
            biased;
            guard = &mut lock => break (guard, None),
            _ = pause_check.tick() => {}
        }
    };

    // Subscribe only once the gate is held, so a queued caller's stream stays
    // silent while another driver's turn is in flight, and before anything is
    // logged, so no compaction event slips past.
    let (mut rx, _cancel) = state.broadcaster.subscribe(&session_id);

    let work = compact_under_gate(
        &state,
        &session_id,
        &acting_client_key_id,
        expired_pause,
        call_params,
    );
    tokio::pin!(work);

    // Forward events while the compaction runs. A disconnected or lagged
    // caller stops the forwarding but never the compaction itself: it already
    // holds the gate and its record is written atomically.
    let mut streaming = true;
    let outcome = loop {
        tokio::select! {
            biased;
            recv = rx.recv(), if streaming => match recv {
                Ok(record) => {
                    if !emit(&tx, &notification(&record)).await {
                        streaming = false;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let _ = emit(&tx, &lagged_error()).await;
                    streaming = false;
                }
                Err(RecvError::Closed) => streaming = false,
            },
            outcome = &mut work => break outcome,
        }
    };
    if !streaming {
        return;
    }
    // Flush any events published between the last poll and the work returning.
    while let Ok(record) = rx.try_recv() {
        if !emit(&tx, &notification(&record)).await {
            return;
        }
    }
    match outcome {
        Ok(result) => emit_terminal(&tx, &resp_id, |id| result_obj(id, result)).await,
        Err((code, message)) => {
            emit_terminal(&tx, &resp_id, |id| error_obj(id, code, message)).await
        }
    }
}

/// Everything `session.compact` does while holding the turn gate. Returns the
/// terminal result (the `completed` event view) or the `(code, message)` error.
async fn compact_under_gate(
    state: &AppState,
    session_id: &str,
    acting_client_key_id: &str,
    expired_pause: Option<(String, Vec<Value>)>,
    call_params: CompactParams,
) -> Result<Value, (i64, String)> {
    let internal = |e: rusqlite::Error| {
        tracing::error!("database error in /rpc: {e}");
        (-32603, "Internal error".to_string())
    };
    if let Some((owner, _)) = &expired_pause {
        log_turn_abandoned(state, session_id, owner);
    }

    // Re-read the session now the gate is held: the turn that was in flight
    // while this call queued may have moved it to `error`, or a close may have
    // ended it. The fresh record also carries the authoritative compaction
    // config for the stored-prompt fallback.
    let session = match state
        .store
        .with_conn(|c| sessions::get_session(c, session_id))
    {
        Ok(Some(s)) => s,
        Ok(None) => return Err((-32000, "session no longer exists".to_string())),
        Err(e) => return Err(internal(e)),
    };
    if session.state != STATE_OPEN {
        return Err((-32000, format!("session is {}, not open", session.state)));
    }

    // Retire a reclaimed expired pause so the persisted history is a valid
    // provider transcript again (`assistant(tool_use) → user(tool_result…)`):
    // the server results dispatched before the pause plus a synthetic error
    // result for every unanswered client call — the same content a timed-out
    // `session.sendMessage` records — as one synthetic `user` server message,
    // followed (again as `session.sendMessage` does) by a `tool.result` event
    // for every block that was not already logged at server dispatch time.
    if let Some((_, stashed_server_results)) = expired_pause {
        let tool_uses = state
            .store
            .with_conn(|c| sessions::last_assistant_tool_uses(c, &session.id))
            .map_err(internal)?;
        let content = match cancel_abandoned_tool_uses(
            &tool_uses,
            &stashed_server_results,
            &Value::Array(Vec::new()),
        ) {
            Ok(content) => content,
            Err(reason) => {
                fail_tool_result_merge(state, &session.id, acting_client_key_id, &reason);
                return Err((-32000, format!("tool result merge invalid: {reason}")));
            }
        };
        broadcast::insert_and_publish(
            &state.store,
            &state.broadcaster,
            &session.id,
            Some(acting_client_key_id),
            EventType::ServerMessageSend,
            &json!({
                "role": "user",
                "content": content,
                "synthetic": session::SYNTHETIC_ABANDONED_TOOL_RESULTS,
            }),
        )
        .map_err(internal)?;
        let server_ids: HashSet<&str> = stashed_server_results
            .iter()
            .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
            .collect();
        for block in tool_result_blocks(&content) {
            if block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_some_and(|id| server_ids.contains(id))
            {
                continue;
            }
            broadcast::insert_and_publish(
                &state.store,
                &state.broadcaster,
                &session.id,
                Some(acting_client_key_id),
                EventType::ToolResult,
                &block,
            )
            .map_err(internal)?;
        }
    }

    let profile = match state
        .store
        .with_conn(|c| profiles::get(c, &session.profile_id))
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err((
                -32000,
                "profile_unavailable: the profile bound to this session is no longer available"
                    .to_string(),
            ))
        }
        Err(e) => return Err(internal(e)),
    };

    // A manual trigger has no "call that just happened" to read usage from, so
    // the `started` event's `token_count` is the best-effort figure from the
    // session's newest persisted `provider.response` — informational only, and
    // `None` when the session has made no provider call with usable usage yet.
    let token_count = state
        .store
        .with_conn(|c| sessions::last_provider_token_count(c, &session.id))
        .map_err(internal)?;

    // This call's prompt wins; otherwise a `mode: client` session's stored
    // prompt; otherwise the engine's built-in default (`None` here). An `auto`
    // session has no stored prompt field at all.
    let prompt = call_params.prompt.or_else(|| match &session.compaction {
        Some(CompactionConfig::Client { prompt }) => prompt.clone(),
        _ => None,
    });

    match session::run_compaction(
        &state.store,
        &state.http,
        &state.broadcaster,
        &session,
        &profile,
        &state.provider_registry,
        prompt.as_deref(),
        session::CompactionTrigger::Client { token_count },
        acting_client_key_id,
        &state.telemetry_metrics,
        &mut Vec::new(),
    )
    .await
    {
        Ok(completed) => {
            // The history was just reset, so any auto-compaction backoff from
            // an earlier failed attempt no longer applies.
            state
                .compaction_backoff
                .lock()
                .expect("compaction backoff mutex poisoned")
                .remove(&session.id);
            Ok(event_view(&completed))
        }
        // A logical failure (provider config, providers exhausted, a truncated
        // or empty summary): the session stays open and compactable, and its
        // previous effective history is untouched.
        Err(session::TurnError::CompactionFailed(detail)) => {
            Err((-32000, format!("compaction failed: {detail}")))
        }
        Err(e) => {
            tracing::error!("session compaction failed: {e}");
            Err((-32603, "Internal error".to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// Sandbox methods (session.startRemoteSandbox / stopRemoteSandbox /
// execRemoteSandbox / reportLocalSandbox)
// ---------------------------------------------------------------------------

/// The shared driver-registration gate: every sandbox method requires the
/// caller to have registered via `session.registerDriver` first — the exact
/// `-32001` gate `session.sendMessage` uses. Returns the error pair to emit,
/// or `None` when the caller may proceed.
fn require_registered_driver(
    state: &AppState,
    session: &SessionRecord,
    acting_client_key_id: &str,
    method: &str,
) -> Option<(i64, String)> {
    if state.is_registered_driver(&session.id, acting_client_key_id) {
        None
    } else {
        Some((
            -32001,
            format!("call session.registerDriver before {method}"),
        ))
    }
}

/// Log one sandbox lifecycle event through the broadcast choke point,
/// swallowing (but tracing) a persistence failure — lifecycle visibility must
/// never turn a working sandbox operation into an RPC error after the fact.
fn log_sandbox_event(
    state: &AppState,
    session_id: &str,
    client_key_id: &str,
    event_type: EventType,
    payload: &Value,
) -> Option<crate::store::sessions::EventRecord> {
    match broadcast::insert_and_publish(
        &state.store,
        &state.broadcaster,
        session_id,
        Some(client_key_id),
        event_type,
        payload,
    ) {
        Ok(record) => Some(record),
        Err(e) => {
            tracing::error!("failed to log {event_type}: {e}");
            None
        }
    }
}

/// `session.startRemoteSandbox` (`params: {"image": string}`).
///
/// Validates `image` against **this session's own profile's**
/// `available_sandboxes` only — the profile bound to the session is the only
/// trust boundary for what the session may launch; an image declared on some
/// *other* profile is rejected identically to one unknown anywhere
/// (`-32011 sandbox_image_not_allowed`). On success the handle is retained
/// session-wide (one remote sandbox per session, shared by every driver, like
/// MCP servers) and `session.sandbox.start` → `session.sandbox.running`
/// bracket the driver call; a driver failure logs `session.sandbox.error`
/// (`phase: "start"`) and returns `-32012 sandbox_start_failed`.
async fn start_remote_sandbox_rpc(
    state: &AppState,
    session: &SessionRecord,
    acting_client_key_id: &str,
    id_present: bool,
    req_id: Value,
    params: &Value,
) -> Response {
    if let Some((code, msg)) = require_registered_driver(
        state,
        session,
        acting_client_key_id,
        "session.startRemoteSandbox",
    ) {
        return single_or_empty(id_present, error_obj(req_id, code, msg));
    }
    if session.state != STATE_OPEN {
        let state_str = session.state.clone();
        return single_or_empty(
            id_present,
            error_obj(req_id, -32000, format!("session is {state_str}, not open")),
        );
    }
    let image = match params.get("image").and_then(Value::as_str) {
        Some(i) if !i.trim().is_empty() => i.to_owned(),
        _ => {
            return single_or_empty(
                id_present,
                error_obj(req_id, -32602, "Invalid params: missing \"image\""),
            )
        }
    };

    // Load this session's own profile — the same lookup sendMessage performs —
    // and enforce its allowlist. Never any other profile's list.
    let profile =
        match state
            .store
            .with_conn(|c| profiles::get(c, &session.profile_id))
        {
            Ok(Some(p)) => p,
            Ok(None) => return single_or_empty(
                id_present,
                error_obj(
                    req_id,
                    -32000,
                    "profile_unavailable: the profile bound to this session is no longer available",
                ),
            ),
            Err(e) => {
                tracing::error!("database error in /rpc: {e}");
                return single_or_empty(id_present, error_obj(req_id, -32603, "Internal error"));
            }
        };
    if !profile.sandbox_image_names().iter().any(|n| n == &image) {
        return single_or_empty(
            id_present,
            error_obj(
                req_id,
                -32011,
                format!(
                    "sandbox_image_not_allowed: image {image:?} is not in this session \
                     profile's available_sandboxes"
                ),
            ),
        );
    }
    // One remote sandbox per session: a second start is rejected before any
    // lifecycle event fires (stop the current one first).
    if state.sandbox(&session.id).is_some() {
        return single_or_empty(
            id_present,
            error_obj(
                req_id,
                -32000,
                "a remote sandbox is already running for this session; \
                 call session.stopRemoteSandbox first",
            ),
        );
    }

    // Request accepted: log the intent, then call the driver. ensure_image is
    // defensive — the profile-write provisioning may not have finished, or the
    // server may have restarted since.
    log_sandbox_event(
        state,
        &session.id,
        acting_client_key_id,
        EventType::SandboxStart,
        &json!({ "image": image, "dispatch": "remote" }),
    );
    let started = async {
        state.sandbox_driver.ensure_image(&image).await?;
        state.sandbox_driver.start(&image).await
    }
    .await;
    let handle = match started {
        Ok(h) => h,
        Err(e) => {
            let detail = e.to_string();
            log_sandbox_event(
                state,
                &session.id,
                acting_client_key_id,
                EventType::SandboxError,
                &json!({
                    "image": image,
                    "phase": "start",
                    "detail": detail,
                    "dispatch": "remote",
                    "unsandboxed": false,
                }),
            );
            return single_or_empty(
                id_present,
                error_obj(req_id, -32012, format!("sandbox_start_failed: {detail}")),
            );
        }
    };

    let sandbox_id = handle.id.clone();
    state.insert_sandbox(&session.id, handle);
    let running = log_sandbox_event(
        state,
        &session.id,
        acting_client_key_id,
        EventType::SandboxRunning,
        &json!({
            "image": image,
            "sandbox_id": sandbox_id,
            "dispatch": "remote",
            "unsandboxed": false,
        }),
    );
    let started_at = running
        .map(|r| Value::String(r.created_at))
        .unwrap_or(Value::Null);
    single_or_empty(
        id_present,
        result_obj(
            req_id,
            json!({ "sandbox_id": sandbox_id, "image": image, "started_at": started_at }),
        ),
    )
}

/// `session.execRemoteSandbox` (`params: {"command": string}`).
///
/// The **manual** remote-dispatch path: a synchronous utility call (no
/// session-loop involvement) running one fully-interpolated command in the
/// session's live remote sandbox and returning `{stdout, stderr, exit_code}`
/// raw — the harness constructs its own tool_result from it. A driver failure
/// also logs `session.sandbox.error` (`phase: "exec"`) so a broken remote
/// sandbox is visible in the event log even when dispatched manually.
async fn exec_remote_sandbox_rpc(
    state: &AppState,
    session: &SessionRecord,
    acting_client_key_id: &str,
    id_present: bool,
    req_id: Value,
    params: &Value,
) -> Response {
    if let Some((code, msg)) = require_registered_driver(
        state,
        session,
        acting_client_key_id,
        "session.execRemoteSandbox",
    ) {
        return single_or_empty(id_present, error_obj(req_id, code, msg));
    }
    let command = match params.get("command").and_then(Value::as_str) {
        Some(c) => c.to_owned(),
        None => {
            return single_or_empty(
                id_present,
                error_obj(req_id, -32602, "Invalid params: missing \"command\""),
            )
        }
    };
    let Some(entry) = state.sandbox(&session.id) else {
        return single_or_empty(
            id_present,
            error_obj(
                req_id,
                -32013,
                "sandbox_not_running: no remote sandbox is running for this session",
            ),
        );
    };
    // The handle lock is held across the driver await, exactly like an MCP
    // tools/call dispatch holds its session lock.
    let handle = entry.lock().await;
    match state.sandbox_driver.exec(&handle, &command).await {
        Ok(r) => single_or_empty(
            id_present,
            result_obj(
                req_id,
                json!({ "stdout": r.stdout, "stderr": r.stderr, "exit_code": r.exit_code }),
            ),
        ),
        Err(e) => {
            let detail = e.to_string();
            log_sandbox_event(
                state,
                &session.id,
                acting_client_key_id,
                EventType::SandboxError,
                &json!({
                    "image": handle.image,
                    "sandbox_id": handle.id,
                    "phase": "exec",
                    "detail": detail,
                    "dispatch": "remote",
                    "unsandboxed": false,
                }),
            );
            single_or_empty(
                id_present,
                error_obj(req_id, -32000, format!("sandbox exec failed: {detail}")),
            )
        }
    }
}

/// `session.reportLocalSandbox` (`params: {"state", "image", "unsandboxed"?,
/// "container_id"?, "detail"?}`).
///
/// Client-originated **local**-sandbox lifecycle telemetry: maps `state` to
/// the matching lifecycle event with `"dispatch": "local"`, attributed to the
/// caller's client key, through the same broadcast choke point as everything
/// else. Deliberately performs **no** `available_sandboxes` validation — a
/// local sandbox is the harness developer's own trust decision, and this path
/// is visibility, not a security gate. Scoped to local only (no `scope`
/// parameter exists), so a client can never forge a *remote* lifecycle event
/// here; any registered driver may report, current turn owner or not.
fn report_local_sandbox_rpc(
    state: &AppState,
    session: &SessionRecord,
    acting_client_key_id: &str,
    id_present: bool,
    req_id: Value,
    params: &Value,
) -> Response {
    if let Some((code, msg)) = require_registered_driver(
        state,
        session,
        acting_client_key_id,
        "session.reportLocalSandbox",
    ) {
        return single_or_empty(id_present, error_obj(req_id, code, msg));
    }
    let event_type = match params.get("state").and_then(Value::as_str) {
        Some("running") => EventType::SandboxRunning,
        Some("stopped") => EventType::SandboxStopped,
        Some("error") => EventType::SandboxError,
        _ => {
            return single_or_empty(
                id_present,
                error_obj(
                    req_id,
                    -32602,
                    "Invalid params: \"state\" must be \"running\", \"stopped\", or \"error\"",
                ),
            )
        }
    };
    let unsandboxed = match params.get("unsandboxed") {
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => {
                return single_or_empty(
                    id_present,
                    error_obj(
                        req_id,
                        -32602,
                        "Invalid params: \"unsandboxed\" must be a boolean",
                    ),
                )
            }
        },
        None => false,
    };
    let image = match params.get("image") {
        Some(Value::String(_)) if unsandboxed => {
            return single_or_empty(
                id_present,
                error_obj(
                    req_id,
                    -32602,
                    "Invalid params: \"image\" must be null when \"unsandboxed\" is true",
                ),
            )
        }
        Some(Value::String(image)) => Value::String(image.clone()),
        Some(Value::Null) | None if unsandboxed => Value::Null,
        Some(Value::Null) | None => {
            return single_or_empty(
                id_present,
                error_obj(req_id, -32602, "Invalid params: missing \"image\""),
            )
        }
        Some(_) => {
            return single_or_empty(
                id_present,
                error_obj(
                    req_id,
                    -32602,
                    "Invalid params: \"image\" must be a string or null",
                ),
            )
        }
    };
    let payload = json!({
        "dispatch": "local",
        "image": image,
        "unsandboxed": unsandboxed,
        "container_id": params.get("container_id").cloned().unwrap_or(Value::Null),
        "detail": params.get("detail").cloned().unwrap_or(Value::Null),
    });
    match broadcast::insert_and_publish(
        &state.store,
        &state.broadcaster,
        &session.id,
        Some(acting_client_key_id),
        event_type,
        &payload,
    ) {
        Ok(_) => single_or_empty(id_present, result_obj(req_id, json!({ "reported": true }))),
        Err(e) => {
            tracing::error!("database error in /rpc: {e}");
            single_or_empty(id_present, error_obj(req_id, -32603, "Internal error"))
        }
    }
}

/// Local-subagent lifecycle telemetry. Like local sandbox telemetry this is
/// visibility only: a registered harness may report it, but the server never
/// treats it as an authoritative process handle.
fn report_local_subagent_rpc(
    state: &AppState,
    session: &SessionRecord,
    acting_client_key_id: &str,
    id_present: bool,
    req_id: Value,
    params: &Value,
) -> Response {
    if let Some((code, msg)) = require_registered_driver(
        state,
        session,
        acting_client_key_id,
        "session.reportLocalSubagent",
    ) {
        return single_or_empty(id_present, error_obj(req_id, code, msg));
    }
    let event_type = match params.get("state").and_then(Value::as_str) {
        Some("start") => EventType::SubagentStart,
        Some("running") => EventType::SubagentRunning,
        Some("completed") => EventType::SubagentCompleted,
        Some("failed") => EventType::SubagentFailed,
        Some("cancelled") => EventType::SubagentCancelled,
        _ => return single_or_empty(id_present, error_obj(req_id, -32602, "Invalid params: \"state\" must be \"start\", \"running\", \"completed\", \"failed\", or \"cancelled\"")),
    };
    let required = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
    };
    let (Some(subagent_id), Some(harness), Some(model)) = (
        required("subagent_id"),
        required("harness"),
        required("model"),
    ) else {
        return single_or_empty(id_present, error_obj(req_id, -32602, "Invalid params: \"subagent_id\", \"harness\", and \"model\" must be non-empty strings"));
    };
    for name in ["detail", "reason"] {
        if !matches!(
            params.get(name),
            None | Some(Value::Null) | Some(Value::String(_))
        ) {
            return single_or_empty(
                id_present,
                error_obj(
                    req_id,
                    -32602,
                    format!("Invalid params: \"{name}\" must be a string or null"),
                ),
            );
        }
    }
    if !params.get("exit_code").is_none_or(|v| v.as_i64().is_some()) {
        return single_or_empty(
            id_present,
            error_obj(
                req_id,
                -32602,
                "Invalid params: \"exit_code\" must be an integer",
            ),
        );
    }
    let mut payload = json!({
        "dispatch": "local", "subagent_id": subagent_id, "harness": harness,
        "model": model, "detail": params.get("detail").cloned().unwrap_or(Value::Null),
    });
    match event_type {
        EventType::SubagentCompleted => {
            payload["exit_code"] = params.get("exit_code").cloned().unwrap_or(Value::Null)
        }
        EventType::SubagentFailed => {
            payload["reason"] = params.get("reason").cloned().unwrap_or(Value::Null);
            payload["exit_code"] = params.get("exit_code").cloned().unwrap_or(Value::Null);
        }
        EventType::SubagentCancelled => {
            payload["reason"] = params.get("reason").cloned().unwrap_or(Value::Null)
        }
        _ => {}
    }
    match broadcast::insert_and_publish(
        &state.store,
        &state.broadcaster,
        &session.id,
        Some(acting_client_key_id),
        event_type,
        &payload,
    ) {
        Ok(_) => single_or_empty(id_present, result_obj(req_id, json!({ "reported": true }))),
        Err(e) => {
            tracing::error!("database error in /rpc: {e}");
            single_or_empty(id_present, error_obj(req_id, -32603, "Internal error"))
        }
    }
}

fn cancel_subagent_rpc(
    state: &AppState,
    session: &SessionRecord,
    acting_client_key_id: &str,
    id_present: bool,
    req_id: Value,
    params: &Value,
) -> Response {
    if let Some((code, msg)) = require_registered_driver(
        state,
        session,
        acting_client_key_id,
        "session.cancelSubagent",
    ) {
        return single_or_empty(id_present, error_obj(req_id, code, msg));
    }
    let Some(subagent_id) = params
        .get("subagent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    else {
        return single_or_empty(
            id_present,
            error_obj(req_id, -32602, "Invalid params: missing \"subagent_id\""),
        );
    };
    let cancelled = {
        let mut all = state.subagents.lock().expect("subagents mutex poisoned");
        let Some(task) = all
            .get_mut(&session.id)
            .and_then(|m| m.get_mut(subagent_id))
        else {
            return single_or_empty(
                id_present,
                error_obj(
                    req_id,
                    -32014,
                    format!(
                        "subagent_not_found: no tracked subagent {subagent_id:?} for this session"
                    ),
                ),
            );
        };
        if task.status != crate::engine::subagent::SubagentStatus::Running {
            None
        } else {
            if let Some(join) = task.task.take() {
                join.abort();
            }
            task.status = crate::engine::subagent::SubagentStatus::Cancelled;
            task.reason = Some("explicit".to_owned());
            Some((task.harness.clone(), task.model.clone()))
        }
    };
    let was_running = cancelled.is_some();
    if let Some((harness, model)) = cancelled {
        if let Err(e) = broadcast::insert_and_publish(
            &state.store,
            &state.broadcaster,
            &session.id,
            Some(acting_client_key_id),
            EventType::SubagentCancelled,
            &json!({ "dispatch": "remote", "subagent_id": subagent_id, "harness": harness, "model": model, "detail": Value::Null, "reason": "explicit" }),
        ) {
            tracing::error!("failed to log subagent cancellation: {e}");
        }
    }
    single_or_empty(
        id_present,
        result_obj(
            req_id,
            json!({ "cancelled": true, "subagent_id": subagent_id, "was_running": was_running }),
        ),
    )
}

fn update_client_tools_rpc(
    state: &AppState,
    session: &SessionRecord,
    acting_client_key_id: &str,
    id_present: bool,
    req_id: Value,
    params: &Value,
) -> Response {
    if let Some((code, msg)) = require_registered_driver(
        state,
        session,
        acting_client_key_id,
        "session.updateClientTools",
    ) {
        return single_or_empty(id_present, error_obj(req_id, code, msg));
    }
    if session.state != STATE_OPEN {
        return single_or_empty(
            id_present,
            error_obj(
                req_id,
                -32000,
                format!("session is {}, not open", session.state),
            ),
        );
    }
    let Some(raw_tools) = params.get("tools").cloned() else {
        return single_or_empty(
            id_present,
            error_obj(req_id, -32602, "Invalid params: missing \"tools\""),
        );
    };
    let tools: Vec<ClientToolDef> =
        match serde_json::from_value::<Vec<ClientToolDef>>(raw_tools.clone()) {
            Ok(tools) if tools.iter().all(|t| !t.name.trim().is_empty()) => tools,
            Err(e) => {
                return single_or_empty(
                    id_present,
                    error_obj(req_id, -32602, format!("Invalid params: {e}")),
                )
            }
            _ => {
                return single_or_empty(
                    id_present,
                    error_obj(
                        req_id,
                        -32602,
                        "Invalid params: every tool requires a non-empty \"name\"",
                    ),
                )
            }
        };
    let profile = match state
        .store
        .with_conn(|c| profiles::get(c, &session.profile_id))
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            return single_or_empty(id_present, error_obj(req_id, -32603, "Internal error"))
        }
        Err(e) => {
            tracing::error!("database error in /rpc: {e}");
            return single_or_empty(id_present, error_obj(req_id, -32603, "Internal error"));
        }
    };
    if tools
        .iter()
        .any(|t| t.name == crate::engine::subagent::REMOTE_STATUS_TOOL_NAME)
    {
        return single_or_empty(id_present, error_obj(req_id, -32015, "tool_not_allowed: tool \"remote_subagent_status\" is not in the profile's allowlist"));
    }
    if let Err(_e) = enforce_tool_allowlist(&profile, &tools) {
        let name = tools
            .iter()
            .find(|t| {
                !profile
                    .allowed_tools
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(&t.name)))
            })
            .map(|t| t.name.as_str())
            .unwrap_or("");
        return single_or_empty(
            id_present,
            error_obj(
                req_id,
                -32015,
                format!("tool_not_allowed: tool {name:?} is not in the profile's allowlist"),
            ),
        );
    }
    if let Err(e) = state
        .store
        .with_conn(|c| sessions::set_client_tools(c, &session.id, acting_client_key_id, &raw_tools))
    {
        tracing::error!("database error in /rpc: {e}");
        return single_or_empty(id_present, error_obj(req_id, -32603, "Internal error"));
    }
    single_or_empty(id_present, result_obj(req_id, json!({ "updated": true })))
}

// ---------------------------------------------------------------------------
// session.subscribe
// ---------------------------------------------------------------------------

/// Drive one `session.subscribe`: replay persisted events after
/// `since_event_id` (if given), then forward live events indefinitely until the
/// client disconnects, `session.unsubscribe` fires, or the channel closes.
async fn drive_subscribe(
    state: AppState,
    session: SessionRecord,
    params: Value,
    tx: mpsc::Sender<Bytes>,
) {
    // Subscribe first, then replay: any event committed during the replay query
    // is also buffered on `rx`, and the id-dedup below drops the overlap so the
    // replay→live seam neither gaps nor double-delivers.
    let (mut rx, cancel) = state.broadcaster.subscribe(&session.id);

    let since = params
        .get("since_event_id")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut replayed: HashSet<String> = HashSet::new();
    if let Some(since_id) = since {
        // An unknown/foreign `since_event_id` falls back to replaying from the
        // start rather than erroring.
        let mut cursor = state
            .store
            .with_conn(|c| rowid_of_event(c, &session.id, &since_id))
            .ok()
            .flatten()
            .unwrap_or(0);
        loop {
            let page = state
                .store
                .with_conn(|c| sessions::list_events(c, &session.id, cursor, REPLAY_CHUNK));
            let (rows, has_more) = match page {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("subscribe replay failed: {e}");
                    let _ = emit(&tx, &error_obj(Value::Null, -32000, "replay failed")).await;
                    return;
                }
            };
            for (rowid, record) in &rows {
                replayed.insert(record.id.clone());
                if !emit(&tx, &notification(record)).await {
                    return;
                }
                cursor = *rowid;
            }
            if !has_more {
                break;
            }
        }
    }

    // Live phase. `dedup` stays on only until the first event newer than the
    // replay (events arrive in insertion order, so once we pass the replayed
    // prefix no further duplicates are possible and we can drop the set).
    let mut dedup = true;
    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => return, // session.unsubscribe / channel removed
            recv = rx.recv() => match recv {
                Ok(record) => {
                    if dedup {
                        if replayed.remove(&record.id) {
                            continue;
                        }
                        dedup = false;
                        replayed.clear();
                    }
                    if !emit(&tx, &notification(&record)).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let _ = emit(&tx, &lagged_error()).await;
                    return;
                }
                Err(RecvError::Closed) => return, // session closed
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC envelope + NDJSON framing helpers
// ---------------------------------------------------------------------------

/// A `session.event` notification wrapping one event record.
fn notification(record: &crate::store::sessions::EventRecord) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session.event",
        "params": event_view(record),
    })
}

/// A terminal JSON-RPC success response.
fn result_obj(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A terminal JSON-RPC error object.
fn error_obj(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// The distinguishable "you fell behind" error: shared by `sendMessage` and
/// `subscribe`, it closes the stream so the client reconnects with
/// `since_event_id` and reconciles via `GET /api/v1/sessions/{id}/events`.
fn lagged_error() -> Value {
    json!({
        "jsonrpc": "2.0",
        "error": { "code": -32000, "message": "lagged; reconnect with since_event_id" }
    })
}

/// Serialize a JSON-RPC object as one NDJSON line (trailing `\n`).
fn line_bytes(v: &Value) -> Bytes {
    let mut buf = serde_json::to_vec(v).unwrap_or_default();
    buf.push(b'\n');
    Bytes::from(buf)
}

/// Send one NDJSON line to the response stream. Returns `false` if the client
/// has disconnected (the receiver was dropped), signalling the driver to stop.
async fn emit(tx: &mpsc::Sender<Bytes>, v: &Value) -> bool {
    tx.send(line_bytes(v)).await.is_ok()
}

/// Emit a terminal response frame built from the echoed request id — unless the
/// request was a JSON-RPC notification (no `id`), in which case no response is
/// sent at all. `build` is only invoked when a response is due.
async fn emit_terminal(
    tx: &mpsc::Sender<Bytes>,
    resp_id: &Option<Value>,
    build: impl FnOnce(Value) -> Value,
) {
    if let Some(id) = resp_id {
        let _ = emit(tx, &build(id.clone())).await;
    }
}

/// Emit a terminal internal-error response for a persistence failure (suppressed
/// for a notification with no `id`).
async fn emit_internal(tx: &mpsc::Sender<Bytes>, resp_id: &Option<Value>, e: rusqlite::Error) {
    tracing::error!("database error in /rpc: {e}");
    emit_terminal(tx, resp_id, |id| error_obj(id, -32603, "Internal error")).await;
}

/// A single-object NDJSON response (envelope errors, `session.unsubscribe`).
fn single(obj: Value) -> Response {
    ndjson_response(Body::from(line_bytes(&obj)))
}

/// A single-object response, or — when the request carried no `id` (a JSON-RPC
/// notification) — an empty stream, since a notification is never answered.
fn single_or_empty(id_present: bool, obj: Value) -> Response {
    if id_present {
        single(obj)
    } else {
        ndjson_response(Body::empty())
    }
}

/// Spawn `f` as the driver of a streamed NDJSON response, wiring it to a fresh
/// channel whose receiver backs the HTTP body.
fn spawn_stream<F, Fut>(f: F) -> Response
where
    F: FnOnce(mpsc::Sender<Bytes>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Bytes>(STREAM_BUFFER);
    tokio::spawn(f(tx));
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<Bytes, Infallible>(b), rx))
    });
    ndjson_response(Body::from_stream(stream))
}

/// Wrap a body as an `application/x-ndjson` response.
fn ndjson_response(body: Body) -> Response {
    Response::builder()
        .header(CONTENT_TYPE, "application/x-ndjson")
        .body(body)
        .expect("static header is always valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_rejects_missing_server_result_id() {
        let assistant = vec![
            json!({ "type": "tool_use", "id": "server_1" }),
            json!({ "type": "tool_use", "id": "client_1" }),
        ];
        let client = json!([{
            "type": "tool_result",
            "tool_use_id": "client_1",
            "content": "ok"
        }]);
        let err = merge_tool_results(&assistant, &[], &client).unwrap_err();
        assert!(
            err.contains("server_1"),
            "missing id should be named: {err}"
        );
    }

    #[test]
    fn merge_rejects_duplicate_client_result_id() {
        let assistant = vec![json!({ "type": "tool_use", "id": "client_1" })];
        let result = json!({
            "type": "tool_result",
            "tool_use_id": "client_1",
            "content": "ok"
        });
        let err =
            merge_tool_results(&assistant, &[], &json!([result.clone(), result])).unwrap_err();
        assert!(
            err.contains("duplicate"),
            "duplicate should be named: {err}"
        );
    }
}
