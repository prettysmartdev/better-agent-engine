# Message Types

Every row in `session_events` has an `event_type` field drawn from the closed
set below. Adding a new event type requires a code change in the server and
all SDKs — the enum is exhaustive in every language so unhandled variants are
compile or type errors.

Events are returned in the `events` array on `POST …/messages` (events
appended during that call) and via `GET …/events` (full session history).

**EventView shape** (all endpoints):

```json
{
  "id":           "evt_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "session_id":   "ses_…",
  "client_key_id":"key_…",
  "event_type":   "<one of the strings below>",
  "payload":      { … },
  "created_at":   "2026-07-06T18:26:10.000Z"
}
```

`client_key_id` is the client key that created the session; it is `null` on
events emitted by the server on behalf of a deleted key.

---

## Catalog

### `client.message.send`

The client sent a user turn.

```json
{
  "role": "user",
  "content": "What time is it?"
}
```

`content` is either a plain string or an array of content blocks
(`text`, `tool_result`, etc.).

---

### `server.message.send`

The server's final assistant turn for this iteration of the loop.

```json
{
  "role": "assistant",
  "content": [
    {"type": "text", "text": "It is currently 18:26 UTC."}
  ]
}
```

`content` is an array of content blocks. When the loop pauses to return a
`tool_use` block to the client, this event is still emitted with that
`tool_use` content so the full round-trip is visible in the event log.

---

### `provider.request`

The full request payload about to be sent to the LLM provider, including
which attempt number and whether this is the primary or a fallback. The
auth token is **never** included.

```json
{
  "attempt":   0,
  "kind":      "primary",
  "provider":  "anthropic",
  "base_url":  "https://api.anthropic.com",
  "model":     "claude-sonnet-4-6",
  "max_tokens": 8096,
  "messages":  [ {"role": "user", "content": "…"}, … ],
  "tools":     [ … ]
}
```

- `attempt` is 0-indexed.
- `kind` is `"primary"` or `"fallback"`.
- Inserted **before** each provider attempt (primary + every fallback).

---

### `provider.response`

The raw response received from the LLM provider (or the failure reason).

**Success:**

```json
{
  "attempt":  0,
  "kind":     "primary",
  "provider": "anthropic",
  "ok":       true,
  "status":   200,
  "body":     { "role": "assistant", "stop_reason": "end_turn", "content": [ … ] }
}
```

**Failure:**

```json
{
  "attempt":  0,
  "kind":     "primary",
  "provider": "anthropic",
  "ok":       false,
  "status":   429,
  "error":    "rate limit exceeded",
  "body":     null
}
```

- `status` is the HTTP status code, or `null` on a transport-level failure.
- `error` is a human-readable failure reason.
- Inserted **after** each attempt, success or failure.

---

### `tool.call`

The server or harness is about to invoke a tool.

**Client-side dispatch:**

```json
{
  "id":       "tu_abc123",
  "name":     "get_current_time",
  "input":    {},
  "dispatch": "client"
}
```

**MCP dispatch (stub):**

```json
{
  "id":       "tu_xyz789",
  "name":     "some_mcp_tool",
  "input":    {"query": "…"},
  "dispatch": "mcp"
}
```

- `dispatch` is `"client"` for tools declared at session open and `"mcp"` for
  all others (handled server-side as MCP stubs today).

---

### `tool.result`

The result returned from a tool call.

**Client-side result:**

```json
{
  "tool_use_id": "tu_abc123",
  "dispatch":    "client",
  "content":     "2026-07-06T18:26:10Z"
}
```

**MCP stub result:**

```json
{
  "tool_use_id": "tu_xyz789",
  "dispatch":    "mcp",
  "status":      "stub",
  "content":     []
}
```

`content` mirrors the `tool_result` block the provider receives.

---

### `mcp.request`

A request sent to an MCP server. Currently a stub — full MCP implementation
is a later work item.

```json
{
  "status": "stub",
  "tool":   "some_mcp_tool",
  "input":  {"query": "…"}
}
```

---

### `mcp.response`

A response from an MCP server. Currently a stub.

```json
{
  "status": "stub",
  "tool":   "some_mcp_tool"
}
```

---

### `session.open`

Emitted when the session is created.

```json
{
  "client_version": "1.0.0",
  "tools":          ["get_current_time"]
}
```

- `client_version` is `null` if not provided at session creation.
- `tools` is the list of tool names declared at open.

---

### `session.close`

Emitted when the session is closed normally.

```json
{
  "reason": "client_close"
}
```

| `reason` | When |
|---|---|
| `"client_close"` | Client called `DELETE /api/v1/sessions/{id}`. |
| `"client_key_revoked"` | The client key was revoked via the admin API. |

---

### `session.error`

Emitted when the session is terminated due to an error.

```json
{
  "reason": "all_providers_failed"
}
```

| `reason` | When |
|---|---|
| `"provider_config"` | The provider config could not be loaded (e.g. missing env var). |
| `"provider_call_failed"` | The primary provider failed; fallback walk begins. |
| `"all_providers_failed"` | Primary and all fallbacks failed; session moved to `error`. |
| `"loop_limit"` | The per-turn iteration cap (8) was hit. |
| `"profile_unavailable"` | The profile was deleted mid-session. |

Note: `"provider_call_failed"` is recorded once when the primary fails but
a fallback attempt follows. If a fallback succeeds, the session continues
normally. Only `"all_providers_failed"` moves the session to `error`.

---

### `session.compaction`

Reserved — not emitted yet. Will be used when session history is compacted
into a summary to manage context length. No payload schema defined.

---

## Typical event sequences

**Simple text turn:**

```
client.message.send
provider.request       (attempt 0, kind: primary)
provider.response      (ok: true)
server.message.send
```

**Failed primary, working fallback:**

```
client.message.send
provider.request       (attempt 0, kind: primary)
provider.response      (ok: false)
session.error          (reason: provider_call_failed)
provider.request       (attempt 1, kind: fallback)
provider.response      (ok: true)
server.message.send
```

**Client-side tool call (two `POST …/messages` calls):**

Call 1:
```
client.message.send
provider.request
provider.response      (ok: true)
tool.call              (dispatch: client)
server.message.send    (content has tool_use block — loop paused)
```

Call 2:
```
client.message.send    (content has tool_result block)
tool.result            (dispatch: client)
provider.request
provider.response      (ok: true)
server.message.send    (final text)
```

**MCP stub tool call (single `POST …/messages` call, server-side):**

```
client.message.send
provider.request
provider.response      (ok: true)
tool.call              (dispatch: mcp)
mcp.request
mcp.response
tool.result            (dispatch: mcp, status: stub)
provider.request
provider.response      (ok: true)
server.message.send
```
