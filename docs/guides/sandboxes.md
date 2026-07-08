# Sandboxes

BAE can run shell commands for an agent inside a sandboxed container — either
the **server's** sandbox (a container the server starts via Docker or Apple
Containers and execs into on the agent's behalf) or the **client harness's**
own local container engine. This guide walks through declaring which images a
profile may use, starting a remote sandbox, binding the builtin
`run_shell_command`/`run_shell_named` tools in each of the three client SDKs,
and the full lifecycle event trail both kinds of sandbox produce.

---

## Prerequisites

- A running BAE server (see [Quickstart](quickstart.md)) with `docker` (or
  `container`, on macOS) on `PATH` if you want to exercise a real sandbox —
  the concepts below apply the same way whether or not the underlying binary
  is actually present, since the server itself falls back to a structured
  `Unsupported` error rather than crashing.
- One of the three client SDKs, or `curl` for the raw wire-level walkthrough.

---

## Step 1 — Declare `available_sandboxes` on a profile

`available_sandboxes` is a profile field: an array of container image name
strings the profile is allowed to launch sandboxes from.

```sh
curl -s -X POST http://127.0.0.1:8081/admin/v1/profiles \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "sandboxed-assistant",
    "primary_provider": "anthropic-sonnet",
    "available_sandboxes": ["python:3.12", "node:22"],
    "allowed_tools": []
  }'
```

The instant this write succeeds, the server spawns a **detached background
task** (never on the request's critical path) that calls `ensure_image` for
each declared name, sequentially: `docker image inspect` (or `container
images inspect`) to check whether the image is already present, `pull` if
not. Each image logs exactly one line as it resolves:

```
INFO sandbox image already available profile_id="pro_…" image="python:3.12"
INFO sandbox image pulled successfully  profile_id="pro_…" image="node:22"
ERROR failed to ensure sandbox image     profile_id="pro_…" image="node:22" error="…"
```

Every declared image starts at `pending` the instant the profile write
succeeds (before the background task even runs), so a client that connects a
moment later sees `pending` rather than nothing. Check the current state at
any time:

```sh
curl http://127.0.0.1:8081/admin/v1/sandbox-status
```

```json
{
  "items": [
    {
      "profile_id": "pro_…",
      "images": [
        {"name": "python:3.12", "status": "available"},
        {"name": "node:22", "status": "error", "detail": "…"}
      ]
    }
  ]
}
```

`available_sandboxes` is a plain JSON array of strings, validated with the
same `require_string_array` helper `mcp_servers`/`fallback_providers` already
use — a non-string element is `400 bad_request`. Replacing a profile
(`PUT /admin/v1/profiles/{id}`) with a new/expanded image list re-triggers
provisioning for the newly added names the same way.

> An **empty** `available_sandboxes` (the default) means this profile can
> never start a remote sandbox at all — `session.startRemoteSandbox` always
> fails with `sandbox_image_not_allowed` regardless of what image is
> requested.

---

## Step 2 — The driver-connect notification

When a client key registers as a driver on a session
(`session.registerDriver`) whose profile has a non-empty
`available_sandboxes`, the server immediately follows the ordinary
`session.driver.register` event with a `session.sandbox.available` event:

```json
{
  "images": [
    {"name": "python:3.12", "status": "available"},
    {"name": "node:22", "status": "error", "detail": "…"}
  ]
}
```

This tells the connecting client which images are actually ready to use,
without it having to poll the admin API. `status` is one of
`pending`/`available`/`error`; `detail` is present only on `error` so a
failure is surfaced in-band rather than silently omitted. A profile with an
empty `available_sandboxes` emits no such event.

### The profile-scoping guarantee

**This notification is built by iterating this session's own profile's
`available_sandboxes` list and looking up each name's status — never by
scanning or flattening image status across every profile on the server.**
The server tracks image-pull status in one shared map covering every profile
that has ever declared images (that's how the background-provisioning task
finds it convenient to store results), but a session opened against profile A
must never see an image that only profile B declared, even if that image
happens to be a known, successfully-pulled image on the same running server.
`session.startRemoteSandbox` (below) enforces the identical scoping, so the
notification and the enforcement can never disagree about which images a
given session may use — a profile is a hard trust boundary, exactly like
`allowed_tools`.

---

## Step 3 — Start a remote sandbox

`session.startRemoteSandbox` asks the server to start a container from one of
the session's own profile's `available_sandboxes` images, using the
server-wide configured driver (`BAE_SANDBOX_DRIVER` — see
[Configuration — Sandbox driver](../reference/configuration.md#sandbox-driver)):

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"session.startRemoteSandbox","params":{"image":"python:3.12"}}'
```

```json
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"session.sandbox.start","payload":{"image":"python:3.12","dispatch":"remote"}}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"session.sandbox.running","payload":{"image":"python:3.12","sandbox_id":"…","dispatch":"remote"}}}
{"jsonrpc":"2.0","id":3,"result":{"sandbox_id":"…","image":"python:3.12","started_at":"2026-07-06T18:26:10.000Z"}}
```

Requesting an image not in **this session's own** profile's
`available_sandboxes` — including an image that some *other* profile on the
same server has successfully provisioned — is rejected with
`-32011 sandbox_image_not_allowed` before any container is started. See
[Client API Reference](../reference/client-api.md#sessionstartremotesandbox)
for the full params/result/error table.

One sandbox per session, not per driver — see
[Session-wide, not per-driver](#session-wide-not-per-driver) below.

---

## Step 4 — Bind `run_shell_command`/`run_shell_named` in a client harness

Each SDK ships a `sandbox` module exposing two builder types and two tool
constructors:

- **`SandboxTarget`** — `Local { image }` (the harness's own Docker/Apple
  Containers engine) or `Remote` (the sandbox the server started for this
  session in Step 3).
- **`RemoteMode`** — only meaningful for `Remote` targets: `Auto` (the server
  dispatches and continues the turn itself, never involving the client) or
  `Manual` (the client fetches the raw result and builds the `tool_result`
  itself). See [Auto vs. manual dispatch](#auto-vs-manual-remote-dispatch)
  below.
- **`run_shell_command(session, target, remote_mode)`** — a tool named
  `run_shell_command` whose one required input is `command: string`; the
  model may request **any** shell command.
- **`run_shell_named(session, name, description, command_template, target, remote_mode)`**
  — a tool whose input schema is derived from `{param}`-style placeholders in
  `command_template`; each model-supplied value is shell-escaped before
  substitution (the command-injection boundary).

Sandbox tools need a live `Session`/transport handle (unlike the [file
tools](file-tools.md), which need none), so — unlike every other builtin tool
in this work item — they are built from a `SandboxSession` handle obtained
from the **harness**, registered on the harness, and only bound to a real
transport once `connect()`/`join()` returns. Auto-mode declarations in
particular *must* be registered before `connect()`, since they are sent in
the session-open `sandbox_tools` array.

### Rust

```rust
use bae_rs::sandbox::{run_shell_command, RemoteMode, SandboxTarget};
use bae_rs::{Config, HarnessBuilder};

let harness = HarnessBuilder::new(config);
let sandbox = harness.sandbox_session(); // handle only — transport late-bound

// Remote, manual dispatch: the harness fetches raw output and decides what
// the model sees.
let shell_tool = run_shell_command(
    &sandbox,
    SandboxTarget::Remote,
    RemoteMode::manual(|result| {
        serde_json::json!({ "stdout": result.stdout, "exit_code": result.exit_code })
    }),
);

let harness = harness.with_sandbox_tool(shell_tool);
let mut session = harness.connect().await?;

// Start the session's remote sandbox before the model can use the tool.
session.start_remote_sandbox("python:3.12").await?;

let reply = session.send("Run `python --version` and tell me the result.").await?;
```

### TypeScript

```typescript
import { Harness, runShellCommand, RemoteMode, SandboxTarget } from "@prettysmartdev/bae-ts";

const harness = new Harness(config);
const sandbox = harness.sandboxSession(); // handle only — transport late-bound

const shellTool = runShellCommand(
  sandbox,
  SandboxTarget.remote(),
  RemoteMode.manual((result) => JSON.stringify({ stdout: result.stdout, exit_code: result.exit_code })),
);

harness.registerSandboxTool(shellTool);
const session = await harness.connect();

await session.startRemoteSandbox("python:3.12");

const reply = await session.send("Run `python --version` and tell me the result.");
```

### Python

```python
from bae_py import Harness
from bae_py.sandbox import run_shell_command, RemoteMode, SandboxTarget

harness = Harness(config)
sandbox = harness.sandbox_session()  # handle only — transport late-bound

shell_tool = run_shell_command(
    sandbox,
    SandboxTarget.remote(),
    RemoteMode.manual(lambda result: json.dumps({"stdout": result.stdout, "exit_code": result.exit_code})),
)

harness.register_sandbox_tool(shell_tool)
session = await harness.connect()

await session.start_remote_sandbox("python:3.12")

reply = await session.send("Run `python --version` and tell me the result.")
```

### `run_shell_named` — a constrained alternative

```rust
let restart_tool = run_shell_named(
    &sandbox,
    "restart_service",
    "Restart a named systemd service inside the sandbox.",
    "systemctl restart {service}",
    SandboxTarget::Local { image: "ops-toolbox:latest".into() },
    RemoteMode::Auto, // ignored for a Local target
);
```

`{service}` becomes a required string input; whatever value the model
supplies is shell-escaped (single-quote wrapping, embedded quotes rewritten)
before it is substituted into the template — a value of `a'; rm -rf / #`
becomes one literal, inert argument to `systemctl restart`, never a second
shell command.

### `run_shell_command` is, by design, unconstrained

> **`run_shell_command` lets the model run *any* shell command.** There is no
> allowlist of permitted commands — that is precisely what distinguishes it
> from `run_shell_named`, which only ever runs the one template the harness
> developer wrote. For a `Local` target, **the image you choose is the
> entire security boundary**: an image with no secrets, no host mounts, and
> no network access is safe to expose via `run_shell_command`; an image that
> mounts your home directory or carries cloud credentials is not, no matter
> how carefully you build the rest of your harness. For a `Remote` target,
> the equivalent boundary is whatever the server's sandbox container was
> started from (Step 1's `available_sandboxes` list) — the same reasoning
> applies to choosing what images a profile is allowed to launch. Do not
> reach for `run_shell_command` expecting it to behave like a restricted
> tool; reach for `run_shell_named` instead when the agent should only ever
> run one specific command.

---

## Auto vs. manual remote dispatch

For a `Remote`-target tool, `RemoteMode` decides **who builds the
`tool_result` and whether the turn pauses to ask the client**:

| | `RemoteMode::Auto` | `RemoteMode::Manual(fn)` |
|---|---|---|
| Declared in | the session-open `sandbox_tools` array (never the ordinary `tools` array) | the ordinary client `tools` array, like any other client-dispatched tool |
| Who executes | the **server**, inside `run_turn`, exactly like an MCP tool call | the **client harness**, via a plain `session.execRemoteSandbox` RPC — not part of the turn loop at all |
| Does the turn pause? | **no** — the server calls `SandboxDriver::exec` directly and continues the same provider round-trip | **yes** — `run_turn` returns `Outcome::Paused` with the `tool_use` block; the harness's tool handler fetches the raw result and sends its own `tool_result` via the next `session.sendMessage` |
| Events | `sandbox.request` / `sandbox.response` (mirrors `mcp.request`/`mcp.response`) | none beyond the ordinary client `tool.call`/`tool.result` pair; a driver failure still logs `session.sandbox.error` (phase `exec`) |
| Return type of the constructor | `SandboxToolDef` (never a callable `Tool` — this is a deliberate type-level split so an Auto declaration can never accidentally be registered as an ordinary client tool and silently never fire) | a client-dispatched `Tool` |

Use `Auto` when you want a sandbox shell tool to behave exactly like an MCP
tool — dispatched server-side, no client round-trip, no chance to
post-process. Use `Manual` when the harness needs to inspect, redact, or
transform the raw `{stdout, stderr, exit_code}` before the model sees it —
the `Manual` closure receives an `ExecResult` and returns whatever content
the tool result should carry.

A `Local`-target tool is always client-dispatched (there is no "auto" for a
container the client itself started) — `RemoteMode` is ignored for `Local`.

---

## The `session.sandbox.*` lifecycle

A remote sandbox's life is a small state machine. Every transition is logged
as an event, and every lifecycle event (`running`/`stopped`/`error`) carries
a `"dispatch"` field so a subscriber can tell whether it describes the
**remote** sandbox (server-authored, authoritative) or a **local** one
(client-reported, below).

| Event | Fired when |
|---|---|
| `session.sandbox.start` | `startRemoteSandbox` accepted, about to call the driver |
| `session.sandbox.running` | the driver's `start` succeeded; the sandbox is up |
| `session.sandbox.stop` | `stopRemoteSandbox` accepted (explicit, or session close triggering an implicit stop) |
| `session.sandbox.stopped` | the driver's `stop` succeeded |
| `session.sandbox.error` | any driver call (`ensure_image`/`start`/`exec`/`stop`) failed, at any phase |

Full remote success path:

```
session.sandbox.start     (image, dispatch: remote)
session.sandbox.running   (image, sandbox_id, dispatch: remote)
… agent uses the sandbox …
session.sandbox.stop      (image, sandbox_id, reason: explicit|session_close, dispatch: remote)
session.sandbox.stopped   (image, sandbox_id, reason, dispatch: remote)
```

Closing the session while a remote sandbox is still running triggers the
identical stop sequence automatically, with `reason: "session_close"`
instead of `"explicit"` — the server always eventually logs a terminal event
for its own sandbox. See [Message Types](../reference/message-types.md) for
the exact payload shape of each event.

### Local sandboxes report their own lifecycle

A **local** sandbox — one a client harness starts against its own Docker/
Apple Containers engine — is invisible to the server unless the client tells
it. Every SDK's `SandboxSession` does this automatically, with no separate
opt-in: around the local driver's `start`/`exec`/`stop` calls, it issues
`session.reportLocalSandbox` so the same `session.sandbox.running`/
`stopped`/`error` events appear in the shared event log — visible to every
subscriber, not just the reporting client — carrying `"dispatch": "local"`
and the local `container_id`:

```json
{"image": "python:3.12", "container_id": "…", "detail": null, "dispatch": "local"}
```

This happens:
- lazily, on the first `Local`-target tool call for a given image (or
  eagerly via `Session::start_local_sandbox(image)`) → reports `running`;
- when `Session::close()` runs → stops and reports `stopped` for every local
  sandbox this session started, mirroring how the server stops its own
  remote sandbox at close;
- on any local driver failure (image pull, start, exec, or stop) → reports
  `error` with `detail`, **in addition to** the tool call's own in-band error
  result — the report is pure telemetry and never substitutes for the tool's
  own error handling.

> **`reportLocalSandbox` is self-reported telemetry, not a security
> control.** The server **cannot verify** a client's claim that its local
> container is actually running or stopped — a client could report `running`
> for a container that never existed, and nothing stops it. This is an
> accepted trust boundary: a local sandbox is the harness developer's own
> local resource, and the server's only role is to make that activity
> *visible* in the shared event log for other participants, not to govern or
> verify it. Contrast this with the remote lifecycle above, which **is**
> authoritative — the server itself drives every remote transition, so a
> `session.sandbox.running` event with `"dispatch": "remote"` is a fact the
> server actually observed, while the same event with `"dispatch": "local"`
> is only a claim.
>
> One consequence: **if a client harness crashes or disconnects after
> reporting `running` but before ever reporting `stopped`/`error`, the event
> log simply has no terminal event for that local sandbox.** There is no
> server-side timeout or reconciliation for client-reported local state —
> the server never retains a handle to a local sandbox, so it has nothing to
> positively tear down or time out, unlike the remote case where
> `AppState.sandboxes` is real server-owned state the server can always
> eventually clean up at session close. This is a known, accepted gap
> specific to local sandboxes.

`session.reportLocalSandbox` performs **no `available_sandboxes` validation**
— an arbitrary image name is accepted and logged as-is — because a local
sandbox is never governed by the server's trust boundary in the first place.
It does still require prior driver registration (`-32001` otherwise), like
every other sandbox RPC.

### Session-wide, not per-driver

**The remote sandbox started by `session.startRemoteSandbox` belongs to the
whole session, not to the driver that started it.** If two client keys are
both registered as drivers on the same session, and driver A calls
`startRemoteSandbox`, driver B's own turn can dispatch `Auto`-mode sandbox
tool calls (or `execRemoteSandbox`) against that same sandbox without ever
calling `startRemoteSandbox` itself. This mirrors how MCP servers are
already session-wide shared infrastructure rather than a private per-driver
resource — it is a **deliberate** design choice, not an oversight. A second
`startRemoteSandbox` call on a session that already has one running fails
with `-32000` (one sandbox per session); stop the current one first if you
need to switch images.

---

## Closing the session

```sh
curl -s -X DELETE "http://localhost:8080/api/v1/sessions/$SESSION_ID" \
  -H "Authorization: Bearer $SESSION_KEY"
```

On close, BAE stops any still-running remote sandbox for the session
(`session.sandbox.stop`/`stopped`, `reason: "session_close"`), in addition to
its existing MCP-connection and broadcast-channel teardown. Each SDK's
`Session::close()` similarly stops any local sandbox it started.

> **Abandoned containers from a killed server are not automatically cleaned
> up.** `AppState.sandboxes`/`sandbox_status` are in-memory only — a server
> that is killed (not gracefully shut down) rather than closing sessions
> normally leaves no record of what it started. See
> [Configuration — Abandoned containers are not automatically cleaned
> up](../reference/configuration.md#abandoned-containers-are-not-automatically-cleaned-up)
> for the operator-facing caveat and mitigation.
