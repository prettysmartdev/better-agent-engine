# Client API Reference

The client API is served on `BAE_ADDR` (default `0.0.0.0:8080`). This is the
only port SDKs and agents communicate with; admin operations use the separate
admin port (see [admin-api.md](admin-api.md)).

The client port is a **hybrid**:
- **REST/HTTP** for management — session open/close, metadata, event replay.
- **JSON-RPC 2.0 over NDJSON** for the live session loop — one endpoint,
  `POST /api/v1/sessions/{id}/rpc`.

All REST endpoints use `Content-Type: application/json` with `snake_case` field
names. The `/rpc` endpoint uses `Content-Type: application/x-ndjson`. See
[Wire Protocol](wire-protocol.md) for transport details.

---

## Authentication

Every `/api/v1/sessions*` endpoint requires an `Authorization` header:

```
Authorization: Bearer <key>
```

Both `Bearer ` (capital B) and `bearer ` (lowercase) are accepted. An
absent or empty header returns `401`.

| Endpoint | Required key type |
|---|---|
| `POST /api/v1/sessions` | Client key (`bae_…`) |
| `POST /api/v1/sessions/{id}/join` | Client key (`bae_…`) — may be a **different** client key than the one that created the session, as long as it shares the session's profile |
| All other `/api/v1/sessions/{id}/*` | Session key (`bae_ses_…`) for that session |

A valid session key presented for a different session id returns `401` (the
session key is bound to its session at creation). A session can have
**multiple** valid session keys at once — one per client key that created or
joined it (see [`join`](#post-apiv1sessionsidjoin--join-an-existing-session)
below).

---

## Utility endpoints (no auth)

### `GET /healthz`

Returns `200 OK` with an empty body. No authentication required. Use this for
liveness probes. Plain HTTP — no JSON-RPC envelope.

### `GET /api/v1/meta`

Returns server version information. No authentication required.

**Response `200 OK`:**
```json
{"version": "0.1.0", "api_versions": ["v1"]}
```

---

## Errors (REST endpoints)

Every non-2xx response from REST endpoints follows RFC 7807:

```json
{
  "type": "unauthorized",
  "title": "Unauthorized",
  "status": 401,
  "detail": "invalid or revoked client key"
}
```

| `type` | HTTP status | When |
|---|---|---|
| `unauthorized` | 401 | Missing, invalid, or revoked key. |
| `not_found` | 404 | Session does not exist. |
| `tool_not_allowed` | 403 | A declared tool is not in the profile's `allowed_tools`. |
| `profile_mismatch` | 403 | `POST /join` only — the joining client key's profile differs from the session's profile. |
| `session_closed` | 409 | Session is not open (already closed or errored) — REST endpoints only. |
| `profile_unavailable` | 422 | The profile was deleted after the key was created. |
| `primary_provider_unavailable` | 422 | The profile's `primary_provider` name is not in the server's `[providers]` registry. Logged on every attempt, never deduplicated. See [Profiles](../profiles.md#fatal-primary--non-fatal-fallback). |
| `internal` | 500 | Unexpected server error. |

> **`POST /api/v1/sessions/{id}/rpc`** checks auth before opening the stream;
> a bad key returns `401` (RFC 7807). Once the stream is open, session-state
> errors (session not open, profile deleted mid-session) are delivered as
> JSON-RPC error objects inside the NDJSON stream — not as HTTP error codes.
> See [Wire Protocol — Error codes](wire-protocol.md#error-codes).

---

## Pagination

`GET /api/v1/sessions/{id}/events` accepts `?cursor=<opaque>&limit=<n>`:

```json
{
  "items": [ … ],
  "next_cursor": "42"
}
```

- `next_cursor` is `null` on the last page.
- Default limit: **50**. Maximum: **200**.
- Cursor is opaque — never parse it.

---

## Sessions

### `POST /api/v1/sessions` — open a session

Auth: **client key**.

**Request body:**

```json
{
  "client_version": "1.0.0",
  "tools": [
    {
      "name": "get_current_time",
      "description": "Return the current UTC time as a string",
      "input_schema": {
        "type": "object",
        "properties": {}
      }
    }
  ]
}
```

- `client_version` — optional.
- `tools` — optional, default `[]`. These are the **client-side tools** the
  harness can execute. Every declared tool name must be in the profile's
  `allowed_tools`; an empty `allowed_tools` on the profile means no tools are
  allowed. `description` and `input_schema` are optional per tool.

At session creation, BAE also connects to any MCP servers named in the
profile's `mcp_servers` list, runs the MCP `initialize` handshake, and merges
their tools into the tool list advertised to the provider. A server not found
in the registry is skipped non-fatally (logged as an error).

**Response `201 Created`:**

```json
{
  "session_id": "ses_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "session_key": "bae_ses_1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f",
  "profile": {
    "id": "pro_…",
    "name": "main",
    "allowed_tools": ["get_current_time"],
    "mcp_servers": ["filesystem"],
    "provider": {
      "provider": "anthropic",
      "model": "claude-sonnet-4-6"
    }
  }
}
```

> **`session_key` is shown exactly once.** Store it for all subsequent
> requests on this session. The returned `profile` is sanitized — no
> `auth_token`, no env var names are included.

**Errors:**
- `401 unauthorized` — bad or revoked client key.
- `403 tool_not_allowed` — a declared tool is not in `allowed_tools`.
- `422 profile_unavailable` — the profile was deleted between key creation and
  session open. A `session.error` event is still recorded for audit.
- `422 primary_provider_unavailable` — the profile's `primary_provider` name
  is not in the server's `[providers]` registry. Logged (`tracing::error!`)
  on every attempt, never deduplicated. A `session.error` event (`reason:
  "primary_provider_unavailable"`) is recorded for audit, same posture as
  `profile_unavailable`. No session is created and no session key is issued.

---

### `POST /api/v1/sessions/{id}/join` — join an existing session

Auth: **client key**. May be a different client key than the one that opened
the session — that's the point of this endpoint.

**Request body:** identical shape to `POST /api/v1/sessions`:

```json
{
  "client_version": "1.0.0",
  "tools": [
    { "name": "get_current_time", "description": "…", "input_schema": {} }
  ]
}
```

`tools` are validated against the **shared** profile's `allowed_tools`,
exactly like `create`. A joining client declares its own, independent tool
set — joining never merges with, replaces, or reads any other client's
declared tools. See [Message Types — `session.join`](message-types.md#sessionjoin).

**Response `201 Created`:** identical shape to `create`:

```json
{
  "session_id": "ses_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "session_key": "bae_ses_7f8e9d0c1b2a7f8e9d0c1b2a7f8e9d0c1b2a7f8e9d0c1b2a",
  "profile": { "id": "pro_…", "name": "main", "…": "…" }
}
```

The response mints a **new** session key (distinct from the creator's, and
from any other prior joiner's) bound to the joining client key. MCP
connections are **not** re-resolved on join — they are session-wide
infrastructure established once, at create.

**Checks, in order (first failure wins):**

1. `401 unauthorized` — bad or missing client key.
2. `404 not_found` — no session with this id.
3. `409 session_closed` — the session is `closed` or `error`
   (`detail: "session is already <state>"`, same shape as `DELETE`'s
   conflict). A joiner cannot resurrect a terminal session.
4. `403 profile_mismatch` — the joining client key's `profile_id` differs
   from the session's `profile_id`. This is the hard boundary that keeps a
   client on profile X from ever attaching to a session created under
   profile Y. **No event is logged, no session key is minted, the session is
   untouched** — an authorization failure at the client-key level, same
   posture as `tool_not_allowed`.
5. `422 profile_unavailable` — the shared profile was deleted. Same audit
   posture as `create`: a separate `state='error'` session row is logged; the
   joined session itself is untouched.
6. `422 primary_provider_unavailable` — the shared profile's
   `primary_provider` is not in the registry. Same logging/audit posture as
   `create`'s check above.
7. `403 tool_not_allowed` — a tool the joiner declared is not in the shared
   profile's `allowed_tools` (validated independently of what the creator or
   any other joiner declared).

See [Multi-Client Sessions](../guides/multi-client-sessions.md) for a
worked example and [Wire Protocol — FIFO turn ownership](wire-protocol.md#fifo-turn-ownership-and-driver-registration)
for what happens once both clients start sending messages.

---

### `GET /api/v1/sessions/{id}/participants` — list registered drivers

Auth: **session key** for `{id}`.

**Response `200 OK`:**

```json
{ "drivers": ["key_a1b2c3d4", "key_e5f6a7b8"] }
```

A sorted array of client-key ids currently registered as drivers (via
[`session.registerDriver`](#sessionregisterdriver)), from the server's
**in-memory** registry. This is live-only — it resets on server restart, the
same posture as MCP session state. For durable "who ever joined or
registered" history, use `GET /api/v1/sessions/{id}/events` and look for
`session.open`, `session.join`, and `session.driver.register` events.

**Errors:** `401 unauthorized`, `404 not_found`.

---

### `GET /api/v1/sessions/{id}/events` — replay events

Auth: **session key** for `{id}`.

Returns the full append-only event history for the session, oldest first.
Works on open, closed, and error sessions as long as the session key is valid.

```
GET /api/v1/sessions/ses_…/events?limit=100&cursor=
```

**Response `200 OK`:**

```json
{
  "items": [
    {
      "id": "evt_…",
      "session_id": "ses_…",
      "client_key_id": "key_…",
      "event_type": "session.open",
      "payload": {"client_version": "1.0.0", "tools": ["get_current_time"]},
      "created_at": "2026-07-06T18:26:01.000Z"
    },
    …
  ],
  "next_cursor": null
}
```

See [message-types.md](message-types.md) for the full `event_type` catalog and
payload shapes.

---

### `DELETE /api/v1/sessions/{id}` — close a session

Auth: **session key** for `{id}`.

Inserts a `session.close` event (`{"reason":"client_close"}`) and moves the
session to `closed` state. Also terminates any spawned MCP subprocess connections
and drops the session's broadcast channel.

**Response `200 OK`:**

```json
{
  "session_id": "ses_…",
  "state": "closed"
}
```

**Errors:**
- `401 unauthorized`
- `409 session_closed` — session is already closed or in error state.

---

## `POST /api/v1/sessions/{id}/rpc` — JSON-RPC session loop

Auth: **session key** for `{id}`.

This is the single endpoint for live session interaction. It accepts a
JSON-RPC 2.0 request object and responds with an `application/x-ndjson` stream
of JSON-RPC objects: zero or more `session.event` notifications, followed by a
terminal response (or no terminal response for `session.subscribe` while active).

See [Wire Protocol](wire-protocol.md) for the envelope format, framing rules,
and error codes.

**Request:**

```
POST /api/v1/sessions/ses_…/rpc
Authorization: Bearer bae_ses_…
Content-Type: application/json
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "session.sendMessage",
  "params": { … }
}
```

**Response (always 200 once the stream opens):**

```
Content-Type: application/x-ndjson
```

```
{"jsonrpc":"2.0","method":"session.event","params":{…}}\n
{"jsonrpc":"2.0","method":"session.event","params":{…}}\n
{"jsonrpc":"2.0","id":1,"result":{…}}\n
```

The four supported `method` values are `session.registerDriver`,
`session.sendMessage`, `session.subscribe`, `session.unsubscribe`.

---

### `session.registerDriver`

Register the calling connection's client key as a **driver** on this session
— required once before that client key's first `session.sendMessage` call.
SDK harnesses call this automatically as part of `connect()`/`join()`;
application code normally never calls it directly. See
[Wire Protocol — FIFO turn ownership](wire-protocol.md#fifo-turn-ownership-and-driver-registration)
for the full driver/observer model.

**Params:** `{}`

**Terminal result:**

```json
{ "jsonrpc": "2.0", "id": 1, "result": { "registered": true } }
```

- **Idempotent.** A repeat call from an already-registered client key returns
  `registered: true` without inserting a duplicate `session.driver.register`
  event.
- Records `session_id → client_key_id` in the server's in-memory driver
  registry (see [`GET .../participants`](#get-apiv1sessionsidparticipants--list-registered-drivers))
  and inserts a broadcast `session.driver.register` event — other
  drivers/observers see who registered, live.
- No auto-registration anywhere else: a connection that only ever calls
  `session.subscribe` never needs to register, and `session.sendMessage` will
  never silently register a caller on its behalf.

**JSON-RPC errors:**
- `-32000` — the session is not in `open` state (mirrors `sendMessage`'s
  state gate — a terminal session cannot gain drivers).

---

### `session.sendMessage`

Send a user turn and stream live events as the provider processes it.

**Replaces** `POST /api/v1/sessions/{id}/messages` (removed).

**Requires prior driver registration.** The calling client key must have
already called `session.registerDriver` on this session (see above) — SDK
harnesses do this automatically during `connect()`/`join()`.

**Params:**

```json
{
  "message": {
    "role": "user",
    "content": "What time is it?"
  }
}
```

- `role` — optional, defaults to `"user"`.
- `content` — a plain string, or an array of content blocks. Tool result
  blocks (`{"type":"tool_result",…}`) are passed here on the second leg of a
  tool round-trip.

**Stream:**

Zero or more `session.event` notifications are emitted in order as the turn
progresses (provider request/response, tool calls, MCP request/response, etc.),
followed by a terminal result.

**Terminal result:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "message": {
      "role": "assistant",
      "content": [
        {"type": "text", "text": "It is currently 18:26 UTC."}
      ]
    },
    "events": [
      {
        "id": "evt_…",
        "session_id": "ses_…",
        "client_key_id": "key_…",
        "event_type": "client.message.send",
        "payload": {"role": "user", "content": "What time is it?"},
        "created_at": "2026-07-06T18:26:10.000Z"
      },
      {"id": "evt_…", "event_type": "provider.request",    "payload": {…}, "created_at": "…"},
      {"id": "evt_…", "event_type": "provider.response",   "payload": {…}, "created_at": "…"},
      {"id": "evt_…", "event_type": "server.message.send", "payload": {…}, "created_at": "…"}
    ]
  }
}
```

- `result.events` contains **every event** appended during the turn, in order —
  including `client.message.send`. The live notifications are a filtered subset
  of this (client-generated events are not echoed back as notifications, but are
  present in `result.events`).
- A client that ignores notifications and reads only `result.events` loses nothing.

**Tool call response (loop paused):**

When the assistant response contains `tool_use` blocks, the terminal result
`message` carries them and the harness dispatches client-side tools, then
sends another `session.sendMessage` with `tool_result` blocks:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session.sendMessage",
  "params": {
    "message": {
      "role": "user",
      "content": [
        {
          "type": "tool_result",
          "tool_use_id": "tu_abc123",
          "content": "2026-07-06T18:26:10Z"
        }
      ]
    }
  }
}
```

**Provider failure:**

When all providers fail, the terminal response is still a `result` (not an
error object) with HTTP 200. The `result.message` contains a generic "provider
unavailable" assistant turn; `result.events` includes the full failure trail
(including `session.error` with `reason: "all_providers_failed"`). The session
moves to `error` state. SDKs surface this as `ProvidersFailedError`.

**JSON-RPC errors:**
- `-32700 Parse error` — request body is not valid JSON.
- `-32600 Invalid Request` — not a valid JSON-RPC request; also used for batch arrays.
- `-32601 Method not found` — unknown method.
- `-32602 Invalid params` — missing or wrong-typed params.
- `-32000` — session is not open (`open` state required).
- `-32001 driver_not_registered` — `{"code": -32001, "message": "call session.registerDriver before session.sendMessage"}`.
  Checked **first**, before the state check, param validation, the turn lock,
  or broadcast subscription. Never auto-registers — see
  [`session.registerDriver`](#sessionregisterdriver) above.

**FIFO queuing.** If another driver's turn is already in flight on this
session, this call **blocks** — its NDJSON response opens but stays silent
(zero bytes written) until the in-flight turn completes or is judged
abandoned. This is not an error: no bytes means "still queued," not "stuck."
Apply your own client-side request timeout if you'd rather give up than wait
indefinitely — the server itself never times out a queued (not yet started)
message. See [Wire Protocol — FIFO turn ownership](wire-protocol.md#fifo-turn-ownership-and-driver-registration)
for the full ordering, ownership, and abandonment-timeout semantics.

---

### `session.subscribe`

Open a live event subscription. Useful for an observer connection that is not
driving the turn (a dashboard, a log stream, etc.). **Calling `session.subscribe`
is itself the observer registration act** — there is no separate
"registerObserver" method and nothing is logged when a connection subscribes;
it stands in deliberate contrast to `session.registerDriver`, which does log.

**Params:**

```json
{
  "since_event_id": "evt_…"
}
```

- `since_event_id` — optional. When given, the server replays persisted events
  after this id before switching to the live stream.

**Stream:**

`session.event` notifications are emitted indefinitely. **There is no terminal
response while the subscription is active.** The stream ends on:

- Client disconnect.
- A `session.unsubscribe` call from any connection.
- A `"lagged"` error notification (broadcast channel overrun — see
  [Wire Protocol](wire-protocol.md#lagged-subscriber)).

Live events follow the same filter rule as `sendMessage` notifications: only
non-client-generated events are forwarded.

---

### `session.unsubscribe`

End all active `session.subscribe` streams for this session.

**Params:** `{}`

**Terminal result:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": { "unsubscribed": true }
}
```

---

## Content blocks

`content` on a message can be either a plain string or an array of typed
blocks:

```json
{"type": "text",        "text": "…"}
{"type": "tool_use",    "id": "tu_…", "name": "…", "input": {…}}
{"type": "tool_result", "tool_use_id": "tu_…", "content": <string|block[]>}
```

The server passes these through to/from the provider verbatim. SDKs inspect
`tool_use` blocks to dispatch to registered handlers, then send `tool_result`
blocks back.

---

## The harness tool-call loop

SDK harnesses implement this loop inside `session.send(message)`:

1. POST `session.sendMessage` to `/rpc` with `{message:{role:"user", content}}`.
2. Read NDJSON: fire `on_event` for each notification; await terminal result.
3. If `result.message.content` contains **no** `tool_use` block → return the
   final assistant turn to the caller. **Loop ends.**
4. If there are `tool_use` blocks → for each block, call the registered handler
   by `name` with `input`. Build `tool_result` blocks echoing `tool_use_id`.
5. POST `session.sendMessage` with
   `{message:{role:"user", content:[…tool_result blocks]}}`.
6. Go to step 2.

Notes:
- The harness only dispatches `tool_use` blocks for tools **it declared** at
  session open. Tools the client did not declare are dispatched server-side
  through configured MCP servers and never surface as `tool_use` blocks to the
  client.
- `tool_use.id` must be echoed verbatim as `tool_result.tool_use_id`.
- Hooks (`before_send`, `after_receive`, `before_tool_call`, `after_tool_call`,
  `on_event`) fire at their respective points; an error from any hook aborts
  the loop.
