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

use std::collections::HashSet;
use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use super::sessions::{auth_session, event_view, tool_result_blocks, MessageBody};
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
            }
            single_or_empty(
                id_present,
                result_obj(req_id, json!({ "registered": true })),
            )
        }
        "session.sendMessage" => spawn_stream(move |tx| {
            drive_send_message(state, session, acting_client_key_id, resp_id, params, tx)
        }),
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
async fn drive_send_message(
    state: AppState,
    session: SessionRecord,
    acting_client_key_id: String,
    resp_id: Option<Value>,
    params: Value,
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
    // voluntarily by sending anything. (2) A paused turn whose owner stayed
    // away past the deadline is abandoned: drop the parked guard (releasing
    // the gate to the next FIFO waiter) and log the abandonment. (3) Otherwise
    // queue on the gate; `tokio::sync::Mutex` grants `lock_owned()` in request
    // order, which is the FIFO queue.
    let mut abandoned_owner: Option<String> = None;
    let reclaimed = {
        let mut pending = state
            .pending_turns
            .lock()
            .expect("pending_turns mutex poisoned");
        let (caller_owns, expired) = match pending.get(&session.id) {
            Some(pt) => (
                pt.owner_client_key_id == acting_client_key_id,
                tokio::time::Instant::now() > pt.deadline,
            ),
            None => (false, false),
        };
        if caller_owns {
            pending.remove(&session.id).map(|pt| pt.guard)
        } else {
            if expired {
                if let Some(pt) = pending.remove(&session.id) {
                    abandoned_owner = Some(pt.owner_client_key_id);
                    // Dropping pt (and its parked guard) releases the gate.
                }
            }
            None
        }
    };
    if let Some(owner) = abandoned_owner {
        // Logged through the broadcast choke point so live watchers see the
        // abandonment; the session stays open (unlike a provider failure).
        if let Err(e) = broadcast::insert_and_publish(
            &state.store,
            &state.broadcaster,
            &session.id,
            Some(&owner),
            EventType::SessionError,
            &json!({ "reason": "driver_turn_abandoned", "owner_client_key_id": owner }),
        ) {
            tracing::error!("failed to log driver_turn_abandoned: {e}");
        }
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
    let mut all_events = Vec::new();
    let msg_payload = json!({ "role": message.role, "content": message.content });
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
    for block in tool_result_blocks(&message.content) {
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
    let turn_fut = session::run_turn(
        &state.store,
        &state.http,
        &state.broadcaster,
        &session,
        &profile,
        &state.provider_registry,
        mcp,
        &acting_client_key_id,
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
            // A Paused turn (client-side tool call in flight) keeps holding the
            // FIFO gate across requests: park the guard so only this caller's
            // continuation resumes without queuing. Completed/ProvidersFailed
            // turns let the guard drop at the end of this call, releasing the
            // next FIFO waiter.
            if turn.outcome == session::Outcome::Paused {
                if let Some(guard) = gate_guard.take() {
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
            emit_terminal(&tx, &resp_id, |id| {
                error_obj(id, -32000, "session loop failed")
            })
            .await;
        }
    }
    drop(gate_guard);
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
