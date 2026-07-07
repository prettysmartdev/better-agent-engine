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
result shapes see [Client API — `/rpc` methods](client-api.md#post-apiv1sessionsidrcpjson-rpc-session-loop).

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
| `method` | yes | one of `session.sendMessage`, `session.subscribe`, `session.unsubscribe` |
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
