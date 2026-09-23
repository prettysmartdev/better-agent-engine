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

**`role`** is `"assistant"` for every ordinary turn reply. It is `"user"` only
for the two kinds of **synthetic** server-written message below — a
`server.message.send` event is the only place a `"user"`-role message can
come from the server itself rather than the client.

**`synthetic`** is an optional string present only on those server-written
`"user"`-role messages. Its value names why the message exists; it is never
present on an ordinary reply, and it never reaches the provider (history
replay sends only `{role, content}` — see [`provider.request`](#providerrequest)
below). Two values exist today:

- `"compaction_preamble"` — the fixed-text message a compaction writes
  immediately before its summary, so the replayed history is a valid
  `user → assistant` sequence. See
  [`session.compaction.completed`](#sessioncompactioncompleted) below.
- `"abandoned_tool_results"` — written when `session.compact` reclaims a
  paused turn whose deadline has expired: one synthetic error `tool_result`
  per client tool-use id that was never answered (`"Client tool call was
  abandoned before returning a result."`, `is_error: true`), alongside any
  server-dispatched results already stashed for that turn. This keeps the
  persisted history a valid `assistant(tool_use) → user(tool_result…)`
  transcript before the compaction call runs.

A client or UI rendering a transcript should treat any `synthetic` message as
system-generated, not as something a user typed — SDKs and MAX pass the field
through untouched rather than dropping it.

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
  "tools":     [ … ],
  "normalized": { "prepended_user": true, "merged": [[3, 4]] }
}
```

- `attempt` is 0-indexed.
- `kind` is `"primary"` or `"fallback"`.
- `tools` includes both client-declared tools and any tools fetched from
  connected MCP servers.
- Inserted **before** each provider attempt (primary + every fallback).
- `messages` is always the **exact** canonical (Anthropic-shaped) list the
  provider call is made with — baesrv's internal-only `dispatch`/`caller`
  tool-use fields are stripped before this event is written, never after. For
  `provider: "anthropic"` it is byte-for-byte the `messages` of the request
  body. For `provider: "openai"` the wire body is the deterministic
  translation of this list into Chat Completions shape (`tool_use` blocks
  become `tool_calls`, `tool_result` blocks become `role: "tool"` messages, an
  empty `tools` list is omitted), so replaying it against the OpenAI API
  requires the same translation.
- `normalized` is present only when the Anthropic normalizer actually changed
  the message list before sending it (never for an OpenAI-kind provider,
  which is unchanged from history as stored). It records what changed, so the
  persisted log always shows the true provider input alongside a trace of any
  server-side transformation:
  - `"prepended_user": true` — the list's first message was not `"user"`, so
    the server prepended a fixed `user` message (the same preamble text used
    for compaction) to satisfy the Anthropic Messages API's "first message
    must be `user`" rule. This only happens replaying a session compacted
    before this normalizer existed (no `preamble_event_id` on its
    `session.compaction.completed`).
  - `"merged": [[i, j, …], …]` — one entry per run of consecutive same-role
    messages that had to be folded into one message, each inner array listing
    the 0-indexed positions (after any prepend) that were merged, in order.
  - Either key may be present alone, or both together; the field is omitted
    entirely when the normalizer changed nothing (stripping the internal
    fields alone does not count as a change).
- `purpose: "compaction"` is present (and equal to `"compaction"`) only on the
  request that runs a compaction summary, so it can be told apart from an
  ordinary turn's provider calls — see
  [`session.compaction.completed`](#sessioncompactioncompleted) below.

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
  "body":     { "role": "assistant", "stop_reason": "end_turn", "content": [ … ] },
  "usage":    { "input_tokens": 1200, "output_tokens": 200 }
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
- `usage` is `{"input_tokens", "output_tokens"}`, read from the provider's own
  raw response. For Anthropic, `input_tokens` is the **cache-inclusive**
  total: `usage.input_tokens + usage.cache_read_input_tokens +
  usage.cache_creation_input_tokens` (either cache field defaults to 0 when
  absent) — a request that hits the prompt cache still counts its full input
  toward `mode: "auto"`'s threshold. For OpenAI, it's
  `usage.prompt_tokens`/`usage.completion_tokens`, unchanged. `usage` is
  `null` when the provider omitted it entirely or reported only one of the
  two fields (a partial usage object is treated the same as no usage, never
  as a partial number). Only **successful** responses carry a `usage` member
  at all — failure payloads never gain one. This cache-inclusive total is the
  number `mode: auto` compares against a session's configured compaction
  `size`; see [`session.compaction.started`](#sessioncompactionstarted) below.
- `purpose: "compaction"` is present on both the success and failure shape
  above when this attempt belongs to a compaction call, not an ordinary turn
  — see [`provider.request`](#providerrequest) above. A compaction response
  carrying `purpose: "compaction"` is excluded from
  `last_provider_token_count` (the figure a `trigger: "client"`
  [`session.compaction.started`](#sessioncompactionstarted) reports), so a
  manual compact's own call never feeds back into the next manual compact's
  best-effort token estimate.

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
| `"compaction_store_failed"` | An automatic compaction attempt that ran after a completed turn failed while writing its record (a store error; its transaction rolled back, so the history is unchanged). The turn's own result is still returned normally, and the attempt backs off exactly like a failed summary (see the auto-compaction backoff). Payload includes a human-readable `detail`. | **no** — the session stays `open` |

Note: `"provider_call_failed"` is recorded once when the primary fails but
a fallback attempt follows. If a fallback succeeds, the session continues
normally. Only `"all_providers_failed"` moves the session to `error`.

When `"all_providers_failed"`, `session.sendMessage`'s terminal result still
carries this event in `result.events` — not a JSON-RPC error object.

---

### `session.compaction.started`

Emitted when a session's history is about to be compacted — either
automatically by the server (`compaction: {"mode":"auto"}`, once a completed
turn's reported usage crosses the configured `size`) or explicitly via the
[`session.compact`](00-client-api.md#sessioncompact) JSON-RPC method
(`mode: "client"`, or a manual call on a `mode: "auto"` session).

**Auto trigger:**

```json
{
  "trigger": "auto",
  "reason": "token_threshold",
  "token_count": 128001,
  "threshold_tokens": 128000
}
```

**Manual trigger (`session.compact`), with usable prior usage:**

```json
{
  "trigger": "client",
  "reason": "manual",
  "token_count": 42000,
  "threshold_tokens": null
}
```

**Manual trigger, no usable prior usage yet:**

```json
{
  "trigger": "client",
  "reason": "manual",
  "token_count": null,
  "threshold_tokens": null
}
```

**Auto trigger, retried after a prior auto attempt failed:**

```json
{
  "trigger": "auto",
  "reason": "retry_after_failure",
  "token_count": 140010,
  "threshold_tokens": 128000
}
```

- `trigger` is `"auto"` or `"client"`.
- `reason` is `"manual"` for any `session.compact` call — including one on a
  session configured for `mode: "auto"` — and, for an auto trigger, either
  `"token_threshold"` (the ordinary case) or `"retry_after_failure"`. The
  latter is emitted only for the first **automatic** attempt after a
  previous automatic attempt failed, once the backoff below has been
  satisfied; it never appears with `trigger: "client"` — a manual compact
  always ignores the backoff.
- `token_count`: for `trigger: "auto"`, the just-completed turn's
  `input_tokens + output_tokens` (cache-inclusive for Anthropic, see
  [`provider.response`](#providerresponse) above) that crossed the
  threshold. For `trigger: "client"`, a **best-effort** figure read from the
  session's most recently persisted `provider.response` event's `usage`
  (excluding a compaction call's own response, `purpose: "compaction"`), or
  `null` if that event has no usage or the session has made no provider
  calls yet. This never scans further back than that single newest response.
- `threshold_tokens` is the session's configured `size` for an auto trigger;
  always `null` for a manual trigger, since no auto config governs a
  `session.compact` call.

**Auto-compaction backoff (`reason: "retry_after_failure"`):** after an
automatic attempt fails, the server suppresses further automatic triggers for
that session until *both* of the following hold: at least one more turn has
completed, and a later trigger's `token_count` exceeds the failed attempt's
`token_count`. A successful automatic (or manual) compaction clears this
state; another automatic failure re-arms it at the new token count. This
state lives in server memory only — it is lost on restart, which simply means
the very next crossing after a restart retries immediately rather than
waiting out the backoff. A manual `session.compact` is never subject to it,
and a *successful* manual compact also clears it.

---

### `session.compaction.completed`

Emitted when the compaction provider call finishes and the compacted summary
has been recorded — preceded by its own synthetic preamble — as two
`server.message.send` events.

```json
{
  "preamble_event_id": "evt_…",
  "summary_event_id": "evt_…",
  "compacted_message_count": 17,
  "input_tokens": 42100,
  "summary_tokens": 900
}
```

- `preamble_event_id` — the id of the `server.message.send` event written
  immediately before the summary: a fixed-text, `role: "user"` message
  (`synthetic: "compaction_preamble"`, see [`server.message.send`](#servermessagesend)
  above) that makes the replayed history start `user(preamble) →
  assistant(summary) → …` — a sequence every provider accepts, unlike
  `assistant(summary) → …` on its own. It is **always present** on an event
  written by this version of the server. It is **absent** (omitted, or
  `null` depending on SDK type) only on a `session.compaction.completed`
  event written before this field existed; see "Compatibility" below.
- `summary_event_id` — the id of the `server.message.send` event immediately
  after the preamble, holding the compacted summary text itself. The summary
  text is **not** duplicated into this payload.
- `compacted_message_count` — the number of effective history messages that
  were summarized (the length of the pre-compaction history the server built
  the compaction request from); it excludes both the appended compaction
  instruction and the produced preamble and summary. After a prior
  compaction, that history already starts with that compaction's own
  preamble — there is no special-casing for a second or later compaction.
- `input_tokens` / `summary_tokens` — the compaction call's **own**
  provider-reported usage: `input_tokens` is that request's input usage
  (necessarily including the appended compaction instruction — the server has
  no way to subtract it out and does not attempt to), `summary_tokens` is the
  summary's `output_tokens`.
- `input_tokens` and `summary_tokens` are `null` when a provider returned a
  valid summary without usage accounting; the summary is still committed,
  because missing metrics must not discard a completed model result.

**Failure:** if the compaction's provider call fails outright (every
provider/fallback exhausted, or the primary provider's config doesn't
resolve), or the call succeeds but the response is unusable — truncated by
the provider's own output-token limit (`stop_reason: "max_tokens"` /
`finish_reason: "length"`) or has no non-whitespace text — the compaction is
treated as **failed**: no preamble, no summary, and no
`session.compaction.completed` event are written at all. Only the
`session.compaction.started` event (and the attempt's `provider.request`/
`provider.response` pair(s)) remain as an audit trail. The session's history
is unchanged and it remains compactable on the next attempt. A truncated or
empty response still logs its `provider.response` with `ok: true` (it was a
successful HTTP call) — the failure is a budget/content problem, not a
provider outage, so no further fallback is attempted for that call. See
[`session.compact`](00-client-api.md#sessioncompact) for the exact JSON-RPC
error each of these produces on a manual call, and [`session.compaction.started`](#sessioncompactionstarted)
above for the backoff a **failed automatic** attempt puts on later automatic
triggers.

The compaction call is given its own output-token budget rather than the
profile's ordinary turn `max_tokens`: the larger of the profile's configured
value and 4096.

**Atomicity:** the preamble, summary, and `completed` event are inserted in
one database transaction and broadcast to live watchers only after it
commits — a store error between them can never leave an orphaned preamble or
summary for a later `stream_history` call to pick up.

**Compatibility:** a session compacted before `preamble_event_id` existed has
a `session.compaction.completed` event with no such field. `history_lower_bound`
falls back, in order: `preamble_event_id` if present and it resolves to a
`server.message.send` row; else `summary_event_id` if present and it resolves
to a message row (the pre-change case — the Anthropic normalizer, see
[`provider.request`](#providerrequest) above, then repairs the
`assistant`-first sequence on the wire and records what it changed); else the
full history, so a broken reference never loses context.

**Effect on subsequent turns:** starting with the next `session.sendMessage`
or `session.compact` call, the history sent to the **model** begins at this
event's referenced preamble message (so the sequence starts `user(preamble) →
assistant(summary) → …`), followed by every message recorded after it — the
pre-compaction messages are no longer sent upstream. A session with no
messages after the boundary yet (compaction as the most recent event) simply
replays as `user(preamble) → assistant(summary)`; a valid sequence on its
own. This changes
only what is sent to the model. `GET /api/v1/sessions/{id}/events` (and any
`session.subscribe` replay via `since_event_id`) always returns the complete,
unmodified append-only log, including every event before the compaction —
compaction never rewrites or deletes history, it only changes which rows a
later turn's request is built from.

See [Client API — session creation](00-client-api.md#post-apiv1sessions--open-a-session)
for the `compaction` config and [Client API — `session.compact`](00-client-api.md#sessioncompact)
for the manual-trigger RPC method.

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

**Auto compaction (`compaction: {"mode":"auto","size":...}`, triggered inside
a turn once that turn's own usage crosses `size`):**

```
client.message.send
provider.request
provider.response      (ok: true; usage crosses the configured size)
server.message.send                                 -- the turn's own reply, persisted first
session.compaction.started    (trigger: auto)
provider.request               (compaction call: history + compaction instruction)
provider.response      (ok: true)
server.message.send            (preamble, role: user, synthetic: compaction_preamble)
server.message.send            (compacted summary, role: assistant)
session.compaction.completed
terminal result: {message, events}                  -- this turn's own terminal result;
                                                      -- `events` ends with the full compaction
                                                      -- record above, in this order
```

The preamble, the summary and `session.compaction.completed` are inserted in
one transaction (see "Atomicity" above); `session.compaction.started` and the
compaction's provider pair are ordinary autocommitted rows that survive a
failed attempt as its audit trail. All of them are appended to this
turn's own `result.events`, after that turn's own `server.message.send` —
`result.events` is always the turn's complete log, in the order it happened,
compaction included. This holds even when the automatic attempt **fails**:
`result.events` still ends with that attempt's `session.compaction.started`
and its `provider.request`/`provider.response` pair(s) (no summary, no
`completed` — see "Failure" above).

The *next* turn's `provider.request` contains only the preamble and the
compacted summary plus whatever was recorded after them — never the
pre-compaction messages.

**Manual compaction (`session.compact` RPC call):**

```
session.compact                                     -- session.compaction.started
provider.request               (compaction call: history + compaction instruction)
provider.response      (ok: true)
server.message.send            (preamble, role: user, synthetic: compaction_preamble)
server.message.send            (compacted summary, role: assistant)
                                                     -- session.compaction.completed
```

Unlike `session.sendMessage`, `session.compact` is not itself an
`event_type` — it is the RPC call whose terminal result is the
`session.compaction.completed` event. Every event above still streams to
`session.subscribe` watchers exactly like any other event, in this order,
before the terminal `session.compaction.completed` frame. See [Client API —
`session.compact`](00-client-api.md#sessioncompact) for its full error set.
