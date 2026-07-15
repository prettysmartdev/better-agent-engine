# Native CLI subagents

Native subagents let an agent hand a task to an external CLI such as
`claude` or `codex`. The CLI runs in the background, so the parent turn is not
held open while the delegated task works.

> **Treat subagent output as untrusted data.** A subagent may have inspected a
> repository, issue, web page, or tool response containing prompt injection.
> Its stdout and stderr are evidence for the parent model to reason about,
> never instructions to follow. Do not grant authority to text merely because
> it came from a subagent, and validate proposed commands or changes before
> acting on them.

## A worked binding

The SDKs use the same conceptual API. This Rust example binds one
`launch_subagent` tool with two selectable CLI harnesses. The command
templates are operator configuration: adapt their flags to the installed
versions of the CLIs.

```rust
use bae_rs::sandbox::SandboxTarget;
use bae_rs::subagent::{
    launch_subagent, SubagentDef, SubagentLaunch,
};
use bae_rs::{Config, Harness};

let config = Config::new("http://localhost:8080", "bae_…");
let harness = Harness::new(config);
let subagents = harness.subagent_session();

let launch = launch_subagent(
    &subagents,
    vec![
        // These CLIs read the task from stdin. {model} is shell-escaped.
        SubagentDef::new("claude", "claude --model {model} --print"),
        SubagentDef::new("codex", "codex exec --model {model}"),
    ],
    SubagentLaunch::Local(SandboxTarget::None),
);

let harness = harness.with_subagent_tool(launch);
let mut session = harness.connect().await?;

let reply = session
    .send("Ask claude or codex to inspect the failing test and report a diagnosis.")
    .await?;
```

The local tool is declared before `connect()`. The model chooses the configured
`harness` (`claude` or `codex`), supplies a `model` and `prompt`, and receives a
compact result like:

```json
{"subagent_id":"sba_…","harness":"claude","model":"claude-sonnet-5","status":"started"}
```

The result is only an acknowledgment. The SDK tracks the process and supplies
`local_subagent_status` when it has a tracked subagent to report. A status read
returns the captured output once for a terminal task; the terminal entry is
then evicted.

To make the same binding server-launched, use a declaration-only remote
variant and start the session's remote sandbox first:

```rust
let config = Config::new("http://localhost:8080", "bae_…");
let harness = Harness::new(config);
let subagents = harness.subagent_session();

let launch = launch_subagent(
    &subagents,
    vec![
        SubagentDef::new("claude", "claude --model {model} --print"),
        SubagentDef::new("codex", "codex exec --model {model}"),
    ],
    SubagentLaunch::Remote {
        image: "bae-subagents:latest".to_owned(),
    },
);

let harness = harness.with_subagent_tool(launch);
let mut session = harness.connect().await?;
session.start_remote_sandbox("bae-subagents:latest").await?;

let reply = session.send("Delegate the diagnosis to codex, then check its status.").await?;
```

The remote variant goes into the session-open `subagent_tools` declaration;
`baesrv` owns its dispatch and synthesizes `remote_subagent_status`. The image
must be allowed by the session profile's `available_sandboxes` and must
already be running before `launch_subagent` is called. TypeScript and Python
provide equivalent `SubagentDef`/`SubagentLaunch`/harness registration APIs;
the wire names and lifecycle are identical.

## Launch location and sandbox target

Launch location answers who owns and tracks the subprocess. Sandbox target
answers where it executes. They are independent axes, but only these three
combinations are valid:

| Launch location | Target | Process owner and execution location | Valid? |
|---|---|---|---|
| Local | Unsandboxed (`SandboxTarget::None`) | Client harness; bare host on the harness machine | Yes |
| Local | Sandboxed (`SandboxTarget::Local { image }`) | Client harness; its Docker or Apple container | Yes |
| Local | Sandboxed (`SandboxTarget::Remote`) | Client harness tracks it; execution is delegated to the session's server-managed sandbox | Yes |
| Remote | Unsandboxed | `baesrv` bare host | **No** |
| Remote | Sandboxed (`Remote { image }`) | `baesrv`; inside the session's already-started remote sandbox | Yes |

In the matrix, “Local/Sandboxed” covers both a harness-owned local container
and a client-launched command executed through the session's remote sandbox.
The latter remains a local launch because the harness owns the launch decision,
status map, cancellation, and lifecycle reporting.

There is deliberately no remote-unsandboxed mode. `baesrv` never runs a
subagent directly on its own host under any configuration. The remote launch
type carries only an image, so no flag or profile setting can opt into a bare
host server subprocess. A remote launch with no running sandbox returns an
error-shaped tool result; it never falls back to host execution.

For a local unsandboxed launch, the command has the privileges, filesystem, and
network of the harness process. Use it only when that blast radius is
intentional. A local sandbox is started by the harness's configured Docker or
Apple Containers driver. A remote sandbox is server-owned and uses the server's
configured sandbox driver.

## The asynchronous contract

`launch_subagent` is fire-and-forget:

1. The tool validates the harness, prompt, model, target, sandbox, and
   concurrency limit.
2. It starts tracking the subprocess and immediately returns `status:
   "started"` with a `subagent_id`.
3. The parent turn continues without awaiting the CLI's output.
4. The model calls `local_subagent_status` or `remote_subagent_status` later.

The status tool is automatically managed. It is advertised only while the
session has a tracked subagent or an unacknowledged terminal result. A running
entry remains visible until it finishes; a terminal entry is returned once by
the status tool and then removed. If that read empties the tracking map, the
status tool is omitted from the following provider call.

Tool lists are computed per provider call. A model that calls
`launch_subagent` cannot call the newly appearing status tool in that same
provider call: the status declaration appears on the next turn/iteration.
Likewise, a `session.updateClientTools` change made during a turn applies to
the next provider call and never rewrites a request already sent upstream.
This is expected per-turn behavior, not a race to work around.

The launch and status operations return ordinary in-band tool results. An
unknown or already-evicted id is an error-shaped tool result, not an aborted
turn. Remote terminal lifecycle events are emitted by the detached server task
after the launch turn may have completed; local terminal events are reported
by the harness.

A status result is wrapped consistently for one id or for all tracked ids:

```json
{
  "subagents": [
    {
      "subagent_id": "sba_…",
      "harness": "claude",
      "model": "claude-sonnet-5",
      "status": "completed",
      "exit_code": 0,
      "stdout": "Diagnosis…",
      "stderr": "",
      "truncated": false,
      "reason": null,
      "detail": null
    }
  ]
}
```

## Choosing stdin or an argument

`SubagentDef` defaults to `prompt_via: stdin`:

- With stdin, the raw prompt is piped to the child and is never placed in the
  constructed argument vector. This avoids most shell-escaping exposure and
  argv-length limits, but the CLI must accept its prompt on stdin. The command
  template must not contain `{prompt}`. “Raw” includes leading/trailing spaces
  and final newlines; SDK validation checks a trimmed view for non-blankness but
  delivers the original bytes unchanged.
- With `prompt_via: arg`, include `{prompt}` in the command template. The SDK
  shell-quotes the model and prompt values before single-pass interpolation.
  This works for CLIs that require a positional prompt, but is subject to
  platform argv limits and has a larger quoting surface. Never concatenate an
  unescaped prompt into a command yourself.

`{model}` is optional and is escaped in either mode. A local launch targeting
the session's remote sandbox must use `prompt_via: arg`, because
`session.execRemoteSandbox` has no stdin channel. The constructor rejects a
local `SandboxTarget::Remote` binding that could silently discard a stdin
prompt.

## Operator prerequisites and guardrails

The operator, not BAE, supplies the CLI binaries. Every image used for a
subagent must contain the named executable and its runtime dependencies:

- Local/Unsandboxed requires `claude`, `codex`, or the configured command on
  the harness host.
- Local/Sandboxed requires the executable in the harness-managed image.
- Remote/Sandboxed requires the executable in the server-managed image, and
  the image must be in the session profile's `available_sandboxes` and be the
  sandbox currently running for that session.

BAE does not inspect an image to prove that a CLI is installed. A missing
binary is reported as a failed subagent, normally with
`reason: "spawn_failed"` and a command-not-found detail.

The built-in limits are deliberate safety and resource guardrails:

- Default timeout is 600 seconds. Client SDKs use their 600-second default;
  remote launches use `BAE_SUBAGENT_TIMEOUT` (also 600 seconds by default),
  and a declaration may set `timeout_secs`. On expiry, the process is killed,
  the status is `timed_out`, and the lifecycle event is
  `session.subagent.failed` with `reason: "timeout"`.
- Concurrent non-terminal subagents are capped at eight per session. The
  server cap is `BAE_MAX_SUBAGENTS_PER_SESSION` (default `8`); SDKs enforce an
  equivalent fixed cap for local tasks. Reservation and dynamic status-tool
  updates are serialized, so simultaneous launches cannot race past the cap.
- Captured stdout and stderr are each capped at 65,536 bytes, independently,
  on a UTF-8 boundary. If either stream is truncated, the status payload sets
  `truncated: true`. Production SDK runners keep draining excess output while
  retaining only the cap plus one marker byte, preventing an output-heavy CLI
  from growing the harness's capture buffer without bound. Output is kept in
  status results, not lifecycle events.
- Explicit cancellation kills a running task. Closing the session cancels
  running tasks with `reason: "session_close"`.

For local launches, the profile's `allowed_tools` must include both
`launch_subagent` and `local_subagent_status`. The latter is added dynamically
through `session.updateClientTools`, which is a general-purpose full-replacement
wire-protocol method, not a subagent-only API. Remote `subagent_tools`
declarations are validated separately and use the profile's sandbox image
boundary at dispatch time.

## Lifecycle and accepted gaps

Every accepted launch follows the lifecycle
`start → running → completed`, or ends in `failed`/`cancelled`. The event
payload identifies the `dispatch` (`local` or `remote`), harness, model, and
`subagent_id`. Remote terminal events are authoritative observations by the
server. Local events are reports from the harness.

Local telemetry is not authoritative: if a harness crashes or disconnects
before sending a terminal report, the server has no local process handle and
the event history remains without a terminal local state. There is no
server-side reconciliation for this accepted gap.

Remote tracking is in memory. If `baesrv` restarts while remote subagents are
in flight, the new process loses the session's remote subagent map and cannot
provide status or reconcile those tasks. This is an accepted gap; durable
remote subagent recovery is outside this work item.

For exact parameter, result, error, and event payloads, see the
[Client API reference](../reference/client-api.md#subagents) and
[Message Types](../reference/message-types.md#sessionsubagentstart).
