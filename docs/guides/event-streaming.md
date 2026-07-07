# Event Streaming

BAE delivers live events over the JSON-RPC session loop. This page explains how
to consume them, when to use the subscription model, and how to reconnect
without gaps.

For the wire-level envelope format see [Wire Protocol](../reference/wire-protocol.md).
For the event payload catalog see [Message Types](../reference/message-types.md).

---

## Events from `session.sendMessage`

Every `session.sendMessage` call streams live `session.event` notifications
**while the turn is running**, before the terminal response is written. You do
not need a separate subscription to see events from your own turns.

Each notification is a JSON-RPC object with no `id`:

```json
{
  "jsonrpc": "2.0",
  "method": "session.event",
  "params": {
    "id": "evt_…",
    "session_id": "ses_…",
    "client_key_id": "key_…",
    "event_type": "provider.request",
    "payload": { … },
    "created_at": "2026-07-06T18:26:10.000Z"
  }
}
```

The stream ends with the terminal response (an object carrying the request
`"id"`), whose `result.events` is the **complete turn event list** — including
events that arrive before the terminal frame. Simple callers that ignore
notifications and read only `result.events` lose nothing; live progress is a
bonus, not a replacement.

### Filtered events

The server does not echo events your own connection generated. Two types are
filtered from the live stream:

- `client.message.send` — the event recording your own user turn.
- `tool.result` with `dispatch: "client"` — events for tool results your own
  harness submitted.

Everything else is forwarded: `provider.request`, `provider.response`,
`tool.call`, `mcp.request`, `mcp.response`, `tool.result` (dispatch: `"mcp"`),
`server.message.send`, all `session.*` events.

> Both `sendMessage` inline notifications and `subscribe` (see below) apply the
> same filter — one predicate, one place in the server.

---

## Subscribing as an observer

`session.subscribe` is for a **second, non-driving connection** that wants the
same live feed — a dashboard watching a session it is not driving, or a log
aggregator.

Call it on the same `POST /api/v1/sessions/{id}/rpc` endpoint with the session
key:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session.subscribe",
  "params": { "since_event_id": "evt_…" }
}
```

`since_event_id` is optional. When given, the server first **replays**
persisted events after that id as `session.event` notifications, then switches
seamlessly to the live broadcast. When omitted the subscription starts from the
current moment (no replay).

`session.subscribe` has **no terminal response while active** — the stream
stays open indefinitely, emitting `session.event` notifications, until:

- The client disconnects.
- `session.unsubscribe` is called (see below).
- The broadcast channel is overrun (the server emits a `"lagged"` error, see
  [Wire Protocol](../reference/wire-protocol.md#lagged-subscriber)).

### `session.unsubscribe`

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "session.unsubscribe",
  "params": {}
}
```

Cancels all active `session.subscribe` streams for the session, then returns a
terminal result:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": { "unsubscribed": true }
}
```

---

## Using the SDK

All three SDKs expose the same interface. The `on_event` hook fires for each
live notification during a `send` call; `subscribe`/`unsubscribe` are methods
on the `Session` object.

### TypeScript

```typescript
// on_event hook — fires for each notification during session.send()
harness.setHooks({
  on_event: (event) => {
    console.error(`[event] ${describeEvent(event)}`);
  },
});

const session = await harness.connect();

// Observer subscription on a separate connection
await session.subscribe(
  (event) => {
    console.error(`[subscribe] ${describeEvent(event)}`);
    return true; // keep receiving; return false to stop
  },
  { sinceEventId: lastSeenEventId },
);
```

### Python

```python
def on_event(event: SessionEvent) -> None:
    print(f"[event] {describe_event(event)}", file=sys.stderr)

hooks = Hooks(on_event=on_event)
harness = Harness(config, hooks=hooks)
session = await harness.connect()

# Observer subscription
async def handle_event(event: SessionEvent) -> bool:
    print(f"[subscribe] {describe_event(event)}", file=sys.stderr)
    return True  # keep receiving; return False to stop

await session.subscribe(handle_event, since_event_id=last_seen_event_id)
```

### Rust

```rust
let harness = HarnessBuilder::new(config)
    .on_event(|event| {
        eprintln!("[event] {}", event.event_type);
        HookResult::Continue
    })
    .build();

let mut session = harness.connect().await?;

// Observer subscription
session.subscribe(Some(&last_seen_event_id), |event| {
    eprintln!("[subscribe] {}", event.event_type);
    true // keep receiving; return false to stop
}).await?;
```

---

## Resuming after a disconnect

The server does not buffer events waiting for a disconnected subscriber. If a
subscription drops, events emitted while you were disconnected are only
available via `GET /api/v1/sessions/{id}/events`.

To reconnect without gaps:

1. **Track the last event id** you received (from either `sendMessage`
   notifications or `subscribe` events). Store it durably if you need
   across-process reliability.
2. On reconnect, open a new `session.subscribe` call with
   `since_event_id: <last-seen-id>`.
3. The server replays persisted events after that id, then picks up the live
   stream. Any events you missed while disconnected appear in the replay.

If the session has ended (`closed` or `error` state), `GET .../events` gives
you the complete history; `session.subscribe` will not receive new events after
the `session.close` or `session.error` event but the replay still works.

---

## Typical event sequences

**Simple text turn (inline notifications on `sendMessage`):**

```
session.event notification: client.message.send
session.event notification: provider.request
session.event notification: provider.response
session.event notification: server.message.send
terminal result: {message, events}
```

**MCP tool call (inline notifications):**

```
session.event notification: client.message.send
session.event notification: provider.request
session.event notification: provider.response     (tool_use block)
session.event notification: tool.call             (dispatch: mcp)
session.event notification: mcp.request
session.event notification: mcp.response
session.event notification: tool.result           (dispatch: mcp)
session.event notification: provider.request      (second pass with tool result)
session.event notification: provider.response
session.event notification: server.message.send
terminal result: {message, events}
```

Note: `client.message.send` is present in `result.events` but **not** emitted
as a notification (filtered, as described above).
