//! Session-scoped event broadcasting.
//!
//! Every event that is persisted to `session_events` is *also* published, live,
//! to any connection currently watching that session — the driving
//! `session.sendMessage` call and any `session.subscribe` observers. This module
//! owns two things:
//!
//! - [`EventBroadcaster`] — the in-process registry of per-session
//!   [`tokio::sync::broadcast`] channels (created lazily on first subscribe) plus
//!   a per-session cancellation [`Notify`] used by `session.unsubscribe`.
//! - [`insert_and_publish`] — the **single choke point** for persisting an event:
//!   it inserts the row and, on success, publishes the record on the session's
//!   channel. Every place that used to call [`sessions::insert_event`] directly
//!   goes through here (or, for events inserted inside a larger transaction,
//!   through [`EventBroadcaster::publish`] once the transaction commits).
//!
//! Every event is broadcast to every watcher unconditionally — there is no
//! type-based filtering. The sole exception is that `session.sendMessage` does
//! not echo, back to the caller's own stream, the events that same request
//! literally sent; that suppression is per-connection and lives in `rpc.rs`, so
//! genuine observers on other connections still receive those events live.

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
        self.channels
            .lock()
            .expect("event broadcaster mutex poisoned")
    }

    /// Subscribe to a session's live feed, creating the channel lazily. Returns
    /// the broadcast receiver and the session's cancellation handle (fired by
    /// `session.unsubscribe`). Subscribe **before** doing any catch-up replay so
    /// no event slips through the seam between replay and live delivery.
    pub fn subscribe(&self, session_id: &str) -> (broadcast::Receiver<EventRecord>, Arc<Notify>) {
        let mut map = self.lock();
        let entry = map
            .entry(session_id.to_owned())
            .or_insert_with(|| SessionChannel {
                sender: broadcast::channel(CHANNEL_CAPACITY).0,
                cancel: Arc::new(Notify::new()),
            });
        (entry.sender.subscribe(), entry.cancel.clone())
    }

    /// Publish an event to its session's channel, if one exists. *Every*
    /// persisted event is forwarded to *every* live watcher — nothing is
    /// filtered by type here. The one exception (a client's own
    /// `client.message.send` / client-dispatched `tool.result` is not echoed
    /// straight back to the very request that sent it) is enforced per-connection
    /// in `session.sendMessage`'s forward loop, not globally, so genuine
    /// observers on other connections still receive those events live. A `send`
    /// with no live receivers is a harmless no-op (its `Err` is ignored), and a
    /// session nobody is watching has no channel at all, so this is cheap on the
    /// hot path.
    pub fn publish(&self, event: &EventRecord) {
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
    let record = store
        .with_conn(|c| sessions::insert_event(c, session_id, client_key_id, event_type, payload))?;
    broadcaster.publish(&record);
    Ok(record)
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

    /// Every event type — including a client's own `client.message.send` and a
    /// client-dispatched `tool.result` — is delivered to a live watcher. No type
    /// is filtered at the broadcast layer; the only echo-suppression is
    /// per-connection in `session.sendMessage`'s forward loop, so genuine
    /// observers always receive everything live.
    #[tokio::test]
    async fn every_event_type_is_delivered_to_a_watcher() {
        for et in EventType::ALL {
            let b = EventBroadcaster::new();
            let (mut rx, _cancel) = b.subscribe("ses_test");
            b.publish(&record(et, json!({})));
            let got = rx.recv().await.expect("event delivered");
            assert_eq!(got.event_type, et.as_str(), "{et} should reach a watcher");
        }
    }

    #[tokio::test]
    async fn client_message_send_and_client_tool_result_are_broadcast() {
        let b = EventBroadcaster::new();
        let (mut rx, _cancel) = b.subscribe("ses_test");
        b.publish(&record(
            EventType::ClientMessageSend,
            json!({ "role": "user", "content": "hi" }),
        ));
        b.publish(&record(
            EventType::ToolResult,
            json!({ "dispatch": "client", "content": "42" }),
        ));
        assert_eq!(
            rx.recv()
                .await
                .expect("client.message.send delivered")
                .event_type,
            "client.message.send"
        );
        assert_eq!(
            rx.recv()
                .await
                .expect("client tool.result delivered")
                .event_type,
            "tool.result"
        );
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
