# Work Item: Feature

Title: cli subagents
Issue: issuelink

## Summary:
- This work item aims to add native CLI subagents to BAE.

subagents should be possible to launch as 'local' or 'remote' (i.e. by either a locally-run client harness application or baesrv). They can either run with Sandbox::None, Sandbox::Local or Sandbox::Remote (or whatever the existing convention is)

The client harness defines a subagent as a shell invocation (like claude, codex, OpenCode, etc). If using a subagent within a local or remote sandbox, it's up to the BAE operator to ensure that a container image with the desired subagent CLI installed is available in the right location.

The client harness' definition of the subagent must implore to the model that the subagent is ASYNC and will run IN THE BACKGROUND. This means that the model can call a configured subagent with (harness name, model name, prompt), and the tool call response will simply be "subagent started", NOT the full result of the subagent, since that may take a long time.

Whenever the model requests a subagent be started, it must also be able to query for its status, therefore the clinet harness and/or server must expose a 'subagent_status' tool which lets the model know if the subagent is still running or not, and if it has indeed completed, what its output was. The client harness and baesrv must handle the status automatically whenever a subagent is available and active so that the developer does not need to manually wire in the status tool. It should be automatic, and the tool should dissapear if there is no subagent running.

Ensure that subagents will work locally on the client harness' maching, AND in BOTH Docker and Apple Containers on both the client and server depending on the configuration. Everything should still respect the configured profile's allowed tools and images settings.


Note: `aspec/devops/subagents.md` already exists but describes something unrelated — local-development Claude Code subagents used to *build* BAE itself. This work item is a BAE product feature (subagents an *agent built on BAE* can launch) and does not touch that file.

## User Stories

### User Story 1:
As an: Agent Developer

I want to:
bind a `launch_subagent` tool to my harness that hands off a prompt to an external CLI coding agent (`claude`, `codex`, `opencode`, or any shell-invocable harness I configure) and get back an immediate "started" acknowledgment — never a blocking wait for the subagent's full run — plus a `subagent_status` tool the model can call later to check progress or retrieve the result

So I can:
let my agent delegate a long-running sub-task to another agentic CLI without stalling the parent conversation's turn loop for however long that sub-task takes, and without hand-rolling process spawning, polling, or tool-visibility bookkeeping myself

### User Story 2:
As a: Platform Operator

I want to:
choose, per subagent binding, whether the CLI subprocess is launched by the client harness's own machine or by `baesrv`, and — for a harness-launched subagent — whether it runs unsandboxed or inside a locally-managed container, while a `baesrv`-launched subagent always runs inside the session's server-managed sandbox (never unsandboxed on the server's own host) — reusing the exact same profile `available_sandboxes` image allowlist and `allowed_tools` gating that already govern sandbox and client tools

So I can:
control the blast radius of an agent that can shell out to arbitrary CLI subagents the same way I already control shell/file tool access, without a parallel allowlist system to reason about, and knowing that as long as I've put the right CLI binary in the right image, the feature works with no extra server-side plumbing

### User Story 3:
As a: Dashboard / Observability Developer

I want to:
see a subagent's full lifecycle in the event stream — requested, running, completed/failed/cancelled, tagged with whether it was locally or remotely launched — and see the `subagent_status` tool appear in the model's available tools only while a subagent is actually outstanding, and disappear once it's been resolved

So I can:
show a user "an agent is delegating to `codex` right now" live, without polling application state myself, and trust that the model's own tool list never carries dead/irrelevant tools once nothing is pending

## Implementation Details:

This is new surface, but built almost entirely by composing machinery WI 0006/0008/0009 already shipped: `SandboxDriver`/`SandboxTarget` (server: `server/src/engine/sandbox.rs`; client: `sandbox.rs`/`.ts`/`.py`), the `run_turn` per-tool dispatch partition (`server/src/engine/session.rs:363-671`), shell-escaping for template interpolation (`sandbox.rs`'s `run_shell_named`, `sandbox.rs:949-983`), and the local-vs-server lifecycle-reporting pattern (`session.reportLocalSandbox`, `rpc.rs:1228-1326`). The one genuinely new primitive is **async, fire-and-forget tool dispatch with a dynamically-appearing/disappearing status tool** — nothing in the codebase does this today (confirmed: no polling/background-tool pattern exists anywhere in `server/` or the three client SDKs).

It splits into eight pieces: (A) the conceptual model — two independent axes; (B) subagent definition + tool construction, all three SDKs; (C) local (client-launched) execution and status; (D) remote (server-launched) execution and status; (E) the disappearing-tool mechanism, both directions; (F) cancellation and teardown; (G) new `EventType` variants; (H) docs.

### A. Conceptual model: launch location × sandbox target

Two independent axes, matching the summary's "local or remote... Sandbox::None, Sandbox::Local or Sandbox::Remote":

- **Launch location** — which process owns the CLI subprocess and tracks its lifecycle: **local** (the client harness's own process) or **remote** (`baesrv`'s own process). This determines who can answer "is it done yet" without a network round trip, and therefore which side's tool list needs to grow/shrink the status tool.
- **Sandbox target** — where that subprocess actually executes, reusing the existing `SandboxTarget` vocabulary:
  - For a **local** launch, all three existing variants apply unchanged: `None` (bare host exec on the developer's machine, `sandbox.rs:737-766`'s `exec_none` pattern), `Local { image }` (the harness's own local Docker/Apple Container engine), `Remote` (delegates the actual exec to the session's server-managed sandbox via the existing `session.execRemoteSandbox`/Manual-mode machinery — the *launch decision and status tracking* stay local even though execution happens server-side).
  - For a **remote** launch, `Local` and `Remote` collapse into one thing — "the container `baesrv` itself manages via its own `SandboxDriver`" — since there is no second party for `baesrv` to delegate to. Remote launches therefore accept exactly **one** target shape: `Sandboxed { image }` (the session's already-started remote sandbox, i.e. the same `AppState.sandboxes` entry Auto-mode sandbox tools already use). There is deliberately **no** bare-host option for a remote launch — `baesrv` must never spawn a subagent subprocess directly on its own host; the only three valid execution options in this whole feature are Local/Unsandboxed (harness), Local/Sandboxed (harness), and Remote/Sandboxed (`baesrv`). Call this out explicitly as a deliberate scoping decision, not an oversight, in Codebase Integration.
- No new trust-boundary/allowlist concept is introduced: the subagent tool's *name* goes through the existing `enforce_tool_allowlist` (`server/src/api/client/sessions.rs:148-166`) exactly like any client tool, and any container *image* it uses goes through the existing `available_sandboxes` allowlist exactly like any sandbox tool (`sandbox_tools` is already exempt from `allowed_tools`, `sessions.rs:221-224` — the new `subagent_tools` list, part D, gets the identical exemption for the identical reason).

### B. Subagent definitions and the `launch_subagent` tool (all three SDKs)

New module per SDK (`client-rust/src/subagent.rs`, `.ts`, `.py`), mirroring `sandbox.rs`'s shape:

```rust
pub struct SubagentDef {
    pub harness: String,               // e.g. "claude", "codex", "opencode" — the enum value the LLM selects
    pub command_template: String,      // e.g. "claude --model {model} --print" — {model} substituted, shell-escaped
    pub prompt_via: PromptDelivery,    // Arg (interpolate {prompt} into the template, escaped) | Stdin (default; piped to the subprocess, avoids argv length limits and reduces the template's own injection surface)
    pub timeout: Duration,             // default from BAE_SUBAGENT_TIMEOUT / a client-side default
}

pub enum PromptDelivery { Arg, Stdin }
```

- `launch_subagent(configs: Vec<SubagentDef>, launch: SubagentLaunch) -> Tool | SubagentToolDef` — one tool binding covers a *set* of configured CLI harnesses (the model picks one by name), because the summary's tool contract is a single `(harness_name, model_name, prompt)` call, not one tool per CLI. `input_schema` requires `harness` (a string enum restricted to `configs`' names), `model` (string), `prompt` (string). `SubagentLaunch` is `Local(SandboxTarget)` or `Remote { image: String }` (the single sandboxed-only shape from part A — there is no `Remote(Unsandboxed)` variant to construct in the first place, since `baesrv` never spawns a subagent on its own bare host; only a harness's own `Local(SandboxTarget::None)` may run unsandboxed); the constructor returns a `Tool` (client-dispatched, part C) for `Local(_)` and a `SubagentToolDef` (destined for the new `subagent_tools` declaration, part D) for `Remote(_)` — the exact `SandboxTool::Tool | SandboxTool::Def` split `run_shell_command`'s `build_tool` already establishes (`sandbox.rs:849-854,985-1002`), reused verbatim for the same reason: a harness developer must not be able to register a `Remote` subagent through the client tool registry and have it silently never fire.
- **Interpolation is always shell-escaped** for the `{model}` substitution (and `{prompt}` when `PromptDelivery::Arg` is chosen) using the exact same escaping primitive `run_shell_named` already established (`sandbox.rs:949-983`) — a model-supplied `harness`/`model`/`prompt` is untrusted input by definition, identical rationale to WI 0006's command-injection edge case. `PromptDelivery::Stdin` is the recommended default specifically because it sidesteps both the argv-length limit and most of the escaping surface for the (typically much longer) `prompt` field.
- The launch tool's success `tool_result` is always the same immediate, non-terminal shape regardless of launch location — this is the async contract the summary requires:
  ```json
  {"subagent_id": "sba_...", "harness": "claude", "model": "...", "status": "started"}
  ```
  It never contains the subagent's actual output; that is retrieved later via the status tool (part C/D).

### C. Local (client-launched) execution and status tool

`SubagentSession` (parallel to `SandboxSession`, same late-bound-transport pattern documented at `sandbox.rs:18-39`, so subagent tools are constructed the same "get a handle from `Harness`, build tools against it, register, then `connect()`" way sandbox tools already require): holds `Arc<Mutex<HashMap<subagent_id, SubagentTask>>>` where `SubagentTask` tracks the spawned `tokio::process::Child` (`kill_on_drop(true)`, mirroring `engine/mcp.rs`'s subprocess hygiene), captured stdout/stderr (truncated, see Edge Cases), status (`Running | Completed | Failed | TimedOut | Cancelled`), and timestamps.

- The `Local(_)`-target `Tool`'s handler: on call, spawns the subprocess per the resolved `SubagentTarget` (bare host for `None`, the harness's own local container driver for `Local{image}`, or `Session::exec_remote_sandbox` fire-and-forget-wrapped-in-a-local-task for `Remote`), inserts a `SubagentTask` into the map keyed by a freshly generated `subagent_id`, spawns a background `tokio::spawn` that awaits the child and updates the task's terminal state on exit (or on timeout, killing it), and returns the `{"status":"started",...}` result **immediately** — the handler itself never awaits completion.
- **`local_subagent_status`**: a second builtin tool (constructed alongside `launch_subagent` from the same `SubagentSession`), input `{subagent_id?: string}` — omitted means "every tracked entry not yet reported as terminal-and-acknowledged." Reading it looks up the map; a terminal entry is returned once, then evicted on the *next* call after being seen once terminal (so a model that never checks still sees a truthful "still tracked" state, but the tool naturally clears itself out after acknowledgment — see part E for how this drives the tool's visibility).
- **Telemetry only, no server authority.** Every `start`/`running`/`completed`/`failed`/`cancelled` transition is mirrored to the server via a new `session.reportLocalSubagent` RPC (params `{"state", "subagent_id", "harness", "model", "detail"}`), gated the same `require_registered_driver` way `session.reportLocalSandbox` already is (`rpc.rs:1228-1326` is the direct precedent — same self-reported-telemetry trust posture: the server cannot verify the claim, it only makes the activity visible in the shared event log).
- `Session::close()` kills any still-tracked local subagent process, mirroring how WI 0006 requires `close()` to stop any still-running local sandbox it started.

### D. Remote (server-launched) execution and status tool

New module `server/src/engine/subagent.rs`, explicitly mirroring `engine/sandbox.rs`'s (and, one level further back, `engine/mcp.rs`'s) "Why hand-rolled" / "Lifecycle" doc-comment shape (`sandbox.rs:1-33` is the direct precedent to copy): no SDK, subprocess-per-call via the same injectable `CommandRunner` seam `sandbox.rs:141-165` already established (reused, not reinvented, so subagent tests get the identical offline-mock posture for free).

- `AppState` gains `subagents: Arc<Mutex<HashMap<session_id, HashMap<subagent_id, SubagentTask>>>>` — same nested-map shape `sandbox_status` already uses (`server/src/api/mod.rs:76,161`), same rationale (must be scoped and read per-session, never flattened).
- **New tool-declaration list `subagent_tools`**, a sibling of `sandbox_tools` at session open/join, carrying `Remote`-target `SubagentToolDef`s. Exempted from `allowed_tools` for the identical reason `sandbox_tools` already is (`sessions.rs:221-224`); the trust boundary is the operator's own `SubagentDef` code plus `available_sandboxes` for the image.
- **New, genuinely async run_turn dispatch bucket.** `run_turn`'s existing partition (`session.rs:363-405`) classifies a `tool_use` as `client` / `mcp` / `sandbox`; add a fourth: `subagent`, for names in `subagent_tool_names`. Unlike `sandbox`/`mcp` dispatch (`session.rs:415-633`, which **awaits** the driver call before continuing), subagent dispatch must **not** await completion:
  1. Validate the target: `Sandboxed{image}` (the only remote shape, part A) requires an already-started remote sandbox exactly like Auto sandbox tools do, `session.rs:409-419`'s "no sandbox configured" precedent reused verbatim for "no sandbox started" — there is no bare-host fallback to validate around, since a remote launch cannot express one.
  2. Log `session.subagent.start`, generate `subagent_id`, insert a `Running` `SubagentTask` into `AppState.subagents[session_id]`.
  3. `tokio::spawn` a detached task holding `Arc` clones of the store/broadcaster/subagent map (the exact pattern `admin/profiles.rs`'s `provision_sandbox_images` background task already uses to update state and log from outside any request handler) that runs the subprocess, updates the task to `Completed`/`Failed`/`TimedOut` on exit, and logs `session.subagent.running` (on successful spawn, from inside the synchronous dispatch step, not the spawned task) then `session.subagent.completed`/`failed` (from the spawned task, once the process exits) directly through the broadcast choke point — this is the one place in `run_turn`'s dispatch that produces events *after* the turn that triggered it has already completed, and it must be documented as such.
  4. Immediately push `{"status":"started",...}` as this tool's `tool_result` and continue the loop **in the same turn** — no pause, matching every other server-dispatched tool's non-pausing behavior, but unlike them, without having actually finished the work.
- **`remote_subagent_status`**, resolved without any new RPC or persisted declaration: wherever `run_turn` assembles the tool list handed to the provider each turn (the union of `client_tool_names` definitions, `sandbox_tools`, `subagent_tools`, and MCP-discovered tools), conditionally append a synthetic `remote_subagent_status` definition whenever `AppState.subagents.get(session_id)` is non-empty for this session, recomputed fresh every turn — no persistence, no protocol change, because the server already owns and rebuilds this list per turn. A call to it is dispatched the same way `subagent` tool_use blocks are (server-side, in-process, no pause), reads the map, evicts terminal-and-now-acknowledged entries the same way part C's local version does, and its own visibility next turn falls out of the map being empty or not.

### E. The disappearing tool, both directions

The remote side (D) needs no protocol change — it is naturally "disappearing" because the server freshly computes its own provider-facing tool list every turn from live state. The **local** side is the harder half, because client tool declarations are set once, at session open/join (`store/sessions.rs::set_client_tools`), not resent per turn — there is no existing mechanism for a harness to tell the server "stop/start advertising this tool" mid-session.

**Propose a new JSON-RPC method `session.updateClientTools`** (params `{"tools": [...]}`, full-replace semantics, same shape as the `tools` array already accepted at session open), gated by `require_registered_driver` exactly like `reportLocalSandbox`/`startRemoteSandbox`. The harness's `local_subagent_status` tool builder calls this automatically whenever its tracked-subagent set transitions empty→non-empty (add the tool) or non-empty→empty (remove it), so a harness developer gets the "automatic... disappears when idle" behavior for free, the same "no separate opt-in step" posture WI 0006 established for local-sandbox lifecycle reporting. This is new general-purpose wire-protocol surface (reusable by any future dynamic-tool-list need), not something scoped narrowly to subagents — call this out prominently since it is this work item's one addition to the core protocol rather than to the sandbox/tool-builtin surface.

### F. Cancellation and teardown

- New JSON-RPC method `session.cancelSubagent` (params `{"subagent_id": string}`), same driver-registration gate, kills the tracked process (idempotent — cancelling an already-terminal id is a no-op success, mirroring `stopRemoteSandbox`'s idempotent-stop posture) and logs `session.subagent.cancelled`.
- `Session::cancel_subagent(subagent_id)` client-side equivalent for local subagents — pure in-process kill, no RPC required, though it still calls `session.reportLocalSubagent` for visibility.
- Session close (`api/client/sessions.rs::close`) kills every still-`Running` entry in `AppState.subagents[session_id]` in the same teardown function that already tears down `mcp_sessions`/`sandboxes`/broadcaster state, logging `session.subagent.cancelled` with `"reason": "session_close"` for each — mirrors the exact `session.sandbox.stop`/`stopped` "reason: session_close" precedent (WI 0006, part C).

### G. New `EventType` variants

Extends the enum from 22 to 27 (update `EventType::ALL` and the exhaustiveness test at `server/src/events.rs:104-127,190-222`; **no** `should_broadcast`-style gating function needs updating — despite what WI 0005/0006's specs describe, no such function exists in `engine/broadcast.rs`, which forwards every event unconditionally, `broadcast.rs:17,92-97` — do not introduce type-based filtering as part of this work item either):

| Event | Fired when |
|---|---|
| `session.subagent.start` | launch accepted, about to spawn (part D step 2 / part C's local equivalent via `reportLocalSubagent`) |
| `session.subagent.running` | subprocess spawned successfully |
| `session.subagent.completed` | subprocess exited zero |
| `session.subagent.failed` | subprocess exited non-zero, failed to spawn, or timed out |
| `session.subagent.cancelled` | explicit cancel or session-close teardown |

Every one carries `"dispatch": "local" | "remote"` (mirroring the sandbox lifecycle events' dual-origin field, WI 0006 part F) plus `"harness"`, `"model"`, `"subagent_id"`.

### H. Documentation

- `docs/reference/client-api.md` — new `## Subagents` section following the `## Sandboxes` section's exact per-method shape (`client-api.md:673-847` is the precedent to mirror): `session.reportLocalSubagent`, `session.cancelSubagent`, `session.updateClientTools`.
- `docs/reference/message-types.md` — five new `###` catalog entries in `EventType::ALL` order, plus an addition to "Typical event sequences" showing a full local and a full remote lifecycle.
- New `docs/guides/subagents.md` — worked example binding `launch_subagent` for `claude`/`codex`, covering the launch-location × sandbox-target matrix from part A (the three valid combinations: Local/Unsandboxed, Local/Sandboxed, Remote/Sandboxed — explicitly noting `baesrv` never runs a subagent unsandboxed on its own host), prompt-injection framing for subagent output (part of Edge Cases below), and the stdin-vs-arg prompt delivery tradeoff.

## Edge Case Considerations:

- **There is no bare-host remote execution mode, by design.** `baesrv` never spawns a subagent subprocess directly on its own host under any configuration — the only remote target shape is `Sandboxed{image}` (part A). This is a hard architectural constraint, not an operator-configurable flag: unlike `SandboxTarget::None` for a **local** launch (which runs on the harness's own developer machine and is the operator's own risk to accept), letting the shared server process itself exec arbitrary model-chosen CLI subprocesses unsandboxed would be a materially larger, un-scoped blast radius, so this work item does not introduce a way to enable it at all. The three valid execution options for a subagent, full stop, are Local/Unsandboxed (harness), Local/Sandboxed (harness), and Remote/Sandboxed (`baesrv`).
- **A configured image lacks the named CLI binary.** No validation that e.g. an `available_sandboxes` image actually has `claude` installed — surfaces as an ordinary "command not found" exec failure, reported as `status: "failed"`, exactly like WI 0008's "sandbox images lack `git`" edge case. Document as a stated operator prerequisite, not a BAE-enforced guarantee (this is the summary's own framing: "it's up to the BAE operator...").
- **Runaway/never-exiting subagent process.** Bounded by `SubagentDef.timeout` (client-side default) / `BAE_SUBAGENT_TIMEOUT` (server-side default) — on expiry the process is killed, status becomes `TimedOut` (folds into `session.subagent.failed` with `"reason": "timeout"`), never left running indefinitely.
- **Unbounded stdout/stderr.** Truncate captured output to a fixed cap (e.g. the same order of magnitude other truncation limits in this codebase use) before storing it in the task/returning it from the status tool; note the truncation in the payload (`"truncated": true`) rather than silently dropping data with no signal.
- **Concurrent subagents per session.** Supported (map keyed by `subagent_id`), but bounded by a guardrail (`BAE_MAX_SUBAGENTS_PER_SESSION` server-side, an equivalent client-side constant) so a model cannot fork unboundedly many subprocesses in one session — a demo/production guardrail in the same spirit as WI 0008's `TRIAGE_MAX_ISSUES`, not a hard architectural limit.
- **Subagent output is untrusted data, not instructions.** A CLI subagent's stdout (especially one that itself explored untrusted repository/web content) must be treated by the parent model as data to reason about, never as instructions to follow — identical prompt-injection framing to WI 0008's issue-triage guidance; document this prominently in the new guide (part H) rather than assuming it is obvious.
- **Status tool called with an unknown/already-evicted `subagent_id`.** In-band error-shaped result (`{"error": "unknown subagent_id"}`), never an aborted turn — matches the established "validation failure is a tool result, not a program error" posture (WI 0006's file-tools edge case).
- **A local harness process crashes/disconnects without ever reporting a terminal state.** Known, accepted gap, identical in shape to WI 0006's equivalent gap for `session.reportLocalSandbox`: the server holds no authoritative handle for a *local* subagent (only for remote ones, via `AppState.subagents`), so there is no server-side reconciliation — document this next to the "local telemetry is not authoritative" note already established for sandboxes.
- **Server restart with remote subagents mid-flight.** `AppState.subagents` is in-memory only; a restarted server has no record of previously-spawned subprocesses. Identical accepted-gap posture to WI 0006's sandbox-container reconciliation caveat — document, do not attempt to solve, in this work item.
- **The disappearing-tool ordering is per-turn, not intra-turn.** A model that calls `launch_subagent` cannot see `remote_subagent_status`/`local_subagent_status` appear until the *next* turn's tool list is (re)computed — the tool list handed to a given provider call is fixed before that call starts. This is expected, not a bug; document it so a harness developer doesn't file it as a race.
- **`session.updateClientTools` racing a turn in flight.** If a harness calls it while a `sendMessage` turn is still being processed, the update must apply to the *next* provider call, never retroactively rewrite a request already sent — same "don't mutate history that already went out" posture the rest of the RPC surface follows.
- **Multi-driver sessions.** Any registered driver may call `reportLocalSubagent`/`cancelSubagent`/`updateClientTools` for a session, not only the turn's current owner — mirrors `reportLocalSandbox`'s existing gate (`require_registered_driver`, not turn ownership).

## Test Considerations:

- **Unit — non-blocking dispatch**: a scripted `run_turn` turn whose only `tool_use` targets a `subagent_tools` entry, run against an injectable `CommandRunner` (reusing `sandbox.rs:141-165`'s seam) whose mock subprocess sleeps past the test's own deadline before "exiting" — assert `run_turn` returns/continues **without** awaiting that sleep, the `tool_result` is exactly `{"status":"started",...}`, and `session.subagent.start`/`running` are logged synchronously while `completed` is not logged until the background task later resolves (assert via a channel/notify the mock driver signals on "exit").
- **Integration — status tool visibility, remote**: assert the provider-facing tool list omits `remote_subagent_status` before any launch, includes it starting the turn after a launch, and omits it again the turn after a completed subagent has been read once via the status tool — the exact eviction-after-acknowledgment policy from part D/C, tested precisely rather than just "it eventually goes away."
- **Integration — status tool visibility, local**: drive a scripted `client-rust` harness run through launch → poll → completion, asserting `session.updateClientTools` is called (via the harness's mock transport, the same call-recording technique `MockTransport` already uses for `register_driver`, per WI 0006's Test Considerations) exactly on the empty→non-empty and non-empty→empty transitions, never redundantly.
- **Unit — shell-escaping for `{model}`/`{prompt}` substitution**: reuse WI 0006's exact scripted shell-metacharacter payload suite, parametrized across `PromptDelivery::Arg` and `::Stdin`, asserting `Stdin` mode never places the raw prompt into the constructed argv at all.
- **Integration — lifecycle events, remote success/failure/timeout/cancel paths**: assert the ordered event sequences (`start→running→completed`, `start→running→failed`, `start→running→failed{reason:timeout}`, `start→running→cancelled`) each carry `"dispatch":"remote"` and correct `subagent_id`/`harness`/`model` fields.
- **Integration — cross-profile/allowlist gating**: a `launch_subagent` client tool name absent from a profile's `allowed_tools` is rejected by `enforce_tool_allowlist` exactly like any other client tool (regression against the existing test suite, extended with this tool name); a `Sandboxed{image}` remote target naming an image outside the session's profile's `available_sandboxes` is rejected identically to today's Auto sandbox-tool behavior — no new code path, just a new test case proving no special-casing crept in.
- **Integration — session close teardown**: a still-`Running` remote subagent at close time is killed and logged `cancelled{reason:session_close}`; a still-tracked local subagent triggers the harness's own kill + `reportLocalSubagent` call on `Session::close()`, mirrored across all three SDKs.
- **Remote launches are always sandboxed**: assert `Remote`'s only constructible shape is `Sandboxed{image}` — there is no `Remote(Unsandboxed)` value the type system permits — and that a `subagent` dispatch against a remote target with no already-started sandbox is rejected with the same "no sandbox started" error Auto sandbox tools already produce (`session.rs:409-419`), never falling back to a bare-host exec.
- **Truncation**: a mock subprocess producing output well past the cap results in a `tool_result`/status payload with `"truncated": true` and output capped at the documented limit, never an unbounded string or an OOM in the test process.
- **Cross-SDK parity**: extend the `harness-smoke` scripted scenarios (per `aspec/genai/agents.md`'s Agent 2, offline mock provider) in all three SDKs to exercise the local launch→poll→complete sequence, asserting byte-for-byte identical event/tool-call sequences across Rust/TypeScript/Python — same convention as the existing `MCP_PARITY_SEQUENCE`.
- All new tests remain fully offline: the server-side subprocess spawn is exercised only through the injectable `CommandRunner` mock (no real `claude`/`codex`/`opencode` binary required in CI), and client-side tests inject an equivalent fake subprocess runner — identical posture to WI 0006's Test Considerations closing bullet.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Only three execution options exist for a subagent: Local/Unsandboxed (harness), Local/Sandboxed (harness), and Remote/Sandboxed (`baesrv`) — this is a deliberate scoping decision (part A), not an oversight: `Remote(SubagentLaunch)` has no bare-host/`Unsandboxed` value to construct in the first place, since `baesrv` must never spawn a subagent subprocess directly on its own host. Do not add a "remote unsandboxed" mode, a config flag to enable one, or a `SubagentLaunch::Remote` variant that admits it, in this or any follow-on work.
- `engine/subagent.rs` mirrors `engine/sandbox.rs`'s (and, transitively, `engine/mcp.rs`'s) hand-rolled-subprocess shape and doc-comment structure ("Why hand-rolled" / "Lifecycle") exactly — a reviewer already familiar with either file should recognize `subagent.rs`'s shape immediately. Reuse the `CommandRunner` injectable-subprocess-seam trait from `sandbox.rs:141-165` rather than defining a parallel one.
- New `AppState.subagents` follows the identical `Arc<Mutex<HashMap<_, _>>>`, torn-down-in-`api/client/sessions.rs::close` shape `sandboxes`/`mcp_sessions`/`sandbox_status` already establish — add subagent teardown to that same function, not a separate path.
- `run_turn`'s new fourth dispatch bucket (`subagent`, alongside `client`/`mcp`/`sandbox`) must be structured as a fourth symmetric branch, not a bolted-on special case — the same admonition WI 0006's own Codebase Integration section already makes about the three-bucket split applies doubled to a four-bucket one.
- No new profile fields: reuse `allowed_tools` (tool-name gating, `sessions.rs:148-166`) and `available_sandboxes` (image gating, `rpc.rs:1047-1059`) verbatim — do not add a parallel "allowed subagent harnesses" or "subagent images" field; the developer's own `SubagentDef` registration code is already the trust boundary for which CLI harnesses exist at all (identical posture to `run_shell_named`'s fixed command template being the trust boundary, not a server-side allowlist).
- `session.reportLocalSubagent` / `session.cancelSubagent` / `session.updateClientTools` slot into the existing `rpc()` method match (`api/client/rpc.rs:154-322`) beside `registerDriver`/`sendMessage`/`startRemoteSandbox`/`reportLocalSandbox`, with the same `require_registered_driver` gating (`rpc.rs:940-954`) and NDJSON terminal-response framing — no new transport concept.
- Keep the three SDKs behaviorally identical (`aspec/genai/agents.md`'s stated invariant); `client-rust`'s `ToolHandler` is already async (`tool.rs:26`, confirmed landed from WI 0006) so no further breaking signature change is needed there for this work item.
- `baectl` does not currently expose `available_sandboxes` configuration at all (a pre-existing gap, not introduced here) — this work item does not add subagent-specific `baectl` flags either; out of scope, but do not assume `baectl` can already configure the image allowlist a `Sandboxed`/`Local{image}` subagent target depends on.
- Verify `make build`/`test`/`lint`/`fmt` pass across `server/`, `client-rust/`, `client-typescript/`, `client-python/` with the new module in each; all new tests stay fully offline per Test Considerations, so `make test` remains network- and binary-free exactly as it is today.
