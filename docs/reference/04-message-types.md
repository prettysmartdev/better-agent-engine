# Message Types

Every row in `session_events` has an `event_type` field drawn from the closed
set below. Adding a new event type requires a code change in the server and all
SDKs — the enum is exhaustive in every language so unhandled variants are
compile or type errors.

Events are returned in the `events` array on the terminal result of
`session.sendMessage` (events appended during that call) and via
`GET /api/v1/sessions/{id}/events` (full session history). They are also
delivered as live `session.event` notifications on the `/rpc` NDJSON stream —
see [Event Streaming](../guides/06-event-streaming.md).

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

**Mixed and all-client turns:** when the turn contains at least one
`dispatch:"client"` tool, every `tool_use` block in this event's `content` —
client-, sandbox-, MCP-, and subagent-dispatched alike — carries a `dispatch` field, the
same value as that block's [`tool.call`](#toolcall) event below:

```json
{
  "role": "assistant",
  "content": [
    {"type": "tool_use", "id": "tu_abc123", "name": "get_current_time", "input": {}, "dispatch": "client"},
    {"type": "tool_use", "id": "tu_xyz789", "name": "list_directory", "input": {"path": "/data"}, "dispatch": "mcp"}
  ]
}
```

The `sandbox`/`mcp`/remote-subagent blocks above have already been dispatched and answered by
the server by the time this event is emitted; only the `client` block is left
for the harness to execute. See [Client API — tool call
response](00-client-api.md#sessionsendmessage) for the full client contract.
`dispatch` (and any future `caller` field) is a baesrv-internal routing tag —
`engine::provider::call` strips it from `tool_use` blocks before replaying
history to the LLM, so it never reaches the provider (see
[`provider.request`](#providerrequest) below) — a well-behaved client does not
echo it back either. An all-server turn (no client tool involved) never
pauses, so its `server.message.send` content has no `dispatch` field.

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
[Configuration — `[providers]`](05-configuration.md#providers) for the
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

**Sandbox dispatch (Auto-mode):**

```json
{
  "id":          "tu_def456",
  "name":        "run_shell_command",
  "input":       {"command": "python --version"},
  "dispatch":    "sandbox",
  "server_name": null
}
```

**Subagent dispatch:**

```json
{
  "id":        "tu_sub789",
  "name":      "launch_subagent",
  "input":     {"harness": "claude", "model": "claude-sonnet-5", "prompt": "…"},
  "dispatch":  "subagent",
  "server_name": null
}
```

- `dispatch` is `"client"` for tools declared at session open, `"mcp"` for
  tools handled server-side by a configured MCP server, and `"sandbox"` for
  Auto-mode sandbox tools declared in the session's `sandbox_tools` array and
  dispatched server-side against the session's remote sandbox — see
  [Sandboxes — Auto vs. manual remote dispatch](../guides/03-sandboxes.md#auto-vs-manual-remote-dispatch).
  `"subagent"` is used for server-dispatched remote subagents and their
  synthesized status tool. A local subagent launch is an ordinary `"client"`
  dispatch.
- `server_name` is the MCP server's name from `bae-config.toml` for `"mcp"`
  dispatch, or `null` for `"client"`/`"sandbox"`/`"subagent"` dispatch, or if the tool name
  was not found in any server's tool list (indicates a mis-routed call).

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

**Sandbox result (Auto-mode):**

```json
{
  "tool_use_id": "tu_def456",
  "dispatch":    "sandbox",
  "is_error":    false,
  "content":     [{"type": "text", "text": "Python 3.12.3\n"}]
}
```

**Subagent result (remote launch or status):**

```json
{
  "tool_use_id": "tu_sub789",
  "dispatch":    "subagent",
  "server_name": null,
  "is_error":    false,
  "content":     [
    {"type": "text", "text": "{\"subagent_id\":\"sba_…\",\"harness\":\"claude\",\"model\":\"claude-sonnet-5\",\"status\":\"started\"}"}
  ]
}
```

- `content` mirrors the `tool_result` block the provider receives. For a
  sandbox result, it is rendered from the exec result as stdout, then
  `\n[stderr]\n<stderr>` if stderr is non-empty, then `\n[exit_code: N]` if
  the exit code is non-zero.
- `is_error: true` means the MCP or sandbox call failed (or, for sandbox, the
  command exited non-zero) or returned an error; the session continues and
  the provider receives the error content so it can adjust.
- A sandbox-dispatch call with no remote sandbox currently started for the
  session reuses the exact same error-tool-result shape as an MCP call with
  no configured server: `[{"type":"text","text":"sandbox error: no remote
  sandbox is running for tool '<name>'; call session.startRemoteSandbox
  first"}]`.

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

### `sandbox.request`

An Auto-dispatch sandbox tool call about to run — one per `tool_use`, in
`run_turn`. Deliberately **unprefixed**, mirroring `mcp.request`/
`mcp.response` (the `session.sandbox.*` prefix is reserved for lifecycle
state transitions, not per-call dispatch — see
[Sandboxes](../guides/03-sandboxes.md#auto-vs-manual-remote-dispatch)).

```json
{
  "tool":    "run_shell_command",
  "input":   {"command": "python --version"},
  "command": "python --version"
}
```

- `command` is `input.command`, or `null` if the tool's input has no string
  `command` field (a misconfigured Auto-mode tool declaration).

---

### `sandbox.response`

The result of an Auto-dispatch sandbox tool call.

**Success:**

```json
{
  "sandbox_id": "…",
  "ok":         true,
  "result":     {"stdout": "Python 3.12.3\n", "stderr": "", "exit_code": 0}
}
```

**Driver error:**

```json
{ "sandbox_id": "…", "ok": false, "error": "exec failed: …" }
```

**No sandbox started / missing `command`:**

```json
{ "sandbox_id": null, "ok": false, "error": "no remote sandbox is running for tool 'run_shell_command'; call session.startRemoteSandbox first" }
```

- A non-zero exit code sets `ok: false` and the corresponding `tool.result`'s
  `is_error: true` — the same posture as an MCP tool call: a non-zero exit is
  a tool-level error the model sees and can react to, not a transport-level
  RPC error.

---

### `session.subagent.start`

A configured CLI-subagent launch was validated and accepted. The event is
emitted before the background subprocess is started.

```json
{
  "dispatch": "remote",
  "subagent_id": "sba_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "harness": "claude",
  "model": "claude-sonnet-5",
  "detail": null
}
```

`dispatch` is `remote` for a server-launched subagent and `local` for a
client-harness report. Validation failures produce an error-shaped tool result
and no subagent lifecycle events.

---

### `session.subagent.running`

The launch was handed to the background task and the subprocess was spawned.
Remote launches emit this event synchronously before returning the
`{"status":"started"}` tool result; local SDKs report it after their own
spawn. SDKs hold any immediately produced terminal report until this running
report completes, so a fast process cannot produce `completed` or `failed`
before `running`.

```json
{
  "dispatch": "remote",
  "subagent_id": "sba_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "harness": "claude",
  "model": "claude-sonnet-5",
  "detail": null
}
```

---

### `session.subagent.completed`

The subagent exited successfully with exit code zero.

```json
{
  "dispatch": "remote",
  "subagent_id": "sba_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "harness": "claude",
  "model": "claude-sonnet-5",
  "detail": null,
  "exit_code": 0
}
```

Captured stdout/stderr is returned by the status tool, not copied into the
lifecycle event. Remote terminal events may be appended after the turn that
launched the subagent has already completed.

---

### `session.subagent.failed`

The subprocess failed, exited non-zero, or exceeded its timeout.

```json
{
  "dispatch": "remote",
  "subagent_id": "sba_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "harness": "claude",
  "model": "claude-sonnet-5",
  "detail": "command not found",
  "reason": "spawn_failed",
  "exit_code": null
}
```

`reason` is `nonzero_exit`, `spawn_failed`, or `timeout`. A timed-out
subagent is exposed as `timed_out` by the status tool but uses this event with
`reason: "timeout"`.

---

### `session.subagent.cancelled`

The subagent was killed by explicit cancellation or session teardown.

```json
{
  "dispatch": "remote",
  "subagent_id": "sba_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "harness": "claude",
  "model": "claude-sonnet-5",
  "detail": null,
  "reason": "explicit",
  "exit_code": null
}
```

`reason` is `explicit` for `session.cancelSubagent` (or SDK cancellation)
and `session_close` when the session is closed while the task is running.

---

### `session.open`

Emitted when the session is created.

```json
{
  "client_version": "1.0.0",
  "tools":          ["get_current_time"],
  "sandbox_tools":  ["run_shell_command"],
  "subagent_tools": ["launch_subagent"]
}
```

- `client_version` is `null` if not provided at session creation.
- `tools` is the list of tool names declared at open (client-side tools only).
- `sandbox_tools` is the list of Auto-mode sandbox tool names declared at
  open (see [Client API — `sandbox_tools`](00-client-api.md#post-apiv1sessions--open-a-session)
  and [Sandboxes](../guides/03-sandboxes.md#auto-vs-manual-remote-dispatch)) —
  empty when none were registered.
- `subagent_tools` is the list of remote-launch tool names declared at open;
  empty when none were registered. Local `launch_subagent` tools are listed
  under `tools`, while their `local_subagent_status` tool is dynamic and is
  not listed here.

---

### `session.join`

Emitted when a second (or further) client key mints a session key for an
existing session via `POST /api/v1/sessions/{id}/join`. Same payload shape as
`session.open` — it's the identical "a client key attached, declaring this
tool set" fact, just via the join path instead of create.

```json
{
  "client_version": "1.2.0",
  "tools":          ["only_b"],
  "sandbox_tools":  [],
  "subagent_tools": ["launch_subagent"]
}
```

- `client_version` is `null` if not provided at join.
- `tools`/`sandbox_tools` are the **joining client's own** declared tool
  lists — never merged with the creator's or any other joiner's.
- The event's `client_key_id` column is the **joiner**, not the session's
  original creator.
- See [Client API — `POST .../join`](00-client-api.md#post-apiv1sessionsidjoin--join-an-existing-session)
  and [Multi-Client Sessions](../guides/07-multi-client-sessions.md).

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
- See [Client API — `session.registerDriver`](00-client-api.md#sessionregisterdriver)
  and [Wire Protocol — FIFO turn ownership](01-wire-protocol.md#fifo-turn-ownership-and-driver-registration).

---

### `session.sandbox.available`

Emitted immediately after `session.driver.register`, when the registering
client key's session's own profile has a non-empty `available_sandboxes`. A
profile with an empty `available_sandboxes` emits no such event.

```json
{
  "images": [
    {"name": "python:3.12", "status": "available"},
    {"name": "node:22", "status": "error", "detail": "pull failed: unauthorized"}
  ]
}
```

- `status` is one of `"pending"`, `"available"`, `"error"`.
- `detail` is present only when `status` is `"error"`.
- Built by iterating **this session's own profile's** `available_sandboxes`
  list only — never a flattened, cross-profile view, even though the
  server's image-status tracking internally covers every profile. See
  [Sandboxes — The profile-scoping guarantee](../guides/03-sandboxes.md#the-profile-scoping-guarantee).

---

### `session.sandbox.start` / `session.sandbox.running`

Emitted by [`session.startRemoteSandbox`](00-client-api.md#sessionstartremotesandbox):
`start` when the request is accepted and the image validated, `running` once
the driver's `start` call actually succeeds.

```json
{ "image": "python:3.12", "dispatch": "remote" }
```

```json
{ "image": "python:3.12", "sandbox_id": "…", "dispatch": "remote" }
```

`session.sandbox.running` can also originate from a **local** sandbox via
`session.reportLocalSandbox` (below), distinguished by `"dispatch": "local"`:

```json
{ "image": "python:3.12", "container_id": "…", "detail": null, "dispatch": "local" }
```

---

### `session.sandbox.stop` / `session.sandbox.stopped`

Emitted by [`session.stopRemoteSandbox`](00-client-api.md#sessionstopremotesandbox),
or automatically at session close for a still-running remote sandbox.

```json
{ "image": "python:3.12", "sandbox_id": "…", "reason": "explicit", "dispatch": "remote" }
```

| `reason` | When |
|---|---|
| `"explicit"` | Client called `session.stopRemoteSandbox`. |
| `"session_close"` | The session closed while a remote sandbox was still running. |

`session.sandbox.stopped` mirrors the same shape on success. Like
`running`, `stopped` can also originate from a local sandbox
(`session.reportLocalSandbox`, `"dispatch": "local"`):

```json
{ "dispatch": "local", "image": "python:3.12", "container_id": "…", "detail": null }
```

---

### `session.sandbox.error`

Emitted whenever a sandbox driver call — remote or client-reported-local —
fails, at any lifecycle phase.

**Remote** (`phase` present; no `sandbox_id` for a phase-`start` failure,
since no handle was ever retained):

```json
{ "image": "python:3.12", "phase": "start", "detail": "…", "dispatch": "remote" }
```

```json
{ "image": "python:3.12", "sandbox_id": "…", "phase": "exec", "detail": "…", "dispatch": "remote" }
```

`phase` is one of `"start"`, `"stop"`, `"exec"`.

**Local** (via `session.reportLocalSandbox`; no `phase` field):

```json
{ "dispatch": "local", "image": "python:3.12", "container_id": "…", "detail": "…" }
```

> **`session.sandbox.running`/`stopped`/`error` with `"dispatch": "local"`
> are self-reported client telemetry, not something the server has
> verified** — the server cannot confirm a client's claim about its own
> local container. Contrast with `"dispatch": "remote"`, which the server
> itself authored by actually driving the underlying container lifecycle.
> See [Sandboxes — Local sandboxes report their own
> lifecycle](../guides/03-sandboxes.md#local-sandboxes-report-their-own-lifecycle)
> for the full trust-boundary discussion, including the accepted gap where a
> crashed client leaves a local sandbox with no terminal `stopped`/`error`
> event.

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
| `"tool_result_merge_invalid"` | A paused-turn continuation had a non-user role or did not answer exactly the assistant turn's tool-use ids (missing, duplicate, or unexpected result). Payload includes a human-readable `detail`. | yes — prevents incomplete durable tool history from being replayed upstream |

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

**Mixed client + MCP tool call (two `session.sendMessage` calls — the server
dispatches its own block before pausing, then merges both result sets on
resume):**

Call 1:
```
client.message.send
provider.request
provider.response      (ok: true)
tool.call              (dispatch: mcp, server_name: "filesystem")
mcp.request            (method: tools/call)
mcp.response           (ok: true)
tool.result            (dispatch: mcp, is_error: false)
tool.call              (dispatch: client)
server.message.send    (content has both tool_use blocks, each tagged
                         `dispatch` — loop paused; the mcp block's
                         tool.result is already logged above)
```

Call 2:
```
client.message.send    (content has a tool_result for the client id only —
                         the server merges in its own stashed mcp result to
                         answer both ids before this is recorded)
provider.request
provider.response      (ok: true)
server.message.send    (final text)
```

Note there is no second `tool.result` event for the `mcp` id on resume — it
was already logged in call 1, and the merge does not re-log it. See [Client
API — tool call response](00-client-api.md#sessionsendmessage) for the full
client contract on a mixed turn.

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

**Remote sandbox: start, Auto-dispatch tool call, stop (single
`session.sendMessage` call for the tool round-trip, server-side):**

```
session.registerDriver                              -- session.driver.register, then session.sandbox.available
session.startRemoteSandbox                          -- session.sandbox.start, session.sandbox.running
client.message.send
provider.request
provider.response      (ok: true)
tool.call              (dispatch: sandbox)
sandbox.request        (tool: run_shell_command)
sandbox.response       (ok: true)
tool.result            (dispatch: sandbox, is_error: false)
provider.request
provider.response      (ok: true)
server.message.send
session.stopRemoteSandbox                           -- session.sandbox.stop, session.sandbox.stopped
```

See [Sandboxes](../guides/03-sandboxes.md) for the full lifecycle, the
auto/manual dispatch distinction, and local-sandbox telemetry via
`session.reportLocalSandbox`.

**Local subagent lifecycle (client-launched):**

```text
session.open                    (tools: launch_subagent; status absent)
client.message.send
provider.request
provider.response      (ok: true)
tool.call              (dispatch: client, name: launch_subagent)
session.subagent.start (dispatch: local)
session.subagent.running (dispatch: local)
server.message.send    (launch tool result: status "started"; turn pauses)
… background CLI runs in the client harness …
session.subagent.completed (dispatch: local, exit_code: 0)
client.message.send
provider.request       (local_subagent_status is now advertised)
tool.call              (dispatch: client, name: local_subagent_status)
tool.result            (dispatch: client, captured output)
server.message.send    (status acknowledged; status tool disappears next turn)
```

The local `start`/`running`/terminal events are client-reported telemetry. A
failure or timeout uses `session.subagent.failed`; cancellation uses
`session.subagent.cancelled`.

**Remote subagent lifecycle (server-launched):**

```text
client.message.send
provider.request
provider.response      (ok: true)
tool.call              (dispatch: subagent, name: launch_subagent)
session.subagent.start (dispatch: remote)
session.subagent.running (dispatch: remote)
tool.result            (dispatch: subagent, status "started")
provider.request       (same turn continues; no wait for the CLI)
provider.response
server.message.send
… detached CLI runs inside the session's remote sandbox …
session.subagent.completed (dispatch: remote, exit_code: 0)
client.message.send
provider.request       (remote_subagent_status is now advertised)
tool.call              (dispatch: subagent, name: remote_subagent_status)
tool.result            (dispatch: subagent, captured output; terminal entry acknowledged)
provider.response
server.message.send    (status tool disappears on the following turn)
```

The remote terminal event is produced by the detached task after the launch
turn has returned, so it can arrive independently of the launch turn's event
result. The terminal status response evicts that entry after acknowledging it.

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
produced it. See [Multi-Client Sessions](../guides/07-multi-client-sessions.md)
for the full walkthrough.
