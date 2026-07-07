//! The JSON-RPC 2.0 session loop (`POST /api/v1/sessions/{id}/rpc`).
//!
//! This is the **one** JSON-RPC endpoint on the client port; every other
//! client-port route stays plain REST/JSON with RFC 7807 errors. It carries
//! exactly the live, session-bound message/event loop:
//!
//! - `session.sendMessage` (`{message}`) — replaces the old
//!   `POST /api/v1/sessions/{id}/messages`. Streams a `session.event`
//!   notification for every event the turn produces, in order as it happens,
//!   then one terminal response carrying the same `{message, events}` body the
//!   old route returned.
//! - `session.subscribe` (`{since_event_id?}`) — a non-driving observer feed:
//!   replay persisted events after `since_event_id`, then live notifications
//!   indefinitely. No terminal response while active.
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
use crate::store::sessions::{
    self, rowid_of_event, SessionRecord, STATE_ERROR, STATE_OPEN,
};
use crate::store::profiles;

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
    let (session, _key) = match auth_session(&state, &headers, &id) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

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
            error_obj(req_id, -32600, "Invalid Request: missing or wrong \"jsonrpc\" version"),
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
    let resp_id = if id_present { Some(req_id.clone()) } else { None };

    match method.as_str() {
        "session.sendMessage" => spawn_stream(move |tx| {
            drive_send_message(state, session, resp_id, params, tx)
        }),
        "session.subscribe" => {
            spawn_stream(move |tx| drive_subscribe(state, session, params, tx))
        }
        "session.unsubscribe" => {
            // Ends active session.subscribe streams for this session. The
            // cancellation side effect happens even for a notification; only the
            // terminal response is suppressed when there is no `id`.
            state.broadcaster.cancel_subscriptions(&session.id);
            single_or_empty(id_present, result_obj(req_id, json!({ "unsubscribed": true })))
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

/// Drive one `session.sendMessage`: subscribe to the session feed, record the
/// client turn, run the loop while forwarding its events live, then write the
/// terminal `{message, events}` response.
async fn drive_send_message(
    state: AppState,
    session: SessionRecord,
    resp_id: Option<Value>,
    params: Value,
    tx: mpsc::Sender<Bytes>,
) {
    // Subscribe *before* running the turn so no event the turn produces slips
    // past us. The client's own `client.message.send` / client-dispatch
    // `tool.result` are filtered out of the broadcast, so they are never echoed.
    let (mut rx, _cancel) = state.broadcaster.subscribe(&session.id);

    if session.state != STATE_OPEN {
        let state_str = session.state.clone();
        emit_terminal(&tx, &resp_id, |id| {
            error_obj(id, -32000, format!("session is {state_str}, not open"))
        })
        .await;
        return;
    }

    // `params.message` is required and must carry `content`.
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

    // Record the incoming client turn (and any returned tool_result blocks).
    // These go through the publish choke point but are filtered from broadcast.
    let mut all_events = Vec::new();
    let msg_payload = json!({ "role": message.role, "content": message.content });
    match broadcast::insert_and_publish(
        &state.store,
        &state.broadcaster,
        &session.id,
        Some(&session.client_key_id),
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
            Some(&session.client_key_id),
            EventType::ToolResult,
            &block,
        ) {
            Ok(ev) => all_events.push(ev),
            Err(e) => return emit_internal(&tx, &resp_id, e).await,
        }
    }

    // The profile could have been deleted mid-session.
    let profile = match state.store.with_conn(|c| profiles::get(c, &session.profile_id)) {
        Ok(Some(p)) => p,
        Ok(None) => {
            let _ = broadcast::insert_and_publish(
                &state.store,
                &state.broadcaster,
                &session.id,
                Some(&session.client_key_id),
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
    let mcp = state.mcp_session(&session.id);
    let turn_fut = session::run_turn(
        &state.store,
        &state.http,
        &state.broadcaster,
        &session,
        &profile,
        mcp,
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
        if !emit(&tx, &notification(&record)).await {
            return;
        }
    }

    match turn_result {
        Ok(turn) => {
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
            emit_terminal(&tx, &resp_id, |id| error_obj(id, -32000, "session loop failed")).await;
        }
    }
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
