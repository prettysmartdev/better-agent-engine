# Client API Reference

The client API is served on `BAE_ADDR` (default `0.0.0.0:8080`). This is the
only port SDKs and agents communicate with; admin operations use the separate
admin port (see [admin-api.md](admin-api.md)).

All requests and responses use `Content-Type: application/json` with
`snake_case` field names.

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
| All other `/api/v1/sessions/{id}/*` | Session key (`bae_ses_…`) for that session |

A valid session key presented for a different session id returns `401` (the
session key is bound to its session at creation).

---

## Utility endpoints (no auth)

### `GET /healthz`

Returns `200 OK` with an empty body. No authentication required. Use this for
liveness probes.

### `GET /api/v1/meta`

Returns server version information. No authentication required.

**Response `200 OK`:**
```json
{"version": "0.1.0", "api_versions": ["v1"]}
```

---

## Errors

Every non-2xx response body follows RFC 7807:

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
| `session_closed` | 409 | Session is not open (already closed or errored). |
| `profile_unavailable` | 422 | The profile was deleted after the key was created. |
| `bad_gateway` | 502 | All providers failed. See note below. |
| `internal` | 500 | Unexpected server error. |

> **502 special case.** On `POST …/messages`, a `502` response still returns
> the normal `{message, events}` body — not a problem-doc. The `message`
> contains a generic "provider unavailable" assistant turn and the `events`
> array contains the failure trail. The session state is set to `error`.

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

**Response `201 Created`:**

```json
{
  "session_id": "ses_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "session_key": "bae_ses_1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f",
  "profile": {
    "id": "pro_…",
    "name": "main",
    "allowed_tools": ["get_current_time"],
    "mcp_servers": [],
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

---

### `POST /api/v1/sessions/{id}/messages` — send a turn

Auth: **session key** for `{id}`.

**Request body:**

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

**Response `200 OK`:**

```json
{
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
```

- `events` contains every event appended **during this call**, in order —
  starting with the `client.message.send` for the incoming turn.
- When the assistant response contains `tool_use` blocks, the response pauses
  and is returned to the client with the tool calls. The client harness
  executes the tools and POSTs the `tool_result` blocks back.

**Tool call response (loop paused):**

```json
{
  "message": {
    "role": "assistant",
    "content": [
      {
        "type": "tool_use",
        "id": "tu_abc123",
        "name": "get_current_time",
        "input": {}
      }
    ]
  },
  "events": [ … ]
}
```

The harness then calls the registered tool handler and POSTs the result:

```json
{
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
```

**Errors:**
- `401 unauthorized`
- `409 session_closed` — session is not open.
- `422 profile_unavailable` — profile deleted mid-session; session moved to
  `error`.
- `502` — all providers failed; body is still `{message, events}`.

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
session to `closed` state.

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

SDK harnesses must implement this loop inside `session.send(message)`:

1. `POST …/messages` with `{message:{role:"user", content}}`.
2. If the response `message.content` contains **no** `tool_use` block → return
   the final assistant turn to the caller. **Loop ends.**
3. If there are `tool_use` blocks → for each block, call the registered handler
   by `name` with `input`. Build `tool_result` blocks echoing `tool_use_id`.
4. `POST …/messages` with `{message:{role:"user", content:[…tool_result blocks]}}`.
5. Go to step 2.

Notes:
- The harness only dispatches `tool_use` blocks for tools **it declared** at
  session open. Tools the client did not declare are dispatched server-side as
  MCP stubs and never surface to the client.
- `tool_use.id` must be echoed verbatim as `tool_result.tool_use_id`.
- Hooks (`before_send`, `after_receive`, `before_tool_call`, `after_tool_call`)
  fire at their respective points; an error from any hook aborts the loop.
