# Client API Reference

The client API is served on `BAE_ADDR` (default `0.0.0.0:8080`). This is the
only port SDKs and agents communicate with; admin operations use the separate
admin port (see [02-admin-api.md](02-admin-api.md)).

The client port is a **hybrid**:
- **REST/HTTP** for management — session open/close, metadata, event replay.
- **JSON-RPC 2.0 over NDJSON** for the live session loop — one endpoint,
  `POST /api/v1/sessions/{id}/rpc`.

All REST endpoints use `Content-Type: application/json` with `snake_case` field
names. The `/rpc` endpoint uses `Content-Type: application/x-ndjson`. See
[Wire Protocol](01-wire-protocol.md) for transport details.

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
> See [Wire Protocol — Error codes](01-wire-protocol.md#error-codes).

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
  ],
  "sandbox_tools": [
    {
      "name": "run_shell_command",
      "description": "Run an arbitrary shell command inside the configured sandbox.",
      "input_schema": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] }
    }
  ],
  "subagent_tools": [
    {
      "name": "launch_subagent",
      "description": "Launch a CLI subagent (claude, codex) to work on a task in the background.",
      "input_schema": { "type": "object", "properties": { "harness": { "type": "string" }, "model": { "type": "string" }, "prompt": { "type": "string" } }, "required": ["harness", "model", "prompt"] },
      "image": "bae-subagents:latest",
      "subagents": [
        { "harness": "claude", "command_template": "claude --model {model} --print", "prompt_via": "stdin", "timeout_secs": 600 },
        { "harness": "codex", "command_template": "codex exec --model {model}", "prompt_via": "stdin", "timeout_secs": 600 }
      ]
    }
  ]
}
```

- `client_version` — optional.
- `tools` — optional, default `[]`. These are the **client-side tools** the
  harness can execute. Every declared tool name must be in the profile's
  `allowed_tools`; an empty `allowed_tools` on the profile means no tools are
  allowed. `description` and `input_schema` are optional per tool.
- `sandbox_tools` — optional, default `[]`. **Auto-mode** sandbox tool
  declarations (see [Sandboxes guide — Auto vs. manual remote
  dispatch](../guides/03-sandboxes.md#auto-vs-manual-remote-dispatch)): the
  server dispatches these directly against the session's remote sandbox
  inside `run_turn`, without ever pausing the loop or involving the client.
  Stored per-client, sibling to (never merged with) `tools`. **Not** validated
  against the profile's `allowed_tools` — that check governs client-dispatched
  tools only; the sandbox trust boundary is `available_sandboxes`, enforced
  at [`session.startRemoteSandbox`](#sessionstartremotesandbox) time. Each
  entry's `input_schema` must require a string `command` property (the server
  execs `input.command`). Omit the key entirely when no Auto-mode tool is
  registered — this keeps a pre-work-item-0006 session-open body
  byte-identical.
- `subagent_tools` — optional, default `[]`. Remote-launch declarations for
  `launch_subagent`; each includes an image and one or more configured
  `{harness, command_template, prompt_via, timeout_secs}` entries. These are
  stored per client and are not checked against `allowed_tools`; the image is
  checked against `available_sandboxes` when the remote launch is dispatched.
  The provider receives only the tool name, description, and input schema.
  Invalid declarations are rejected with `422 invalid_subagent_tools`.

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
  ],
  "subagent_tools": []
}
```

`tools` are validated against the **shared** profile's `allowed_tools`,
exactly like `create`. A joining client declares its own, independent tool
set — joining never merges with, replaces, or reads any other client's
declared tools. See [Message Types — `session.join`](04-message-types.md#sessionjoin).

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

See [Multi-Client Sessions](../guides/07-multi-client-sessions.md) for a
worked example and [Wire Protocol — FIFO turn ownership](01-wire-protocol.md#fifo-turn-ownership-and-driver-registration)
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

See [04-message-types.md](04-message-types.md) for the full `event_type` catalog and
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

See [Wire Protocol](01-wire-protocol.md) for the envelope format, framing rules,
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

The eleven supported `method` values are `session.registerDriver`,
`session.sendMessage`, `session.subscribe`, `session.unsubscribe`,
`session.startRemoteSandbox`, `session.stopRemoteSandbox`,
`session.execRemoteSandbox`, `session.reportLocalSandbox` (the last four are
documented in [Sandboxes](#sandboxes) below; see the
[Sandboxes guide](../guides/03-sandboxes.md) for a walkthrough),
`session.reportLocalSubagent`, `session.cancelSubagent`, and
`session.updateClientTools` (see [Subagents](#subagents)).

---

### `session.registerDriver`

Register the calling connection's client key as a **driver** on this session
— required once before that client key's first `session.sendMessage` call.
SDK harnesses call this automatically as part of `connect()`/`join()`;
application code normally never calls it directly. See
[Wire Protocol — FIFO turn ownership](01-wire-protocol.md#fifo-turn-ownership-and-driver-registration)
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

The loop pauses (`Outcome::Paused`) whenever the assistant response contains
at least one `dispatch:"client"` tool_use block. The terminal result
`message.content` carries **every** `tool_use` block from that turn — client,
`sandbox`, and `mcp` alike — each tagged with its `dispatch` (see [Content
blocks](#content-blocks) below):

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "message": {
      "role": "assistant",
      "content": [
        {"type": "tool_use", "id": "tu_abc123", "name": "get_current_time", "input": {}, "dispatch": "client"},
        {"type": "tool_use", "id": "tu_xyz789", "name": "list_directory", "input": {"path": "/data"}, "dispatch": "mcp"}
      ]
    },
    "events": [ … ]
  }
}
```

The `mcp`/remote-subagent block above was already dispatched and answered by the server
*before* the turn paused — its `mcp.request`/`mcp.response`/`tool.result`
events are already present in `result.events`. The client's job:

- **Execute only `dispatch:"client"` blocks.** For each one, call the
  registered handler by `name` with `input` and build a `tool_result` block
  echoing `tool_use_id`.
- **Treat every other block as informational.** A `sandbox`/`mcp`/remote-subagent block (or,
  against an older server that omits `dispatch`, any block whose `name` is
  not in the client's own registered-tool set) is display-only — surface it
  to application code/UI if useful (e.g. "server is running `list_directory`"),
  but do not execute it and do not synthesize a `tool_result` for it. The
  server already owns that result.
- **Return only the client's own results.** Send back a `user` message whose
  `content` is exactly the `tool_result` blocks for the blocks the client
  executed — nothing for the server-dispatched ones:

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

**Absent-dispatch fallback.** A server that predates this contract never sets
`dispatch` and never sends server-dispatched blocks to the client at all — its
`server.message.send`/terminal `message.content` only ever contains blocks the
harness itself declared. A harness talking to such a server falls back to its
old behavior: treat a `tool_use` block as its own iff `name` is in its own
registered-tool set.

**Server-side merge.** The server dispatched and answered the `sandbox`/`mcp`/remote-subagent
blocks itself before pausing, and stashes those results across the pause.
When the client resumes with its own `tool_result`s, the server merges both
result sets into the single following `user` turn recorded in history — one
`tool_result` per `tool_use` id in the paused assistant turn, server results
first-class. If the client mistakenly returns a `tool_result` for a
server-dispatched id, the client's copy is dropped in favor of the server's.
A resume that doesn't answer exactly the paused turn's id set (missing,
duplicate, or unexpected id) is rejected with a `session.error`
(`reason: "tool_result_merge_invalid"`) and a `-32000` JSON-RPC error — the
session moves to `error`, so its incomplete durable tool exchange can never be
replayed upstream. A plain user message is instead an explicit abandonment:
the server synthesizes error results for unanswered client ids, preserves the
plain content, and keeps the session open. See [Wire Protocol — FIFO
turn ownership](01-wire-protocol.md#fifo-turn-ownership-and-driver-registration)
for how the pause/resume gate itself works.

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
message. See [Wire Protocol — FIFO turn ownership](01-wire-protocol.md#fifo-turn-ownership-and-driver-registration)
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
  [Wire Protocol](01-wire-protocol.md#lagged-subscriber)).

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

## Sandboxes

The four methods below implement the remote-sandbox lifecycle and
client-originated local-sandbox telemetry described in the
[Sandboxes guide](../guides/03-sandboxes.md). All four require prior driver
registration (`session.registerDriver`), the same `-32001` gate
`session.sendMessage` uses.

### `session.startRemoteSandbox`

Ask the server to start this session's one remote sandbox from an image in
the session's own profile's `available_sandboxes`.

**Params:**

```json
{ "image": "python:3.12" }
```

- `image` — required, non-empty string.

**Terminal result:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "sandbox_id": "…",
    "image": "python:3.12",
    "started_at": "2026-07-06T18:26:10.000Z"
  }
}
```

`started_at` is the `session.sandbox.running` event's `created_at`, or `null`
if that log write itself failed.

**Events:** `session.sandbox.start` (`{"image", "dispatch":"remote"}`), then
either `session.sandbox.running` (`{"image", "sandbox_id", "dispatch":"remote"}`)
or `session.sandbox.error` (`{"image", "phase":"start", "detail", "dispatch":"remote"}`).

**JSON-RPC errors:**
- `-32001` — caller is not a registered driver.
- `-32602` — missing or blank `image`.
- `-32000` — session not open, the profile was deleted, or **a sandbox is
  already running for this session** (one sandbox per session — see
  [Sandboxes guide — Session-wide, not per-driver](../guides/03-sandboxes.md#session-wide-not-per-driver)).
- `-32011 sandbox_image_not_allowed` — `image` is not in **this session's
  own** profile's `available_sandboxes`, including an image declared only on
  a *different* profile. No container is started.
- `-32012 sandbox_start_failed` — the driver's `ensure_image`/`start` call
  failed (a `session.sandbox.error` event, phase `start`, carries the
  detail).

---

### `session.stopRemoteSandbox`

Stop this session's one remote sandbox.

**Params:** `{}`

**Terminal result:**

```json
{ "jsonrpc": "2.0", "id": 4, "result": { "stopped": true, "image": "python:3.12", "sandbox_id": "…" } }
```

**Events:** `session.sandbox.stop` (`{"image", "sandbox_id", "reason":"explicit", "dispatch":"remote"}`),
then `session.sandbox.stopped` (same shape) on success, or
`session.sandbox.error` (`phase:"stop"`) on failure. The handle is removed
from server state **before** the driver call, so a failed stop never leaves
a phantom sandbox other calls could still dispatch against.

**JSON-RPC errors:**
- `-32001` — caller is not a registered driver.
- `-32013 sandbox_not_running` — no sandbox is currently running for this
  session.
- `-32000` — `"sandbox stop failed: <detail>"` when the driver's `stop` call
  itself errors. This is a generic code (there is no dedicated slug for a
  stop failure) — the authoritative signal is the `session.sandbox.error`
  event; the handle is removed either way, so this response can be treated
  as "the sandbox is gone" regardless.

A session close ([`DELETE /api/v1/sessions/{id}`](#delete-apiv1sessionsid--close-a-session))
triggers the identical stop sequence for any still-running remote sandbox,
with `"reason": "session_close"` instead of `"explicit"`.

---

### `session.execRemoteSandbox`

Run one shell command in the session's already-started remote sandbox and
return the raw result. This is a **manual-dispatch utility call**, not part
of the turn loop — see [Sandboxes guide — Auto vs. manual remote
dispatch](../guides/03-sandboxes.md#auto-vs-manual-remote-dispatch). The caller
(the client harness) builds its own `tool_result` from the response and
sends it via an ordinary `session.sendMessage` continuation.

**Params:**

```json
{ "command": "python --version" }
```

**Terminal result:**

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": { "stdout": "Python 3.12.3\n", "stderr": "", "exit_code": 0 }
}
```

A non-zero `exit_code` is still a **successful** RPC result, not an error —
the command ran and returned a result, whatever that result was.

**Events (failure only):** `session.sandbox.error` (`{"image", "sandbox_id",
"phase":"exec", "detail", "dispatch":"remote"}`). There is no lifecycle
event on success — this is a utility call, not a turn.

**JSON-RPC errors:**
- `-32001` — caller is not a registered driver.
- `-32602` — missing `command`.
- `-32013 sandbox_not_running` — no remote sandbox is running for this
  session; call `session.startRemoteSandbox` first.
- `-32000` — `"sandbox exec failed: <detail>"` when the driver's `exec` call
  itself errors.

---

### `session.reportLocalSandbox`

Report a lifecycle transition for a **local** sandbox — one the calling
client harness started against its own Docker/Apple Containers engine,
invisible to the server otherwise. Every SDK's builtin sandbox tools call
this automatically; see [Sandboxes guide — Local sandboxes report their own
lifecycle](../guides/03-sandboxes.md#local-sandboxes-report-their-own-lifecycle).

**Params:**

```json
{
  "state": "running",
  "image": "python:3.12",
  "container_id": "…",
  "detail": null
}
```

- `state` — required, one of `"running"`, `"stopped"`, `"error"`.
- `image` — required string.
- `container_id` — optional, `null` if not applicable.
- `detail` — optional, `null` unless `state` is `"error"`.

**Terminal result:**

```json
{ "jsonrpc": "2.0", "id": 6, "result": { "reported": true } }
```

**Events:** `state` maps to `session.sandbox.running`/`stopped`/`error`,
payload `{"dispatch":"local", "image", "container_id", "detail"}`, attributed
to the caller's `client_key_id`.

- **No `available_sandboxes` validation is performed** — an arbitrary,
  unregistered image name is accepted and logged as-is. This is deliberate:
  a local sandbox is the harness developer's own local trust decision, never
  a server-governed resource. This method can also never forge a **remote**
  lifecycle event — there is no `"scope"` parameter — the remote lifecycle
  stays exclusively server-authored via the three methods above.
- Any registered driver may call this — it does not need to be the current
  turn's owner, since local sandbox lifecycle is orthogonal to turn
  ownership.

**JSON-RPC errors:**
- `-32001` — caller is not a registered driver.
- `-32602` — invalid `state`, or missing `image`.

---

## Subagents

These methods support the native CLI-subagent tools described in the
[Subagents guide](../guides/05-subagents.md). They use the same JSON-RPC/NDJSON
transport and driver-registration gate as the sandbox methods above. A
subagent launch is asynchronous: the launch tool returns a `started`
acknowledgment, while a status tool returns the eventual output.

Remote subagent declarations are sent as `subagent_tools` alongside `tools`
and `sandbox_tools` when opening or joining a session. The declaration carries
the pinned `launch_subagent` schema plus the configured CLI command templates;
only its name, description, and input schema are exposed to the provider.
Local launches are ordinary client tools, and their automatically managed
`local_subagent_status` declaration is synchronized with
`session.updateClientTools`. SDKs serialize the tracked-task transition with
this full replacement, so concurrent launches cannot exceed the local cap and
an older removal cannot overwrite a newer addition.

All three methods require prior driver registration. Any registered driver may
call them; turn ownership is not required.

### `session.reportLocalSubagent`

Report a lifecycle transition for a **local** subagent, one whose subprocess is
owned by the client harness. SDK local-subagent tools call this automatically.
The server records the report as visibility telemetry and does not verify that
the claimed process exists or has reached the claimed state.

**Params:**

```json
{
  "state": "start",
  "subagent_id": "sba_…",
  "harness": "claude",
  "model": "claude-sonnet-5",
  "detail": null,
  "reason": null,
  "exit_code": null
}
```

- `state` — required; one of `start`, `running`, `completed`, `failed`, or
  `cancelled`.
- `subagent_id`, `harness`, `model` — required non-empty strings.
- `detail`, `reason`, and `exit_code` — optional. `reason` is normally
  `nonzero_exit`, `spawn_failed`, or `timeout` for `failed`, and `explicit` or
  `session_close` for `cancelled`; the server echoes it without validating the
  enum.

**Terminal result:**

```json
{ "jsonrpc": "2.0", "id": 7, "result": { "reported": true } }
```

**Events:** `state` maps to the corresponding
`session.subagent.start`/`running`/`completed`/`failed`/`cancelled` event.
Every payload contains `dispatch: "local"`, `subagent_id`, `harness`,
`model`, and `detail`; terminal events add the applicable `reason` and
`exit_code` fields. A local timeout is reported as `failed` with
`reason: "timeout"`.

**JSON-RPC errors:** `-32001` (unregistered driver), `-32602` (invalid or
missing fields), and `-32603` (internal error).

> **Telemetry limitation:** local reports are not authoritative. If the
> harness crashes or disconnects before reporting a terminal state, the server
> has no process handle and cannot reconcile the missing event.

### `session.cancelSubagent`

Cancel one **remote**, server-tracked subagent. A local subagent is cancelled
in the SDK (`Session::cancel_subagent`); the server does not track its id.

**Params:**

```json
{ "subagent_id": "sba_…" }
```

**Terminal result:**

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "result": { "cancelled": true, "subagent_id": "sba_…", "was_running": true }
}
```

**Events:** For a running task, the server kills the subprocess, retains the terminal
entry for the status tool, and emits `session.subagent.cancelled` with
`dispatch: "remote"` and `reason: "explicit"`. Cancelling an already
terminal task is an idempotent success with `was_running: false` and no new
event. An unknown, local, or already-evicted id returns `-32014
subagent_not_found`.

**JSON-RPC errors:** `-32001` (unregistered driver), `-32602` (invalid
`subagent_id`), `-32014` (not tracked), and `-32603` (internal error).

### `session.updateClientTools`

> **General-purpose wire-protocol surface:** this method is not specific to
> subagents. It replaces the calling client's complete `client_tools` entry
> and is reusable for any future feature that needs a dynamic client tool
> list. Subagents use it to add or remove `local_subagent_status`.

Update the calling client's tool declarations for the next provider call.
The array is a **full replacement**, not a merge or a diff; other clients'
tools and the session's `sandbox_tools`/`subagent_tools` are untouched.

**Params:**

```json
{
  "tools": [
    {
      "name": "get_current_time",
      "description": "Return the current UTC time as a string",
      "input_schema": { "type": "object", "properties": {} }
    }
  ]
}
```

`tools` is required and may be empty. Each tool requires a non-empty `name`;
`description` and `input_schema` are optional, using the same `ClientToolDef`
shape as session open/join. Every name is checked against the profile's
`allowed_tools`; `remote_subagent_status` is reserved and rejected. In
particular, a profile used for local subagents must allowlist both
`launch_subagent` and `local_subagent_status`.

**Terminal result:**

```json
{ "jsonrpc": "2.0", "id": 9, "result": { "updated": true } }
```

The update applies to the **next** provider call. A call racing an in-flight
turn does not rewrite the tool list already sent to that provider.
SDK-managed subagent updates are ordered with their local task-set mutations;
the server therefore receives full replacements in current-state order even
when a status eviction and a new launch happen concurrently.

**Events:** No subagent lifecycle event is emitted. This method updates the
calling client's persisted tool list for subsequent provider calls.

**JSON-RPC errors:** `-32001` (unregistered driver), `-32000` (session is not
open), `-32602` (invalid params), `-32015 tool_not_allowed` (profile
allowlist or reserved name), and `-32603` (internal error).

## Content blocks

`content` on a message can be either a plain string or an array of typed
blocks:

```json
{"type": "text",        "text": "…"}
{"type": "tool_use",    "id": "tu_…", "name": "…", "input": {…}, "dispatch": "client"}
{"type": "tool_result", "tool_use_id": "tu_…", "content": <string|block[]>}
```

The server passes these through to/from the provider verbatim, except that
`dispatch` (and the reserved `caller` field) are stripped from `tool_use`
blocks before the provider ever sees them — see [Message Types —
`server.message.send`](04-message-types.md#servermessagesend).

A `tool_use` block in a `server.message.send` event carries `dispatch`, one of
`"client"`, `"sandbox"`, `"mcp"`, or `"subagent"`, whenever the turn paused for at least one
client-dispatched tool (see [Tool call response](#sessionsendmessage) above).
Older servers that predate this field omit it; a harness talking to such a
server falls back to treating a block as its own iff the block's `name` is in
its own registered-tool set.

---

## The harness tool-call loop

SDK harnesses implement this loop inside `session.send(message)`:

1. POST `session.sendMessage` to `/rpc` with `{message:{role:"user", content}}`.
2. Read NDJSON: fire `on_event` for each notification; await terminal result.
3. If `result.message.content` contains **no** block that is "ours" (see
   below) → return the final assistant turn to the caller. **Loop ends.**
4. For each `tool_use` block that is ours, call the registered handler by
   `name` with `input`. Build a `tool_result` block echoing `tool_use_id`.
   Every other `tool_use` block is informational only — skip it, do not
   synthesize a `tool_result` for it.
5. POST `session.sendMessage` with
   `{message:{role:"user", content:[…tool_result blocks for "ours" only]}}`.
6. Go to step 2.

**Deciding "ours":** a block is ours iff `dispatch == "client"`, or, against a
server that predates the `dispatch` field, `name` is in the harness's own
registered-tool set. Tools the client did not declare are dispatched
server-side (through configured MCP servers, the session's Auto-mode sandbox,
or a remote subagent) — against a current server they still surface as
`tool_use` blocks (tagged `dispatch:"sandbox"`/`"mcp"`/`"subagent"`) so the full turn is visible, but the
harness must not execute or answer them; against an older server they never
surface at all. See [`session.sendMessage` — tool call
response](#sessionsendmessage) for the full contract.

Notes:
- `tool_use.id` must be echoed verbatim as `tool_result.tool_use_id`.
- Hooks (`before_send`, `after_receive`, `before_tool_call`, `after_tool_call`,
  `on_event`) fire at their respective points; an error from any hook aborts
  the loop.
