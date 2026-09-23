# Work Item: Task

Title: harden session compaction (0016) and the first-run experience (0017); make the quickstart a true few-command path
Issue: issuelink

## Summary:
- Post-merge audits of WI 0016 (session compaction, `c893f20`) and WI 0017 (baectl `build`/`ready`/`run`, `8fbfe66`) found defects ranging from release-blocking to cosmetic. This work item fixes every one of them and reworks the getting-started docs so that the `baectl` path is the first, shortest, and verified way into BAE.
- **Release blockers:**
  - `make image`/`make image-max` can no longer build, because `baectl` gained a path dev-dependency that the Dockerfiles don't copy (§B1).
  - Every turn after a compaction on an Anthropic provider fails with a 400, because post-compaction history starts with an `assistant` message (§A1).
- **Other high-severity items:**
  - None of the three SDKs can use compaction at all (§A8).
  - `ready --fix`/`run` wipe a profile's `available_sandboxes` when widening it (§B2).
- The quickstart must get a new user from a clean checkout to an agent answering a question in the fewest possible commands, with no manual profile/key/env wiring and no manual walkthrough in the way (§C).

**Builds on:** `aspec/work-items/0016-session-compaction.md`, `aspec/work-items/0017-extremely-simple-first-run-experience.md`, `aspec/work-items/0012-baectl-quickstart.md`. It does not change their designs except where a section below says so explicitly.

## User Stories

### User Story 1:
As a: New BAE user

I want to:
follow the README's Quickstart and run `baectl setup --yes`, `baectl build reference-assistant`, and `baectl run <printed id>` (after the one-time `baectl` install and exporting my provider key), then see the agent answer

So I can:
get a working agent without reading a walkthrough, answering prompts, or minting profiles and keys by hand

### User Story 2:
As a: BAE contributor

I want to:
run `make image` and then the same loop with `--dev`, and have every step work against my checkout

So I can:
trust that the dev loop and the release images actually build, and that the documented commands are the ones CI exercises

### User Story 3:
As an: Agent developer using compaction (any SDK)

I want to:
set `compaction: {mode: "auto", size: N}` or `{mode: "client"}` when opening a session from the Rust, TypeScript, or Python SDK, call `session.compact()` when I choose, and keep chatting on an Anthropic provider afterwards

So I can:
run long conversations without hitting the context limit, through the SDK instead of raw JSON-RPC

## Implementation Details:

### A. WI 0016 follow-ups (session compaction)

**A1. Post-compaction history must be a valid provider message sequence (High, release-blocking).**
- **The problem:** `run_compaction` stores the summary as `role: assistant` (`server/src/engine/session.rs` ~1291). `stream_history` then starts at the summary, so every later provider request begins `[assistant(summary), user, …]`. Two things make this fail:
  - The Anthropic Messages API rejects a first message that isn't `user` with a 400 ("First message must be `user`"), and also rejects consecutive same-role messages.
  - The Anthropic request path (`strip_nonstandard_block_fields`, `server/src/engine/provider.rs:374`) passes roles through unchanged.
  - The session therefore ends in `ProvidersFailed` on its first turn after any compaction.
- **Why flipping the role doesn't work:** simply storing the summary as `user` is not enough, because the next real message is also `user`.
- **Required fix:** the compacted history must replay as `user(preamble) → assistant(summary) → user(next real message) …`. The preamble is a **persisted event**, like every other message the server sends to a provider, so the log shows exactly what each provider request contained. Nothing is synthesized at history-build time, and there is no provider input that can't be found in `session_events`.
  - **Write order:** `run_compaction` writes, in this order and in one SQLite transaction (see A6):
    1. the preamble: a `server.message.send` event with payload `{"role": "user", "content": [{"type": "text", "text": "<preamble text>"}], "synthetic": "compaction_preamble"}`
    2. the summary: the existing assistant `server.message.send`
    3. `session.compaction.completed`
  - **Preamble text:** a fixed server constant, for example "The earlier part of this conversation was compacted. A summary follows." It must be deterministic, so prompt-cache prefixes stay stable across turns.
  - **The `synthetic` marker:**
    - It's the only thing that distinguishes the preamble from a real user message.
    - It must be a documented payload field, not an out-of-band flag.
    - It is never attributed to a client, because it's a `server.*` event and not a `client.message.send`.
    - Its value names why the message exists, so future synthetic messages can reuse the field with their own value.
  - **The `completed` event:** its payload gains `preamble_event_id` next to `summary_event_id`.
  - **Where history starts:** `history_lower_bound` (`server/src/store/sessions.rs:491`) scopes from the preamble's rowid when `preamble_event_id` is present and valid.
    - If it's absent, as for sessions compacted before this change, fall back to today's summary-based bound. The Anthropic normalizer below then keeps those sessions working.
    - Keep the existing rule that a broken reference means the full history, never lost context.
  - **Building messages from history:** confirm the history-to-provider conversion takes `role` from the `server.message.send` payload, so a `role: "user"` server message becomes a provider `user` message. Fix it if it assumes every `server.message.send` is an assistant message.
  - **Second compaction:** it naturally receives `user(preamble) → assistant(summary) → …` as input. No special case.
- **Defensive layer:** add a provider-request normalization step for Anthropic that guarantees the first message is `user` and merges consecutive same-role messages. The normalizer is a safety net, not the primary fix.
  - Because the normalizer would otherwise change provider input without a trace, whenever it actually alters the message list it must record what it did in that attempt's `provider.request` payload: a `normalized` field listing each change, for example `{"prepended_user": true}` or `{"merged": [[3, 4]]}`.
  - The persisted `provider.request` must always be the exact body sent to the provider.
- **OpenAI:** it already accepts either shape; keep its output unchanged, or document any change.

**A2. `session.compact` must not hang behind a paused turn (Medium).**
- **The problem:** `compact_rpc` waits on the turn gate (`server/src/api/client/rpc.rs:1135`) without checking `pending_turns`. A paused turn parks its gate lock until a `session.sendMessage` reclaims it.
  - A harness that pauses on a tool call and then calls compact deadlocks against its own turn.
  - An observer's compact blocks forever if the paused driver disappears.
- **Required fix:** if the session has a pending (paused) turn, fail fast with a JSON-RPC error (`turn in progress: resolve the paused turn before compacting`). Do not wait.
  - A pause whose turn timeout has already expired should be reclaimed exactly as `sendMessage` reclaims it, and the compaction then proceeds.
  - Document the new error.

**A3. Auto-compaction events in `result.events` (Low).** `maybe_auto_compact` pushes only the `completed` record into `Turn.events`. It should include the full compaction record in order:
- `session.compaction.started`
- the compaction `provider.request` / `provider.response`
- the summary `server.message.send`
- `session.compaction.completed`

That makes the terminal `result.events` the turn's full log, which is the existing convention. Update spec §5.7's wording in `0016` to match.

**A4. Failed auto compaction backoff (Low).** Today a failed auto compaction retries on every turn. Change that:
- After a failure, suppress auto retries until the history has grown by a meaningful amount: at least one more completed turn **and** a token count above the last failed attempt's count.
- Record the reason in the next `session.compaction.started` payload (`reason: "retry_after_failure"`).

**A5. Truncated or empty summaries (Low).** Two cases must fail instead of committing a summary:
- If the summary response has `stop_reason == "max_tokens"` (or the OpenAI equivalent `finish_reason == "length"`), or has empty text content, treat the compaction as **failed**.
  - Leave `started` without `completed`, and write no summary message, so the history is unchanged.
  - Surface the error as today's provider-failure path does.
- Give the compaction call its own output budget, not the profile's turn `max_tokens`. The default is the larger of the profile's value and 4096, configurable if trivial.

**A6. Atomic compaction write (Low).** Insert the preamble, the summary, and `session.compaction.completed` in one SQLite transaction, and broadcast them only after the commit. That way a store error can never leave an orphan preamble or summary that `stream_history` picks up.

**A7. Small server corrections:**
- **`join` and `compaction: null`:** `join` must reject a `compaction` key even when its value is `null`. The spec says the field is not accepted on join; presence is the error.
- **Malformed `compaction`:** a bad `mode` or a missing `size` must return the standard BAE error envelope with `400 bad_request`, not axum's default JSON rejection.
- **Token counting:**
  - The auto trigger's token count for Anthropic must include `cache_read_input_tokens` and `cache_creation_input_tokens` when present. Right now it undercounts as soon as prompt caching is used.
  - `last_provider_token_count` should skip failed `provider.response` rows, and skip the compaction call's own response.
- **Stale comment:** remove the comment at `rpc.rs` ~1222 (`CompactionFailed` arm) that says "or a response with no usage".

**A8. SDK parity for compaction (High).** All three SDKs, named idiomatically per language:
- **Session create:** a `compaction` option at session open / `connect()`, covering `Auto { size }` and `Client { prompt? }` and serialized exactly as the server's `mode`-tagged enum.
  - Rust: `client-rust/src/harness.rs` open body ~715.
  - TypeScript: `client-typescript/src/harness.ts` ~171.
  - Python: `client-python/src/bae_py/harness/core.py` ~192.
- **Compact method:** a `compact(prompt?)` method on the session/harness that calls `session.compact` and returns the typed `session.compaction.completed` record.
- **Typed payloads:** `SessionCompactionStarted` / `SessionCompactionCompleted` in Rust and Python (TypeScript already has them), and `usage?: {input_tokens, output_tokens} | null` on TypeScript's ok-variant `ProviderResponsePayload` (`client-typescript/src/types.ts:185-193`).
- **Docs:** remove the "no dedicated Session method in any of the three SDKs yet" caveat and its raw JSON-RPC workaround (`docs/guides/01-building-a-client.md:422-432`), and replace it with the SDK calls.

**A9. Compaction docs corrections:**
- **Event order:** `docs/guides/06-event-streaming.md:257-270` shows the terminal `sendMessage` result arriving **before** `session.compaction.started`. The server emits compaction events first. Fix the diagram so it agrees with `session.rs`, `01-wire-protocol.md:283-285` and `04-message-types.md:1068-1079`.
- **Errors:** document `session.compact`'s full error set:
  - "all providers exhausted"
  - `provider_config` → `compaction failed: …`
  - the new turn-in-progress error (A2)
  - truncated or empty summary (A5)
- **Return values:** document that `result.events` includes the compaction record after an auto compaction (A3).
- **Stale event counts:** update "27 event types" to 28 in:
  - `client-typescript/src/types.ts:136`
  - `client-typescript/README.md:71`
  - `client-python/README.md:71`
  - `client-python/tests/test_types.py:63`
  - `docs/guides/00-quickstart.md` Next steps
  - the MAX comment at `max/web/src/api/eventTypes.ts:1`
- **Grep check:** grep the repo for any other stale count.

### B. WI 0017 follow-ups (baectl build / ready / run)

**B1. Restore `make image` / `make image-max` (Critical, release-blocking).** `baectl/Cargo.toml` has a `launcher-api = { path = "../launchers/api" }` dev-dependency, but `Dockerfile:19-20` and `Dockerfile.max:20-21` copy only `server/` and `baectl/`, so cargo fails to load the manifest. Fix it one of two ways:
- (preferred) Remove the dev-dependency, and replace the round-trip test with a checked-in copy of the launcher config schema's fixtures. Guard it with a test in `launchers/api` that parses baectl's generated output, so the dependency points the other way and never enters the image build.
- Or `COPY launchers/api/` in both Dockerfiles.

Either way, add a CI job (or extend an existing one) that runs `make image` so this class of break is caught before merge.

**B2. Additive profile widening must preserve every field (High).** `update_fix_args` (`baectl/src/harness/checks.rs:451-486`) omits `available_sandboxes`, and the server treats a missing field as `[]` (`server/src/api/admin/profiles.rs:76`). Widening `default` for reference-assistant therefore wipes `alpine/git` for issue-triage.
- Carry every field the profile update accepts, and unite `available_sandboxes` with the harness's declared sandbox images.
- If `baectl update profile` has no sandbox flag, add `--available-sandbox`.
- Add a regression test asserting that a widen never removes any existing value from any list field.

**B3. Don't create keys that then fail (Medium).** `checks.rs:332-378` applies fixes #2 and #4 before gating on the non-fixable checks #3 and #5, so every failed `run`, and every `ready --fix` answered y, creates an orphaned client key. Change the order:
1. Evaluate all checks.
2. If any non-fixable check fails, abort **before** mutating anything.
3. Only then apply fixes.

A key whose secret can't be persisted to `resolved.json` must never be created.

**B4. Relative `--dir` breaks container `run` (Medium).** `run.rs:155-166` passes `dir.join(".env")` and the `harness.env` path relative, while also setting the child's working directory to `dir`. Canonicalize `--dir` once at argument mapping (`cli.rs:483-505`) for `build`, `ready`, and `run`.

**B5. Provider key env for non-Anthropic providers (Medium).** The examples read `BAE_PROVIDER_KEY_ENV` (default `ANTHROPIC_API_KEY`), and `run` never sets it. With an OpenAI provider, `ready` passes and the harness then exits with "ANTHROPIC_API_KEY is not set".
- `run` must export `BAE_PROVIDER_KEY_ENV=<the provider's env var>` for both local and container kinds.
- Check #5 must validate that variable, so the harness requirement and the check agree.

**B6. Clean build context (Medium).** Generated container builds send the host `target/`, `node_modules`, and `.venv` as build context, and `COPY . .` puts them in `/build`. That risks shipping a host-linked binary.
- Write a `.dockerignore` into the generated build context (or pass an equivalent exclude list) covering `target/`, `node_modules/`, `.venv/`, `__pycache__/`, `.baectl/`, and `.git/`.
- Add `.dockerignore` files to `client-rust/`, `client-typescript/`, and `client-python/` as a second layer.

**B7. Make `ready` friendly on a fresh setup (Medium, first-run UX).** On a fresh `setup`, `ready` (without `--fix`) always reports ✗ for check #4, because only keys recorded in `resolved.json` count. A new user who runs `ready` as the docs suggest sees failures that `run` would fix silently.
- Report checks that `run` / `--fix` resolve automatically as `⚠ will be fixed by run` rather than ✗.
- Use a distinct exit code for "only auto-fixable issues" versus "blocking issues", and document both in `docs/reference/03-baectl.md`.
- Fix hints must be copy-pasteable as printed. Today they print a host `baectl update profile …` that can't reach the loopback-only admin port. Print the `docker compose exec baesrv baectl …` (or `container exec bae …`) form that matches how `setup` launched, and say that `--fix` also records the key.

**B8. Harness env handling (Low–Medium).** Check #5 ignores values already saved in `harness.env`, and it aborts `run` before `resolve_container_env` can prompt, so the prompt is unreachable except via `--no-ready`.
- Check #5 must count `harness.env` values as present.
- In an interactive TTY, `run` must reach the prompt for missing container env vars instead of aborting; with no TTY, abort as now.

**B9. Manifest and name validation (Low).**
- Add `#[serde(deny_unknown_fields)]` to the `bae-harness.toml` structs, so that a typo such as `allowed_tool` exits 2 naming the unknown key.
- Validate the harness name and `--id` against Docker's tag and container-name rules (lowercase `[a-z0-9][a-z0-9_.-]*`), exiting 2 with a precise message.
- Only mention `--id` in the error when `--id` was actually given.

**B10. External harness layout (Low).** Generated build and run defaults assume the bundled examples' layout: `--example <name>`, `examples/<name>/main.*`, `pip install .`, and `npm run build`. Add optional `bae-harness.toml` fields for `build` / `run` overrides per launcher kind (`[harness.container] build = "…"`, `entrypoint = "…"`). Document that a harness outside the bundled layout must set them, and remove the claim that "nothing extra is required of the harness author" where it isn't true.

**B11. Engine-gated test fixes (High for CI trust).**
- The mutation-free snapshot test (`baectl/tests/harness_engine.rs:318-327`) compares `list keys` output that includes `last_used_at`. Project that column out, or compare ids/profiles only.
- `build` shells out to `date` for `created_at` (`build.rs:675-684`), which breaks the "no host toolchain" test's `PATH`. Generate the timestamp in Rust.
- Run the gated suite (`BAECTL_HARNESS_ENGINE_TESTS=1`) in CI on a Docker-capable runner, including at least one container build per SDK. Container builds and the TypeScript 7 build inside the generated Dockerfile have never been exercised end to end.

**B12. Housekeeping (Low).**
- Deduplicate `parse_max_port` (`checks.rs:90`, `setup.rs:496`).
- Delete the now-unused `server/tests/fixtures/*.py` MCP fixtures replaced by `mcp_test_fixture`.
- Add a line to `aspec/work-items/0017` or the changelog recording the MCP spawn-deadline change (4s → 15s, `server/src/engine/mcp.rs`) that shipped with 0017 without being specified there.

### C. Fastest-possible getting started (docs + small baectl UX)

**Goal:** the README and `docs/guides/00-quickstart.md` open with a copy-paste block that, after installing `baectl` and exporting a provider key, is exactly:

```sh
baectl setup --yes
baectl build reference-assistant
baectl run reference-assistant-rust-local
```

That block runs with no prompts, no manual steps between commands, and nothing that silently depends on the reader having done a walkthrough.

**C1. `baectl setup --yes` (non-interactive).** Accept every wizard default and pick the provider from the environment:
- `ANTHROPIC_API_KEY`, then `OPENAI_API_KEY`, in the wizard's existing detection order.
- If no provider key is found, exit non-zero with the single `export …` line to run.

It must be idempotent: re-running `setup --yes` in a directory that already has a running setup reports "already set up" and exits 0 instead of prompting Edit/Launch.

**C2. Local TypeScript runs with no manual `npm install`.** Add an optional `prepare` command to `bae-harness.toml` (e.g. `prepare = "npm install"` for the TypeScript examples; skipped for Rust and Python, which prepare themselves). `run` executes it for `--launcher local` only when needed; for `npm`, a missing `node_modules` or a lockfile newer than it. Remove the manual `(cd client-typescript && npm install)` step from both quickstarts.

**C3. Restructure `docs/guides/00-quickstart.md`:**
- **Top:** a short prerequisites list, then the one install step for `baectl` (source build today). Then the three commands, what you'll see, and "next: try the webapp launcher" as one more `build`/`run` pair.
- **Keep it short:** a first-time reader should reach a working agent without scrolling past anything they don't need.
- **Move the rest:** the manual walkthrough (Parts 1–3: hand-made profile and key, per-SDK example commands, the echo webapp) goes to a separate page, `docs/guides/00a-quickstart-step-by-step.md`, linked once from the quickstart as "want to see each layer by hand?".
- **Troubleshooting:** keep the section, but reorder it around the `baectl` path first: what `ready` output means, `setup --yes` provider-key errors, and Docker not running. Manual-path entries move with the walkthrough.
- **Remove stale claims:** that `ready` "exits non-zero" on a fresh setup (after B7), the TypeScript `npm install` step (after C2), and "27 event types".

**C4. Restructure `docs/guides/developer/00-quickstart.md`** the same way: `make image` + `make build-baectl`, then the same three commands with `--dev`, all at the top. Move the per-part build walkthrough (including the Part 3 `docker tag` retag dance) below or to a linked page. After B1, `make image` must actually work; state it once, not repeatedly.

**C5. README Quickstart.** `README.md`'s `## Quickstart` section doesn't mention `build`/`run` at all today; it describes `setup` then "create a profile and client key … and drive it". Replace it with the same copy-paste block as C3 and a link to the full quickstart. Keep the `docker run` one-liner only as a secondary "just the server" option.

**C6. Keep other docs consistent** with C1/C2/B7/B9/B10:
- `docs/reference/03-baectl.md` (`setup --yes`, `ready` exit codes, `prepare`, `--available-sandbox` if added)
- `docs/reference/07-harness-manifest.md` (`prepare`, container overrides, unknown-key rejection)
- `aspec/uxui/cli.md`
- each example README

**C7. Docs are tested.** Add a quickstart smoke test that runs the quickstart's copy-paste block verbatim, extracted from the Markdown so the docs and the test can't drift. It should run in `e2e/` or the engine-gated suite, against a mock provider (a server config pointing at the existing test mock) so no real API key is needed, and assert that the agent's reply is printed. Run it with `--dev` in CI.

**Out of scope:** the published one-line `curl | sh` installer for `baectl` (it needs release infrastructure; track it separately). Until it ships, the quickstart shows the source-build step with a single "installer coming" note, not a full placeholder code block.

## Edge Case Considerations:
- **A1:**
  - A session with zero messages after the boundary (compaction as the last event) must replay as `user(preamble) → assistant(summary)` followed by the new user message; that is valid.
  - Two sequential compactions must replay only from the latest boundary, with one preamble.
  - The preamble appears in `session.history`, observer replays, live notifications, and MAX exactly like any other persisted event, as a `server.message.send` carrying `synthetic: "compaction_preamble"`.
    - SDKs and MAX must pass the `synthetic` field through and never drop it.
    - Any UI that renders a transcript should label the preamble as a system-generated compaction marker rather than show it as something the user typed.
  - The Anthropic normalizer must not reorder or merge `tool_use`/`tool_result` blocks in a way that breaks pairing; merge by concatenating content arrays in order.
- **A2:** a compact from a non-driver still returns the existing driver-gate error before the paused-turn check. A compact during a *running* (not paused) turn still waits on the gate, as today, because that turn will release it.
- **A4:** a manual `session.compact` ignores the backoff; the backoff only suppresses automatic triggers.
- **B2:** if the harness declares nothing about sandboxes, the widen must still send the profile's existing `available_sandboxes` unchanged, never `[]`.
- **B3:** `ready` without `--fix` stays strictly read-only, as today.
- **B7:** a fresh `setup --yes` directly followed by `ready` should show only ⚠ items and exit with the "auto-fixable only" code; `run` then succeeds without prompting.
- **C1:**
  - With `--yes` and `--apple`, generate `bae-setup.sh` the same way.
  - With both Anthropic and OpenAI keys set, pick the first in the wizard's order and print which was chosen.
  - Never echo key values.
- **C2:** `prepare` failures abort `run` with the command's exit code and output; `prepare` never runs for container launchers, where the generated Dockerfile already installs dependencies.

## Test Considerations:
- **A1:**
  - Add a strict Anthropic mock provider mode (or an assertion helper on the existing mock) that rejects a first non-`user` message and consecutive same-role messages with a 400, as the real API does.
  - Enable it for all compaction integration tests.
  - The existing tests that assert the assistant-first shape (`server/tests/integration.rs` ~7576, ~7810) must be updated to the new sequence.
  - Assert on the persisted log after a compaction, not just on the provider request:
    - the preamble event, then the summary event, then `completed`, in that order
    - `completed.preamble_event_id` and `completed.summary_event_id` resolving to them
    - the preamble carrying `synthetic: "compaction_preamble"`
    - the next `provider.request` body equal to the persisted events from the preamble onward
  - Test the pre-change fallback: a seeded session whose `completed` has no `preamble_event_id` still replays, and the normalizer's `normalized` field is recorded on its `provider.request`.
- **A2:** integration test: pause a turn on a tool call, call `session.compact` from the same driver, and assert an immediate turn-in-progress error (with a short test timeout, so a regression hangs visibly rather than silently). Plus a test with an expired pause that proceeds.
- **A3–A6:** unit and integration tests for:
  - the full `result.events` ordering after an auto compaction
  - the backoff suppressing an immediate retry
  - `max_tokens` / empty summary → failed, with history unchanged
  - an injected store failure between the two writes leaving no summary behind
- **Coverage gaps from the audit to close:**
  - history scoping from an earlier *successful* compaction after a later failed one
  - an integration test of two sequential compactions
  - reading a seeded pre-0009 session row with a NULL `compaction` column
- **A8:** per-SDK unit tests:
  - serializing both `compaction` modes
  - `compact()` request/response shape, against the same JSON fixtures as the server tests where possible
  - a TypeScript type test that `provider.response` exposes `usage`
  - MAX test coverage for the two new event mappings
- **B1:** CI job running `make image` (and `make image-max`, if its runtime cost is acceptable) on every PR that touches `baectl/`, `server/`, `launchers/`, or a Dockerfile.
- **B2–B10:** each defect gets a regression test that fails on current `main`:
  - widen preserves sandboxes
  - a failed `run` creates no key (count keys before and after)
  - relative `--dir` container run
  - `BAE_PROVIDER_KEY_ENV` exported for an OpenAI provider
  - generated build context excludes `target/`
  - `ready` ⚠ vs ✗ and its exit codes
  - `harness.env` values satisfy check #5
  - unknown manifest key → exit 2
  - invalid name → exit 2
- **B11 / C7:** the engine-gated suite and the quickstart smoke test run in CI with `--dev`, including at least one container build per SDK.
- **Suites that must pass:**
  - `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` in `server/`, `baectl/`, and `client-rust/`
  - TypeScript vitest + typecheck
  - Python pytest
  - MAX vitest + `tsc -b`

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Keep the three SDKs at feature parity (`aspec/foundation.md`): no compaction capability ships in one SDK without the other two.
- Keep all durable state on the server, and make everything auditable and debuggable. Every message that reaches a provider must exist as a persisted event, and every server-side transformation of provider input must be recorded, like the A1 preamble and the normalizer's `normalized` field. Replaying `session_events` must be enough to reconstruct exactly what the model saw.
- Document the `synthetic` field and the `preamble_event_id` field in `docs/reference/04-message-types.md`, in the SDK types in all three languages, and in MAX's event rendering.
- Where this work item changes behaviour described in `0016` or `0017` (A3 `result.events`, B7 `ready` exit codes, C1 `setup --yes`, C2 `prepare`), update those specs' text so the aspec stays the source of truth, and update `aspec/uxui/cli.md` and `aspec/architecture/design.md` as needed.
- Suggested implementation order: B1 → A1 → A2 → B2/B3 → A8 → C1/C2/B7 → C3–C7 → the remaining Low items. Land B1 and A1 first, since they block releases and any real use of compaction.
