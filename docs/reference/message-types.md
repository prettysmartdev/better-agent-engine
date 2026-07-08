# Message Types

Every row in `session_events` has an `event_type` field drawn from the closed
set below. Adding a new event type requires a code change in the server and all
SDKs — the enum is exhaustive in every language so unhandled variants are
compile or type errors.

Events are returned in the `events` array on the terminal result of
`session.sendMessage` (events appended during that call) and via
`GET /api/v1/sessions/{id}/events` (full session history). They are also
delivered as live `session.event` notifications on the `/rpc` NDJSON stream —
see [Event Streaming](../guides/event-streaming.md).

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
- `tools` includes both client-declared tools and any tools fetched from
  connected MCP servers.
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

#### Raw-logged vs. canonical-returned (OpenAI-kind providers)

`body` is always the **raw, untranslated wire response** — for a `provider`
whose registry entry has `provider = "anthropic"`, that's the Anthropic
Messages API shape, unchanged. For a `provider = "openai"` entry, `body` is
the **raw OpenAI Chat Completions response** (`choices[0].message` with
`tool_calls`, etc.) — it is *not* translated before being logged here, so the
event log is a faithful record of what the provider actually said on the
wire.

This is deliberately different from what the rest of the turn sees: `engine::session::run_turn`
only ever consumes the **canonical** shape (the same
`{"content": [{"type": "text"|"tool_use"|"tool_result", …}]}` block format
used internally today, and by `anthropic`-kind providers natively) —
`engine::provider::call()` translates an OpenAI response into this canonical
shape internally before handing it back to the turn loop. So `tool.call`,
`server.message.send`, and everything else derived from the turn's own
history are always canonical, regardless of which provider kind served the
attempt — only `provider.response`'s `body` field preserves the raw,
kind-specific wire shape. See
[Configuration — `[providers]`](configuration.md#providers) for the
`provider` field and [Profiles](../profiles.md#provider-config) for how a
profile selects providers by name.

---

### `tool.call`

The server or harness is about to invoke a tool.

**Client-side dispatch:**

```json
{
  "id":          "tu_abc123",
  "name":        "get_current_time",
  "input":       {},
  "dispatch":    "client",
  "server_name": null
}
```

**MCP dispatch:**

```json
{
  "id":          "tu_xyz789",
  "name":        "list_directory",
  "input":       {"path": "/data"},
  "dispatch":    "mcp",
  "server_name": "filesystem"
}
```

- `dispatch` is `"client"` for tools declared at session open and `"mcp"` for
  tools handled server-side by a configured MCP server.
- `server_name` is the MCP server's name from `bae-config.toml`, or `null` if
  dispatch is `"client"` or if the tool name was not found in any server's tool
  list (indicates a mis-routed call).

---

### `tool.result`

The result returned from a tool call.

**Client-side result:**

```json
{
  "tool_use_id": "tu_abc123",
  "dispatch":    "client",
  "server_name": null,
  "is_error":    false,
  "content":     "2026-07-06T18:26:10Z"
}
```

**MCP result (success):**

```json
{
  "tool_use_id": "tu_xyz789",
  "dispatch":    "mcp",
  "server_name": "filesystem",
  "is_error":    false,
  "content":     [{"type": "text", "text": "README.md\ndata.csv"}]
}
```

**MCP result (error):**

```json
{
  "tool_use_id": "tu_xyz789",
  "dispatch":    "mcp",
  "server_name": "filesystem",
  "is_error":    true,
  "content":     "MCP error: connection refused"
}
```

- `content` mirrors the `tool_result` block the provider receives.
- `is_error: true` means the MCP call failed or returned an error; the session
  continues and the provider receives the error content so it can adjust.

---

### `mcp.request`

A request sent to an MCP server.

```json
{
  "method":      "tools/call",
  "server_name": "filesystem",
  "tool":        "list_directory",
  "input":       {"path": "/data"}
}
```

---

### `mcp.response`

A response from an MCP server.

**Success:**

```json
{
  "server_name": "filesystem",
  "ok":          true,
  "result":      {
    "content": [{"type": "text", "text": "README.md\ndata.csv"}],
    "isError": false
  }
}
```

**Failure:**

```json
{
  "server_name": "filesystem",
  "ok":          false,
  "error":       "stdio process exited unexpectedly"
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
- `tools` is the list of tool names declared at open (client-side tools only).

---

### `session.join`

Emitted when a second (or further) client key mints a session key for an
existing session via `POST /api/v1/sessions/{id}/join`. Same payload shape as
`session.open` — it's the identical "a client key attached, declaring this
tool set" fact, just via the join path instead of create.

```json
{
  "client_version": "1.2.0",
  "tools":          ["only_b"]
}
```

- `client_version` is `null` if not provided at join.
- `tools` is the **joining client's own** declared tool list (client-side
  tools only) — never merged with the creator's or any other joiner's.
- The event's `client_key_id` column is the **joiner**, not the session's
  original creator.
- See [Client API — `POST .../join`](client-api.md#post-apiv1sessionsidjoin--join-an-existing-session)
  and [Multi-Client Sessions](../guides/multi-client-sessions.md).

---

### `session.driver.register`

Emitted the first time a client key registers as a driver via
`session.registerDriver`. Idempotent registration does **not** re-emit this
event — only the first call for a given client key on a given session logs
it.

```json
{}
```

- Empty payload — the actor is fully captured by the event's `client_key_id`
  column (mirroring how `session.open`/`session.join` also rely on that
  column, not the payload, to identify the acting client).
- See [Client API — `session.registerDriver`](client-api.md#sessionregisterdriver)
  and [Wire Protocol — FIFO turn ownership](wire-protocol.md#fifo-turn-ownership-and-driver-registration).

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

Emitted on a session-affecting error. Most reasons move the session to
`error` state; two (marked below) do not — `session.error` is also used as a
non-fatal audit/visibility signal.

```json
{
  "reason": "all_providers_failed"
}
```

| `reason` | When | Moves session to `error`? |
|---|---|---|
| `"provider_config"` | The provider config could not be loaded (e.g. missing env var), or — since work item 0005 — a message-time re-check found the profile's `primary_provider` name missing from the registry. In the latter case the payload also carries `"detail"` naming the missing provider. | yes |
| `"provider_call_failed"` | The primary provider failed; fallback walk begins. | no (fallback in progress) |
| `"all_providers_failed"` | Primary and all fallbacks failed; session moved to `error`. | yes |
| `"loop_limit"` | The per-turn iteration cap (8) was hit. | yes |
| `"profile_unavailable"` | The profile was deleted mid-session. | yes |
| `"primary_provider_unavailable"` | `POST /api/v1/sessions` or `POST /api/v1/sessions/{id}/join` rejected the request because the profile's `primary_provider` name isn't in the `[providers]` registry. Payload: `{"profile_id": "pro_…", "primary_provider": "name"}`. Logged on this **separate audit session row** (`state='error'`) — the real session, if any, is untouched. See [Profiles](../profiles.md#fatal-primary--non-fatal-fallback). | n/a — no real session was created |
| `"driver_turn_abandoned"` | A paused turn's owning driver didn't return with its continuation before `BAE_TURN_TIMEOUT` elapsed; the FIFO gate was released to the next queued driver. Payload: `{"owner_client_key_id": "key_…"}` (also the event's `client_key_id` column). | **no** — the session stays `open`; other drivers are unaffected |

Note: `"provider_call_failed"` is recorded once when the primary fails but
a fallback attempt follows. If a fallback succeeds, the session continues
normally. Only `"all_providers_failed"` moves the session to `error`.

When `"all_providers_failed"`, `session.sendMessage`'s terminal result still
carries this event in `result.events` — not a JSON-RPC error object.

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

**Client-side tool call (two `session.sendMessage` calls):**

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

**MCP tool call (single `session.sendMessage` call, server-side):**

```
client.message.send
provider.request
provider.response      (ok: true)
tool.call              (dispatch: mcp, server_name: "filesystem")
mcp.request            (method: tools/call)
mcp.response           (ok: true)
tool.result            (dispatch: mcp, is_error: false)
provider.request
provider.response      (ok: true)
server.message.send
```

**Multi-driver session (create, join, both drive):**

```
session.open                    (client_key_id: key_A)
session.driver.register         (client_key_id: key_A)
session.join                    (client_key_id: key_B)
session.driver.register         (client_key_id: key_B)
client.message.send             (client_key_id: key_A)   -- A's turn
provider.request
provider.response      (ok: true)
server.message.send             (client_key_id: key_A)
client.message.send             (client_key_id: key_B)   -- B's turn, only starts after A's completes
provider.request
provider.response      (ok: true)
server.message.send             (client_key_id: key_B)
```

`GET /api/v1/sessions/{id}/events` returns this exact sequence for either
participant — every event is attributed to whichever client key actually
produced it. See [Multi-Client Sessions](../guides/multi-client-sessions.md)
for the full walkthrough.
