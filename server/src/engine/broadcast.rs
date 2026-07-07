//! Session-scoped event broadcasting.
//!
//! Every event that is persisted to `session_events` is *also* published, live,
//! to any connection currently watching that session — the driving
//! `session.sendMessage` call and any `session.subscribe` observers. This module
//! owns three things:
//!
//! - [`EventBroadcaster`] — the in-process registry of per-session
//!   [`tokio::sync::broadcast`] channels (created lazily on first subscribe) plus
//!   a per-session cancellation [`Notify`] used by `session.unsubscribe`.
//! - [`insert_and_publish`] — the **single choke point** for persisting an event:
//!   it inserts the row and, on success, publishes the record on the session's
//!   channel. Every place that used to call [`sessions::insert_event`] directly
//!   goes through here (or, for events inserted inside a larger transaction,
//!   through [`EventBroadcaster::publish`] once the transaction commits).
//! - [`should_broadcast`] — the shared filter predicate deciding which event
//!   types are forwarded to live watchers, so the rule lives in exactly one
//!   place.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{broadcast, Notify};

use crate::events::EventType;
use crate::store::sessions::{self, EventRecord};
use crate::store::Store;

/// Per-session broadcast channel capacity. A slow watcher that falls this far
/// behind gets a [`broadcast::error::RecvError::Lagged`], which the streaming
/// layer surfaces as a distinguishable JSON-RPC error so the client reconnects
/// with `since_event_id` rather than silently missing events.
const CHANNEL_CAPACITY: usize = 256;

/// One session's live-delivery state: the fan-out sender and a cancellation
/// handle that `session.unsubscribe` fires to end active subscriptions.
struct SessionChannel {
    sender: broadcast::Sender<EventRecord>,
    cancel: Arc<Notify>,
}

/// In-process registry of per-session broadcast channels.
///
/// Cheap to clone (an `Arc` internally) and shared on [`crate::api::AppState`].
/// A channel is created lazily the first time something subscribes to a session
/// and dropped when the session is closed, so idle entries never accumulate.
#[derive(Clone, Default)]
pub struct EventBroadcaster {
    channels: Arc<Mutex<HashMap<String, SessionChannel>>>,
}

impl EventBroadcaster {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionChannel>> {
        self.channels.lock().expect("event broadcaster mutex poisoned")
    }

    /// Subscribe to a session's live feed, creating the channel lazily. Returns
    /// the broadcast receiver and the session's cancellation handle (fired by
    /// `session.unsubscribe`). Subscribe **before** doing any catch-up replay so
    /// no event slips through the seam between replay and live delivery.
    pub fn subscribe(&self, session_id: &str) -> (broadcast::Receiver<EventRecord>, Arc<Notify>) {
        let mut map = self.lock();
        let entry = map.entry(session_id.to_owned()).or_insert_with(|| SessionChannel {
            sender: broadcast::channel(CHANNEL_CAPACITY).0,
            cancel: Arc::new(Notify::new()),
        });
        (entry.sender.subscribe(), entry.cancel.clone())
    }

    /// Publish an event to its session's channel, if one exists and the event is
    /// forwardable per [`should_broadcast`]. A `send` with no live receivers is a
    /// harmless no-op (its `Err` is ignored), and a session nobody is watching
    /// has no channel at all, so this is cheap on the hot path.
    pub fn publish(&self, event: &EventRecord) {
        if !should_broadcast(event) {
            return;
        }
        let map = self.lock();
        if let Some(entry) = map.get(&event.session_id) {
            let _ = entry.sender.send(event.clone());
        }
    }

    /// End every active `session.subscribe` stream for this session cleanly
    /// (fired by `session.unsubscribe`). Waiters currently parked on the cancel
    /// handle wake and close their streams; if no channel exists this is a no-op.
    pub fn cancel_subscriptions(&self, session_id: &str) {
        if let Some(entry) = self.lock().get(session_id) {
            entry.cancel.notify_waiters();
        }
    }

    /// Drop a session's channel at close so idle entries don't accumulate.
    /// Dropping the sender makes every live receiver observe
    /// [`broadcast::error::RecvError::Closed`], which ends their streams.
    pub fn remove(&self, session_id: &str) {
        if let Some(entry) = self.lock().remove(session_id) {
            // Wake any parked subscribers so they notice the channel is gone
            // promptly rather than only on the next event.
            entry.cancel.notify_waiters();
        }
    }
}

/// Persist an event **and** publish it live — the single choke point every
/// event insert funnels through. Insert first; only publish a record that was
/// actually committed.
pub fn insert_and_publish(
    store: &Store,
    broadcaster: &EventBroadcaster,
    session_id: &str,
    client_key_id: Option<&str>,
    event_type: EventType,
    payload: &Value,
) -> rusqlite::Result<EventRecord> {
    let record = store.with_conn(|c| {
        sessions::insert_event(c, session_id, client_key_id, event_type, payload)
    })?;
    broadcaster.publish(&record);
    Ok(record)
}

/// The shared filter predicate: which persisted events are forwarded to live
/// watchers. Everything is forwarded **except** the events a client generates in
/// the very request that produces them, which would just be echoed back:
///
/// - `client.message.send` — the driving client's own turn input;
/// - `tool.result` with `dispatch == "client"` — the client returning output for
///   a tool it dispatched itself.
///
/// Everything else — `provider.request`/`provider.response`, `tool.call`,
/// MCP-dispatched `tool.result`, `mcp.request`/`mcp.response`,
/// `server.message.send`, and every `session.*` event — is forwarded. This rule
/// lives here and only here; both `session.sendMessage` and `session.subscribe`
/// share it via [`EventBroadcaster::publish`].
pub fn should_broadcast(event: &EventRecord) -> bool {
    if event.event_type == EventType::ClientMessageSend.as_str() {
        return false;
    }
    if event.event_type == EventType::ToolResult.as_str()
        && event.payload.get("dispatch").and_then(Value::as_str) == Some("client")
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(event_type: EventType, payload: Value) -> EventRecord {
        EventRecord {
            id: "evt_test".into(),
            session_id: "ses_test".into(),
            client_key_id: None,
            event_type: event_type.as_str().to_owned(),
            payload,
            created_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn client_message_send_is_not_broadcast() {
        assert!(!should_broadcast(&record(
            EventType::ClientMessageSend,
            json!({ "role": "user", "content": "hi" })
        )));
    }

    #[test]
    fn client_dispatched_tool_result_is_not_broadcast() {
        assert!(!should_broadcast(&record(
            EventType::ToolResult,
            json!({ "dispatch": "client", "content": "42" })
        )));
    }

    #[test]
    fn mcp_dispatched_tool_result_is_broadcast() {
        assert!(should_broadcast(&record(
            EventType::ToolResult,
            json!({ "dispatch": "mcp", "content": "42" })
        )));
    }

    #[test]
    fn should_broadcast_is_exhaustive_over_every_event_type() {
        // Assert the filter for every EventType variant. The `match` below has no
        // wildcard arm, so adding a 13th variant forces a deliberate decision
        // here rather than silently defaulting its broadcast behaviour.
        for et in EventType::ALL {
            // With a neutral payload (no client dispatch marker), only the
            // client's own message-send is withheld from live watchers.
            let expected = match et {
                EventType::ClientMessageSend => false,
                EventType::ServerMessageSend
                | EventType::ProviderRequest
                | EventType::ProviderResponse
                | EventType::ToolCall
                | EventType::ToolResult
                | EventType::McpRequest
                | EventType::McpResponse
                | EventType::SessionOpen
                | EventType::SessionClose
                | EventType::SessionError
                | EventType::SessionCompaction => true,
            };
            assert_eq!(
                should_broadcast(&record(et, json!({}))),
                expected,
                "unexpected default broadcast decision for {et}"
            );
        }

        // The one payload-scoped exclusion: a client-dispatched tool.result is
        // withheld (it would be echoed back to its author), while the same event
        // type from an MCP dispatch is forwarded.
        assert!(!should_broadcast(&record(
            EventType::ToolResult,
            json!({ "dispatch": "client", "content": "x" })
        )));
        assert!(should_broadcast(&record(
            EventType::ToolResult,
            json!({ "dispatch": "mcp", "content": "x" })
        )));
    }

    #[test]
    fn provider_and_session_events_are_broadcast() {
        for et in [
            EventType::ProviderRequest,
            EventType::ProviderResponse,
            EventType::ToolCall,
            EventType::McpRequest,
            EventType::McpResponse,
            EventType::ServerMessageSend,
            EventType::SessionOpen,
            EventType::SessionClose,
            EventType::SessionError,
        ] {
            assert!(should_broadcast(&record(et, json!({}))), "{et} should broadcast");
        }
    }

    #[test]
    fn publish_without_subscriber_is_noop() {
        let b = EventBroadcaster::new();
        // No channel exists yet; publishing must not panic or create one.
        b.publish(&record(EventType::ServerMessageSend, json!({})));
    }

    #[tokio::test]
    async fn subscribe_then_publish_delivers() {
        let b = EventBroadcaster::new();
        let (mut rx, _cancel) = b.subscribe("ses_test");
        b.publish(&record(EventType::ServerMessageSend, json!({ "x": 1 })));
        let got = rx.recv().await.expect("event delivered");
        assert_eq!(got.event_type, "server.message.send");
    }

    #[tokio::test]
    async fn remove_closes_receivers() {
        let b = EventBroadcaster::new();
        let (mut rx, _cancel) = b.subscribe("ses_test");
        b.remove("ses_test");
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }
}
