# Work Item: Feature

Title: session compaction
Issue: issuelink

## Summary:
- This work item introduces compaction for bae sessions

There are two compaction methods: auto and client

When a session is created, a new optional field may be passed which provides compaction settings for that session.

If the session is created with `compaction: {mode: auto, size: 128000}`, then the bae server must be responsible for automatically compacting a session's log when it reaches the configured size. if `compaction: {mode: client}` or NO `compaction` field is passed, then the server should not perform auto compaction and the bae client harness must be responsible for compaction.

Compaction consists of having bae instruct the model to create a compacted summary of all the key aspects of the session to that point and return a single message containing the fully compacted session.

BAE itself must emit a `session.compaction.started` event when either the server (in auto mode) or the client harness (in client mode) triggers compaction. When the model completes compaction, a `session.compaction.completed` event must be emitted, and the server must understand that all messages sent to the model from that point must start at the most recent compaction event (i.e. the first message in the request sent to the LLM is the compacted history, followed by all messages that come after the most recent compaction).

Determine the best and most efficient method to handle compaction and the new partial-session-message-history mechaism (leveraging sqlite to be as efficient as possible and not storing more than needed in bae server memory).

All compaction related events must be stored and streamed to/from clients just as with all other bae event types.

Determine the best method of allowing for both default/builtin compaction prompts and allow mode:client to also allow for a custom compaction prompt if the harness developer desires.

**Design decisions this spec makes:**
- `EventType::SessionCompaction` (`server/src/events.rs`, `#[serde(rename = "session.compaction")]`) already exists but is a single event, not a started/completed pair. It is **replaced** by two new variants — `session.compaction.started` and `session.compaction.completed` — since a single event cannot represent "in progress" (a client watching the live stream needs to know a compaction is underway, e.g. to stop sending turns, before it knows the outcome). This is a breaking rename of one unreleased enum variant; per `aspec/work-items/0015-plain-hash-key-auth.md`'s established precedent, bae has no tagged release and no external users, so no migration/back-compat shim is warranted.
- Auto-mode compaction is a **third bounded exception** to `aspec/architecture/design.md` Principle 2 ("thin protocol, customizable harness"), alongside the two existing exceptions (server-side MCP dispatch, server-side sandbox Auto dispatch): the server measures session size and triggers compaction inline in `run_turn` only when the *session itself* was configured for it at creation time. It is never triggered on a session that didn't ask for it.
- Client-mode compaction is a new JSON-RPC method, `session.compact`, following the exact shape of the existing driver-gated methods (`session.startRemoteSandbox`, `session.execRemoteSandbox`): it requires a registered driver, runs inside the same per-session turn-gate mutex as `session.sendMessage`, and goes through the same `broadcast::insert_and_publish` choke point as every other event.
- Compaction size accounting is measured in **tokens, not bytes or characters** — model context windows are token-denominated, so a byte-based proxy would be inaccurate in exactly the cases (large tool payloads, non-English text, base64-ish tool output) where it matters most. The server does not run a tokenizer itself; it reads the `usage` object every provider response already includes (Anthropic: `usage.input_tokens`/`usage.output_tokens`; OpenAI: `usage.prompt_tokens`/`usage.completion_tokens`) and compares `input + output` for the most recently completed turn against the configured `size`. This is the simplest correct trigger available (a given call's `input_tokens` already reflects the full accumulated history token count, since the whole history is resent every call) and needs no persisted running counter — see §4/§5.

## User Stories

### User Story 1:
As a: Agent Developer running long-lived sessions in `mode: auto`

I want to:
have the bae server automatically summarize and compact a session's history once it crosses a configured size, without my harness having to track token counts or context-window limits itself

So I can:
run sessions far longer than a single context window without ever hitting a provider context-length error, and without duplicating context-budget bookkeeping in every harness I write.

### User Story 2:
As a: Client Harness Developer running `mode: client` (or no `compaction` field)

I want to:
decide for myself when to compact a session (e.g. based on my own UX signals, cost budget, or a custom summarization prompt tailored to my agent's domain) by calling an explicit RPC, rather than have the server silently rewrite history under me

So I can:
keep full control over compaction timing and content while still getting the event-sourced, replayable audit trail (`session.compaction.started`/`completed`) that every other bae event type provides.

### User Story 3:
As a: Client (harness or observer) subscribed to a session's live event stream

I want to:
see `session.compaction.started` and `session.compaction.completed` events flow through the same subscribe/replay mechanism as every other event, carrying enough payload to know why compaction fired and what it produced

So I can:
render an accurate transcript/timeline (e.g. in MAX's observer-only webapp) and correctly reconstruct "current" history for any UI that mirrors what the server will actually send to the model next.

## Implementation Details:

> **Amended by `aspec/work-items/0018-compaction-and-first-run-hardening.md`
> (§A1–A7).** In particular: the compacted summary below (§5 step 5) is now
> preceded by a persisted synthetic `user`-role preamble message, so the
> post-compaction history replays as a provider-valid `user → assistant`
> sequence instead of starting `assistant`-first; `session.compaction.completed`
> (§3) gains `preamble_event_id`; and §6's history-scoping bound resolves from
> the preamble when present, falling back to the pre-0018 summary-based bound
> for sessions compacted before that change. §5.7 below is amended directly.
> Where this note and the section text below disagree, 0018 is current.

### 1. Session-level compaction config (`server/src/api/client/sessions.rs`, `server/src/store/sessions.rs`)
- Add `CompactionConfig` to `sessions.rs` (near `ClientToolDef`/`CreateSession`, `sessions.rs:100-128`):
  ```rust
  #[derive(Debug, Clone, Deserialize, Serialize)]
  #[serde(tag = "mode", rename_all = "lowercase")]
  pub enum CompactionConfig {
      Auto { size: u64 },
      Client { #[serde(default)] prompt: Option<String> },
  }
  ```
  Tagged by `mode` so the wire shape is exactly `{"mode":"auto","size":128000}` / `{"mode":"client"}` / `{"mode":"client","prompt":"..."}` as specified. `size` is a **token** threshold, not a byte count — it is compared against the model provider's own reported `input_tokens + output_tokens` for the most recently completed turn (see §4). All model context windows are token-denominated, so `size: 128000` reads naturally as "compact once this session is using ~128k tokens of context," matching how a harness developer would reason about e.g. Claude's or GPT's context limit.
- Add `pub compaction: Option<CompactionConfig>` (`#[serde(default)]`) to `CreateSession` (`sessions.rs:110`). Absent field or `null` means no compaction config — equivalent to `mode: client` with no custom prompt, per the summary's "or NO `compaction` field is passed" rule. Treat this as sugar: normalize `None` to `CompactionConfig::Client { prompt: None }` once at session-creation time so downstream code only ever branches on the two variants, not three states.
- `create()` (`sessions.rs:293`) passes the (normalized) config into `sessions::create_session(...)` (`sessions.rs:359-370`) for persistence.
- **`join()`** (`sessions.rs:441`, reuses `CreateSession`'s body shape): compaction is a session-level setting fixed at creation, exactly like `profile_id`. Reject a `compaction` field on `join` with the same "immutable session-level config" posture the codebase already applies elsewhere — do not silently ignore it, since a joining client believing it changed the mode would be a silent correctness bug.

### 2. Persistence (`server/src/store/migrations/0009_sessions_compaction.sql`, `server/src/store/sessions.rs`)
- New migration, following the exact `ALTER TABLE sessions ADD COLUMN` convention used for `sandbox_tools`/`subagent_tools`:
  ```sql
  ALTER TABLE sessions ADD COLUMN compaction TEXT;
  ```
  Stores the serialized `CompactionConfig` JSON (or NULL, though per the normalization above the app layer never writes NULL for a newly created session — NULL only appears for rows created before this migration, which is a boot-time schema concern, not a runtime one).
  Also add the index called out by the research as currently missing anywhere in the schema, since compaction's core lookup (find the most recent `session.compaction.completed` event for a session) becomes a hot per-turn query:
  ```sql
  CREATE INDEX idx_session_events_session_type ON session_events(session_id, event_type, rowid);
  ```
- `SESSION_COLS` (`sessions.rs:67`) grows a `compaction` column; `row_to_session` (`sessions.rs:70-95`) parses it into `SessionRecord.compaction: Option<CompactionConfig>` the same way `sandbox_tools`/`subagent_tools` are parsed from their JSON columns today.
- `SessionRecord` (`sessions.rs:33+`) gains the `compaction` field.

### 3. Event types (`server/src/events.rs`)
- Remove the existing single `SessionCompaction` variant (`#[serde(rename = "session.compaction")]`) and replace it with:
  ```rust
  #[serde(rename = "session.compaction.started")]
  SessionCompactionStarted,
  #[serde(rename = "session.compaction.completed")]
  SessionCompactionCompleted,
  ```
- Update `EventType::ALL` (`events.rs`, currently `[EventType; 27]`) to `[EventType; 28]` with both new variants in place of the one removed, and update the exhaustiveness test in the `#[cfg(test)]` module in the same file.
- Payload shapes (freeform JSON per the codebase's convention, e.g. mirroring `SandboxStart`/`SandboxRunning`'s request/resolution pairing), all token-denominated:
  - `session.compaction.started`: `{"trigger": "auto" | "client", "reason": "token_threshold" | "manual", "token_count": <u64 | null>, "threshold_tokens": <u64 | null>}`. For `trigger: "auto"` (`reason: "token_threshold"`), `token_count` is the just-completed turn's `input_tokens + output_tokens` that crossed `threshold_tokens` (the session's configured `size`). For `trigger: "client"` (`reason: "manual"`), `threshold_tokens` is `null` (no auto config governs this session/call) and `token_count` is a **best-effort** figure — the most recently persisted `provider.response` event's stored `usage` (see §4), or `null` if the session has no provider calls yet.
  - `session.compaction.completed`: `{"summary_event_id": "<the resulting message's own event id>", "compacted_message_count": <u64>, "input_tokens": <u64>, "summary_tokens": <u64>}`. `input_tokens`/`summary_tokens` come directly from the compaction call's **own** provider response usage (`input_tokens` = tokens of the pre-compaction history that was summarized, i.e. confirms the "before" size; `summary_tokens` = `output_tokens` of the produced summary, i.e. the "after" size) — no separate estimation logic needed, since the compaction call is itself a provider call with its own `usage`. The compacted summary text itself is **not** duplicated into this payload — it already lives in the synthetic message event this event points to (see §4), keeping with "don't store more than needed."

### 4. Token usage extraction (`server/src/engine/provider.rs`)
- Add a pure helper alongside `from_openai_response` (`provider.rs:552`):
  ```rust
  /// (input_tokens, output_tokens) from a provider's raw response, or `None`
  /// if the provider omitted usage. Anthropic: `usage.input_tokens` /
  /// `usage.output_tokens`. OpenAI: `usage.prompt_tokens` / `usage.completion_tokens`.
  pub fn usage_tokens(provider: ProviderKind, raw: &Value) -> Option<(u64, u64)>
  ```
  Read from `raw` (the untranslated wire body, already held on `ProviderResponse.raw` — `provider.rs:278`), never from `canonical`, since `usage` is not part of the canonical content translation. Both providers' native response shapes carry `usage` as a top-level sibling of `content`/`choices`, so this needs no new request-side plumbing — it's a pure read of a field the provider already sends back today and the server already stores in full (`provider.response`'s `body: resp.raw`) but has never parsed.
- `run_turn` (`session.rs:372-375`, where the `provider.response` event is inserted with `"body": resp.raw`) additionally extracts `usage_tokens(cfg.provider, &resp.raw)` and adds it to that same event's payload as a small explicit field — `"usage": {"input_tokens": i, "output_tokens": o}` (or omitted/`null` if the provider didn't return one) — so downstream consumers (the auto-trigger check, and the best-effort lookup in §3 for manual-trigger `started` events) never need to re-parse the full `body` to find it.

### 5. The compaction turn itself (`server/src/engine/session.rs`)
- New function `run_compaction(store, http, broadcaster, session, profile, provider_registry, prompt: Option<&str>, trigger: CompactionTrigger, acting_client_key_id, metrics) -> Result<EventRecord, TurnError>`, placed alongside `run_turn` (`session.rs:148`) and reusing its provider-call plumbing (same `provider_registry` lookup, same HTTP client) rather than duplicating it.
- Steps:
  1. Call `sessions::stream_history` (see §6 for its updated, compaction-aware behavior) to get the current effective history (already scoped to since-last-compaction, so a second compaction never re-summarizes already-compacted content).
  2. Insert `session.compaction.started` via `broadcast::insert_and_publish` (`engine/broadcast.rs:123`), with the trigger/reason/token payload from §3.
  3. Build the provider request: `history` messages, followed by a final synthetic `user`-role message containing the compaction instruction — either `prompt` (client-supplied custom prompt) or the **built-in default prompt** (a `const DEFAULT_COMPACTION_PROMPT: &str` in `session.rs`, instructing the model to produce a single self-contained summary covering key facts, decisions, open tasks, and any state the harness will need to continue the conversation). This reuses the existing message-shape convention exactly — the server has no system-role concept today (confirmed: `provider::to_openai_messages`, `engine/provider.rs:457`, only ever emits `user`/`assistant`/`tool`), so introducing a system-role message here would be new machinery; appending a `user` turn keeps compaction consistent with every other provider call the server makes.
  4. Call the provider (same call path `run_turn` uses; also goes through `usage_tokens` extraction from §4), take the single resulting assistant message and its `(input_tokens, output_tokens)`.
  5. Insert **one** synthetic `server.message.send` event holding the compacted summary — this is the "single message containing the fully compacted session" the summary calls for, and it is what a subsequent `stream_history` call will treat as the new starting point.
  6. Insert `session.compaction.completed`, with `input_tokens`/`summary_tokens` taken directly from step 4's usage (§3) — no separate size computation needed.
  7. Return the **full compaction record** — `session.compaction.started`, the compaction call's own `provider.request`/`provider.response` pair(s), the preamble and summary `server.message.send` events, and `session.compaction.completed`, in that order — as a `Vec<EventRecord>`, not just the terminal `completed` event. **Amended by work item 0018 (A3):** the auto path in `run_turn` appends every one of these to `Turn.events`, after the turn's own `server.message.send`, so the terminal `session.sendMessage` result's `events` array ends with the complete compaction record — matching this work item's own "the terminal result is the turn's full log" convention rather than surfacing only `completed`. A failed automatic attempt still contributes what it persisted (`started` plus the provider pair(s)) to `result.events`. The `session.compact` RPC handler's own response stays just the `session.compaction.completed` event (see §7 below) — its notification stream carries the rest, as documented in `docs/reference/00-client-api.md#sessioncompact`.
- **Auto trigger point is *after* a turn, not before it.** Token usage for a turn is only known once the provider has actually responded (the server doesn't run its own tokenizer — §4), so the check cannot be a pre-turn estimate the way a byte-count check could have been. Instead: `run_turn`'s iteration loop (`session.rs:301+`, the `for _ in 0..MAX_ITERATIONS` tool round-trip loop) already makes one-or-more provider calls per turn; track the **last** successful call's `(input_tokens, output_tokens)` in a local `Option<(u64, u64)>` through that loop. Once the loop reaches a true turn boundary (`Outcome::Completed` or `Outcome::Paused` — never mid-loop, while a `tool_use`/`tool_result` exchange is still in flight in memory, see Edge Cases) and the session's `CompactionConfig` is `Auto { size }`, compare `input_tokens + output_tokens` from that last call against `size`. If over threshold, call `run_compaction` **before returning** `Turn` to the RPC handler — so the *next* `session.sendMessage` (not this one) is the first thing that sees the fresh, compacted history. This is simpler than a pre-turn estimate, requires no SQL aggregate at all (the number is already sitting in memory from the call `run_turn` just made), and matches the trigger condition exactly as specified: "if input + output is greater than the size limit, compaction should be triggered."

### 6. Compaction-aware history assembly (`server/src/store/sessions.rs`)
- Add `rowid_of_last_compaction(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<i64>>`, following the exact idiom of the existing `rowid_of_event` (`sessions.rs:389-400`):
  ```sql
  SELECT rowid FROM session_events
  WHERE session_id = ?1 AND event_type = 'session.compaction.completed'
  ORDER BY rowid DESC LIMIT 1
  ```
- Modify `stream_history` (`sessions.rs:321-346`) to take the compaction rowid into account:
  ```sql
  SELECT event_type, payload FROM session_events
  WHERE session_id = ?1
    AND event_type IN ('client.message.send', 'server.message.send')
    AND rowid >= ?2
  ORDER BY rowid
  ```
  where `?2` is `rowid_of_last_compaction(...).unwrap_or(0)`. Because the compacted summary was itself inserted as a `server.message.send` event (§5 step 5), it is naturally included as the first row of this scoped query with no separate "prepend synthetic message" logic needed — the existing query shape already produces "compacted history, then everything after" for free once the `rowid >=` bound is added. This is the most efficient approach available: no additional in-memory reconstruction, no duplicated storage of the pre-compaction log (it still exists in `session_events` for audit/replay via `list_events`, just excluded from what's sent to the model), and the streaming-cursor behavior (`stmt.query` + `rows.next()`) is preserved unchanged.
- **No token-counting query is needed for the auto-trigger check** — unlike a byte-count design, the number being compared (§5) is the provider's own reported usage, already sitting in memory in `run_turn` from the call it just made. The only SQL this feature adds beyond `rowid_of_last_compaction` is a single cheap best-effort lookup for a **manually**-triggered (`session.compact`) `started` event's informational `token_count` (§3), since a client-mode compaction has no "call that just happened" to read from:
  ```sql
  SELECT payload FROM session_events
  WHERE session_id = ?1 AND event_type = 'provider.response'
  ORDER BY rowid DESC LIMIT 1
  ```
  parsing out just the small `usage` sub-object added in §4 — a single indexed row fetch, not a scan, and `None` (not an error) if the session has made no provider calls yet.

### 7. `session.compact` JSON-RPC method (`server/src/api/client/rpc.rs`)
- Add a `"session.compact"` arm alongside the existing driver-gated methods (`"session.startRemoteSandbox"` at `rpc.rs:270`, `"session.execRemoteSandbox"` at `rpc.rs:313`), requiring driver registration and running inside the same per-session `AppState.turn_gates` mutex `session.sendMessage` uses (so a compaction can never race a concurrent turn on the same session).
- Params: `{"prompt": Option<String>}`. If the session's `CompactionConfig` is `Auto`, `session.compact` is still permitted (a harness may want to force an early compaction) — but its `prompt` param, if given, is used for this one manual invocation without altering the session's stored auto-config.
- Response: the resulting `session.compaction.completed` `EventRecord` (mirroring how `session.sendMessage`/`session.execRemoteSandbox` return their produced events), and, like every other RPC-driven event, it flows to all `session.subscribe` watchers via the same broadcast path.

### 8. Default vs. custom compaction prompts
- **Built-in default**: `const DEFAULT_COMPACTION_PROMPT` in `server/src/engine/session.rs`, used whenever no custom prompt is supplied — both for `mode: auto` (which has no per-invocation way to supply a prompt, since it's server-triggered) and for `mode: client` when the harness calls `session.compact` with no `prompt`.
- **Custom prompt for `mode: client`**: sourced from two places, in priority order — (a) the `prompt` field on `session.compact`'s RPC params (§7), overriding the session default for that one call; (b) the `CompactionConfig::Client { prompt }` field set at session creation (§1), used as the default for that session's `session.compact` calls when the RPC call itself doesn't override it. `mode: auto` has no custom-prompt field at all — the summary's "auto" mode only ever specifies `size`, so a custom prompt for an auto-triggered compaction is out of scope for this work item (a harness that wants a custom prompt should use `mode: client` and drive `session.compact` itself).
- No new "system prompt" concept is introduced server-wide (see §5 step 3) — this keeps the prompt mechanism scoped entirely to compaction rather than adding general server-side prompt injection, which would cut against Principle 2.

## Edge Case Considerations:
- **Compaction with an empty or trivial history.** If `stream_history` (scoped to since-last-compaction) returns zero or one message, compacting produces no meaningful token reduction. `run_compaction` proceeds anyway when explicitly requested via `session.compact` (the caller asked for it); the auto-mode check is naturally immune to re-triggering on trivial history because it compares the *next* turn's actual usage, not a stale/lifetime total.
- **Concurrent turn and compaction on the same session.** `session.compact` and `session.sendMessage` must serialize through the same per-session turn-gate mutex (`AppState.turn_gates`) — a compaction must never run concurrently with a live turn, since both read/mutate what "current history" means. Auto-mode compaction, triggered from inside `run_turn` itself, is already serialized for free since it runs synchronously within the same turn-holder's execution.
- **Provider failure mid-compaction.** If the provider call in `run_compaction` fails (analogous to `run_turn`'s `Outcome::ProvidersFailed`), no `session.compaction.completed` event is emitted — only `session.compaction.started` was already persisted. `stream_history` must treat a `started`-without-matching-`completed` as "compaction did not happen" and continue scoping history from the *previous* successful compaction (or session start), i.e. `rowid_of_last_compaction` must key strictly off `session.compaction.completed`, never `.started`. The stuck `started` event remains in the log purely as an audit trail of the failed attempt; leave the session compactable again on the next attempt (auto-retry next turn, or another `session.compact` call) rather than latching a "compaction failed" terminal state.
- **Provider response omits `usage` (some OpenAI-compatible/proxy endpoints don't always return it).** `usage_tokens` returns `None`; the auto-mode threshold check for that turn is simply skipped (logged at debug, not an error) rather than treating a missing value as `0` (which would silently disable auto-compaction forever) or as "over threshold" (which would compact on every turn). The next turn that *does* return usage resumes normal checking. This is a real behavior gap worth calling out explicitly in docs: `mode: auto` depends on the configured provider actually reporting `usage`.
- **Multi-iteration turns (tool-use round-trips within one `run_turn` call, `session.rs`'s `MAX_ITERATIONS` loop).** The auto-mode check only ever runs once, at the very end of the loop, using the *last* provider call's usage — never mid-loop. A tool-use/tool-result exchange mid-turn is held in an in-memory `history` extension that hasn't yet round-tripped through `stream_history`-visible `client.message.send`/`server.message.send` events; compacting mid-loop would require reconciling that in-flight, not-yet-persisted state against a freshly-scoped history, which is unnecessary complexity for no real benefit — a turn that runs long enough to cross the threshold mid-loop simply compacts at the end of that same turn, one turn later than a hypothetical mid-loop check would, and is fully bounded by `MAX_ITERATIONS`.
- **`mode: auto` threshold crossed by a single oversized turn.** A single turn (however many iterations) could push usage well past `size` in one jump. Per the point above, the server always finishes the current turn's `Outcome` first, then compacts — a turn's own messages are never split across a compaction boundary, and the *next* turn is the first to see compacted history.
- **`join()` and compaction config.** A second client joining an existing session must not be able to change or add a `compaction` field (§1) — reject rather than silently ignore, since a joining client believing it altered the mode is a silent correctness bug for whichever behavior (auto-server vs. client-driven) actually governs the session.
- **`mode: client` (or no field) with a harness that never calls `session.compact`.** This is valid and expected — the server performs no auto-compaction, the session simply keeps growing until the harness compacts it, and (per the existing behavior) an eventual provider context-length error is the harness's responsibility to avoid, exactly as it is today with no compaction feature at all.
- **`size` given as 0 or an unreasonably small token count in `mode: auto`.** Validate `size` at session-creation time (`create()`, `sessions.rs:293`) against a sane minimum (e.g. reject anything below a small floor like 1,000 tokens, since it would force compaction on nearly every turn and likely exceed the compaction call's own output before it can even summarize); return a `400`/JSON-RPC invalid-params error rather than accepting a value that produces a compact-storm.
- **Replay via `GET /api/v1/sessions/{id}/events` (`list_events`, `sessions.rs:405`).** Compaction events and the pre-compaction messages must remain fully visible in event replay — compaction only changes what's sent to the *model*, never what's retained/streamed in the append-only log. A client rebuilding full transcript history from replay must see the entire log, including everything before a compaction; only the model-facing `stream_history` path is scoped.
- **`EventType::SessionCompaction` rename is breaking.** Since there is no tagged bae release and no external consumers (per WI 0015's established precedent), this is a plain rename with no dual-emit/back-compat period — do not special-case the old wire string anywhere.

## Test Considerations:
- **Unit (`server/src/engine/provider.rs`)**: `usage_tokens` extracts `(input_tokens, output_tokens)` from an Anthropic-shaped raw response (`usage.input_tokens`/`usage.output_tokens`) and an OpenAI-shaped one (`usage.prompt_tokens`/`usage.completion_tokens`); returns `None` when the `usage` object is absent entirely, and when only one of the two fields is present (a malformed/partial usage object is treated the same as "no usage," not a partial number).
- **Unit (`server/src/store/sessions.rs`)**: `rowid_of_last_compaction` returns `None` on a session with no compaction events, and the correct rowid after one or more `session.compaction.completed` events exist (ignoring any `session.compaction.started` events without a matching completion, per the edge case above). `stream_history` returns full history when no compaction has occurred, and returns only the compacted-summary message plus later messages once a `session.compaction.completed` event exists — including the case of two compactions in sequence (history scopes to the *most recent* one only).
- **Unit (`server/src/events.rs`)**: the existing exhaustiveness test continues to pass with `EventType::ALL` at length 28 and the two new variants' wire strings (`"session.compaction.started"`, `"session.compaction.completed"`) asserted directly, matching the pattern already used for other variants in that test module.
- **Unit (`CompactionConfig` (de)serialization, `sessions.rs`)**: `{"mode":"auto","size":128000}` deserializes to `Auto { size: 128000 }`; `{"mode":"client"}` and `{"mode":"client","prompt":"..."}` deserialize to the two `Client` shapes; an absent `compaction` field on `CreateSession` normalizes to `Client { prompt: None }`; an invalid `mode` value is rejected with a clear deserialization error; a `size` below the configured minimum is rejected at session-creation time.
- **Integration (`server/tests/integration.rs`, following its existing mock-provider pattern, e.g. `mock_handler` keyed by URL path segment)**: extend the mock provider's scripted responses to include a controllable `usage: {input_tokens, output_tokens}` (or OpenAI equivalent) object per response, plus a scripted `/compact`-style path for the summarization call. Cover:
  - `session_compaction_auto_triggers_at_token_threshold`: create a session with `compaction: {mode: auto, size: <small test threshold, e.g. 100>}`, script the mock provider to return usage that crosses it on a given turn, and assert the exact event sequence has that turn's own `client.message.send`/`provider.request`/`provider.response`/`server.message.send` complete first, **followed by** `session.compaction.started` → `provider.request` → `provider.response` → `session.compaction.completed` — extending the existing exact-sequence assertion style used in `session_lifecycle_exact_event_sequence_and_replay` (`integration.rs:1105`) — and that the *next* turn's `provider.request` payload is the compacted history, not the pre-compaction one.
  - `session_compaction_auto_does_not_trigger_under_threshold`: same setup with usage that stays under `size` — no compaction events appear across several turns.
  - `session_compaction_auto_skips_check_when_usage_missing`: mock response omits `usage` entirely; assert no compaction is triggered even though the harness has sent enough turns that it "should" have crossed a byte-based proxy, and that a later turn with `usage` present resumes normal checking.
  - `session_compaction_client_manual_rpc`: create a session with `compaction: {mode: client}` (or omit the field), drive a couple of turns, call `session.compact` directly via RPC, and assert the same started/completed event pair (with `token_count` in `started` populated from the prior turn's stored usage) plus that a subsequent `session.sendMessage`'s `provider.request` payload contains only the compacted summary message plus post-compaction turns — directly exercising the `stream_history` change.
  - `session_compaction_custom_prompt`: `session.compact({"prompt": "..."})` results in the mock provider receiving the custom prompt text as the final message, not the built-in default.
  - `session_compaction_replay_preserves_full_history`: after a compaction, `GET /api/v1/sessions/{id}/events` still returns every pre-compaction event — only the next provider-facing turn is scoped, not the persisted/replayable log.
  - `session_join_rejects_compaction_field`: a `POST /api/v1/sessions/{id}/join` body carrying a `compaction` field is rejected.
  - A provider-failure case (mock returns an error/500 during a compaction call) asserting `session.compaction.started` was persisted without a following `.completed`, and that the *next* turn's history still scopes correctly from the last successful compaction (or session start).
- All new tests must run fully offline against the existing in-process mock LLM provider, matching every other test in `integration.rs` — no real API keys or network calls.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec, in particular `aspec/architecture/design.md`'s Principle 2 (thin protocol, customizable harness) and its existing enumerated bounded exceptions — auto-mode compaction is presented explicitly as a third such exception, not a precedent-free special case.
- `session_events` remains strictly append-only (`server/src/store/sessions.rs`'s module doc, "there is no update or delete path for events") — compaction never rewrites or deletes prior events; it only changes which rows a later SQL query selects for provider-facing history.
- All new events (`session.compaction.started`/`.completed`) go through the single existing choke point, `broadcast::insert_and_publish` (`server/src/engine/broadcast.rs:123`) — no new persistence or streaming transport is introduced.
- The new `session.compact` RPC follows the exact driver-registration-gated, turn-gate-serialized pattern already established by `session.startRemoteSandbox`/`session.execRemoteSandbox` (`server/src/api/client/rpc.rs`) rather than inventing a new authorization or concurrency model.
- New SQLite migration (`0009_sessions_compaction.sql`) follows the existing incremental `ALTER TABLE ... ADD COLUMN` + doc-comment convention seen in migrations `0007`/`0008`; `MIGRATIONS`/`LATEST_VERSION` in `server/src/store/migrations.rs` must be updated together with the new file.
- `usage_tokens` lives in `server/src/engine/provider.rs` alongside the other pure, wire-format-translation helpers (`from_openai_response`, `to_openai_messages`) — it reads a field the server already receives and stores in full but has never parsed; no new provider dependency (e.g. a tokenizer crate) is introduced anywhere in this work item.
- Confirm `make test-server` (and the broader `make test`/`make image` targets, per the precedent set in WI 0015) pass, including the updated `EventType` exhaustiveness test and the new integration tests above.
