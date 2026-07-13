# Work Item: Bug

Title: split tool-call dispatch — client-side and server-side tools in one assistant turn
Issue: issuelink

## Summary:

- When a single assistant turn contains **both** a client-dispatched tool and a
  server-dispatched tool (an MCP tool or an Auto-mode sandbox tool), baesrv
  mis-routes the whole turn and the run crashes.

`server/src/engine/session.rs::run_turn` classifies every `tool_use` block in a
turn as `client`, `sandbox`, or `mcp` (`session.rs:334-359`, already emitted as
per-tool `tool.call` events with a `dispatch` tag). But the actual **dispatch
decision** is not per-tool: `has_client_tool` (`session.rs:362-383`) checks
whether *any* block is a client tool and, if so, ships the **entire** assistant
message — including the MCP/sandbox blocks — to the client via
`server.message.send`, then pauses. The client SDK harness then walks every
`tool_use` block, finds a name it never registered (e.g. an MCP tool), and
aborts the whole run with a hard error (`client-rust/src/error.rs:50`,
"server requested unknown tool '…' not in the registry").

Observed repro (issue-triage example, `TRIAGE_EXEC_MODE=none`): the model
answered one turn with *"let me fetch the issue and explore the repo
simultaneously"* and emitted two parallel tool calls in the same assistant
message — `issue_read` (GitHub MCP, server-dispatched) **and** `explore_files`
(builtin file tool, client-dispatched). Because `explore_files` is a client
tool, baesrv handed both blocks to the client; the harness had no `issue_read`
handler and killed the run. The GitHub MCP tool was never dispatched at all.

This is latent in **every** `none`/`local-sandbox` session the moment the model
parallelizes a client tool with an MCP or sandbox tool — which modern parallel
tool calling makes routine — and it is not specific to any repo, provider, or
container engine.

The fix is a client/server collaboration: **each tool call in a multi-tool
assistant turn must be routed to its correct executor** — MCP/sandbox tools
dispatched server-side, client tools handed to the client — the two result sets
reassembled into the single `user` turn the provider requires, and the client
SDKs taught to execute only the tools meant for them and treat the rest as
informational (never fatal).

## User Stories

### User Story 1:
As an: Agent Developer

I want to:
have my agent call a client-side tool and a server-side MCP (or sandbox) tool in
the *same* assistant turn — exactly what the model does when it parallelizes
independent work — and have both execute and both results feed back into the
conversation, instead of the run crashing with "unknown tool"

So I can:
write agents that compose builtin/file/shell tools with MCP tools freely,
without hand-constraining the model to one tool per turn or avoiding client
tools entirely just to dodge a routing bug

### User Story 2:
As a: Client SDK / Harness Author

I want to:
have the harness dispatch only the tool calls that belong to *this* client and
treat every other `tool_use` block in an assistant message as
informational-for-display, rather than assuming every block in the message is
mine to execute

So I can:
render "the server is running these MCP/sandbox tools" in the UI while my code
only runs its own registered tools, and never crash on a block that was never
mine to handle

### User Story 3:
As a: Dashboard / Observability Developer

I want to:
see the full per-tool picture of a mixed turn in the event stream — each
`tool.call` tagged with its `dispatch`, each MCP/sandbox `tool.result` logged
live while the client works its own tools, and the client's `tool.result`s as
they return

So I can:
show a user everything an agent did in a turn (server-side and client-side, in
parallel) without needing to reverse-engineer routing from tool names.

## Implementation Details:

This builds directly on the turn loop and FIFO turn lock shipped in
`aspec/work-items/0005-parallel-client-handling.md`
(`server/src/engine/session.rs::run_turn`, `server/src/api/client/rpc.rs`
`drive_send_message`, `AppState.pending_turns`/`PendingTurn`). It has five
pieces: (A) per-tool split dispatch in `run_turn`; (B) persisting the
server-side results across the pause and reassembling them with the client's
results on resume; (C) the `server.message.send` wire-format change so the
client can tell blocks apart; (D) the three SDK harnesses; (E) docs.

Scope note on "server-dispatched": throughout, "server-dispatched" means both
Auto-mode **sandbox** tools (`dispatch: "sandbox"`, `session.rs:401-520`) and
**MCP** tools (`dispatch: "mcp"`, `session.rs:522-574`). The current
`has_client_tool` bug leaks *both* kinds to the client when mixed with a client
tool, so the fix must handle both. "Client-dispatched" means a tool the acting
client declared at session open (`client_tool_names`, `session.rs:336`).

### A. Per-tool split dispatch in `run_turn`

Today the tool-handling section is an all-or-nothing fork: `has_client_tool` →
pause with the whole message (`session.rs:362-383`); else → dispatch every block
server-side in-process and loop (`session.rs:385-…`). Replace the fork with a
**partition**:

- After the per-tool `tool.call` events are emitted (`session.rs:334-359`, keep
  as-is — the `dispatch` tag computed there becomes the partition key),
  split `tool_uses` into:
  - `server_uses` — blocks whose `dispatch` is `"sandbox"` or `"mcp"`;
  - `client_uses` — blocks whose `dispatch` is `"client"`.
- **Always dispatch `server_uses` first, server-side**, reusing the existing
  sandbox (`session.rs:401-520`) and MCP (`session.rs:522-574`) dispatch code
  verbatim — including their `sandbox.request`/`sandbox.response`,
  `mcp.request`/`mcp.response`, and `tool.result` event logging, so observers
  see server-side work live even while the turn later pauses for the client.
  Collect each server tool's `tool_result` block (the same shape pushed to
  `result_blocks` today, `session.rs:513-518`) into a `Vec<Value>`
  `server_tool_results`.
- Then branch on `client_uses`:
  - **`client_uses` empty (all-server turn):** unchanged behavior — append
    `server_tool_results` to history in-memory and continue the provider loop
    (`session.rs:390`). No pause, no persistence change. This is the common MCP
    path and must not regress.
  - **`client_uses` non-empty (mixed or all-client turn):** persist the
    assistant message as `server.message.send` exactly as the current pause path
    does (`session.rs:369-377`), then return `Outcome::Paused` — but now carry
    `server_tool_results` out of `run_turn` so the caller can stash them across
    the pause (see B). The persisted assistant message still contains **all**
    `tool_use` blocks (both client and server), because the provider requires
    the following `user` turn to answer every `tool_use` id in this assistant
    turn.
- Extend the returned `Turn` (`session.rs`, the `Turn { message, events, outcome }`
  struct) with `pending_tool_results: Vec<Value>` — empty except on a mixed-turn
  `Paused`, where it holds `server_tool_results`. (All-client turns keep it
  empty, so pure-client behavior is unchanged.)

Generality: this also fixes a client+**sandbox** mix (today equally broken —
`has_client_tool` leaks Auto-mode sandbox blocks to the client), not just
client+MCP.

### B. Persist server results across the pause; reassemble on resume

The load-bearing constraint: provider history is replayed **only** from
`client.message.send` and `server.message.send` events
(`server/src/store/sessions.rs::stream_history`, `sessions.rs:298-301`) —
`tool.result` events are **not** replayed. And the provider (Anthropic Messages
API) requires every `tool_use` in the assistant turn to be answered by a
`tool_result` in the **single** immediately-following `user` message; roles must
alternate, so two separate `user` turns (one server-side, one client) will not
work. Therefore the server-side results and the client's results must land in
**one** `client.message.send` event.

Do the merge at resume time, reusing the `PendingTurn` machinery WI 0005 already
parks across the pause — **no `stream_history` change required**:

- Extend `PendingTurn` (`server/src/api/client/rpc.rs`, defined per WI 0005
  around `rpc.rs:398-450`/`619-624`) with `server_tool_results: Vec<Value>`.
- In `drive_send_message`, on an `Outcome::Paused` whose `Turn.pending_tool_results`
  is non-empty, stash those blocks into the parked `PendingTurn` alongside the
  existing guard/owner/deadline (`rpc.rs:619-624`).
- On resume (the same owner returns with its client `tool_result`s), **before**
  recording the incoming message as `client.message.send` (`rpc.rs:499-510`),
  merge the stashed `server_tool_results` into `message.content`:
  - Concatenate so the merged `user` message contains a `tool_result` for
    **every** `tool_use` id in the preceding assistant turn.
  - Order the blocks to match the assistant turn's `tool_use` order (cosmetic,
    but keeps transcripts readable).
  - Server-side results **win**: if the client erroneously returned a
    `tool_result` for a server-dispatched id (it should not — see D), drop the
    client's copy for that id and keep the server's.
  - Log the server-side `tool_result` blocks that were merged in as their own
    `tool.result` events too (mirroring the loop at `rpc.rs:511-523`) if they
    were not already logged at dispatch time — but per A they *are* logged at
    dispatch time, so this is just the existing client-block loop; do not
    double-log the same block.
- The single merged `client.message.send` now carries all results;
  `stream_history` replays `…, assistant(all tool_uses), user(all tool_results), …`
  and the next provider call is well-formed. `run_turn` continues normally.

Concurrency ("simultaneously" from the design discussion): v1 dispatches
`server_uses` synchronously *before* pausing, so the client and the server do
not literally run at the same wall-clock instant. This is deliberate — MCP/sandbox
latency is normally far below the client's own tool work (e.g. `git clone` +
`explore_files`), and true cross-pause backgrounding needs a session-scoped
background task that persists its results independently of the HTTP request that
started it. Note that as a **v2** optimization (spawn `server_uses` as a
detached task, join on resume) and keep v1 correct and simple.

### C. Wire format: mark each `tool_use` block's dispatch for the client

The message the client executes from (`server.message.send` content) currently
carries `id`/`name`/`input` and a non-standard `caller` object per block, but
**not** the `dispatch` tag (that lives only on the separate `tool.call` event).
The client cannot filter blocks it should skip without it.

- When persisting/sending the mixed-turn assistant message, add a `dispatch`
  field (`"client"` | `"sandbox"` | `"mcp"`) to each `tool_use` block, matching
  the value on that tool's `tool.call` event.
- **Provider-replay safety:** `server.message.send` is both sent to the client
  *and* replayed into provider history by `stream_history`. baesrv already
  carries the non-standard `caller` field on persisted blocks without breaking
  replay, but do not rely on that by accident: ensure the provider request
  builder strips non-standard block fields (`dispatch`, `caller`) before the
  upstream call — check/adjust `server/src/engine/provider.rs`
  (`to_openai_messages`, `provider.rs:406`, and the Anthropic request path) so
  neither field is sent to the LLM. Add a regression test asserting a replayed
  `tool_use` block reaches the provider body without `dispatch`/`caller`.

### D. Client SDK harnesses (all three: rust / typescript / python)

The harnesses must stop assuming every `tool_use` block in an assistant message
is theirs to run. New rule, identical across SDKs:

- Iterate the assistant message's `tool_use` blocks. For each block, decide if it
  is **ours** to execute:
  - Authoritative signal: `dispatch == "client"` (from C).
  - Fallback when `dispatch` is absent (older server): the tool name is in this
    client's registered-tool set.
- Execute only "ours"; produce a `tool_result` only for those, and send back a
  message containing exactly that set.
- Every other block is **informational**: surface it to application code / UI
  (so a dashboard can show "server is running `issue_read`") but do **not**
  execute it and do **not** synthesize a `tool_result` for it — the server owns
  that result (B).
- The existing hard error (`client-rust/src/error.rs:50`, and its TS/Python
  equivalents) must fire **only** for a block that *is* ours (`dispatch:client`
  / registered) yet has no registered handler — the genuine
  "declared-tool-set vs profile-allowlist out of sync" case the error comment
  already describes (`error.rs:49`). A server-dispatched block must never reach
  that path.

Files (verify exact loop per SDK during implementation):
- `client-rust/src/harness.rs` (tool-execution loop; `error.rs` UnknownTool
  variant becomes conditional on dispatch/registry).
- `client-typescript/` harness equivalent.
- `client-python/` harness equivalent.

Keep the three behaviorally identical (the repo's stated invariant that the
example agents behave identically across SDKs — `aspec/genai/agents.md`).

### E. Documentation

- `docs/reference/wire-protocol.md` (or the apis reference,
  `aspec/architecture/apis.md`): document (1) the `dispatch` field on
  `server.message.send` `tool_use` blocks, (2) the contract that a client
  executes only `dispatch:client` blocks and returns only those results, and
  (3) that the server dispatches and answers `sandbox`/`mcp` blocks itself and
  merges both result sets into the single following `user` turn.
- Update the `run_turn` doc comment (`session.rs:104-…`) to describe the
  partitioned dispatch and the mixed-turn `Paused` carrying
  `pending_tool_results`.

## Edge Case Considerations:

- **Mixed turn where the client tool errors or the client abandons the turn.**
  The server-side results were already dispatched and stashed in `PendingTurn`;
  on `BAE_TURN_TIMEOUT` abandonment (WI 0005, `rpc.rs` step 2) they are dropped
  with the parked turn. No orphaned state; the whole turn is abandoned. Confirm
  the stashed `server_tool_results` are freed when the guard is dropped.
- **All-server turn must not regress.** `client_uses` empty → in-process loop,
  no pause, no `client.message.send` for intermediate results. This is the
  hot MCP path; guard it with an explicit test.
- **All-client turn must not regress.** `server_uses` empty → pause with an
  empty `pending_tool_results`; resume merges nothing; identical to today.
- **Client wrongly returns a result for a server-dispatched id.** Server result
  wins on merge (B); the client copy is dropped. A well-behaved SDK (D) never
  sends one.
- **Duplicate / missing tool_use ids after merge.** Before the resume provider
  call, assert the merged `user` turn answers exactly the set of `tool_use` ids
  in the preceding assistant turn — no missing (would 400 at Anthropic), no
  duplicates. Fail loudly server-side with a `session.error` rather than
  forwarding a malformed body upstream.
- **`dispatch`/`caller` leaking to the provider.** Covered in C; test that the
  provider body is clean.
- **Tool name collision** between a client-registered tool and an MCP tool of
  the same name. `dispatch` from the server is authoritative (C/D); the SDK must
  prefer it over local-registry membership so a collision routes correctly.
- **Unroutable "mcp" block** (model called a tool advertised by no live MCP
  server; `mcp_routes.get(name)` is `None`, `session.rs:568-574`). Already
  handled server-side as an error-shaped `tool.result` and the turn continues —
  it must **not** be reclassified as a client tool by the partition. Confirm an
  unroutable block stays in `server_uses`.
- **Multi-driver sessions (WI 0005).** Only the acting driver's client tools are
  `dispatch:client`; another driver's private tools are not advertised on this
  turn, so they never appear as `tool_use` blocks. The partition uses the acting
  client's `client_tool_names`, unchanged.

## Test Considerations:

- Server unit test: a synthetic assistant turn with one `mcp` block + one
  `client` block → `run_turn` dispatches the MCP block (asserts
  `mcp.request`/`mcp.response`/`tool.result` logged), returns `Paused` with
  `pending_tool_results` holding the MCP result, and the persisted
  `server.message.send` contains both blocks each tagged with `dispatch`.
- Server unit test: resume with the client's single-tool result → the recorded
  `client.message.send` contains **both** results, ordered, ids complete;
  `stream_history` yields a well-formed alternating transcript.
- Server regression: all-server turn (two MCP blocks) still loops in-process
  with no pause and no `client.message.send`; all-client turn unchanged.
- Server test: client+**sandbox** mix routes the sandbox block server-side (the
  generalization in A).
- Provider-body test: replayed `tool_use` blocks reach `provider.rs` without
  `dispatch`/`caller`.
- Merge-validation test: a resume missing a server id, or with a duplicate id,
  raises `session.error` instead of calling the provider.
- SDK tests (rust/ts/python, kept identical): a `server.message.send` with a
  `dispatch:mcp` block + a `dispatch:client` block → the harness executes only
  the client tool, returns only its result, exposes the MCP block as
  informational, and does **not** raise UnknownTool. A `dispatch:client` block
  with no handler **does** raise the (existing) error.
- End-to-end: re-run the issue-triage example (`TRIAGE_EXEC_MODE=none`) against a
  repo whose triage provokes a parallel `issue_read` + `explore_files` turn;
  assert it completes instead of crashing. This is the original repro.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Mirror WI 0005's use of `AppState`/`PendingTurn` for cross-pause state rather
  than inventing a parallel mechanism; extend the closed `EventType` set only if
  a genuinely new event is needed (this work reuses `tool.call`, `tool.result`,
  `mcp.request`/`response`, `server.message.send`, `client.message.send` —
  likely **no** new event types).
- Preserve the "three SDKs behave identically" invariant (`aspec/genai/agents.md`).
- Keep the all-server MCP path (the common case) allocation- and latency-neutral;
  the new work happens only when a turn actually mixes client and server tools.
