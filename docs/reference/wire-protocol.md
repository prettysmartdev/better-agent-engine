# Wire Protocol

The client port (`BAE_ADDR`, default `0.0.0.0:8080`) is a **hybrid**:

- **REST/HTTP** for management operations — session open/close, metadata,
  event history. These endpoints accept a single JSON body and return a single
  buffered JSON response, exactly as documented in the
  [Client API](client-api.md).
- **JSON-RPC 2.0 over NDJSON** for the session loop — one endpoint,
  `POST /api/v1/sessions/{id}/rpc`, carries all live-interaction methods.

The admin port (`BAE_ADMIN_ADDR`, default `127.0.0.1:8081`) is **REST/HTTP
throughout** — no JSON-RPC anywhere on the admin port.

This page documents the JSON-RPC transport mechanics. For method params and
result shapes see [Client API — `/rpc` methods](client-api.md#post-apiv1sessionsidrpc--json-rpc-session-loop).

---

## JSON-RPC 2.0 envelope

### Request

Send a single JSON object:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "session.sendMessage",
  "params": { "message": { "role": "user", "content": "Hello" } }
}
```

| Field | Required | Value |
|---|---|---|
| `jsonrpc` | yes | must be the string `"2.0"` |
| `id` | recommended | any JSON string or integer — echoed back on the terminal response. Omitting `id` makes the request a **notification**: the server performs the method's side effect but sends **no terminal response** (not even an error). Always send an `id` unless you deliberately want fire-and-forget. |
| `method` | yes | one of `session.registerDriver`, `session.sendMessage`, `session.subscribe`, `session.unsubscribe` |
| `params` | yes | method-specific object (see [Client API](client-api.md)) |

### Notification (server → client)

An object with **no `id`** member is a notification. Notifications are
informational — no reply is expected.

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

### Terminal response

An object **carrying the request `id`** is the terminal response. On success:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": { … }
}
```

On error:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": { "code": -32601, "message": "Method not found" }
}
```

---

## NDJSON framing

`POST /api/v1/sessions/{id}/rpc` always responds with
`Content-Type: application/x-ndjson`. The HTTP body is a stream of
newline-terminated JSON objects written as the session progresses:

```
<JSON object>\n
<JSON object>\n
…
<terminal JSON object>\n
```

- Each line is a complete, self-contained JSON-RPC object.
- The HTTP response body remains open until the terminal object is written and
  the body is closed.
- Objects without `"id"` are notifications (live events, emitted in order).
- The object carrying the request `"id"` is the terminal response (last object
  for `sendMessage` and `unsubscribe`; `subscribe` has no terminal while active).
- A client needs to branch only once: **does this object have an `id`?**

Every other client-port endpoint returns a single buffered JSON body, as
documented in [Client API](client-api.md). The NDJSON framing is specific to
`/rpc`.

---

## Authentication

The `Authorization: Bearer <session-key>` HTTP header applies to
`POST /api/v1/sessions/{id}/rpc` exactly as it does to all other
session-scoped routes. The session id in the URL path is already the session
scope; JSON-RPC `params` do not need to repeat it.

Auth is checked **before** the NDJSON body opens. A bad or missing session key
returns `401` with an RFC 7807 error body — the same shape as other REST
endpoints. Once the stream is open, session-state errors (session not open,
profile deleted mid-session) are delivered as JSON-RPC error objects in the
stream (see error codes below), not as HTTP errors.

---

## Error codes

Errors on `POST /api/v1/sessions/{id}/rpc` are JSON-RPC error objects:

| Code | Meaning | When |
|---|---|---|
| `-32700` | Parse error | Request body is not valid JSON. |
| `-32600` | Invalid Request | Well-formed JSON that is not a valid JSON-RPC request object, or a batch array (batches are unsupported). |
| `-32601` | Method not found | `method` is not one of the three supported values. |
| `-32602` | Invalid params | Required params are missing or have the wrong type. |
| `-32603` | Internal error | Unexpected server error (e.g. database failure). |
| `-32000` | Application error | Session is not in `open` state, profile was deleted mid-session, or the broadcast channel was overrun (see `"lagged"` below). |
| `-32001` | Driver not registered | `session.sendMessage` called before `session.registerDriver` on this connection's client key. See [FIFO turn ownership](#fifo-turn-ownership-and-driver-registration) below. |

All other client-port endpoints (meta, session open/getEvents/close) return RFC
7807 error bodies on non-2xx status codes — unchanged from before.

### Provider failure is a `result`, not an error

When all LLM providers fail, `session.sendMessage` still returns a **terminal
`result`** (HTTP 200) whose `message` contains a generic "provider unavailable"
assistant turn and whose `events` include the full failure trail. A JSON-RPC
`error` object is **not** used for provider failures. SDKs detect this case by
scanning `result.events` for a `session.error` event with
`reason == "all_providers_failed"` and surface it as their `ProvidersFailedError`
type.

### Lagged subscriber

If a `session.subscribe` or `session.sendMessage` notification stream falls
behind faster than the broadcast channel can buffer (capacity: 256 events), the
stream closes with:

```json
{"jsonrpc":"2.0","error":{"code":-32000,"message":"lagged; reconnect with since_event_id"}}
```

Note the **absence of `"id"`** — this is a server-originated error notification,
not a response. On seeing it, reconnect via a new `session.subscribe` call with
`since_event_id` set to the last event id you received, then reconcile any gap
via `GET /api/v1/sessions/{id}/events`.

---

## FIFO turn ownership and driver registration

A session can have multiple **drivers** — client keys that send messages —
plus any number of **observers** — connections that only call
`session.subscribe`. Registration is explicit and asymmetric:

- **Driver**: call `session.registerDriver` once (`{}`, no params) before
  your first `session.sendMessage`. SDK harnesses do this automatically as
  part of `connect()`/`join()`. Idempotent, logs a `session.driver.register`
  event on first registration only.
- **Observer**: call `session.subscribe`. This *is* the registration act —
  nothing else is required, and nothing is logged (subscribing is not an
  audited action; sending messages is).

A `session.sendMessage` call from a client key that never registered as a
driver is rejected with `-32001` before anything else happens — no state
check, no param validation, no queuing.

### The single-active-message mutex

At most one driver's message is being processed per session at a time. A
second driver's `session.sendMessage` call while another turn is in flight
**queues in FIFO order** — first call to actually reach the front of the
queue wins, regardless of which HTTP request the underlying transport
happened to schedule first. The queued caller's NDJSON response opens
immediately but writes nothing until its turn is dequeued; there is no
polling, no retry — the connection simply waits.

**A turn is a logical unit, not a single HTTP request.** When a turn pauses
for a client-side tool call (`Outcome::Paused` — the assistant response
contains at least one `dispatch:"client"` `tool_use` block the harness must
dispatch), the FIFO gate is not released. A paused turn may also carry
`sandbox`/`mcp` `tool_use` blocks the server already dispatched and answered
itself — see [Client API — tool call response](client-api.md#sessionsendmessage)
for the client contract and how the server merges both result sets back
together on resume. The gate stays held by that turn's owner — the client key
that sent the original message — until:

- The **same** client key sends the `tool_result` continuation, or deliberately
  replaces it with a fresh plain message, reusing the held gate with no
  queuing, or
- `BAE_TURN_TIMEOUT` (default 120s, see [Configuration](configuration.md))
  elapses without that continuation arriving, at which point the turn is
  **abandoned**: the next arrival reclaims the gate, and a
  broadcast `session.error` event (`reason: "driver_turn_abandoned"`,
  `{"owner_client_key_id": "key_…"}`) is logged. Before that new message is
  sent upstream, the server completes every unanswered client tool call with
  a synthetic error result and merges any parked MCP/sandbox results. The
  session itself stays `open` with valid replay history.

Only the owning client key may submit that turn's continuation; a *different*
driver's `session.sendMessage` call during someone else's paused turn simply
queues (blocks) like any other contender for the gate — it is never rejected
with an error, it waits its turn. Ownership is tracked by `client_key_id`,
not by the shape of the message: the owner may abandon its own pending tool
call voluntarily by sending a fresh message instead of a `tool_result`
continuation. The server puts synthetic error results for unanswered client
ids and that fresh content in the single following `user` turn, so role
alternation and exact tool-result coverage remain valid.

### "Remaining connected" is a return-before-timeout guarantee, not a held socket

The summary behind this feature describes a persistent-connection model — "if
client A sends a message, it must remain connected in order to handle
client-side tool calls, or else the handling of that message is terminated."
`POST /api/v1/sessions/{id}/rpc` is a one-shot request/NDJSON-response call:
a `Paused` outcome's terminal response necessarily ends that HTTP exchange —
there is no socket left to "remain connected" on.

**The translation:** "remaining connected" is operationalized as *returning
with the continuation before `BAE_TURN_TIMEOUT` elapses*. A driver that
receives a `tool_use` block, dispatches it locally, and calls
`session.sendMessage` again with the `tool_result` well within the timeout
window has behaved exactly as if it had "remained connected," even though
each leg is its own independent HTTP request. Only exceeding the timeout —
not disconnecting between requests — triggers abandonment.

### Per-turn tool scoping and event attribution

Each turn only ever advertises the **acting** driver's own declared tools
(plus the session's shared, session-wide MCP tools) to the provider — a
different driver's private tool declarations are never sent during another
driver's turn, so the model cannot request a tool the current turn's owner
doesn't implement. Every event a turn produces (`provider.request`,
`tool.call`, `client.message.send`, etc.) is attributed to the **acting**
client key in its `client_key_id` column — not always the session's original
creator — so `GET /api/v1/sessions/{id}/events` reconstructs who actually did
what.

### Joining mid-turn

`POST /api/v1/sessions/{id}/join` and `session.registerDriver` are unaffected
by an in-flight turn — a joining client can attach and register at any time.
It simply queues behind the FIFO gate like any other driver once it calls
`session.sendMessage`.

---

## Batch requests

JSON-RPC 2.0 batch requests (arrays of request objects) are **not supported**
on `POST /api/v1/sessions/{id}/rpc`. Sending an array returns:

```json
{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request: batch requests are not supported"}}
```

Batch responses would require buffering the entire response before writing,
which is incompatible with the streaming model used by `session.sendMessage`.

---

## The `GET /healthz` exception

`GET /healthz` is plain HTTP on the client port — unauthenticated, single
buffered empty body, no JSON-RPC envelope. It is outside the REST/JSON-RPC
split and exists solely for liveness probes.
