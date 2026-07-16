# Live Events

Two ways to receive live events: inline notifications during
`session.sendMessage`, and an independent observer subscription.

For the conceptual overview see [Event Streaming](../guides/06-event-streaming.md).

---

## Prerequisites

- BAE server running on `http://localhost:8080`.
- An open session (`SESSION_ID`, `SESSION_KEY`).

---

## Method 1 — Inline notifications during `session.sendMessage`

Every `session.sendMessage` call streams `session.event` notifications before
the terminal result. No additional call is needed — beyond the one-time
`session.registerDriver` every session key must make before its first
`session.sendMessage` (SDKs do this automatically inside `connect()`):

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.registerDriver","params":{}}'
```

### curl

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"session.sendMessage","params":{"message":{"role":"user","content":"What is 2+2?"}}}' \
| while IFS= read -r line; do
    # Check whether this line is a notification or the terminal result
    if echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); exit(0 if 'id' not in d else 1)" 2>/dev/null; then
      event_type=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['params']['event_type'])")
      echo "[event] $event_type"
    else
      echo "[terminal] $(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['result']['message']['content'][0]['text'])")"
    fi
  done
```

Output:
```
[event] provider.request
[event] provider.response
[event] server.message.send
[terminal] 4
```

(`client.message.send` is filtered — not echoed as a notification, but
present in the terminal `result.events`.)

### TypeScript SDK

```typescript
harness.setHooks({
  on_event: (event) => {
    process.stderr.write(`[event] ${event.event_type}\n`);
  },
});

const session = await harness.connect();
const reply = await session.send("What is 2+2?");
console.log(reply.content[0]?.text ?? "");
```

---

## Method 2 — Observer subscription (`session.subscribe`)

`session.subscribe` is for a **separate, non-driving connection** — a second
process watching a session it is not sending to, or a log aggregator.

### curl — subscribe (separate terminal)

```sh
# Start a subscriber that replays all events since the start, then
# watches live. Press Ctrl-C to disconnect.
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"session.subscribe","params":{"since_event_id":""}}'
```

Each line is a `session.event` notification. There is no terminal response —
the stream stays open until you disconnect or `unsubscribe`.

### curl — unsubscribe (end the subscriber)

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"session.unsubscribe","params":{}}'
# {"jsonrpc":"2.0","id":3,"result":{"unsubscribed":true}}
```

### TypeScript SDK

```typescript
const session = await harness.connect();

// Start a subscriber in the background
const subscribePromise = session.subscribe(
  (event) => {
    process.stderr.write(`[observe] ${event.event_type}\n`);
    return true; // keep receiving
  },
  { sinceEventId: lastSeenEventId }, // omit to start from now
);

// Drive a turn from another connection (or the same session object)
const reply = await session.send("What is 2+2?");
console.log(reply.content[0]?.text ?? "");

// End the subscription
await session.unsubscribe();
await subscribePromise;
```

### Python SDK

```python
import asyncio

async def observe(event) -> bool:
    print(f"[observe] {event.event_type}", file=sys.stderr)
    return True  # return False to stop

session = await harness.connect()

# Run subscriber and turn driver concurrently
async def run():
    subscribe_task = asyncio.create_task(
        session.subscribe(observe, since_event_id=last_seen_event_id)
    )
    reply = await session.send("What is 2+2?")
    print(reply.text())
    await session.unsubscribe()
    await subscribe_task

await run()
await session.close()
```

---

## Resuming after a disconnect

If your subscriber drops, reconnect with `since_event_id` set to the last
event id you received:

```sh
LAST_SEEN="evt_…"

curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session.subscribe\",\"params\":{\"since_event_id\":\"$LAST_SEEN\"}}"
```

The server replays persisted events after `$LAST_SEEN`, then picks up the live
stream — no gap.

If a `"lagged; reconnect with since_event_id"` error appears in the stream,
do the same: reconnect with `since_event_id` and reconcile via
`GET /api/v1/sessions/{id}/events` to find what you missed.
