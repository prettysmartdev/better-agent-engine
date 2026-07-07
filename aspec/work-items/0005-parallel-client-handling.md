# Work Item: Feature

Title: parallel client handling
Issue: issuelink

## Summary:
- This work item explicitly enables "multi-client sessions".

Right now there is a notion of 'one driver, multiple observers' for a single session within BAE.

This work item updates that to explicitly enable multi-driver multi-observer.

How it works is that any client with the correct session key can connect to the session RPC endpoint and register as either a driver or an observer. Observers receive all session events as normal after they subscribe and until they unsubscribe. Drivers send a registration event to indicate they are allowed to drive the session, and the server keeps track of which clients are observers vs drivers.

The main protection on session is a "single active message mutex". This means that when more than one driver client submits a message to a single session, they are queued in a FIFO queue. Each message is processed completely to its terminal state before the next message is dequeued to be processed. The client that sends the message is also responsible for its handling through to completion, meaning that if client A sends a message, it must remain connected in order to handle client-side tool calls or else the handling of that message is terminated. If client A and client B each submit a message, the server will ensure that all tool calls etc. for client A's message is handled only by client A, and then when client B's message is dequed, client B must handle all tool calls for its message until terminally completed. All driver clients receive all events from all other clients, same as observer clients. This queueing system allows mutiple drivers to interact with the same system in a "multiplayer" fashion. Each client could even register different sets of tools but still interact with the same session so long as their tools are all in the allowed_tools list.

One edge case to consider is that if two different clients (with potentially different tools available) want to connect to the same session, they must each use the exact same profile. This means there must be a mapping of client -> profile -> session such that if a client using a different profile attempts to connect to a session, it must not be allowed. This means that two different client keys must be able to request a session token for the same session (therefore creating a new session and requesting a new session key for an existing session must both be possible).

As part of this work item, add 'providers' to the bae-config.toml configuration file such that both MCP servers AND llm providers are both provided to the server at runtime, and profiles must explicitly reference a valid configured provider(s) in the same way it must reference valid MCP server(s). The same rules apply, where if a provider is referenced that is not configured in the config file, it must be logged and skipped. The only difference is that if the 'primary' provider referenced in a profile does not exit, that is a fatal error for that profile and any client associated with that profile must not be allowed to create any sessions or send any messages. Missing fallback providers is allowed and results in error logging.

## User Stories

### User Story 1:
As an: Agent Developer

I want to:
have a teammate's client join the exact session I already opened — using their own client key against the same profile — and both of us send messages into it while the server serializes our turns FIFO and shows every participant the other's provider calls, tool calls, and responses

So I can:
build "multiplayer" agent experiences (e.g. a shared debugging session, a pair-programming agent, a human-in-the-loop review flow) without each participant needing a private copy of the conversation or out-of-band coordination about whose turn it is

### User Story 2:
As a: Platform Operator

I want to:
declare named LLM provider connections once in `bae-config.toml` (mirroring how MCP servers are declared) and have profiles reference them by name, with a profile whose *primary* provider name doesn't resolve refused outright rather than silently failing every session it touches

So I can:
rotate/reconfigure provider credentials and endpoints in one place instead of duplicating `base_url`/`auth_token`/`model` blobs across every profile, and get a loud, immediate failure signal for a profile-level typo instead of a per-message 500

### User Story 3:
As a: Dashboard/Tooling Developer

I want to:
open a pure observer connection (`session.subscribe`) to a session I am not driving and see every driver's activity live, plus be able to tell from the event log which client key opened, joined, or registered as a driver on the session

So I can:
build monitoring, audit, or "watch an agent work" UIs without needing a session key that can drive the conversation, and reconstruct exactly who did what from `GET /api/v1/sessions/{id}/events` alone


## Implementation Details:

This work item has three mostly-independent pieces: (A) multiple client keys attaching to one session ("join"), (B) explicit driver/observer registration plus a FIFO single-active-message mutex, and (C) a `[providers]` registry in `bae-config.toml` that profiles reference by name, with fatal-on-missing-primary semantics. All three build directly on the shape shipped in `aspec/work-items/0003-full-message-passing.md` (`server/src/api/client/rpc.rs`, `server/src/engine/broadcast.rs`, `server/src/config_file.rs`).

### A. Multiple client keys per session ("join")

Today `sessions.client_key_id` (`server/src/store/sessions.rs:29`) records exactly one creator, and `keys::insert_session_key` (`server/src/store/keys.rs:262-290`) is called exactly once, from `POST /api/v1/sessions` (`server/src/api/client/sessions.rs:186-214`). Nothing in the schema actually prevents a second `role='session'` row sharing `name = session_id` — `keys::authenticate_session` (`server/src/store/keys.rs:338-348`) already selects *all* candidate rows for a `session_id` and loops over them (`authenticate`, lines 353-372), so **the auth path already supports multiple session keys per session with no changes**. What's missing is the endpoint to mint the second (third, …) one, and the profile-match guard.

- Add `POST /api/v1/sessions/{id}/join` (client-key auth, REST, alongside `create`/`get_events`/`close` in `server/src/api/client/sessions.rs`). Body: same shape as `CreateSession` (`client_version`, `tools`).
  - Auth via `auth_client` (existing helper, `sessions.rs:51-58`).
  - Load the session via `sessions::get_session`. `404` if it doesn't exist; reject (`409 session_closed`, matching the existing pattern in `close`) if not `STATE_OPEN`.
  - **Profile match is the core guard**: if `client_key.profile_id != session.profile_id`, reject with a new `403 profile_mismatch` — a client using a different profile must never be able to attach to a session (this is the "client → profile → session" mapping the summary requires). Do not touch the session or log any event on this path — it is an authorization failure at the client-key level, same posture as `tool_not_allowed` today.
  - Validate `body.tools` against the (shared) profile's `allowed_tools`, exactly like `create` (`sessions.rs:152-165`) — a joining client can declare its own, independent tool set, so long as every name is in the same profile's allowlist.
  - **Tool declarations are per-client, never merged.** Each driver's declared tool list is private to that driver — joining must never add to, replace, or merge with any other driver's list, and (per "Per-turn tool scoping" in section B) the LLM must only ever see the tool list belonging to whichever driver's message is currently being processed. Change `sessions.client_tools` (still the same `TEXT`/JSON column — no migration) from a flat array to a JSON **object** keyed by `client_key_id`: `{ "key_abc123": [ {tool def}, … ], "key_def456": [ … ] }`. `sessions::create_session` (`store/sessions.rs:71-98`) now writes the creator's declared tools under its own `client_key_id` key instead of as a bare array; add `sessions::set_client_tools(conn, session_id, client_key_id, tools: &Value) -> rusqlite::Result<()>` to `server/src/store/sessions.rs`, called by both `create` and `join` to upsert *only that one client's* entry in the object — it must never read, merge, or overwrite another client's entry.
  - Mint a session key via the existing `keys::insert_session_key(conn, session_id, joining_client_key.id, profile_id, generated)` — already parameterized by `client_key_id`, so calling it again for the same `session_id` with a different `client_key_id` is the entire mechanism; no store-layer change needed here.
  - Insert a new `EventType::SessionJoin` event (see "New event types" below) with `{"client_version", "tools": [...]}`, through `broadcast::insert_and_publish` so existing drivers/observers see the join live.
  - Response: identical shape to `create` — `{session_id, session_key, profile}` (reuse `public_profile`).
- `revoke_client_key` (`server/src/store/keys.rs:387-425`) currently force-closes **every** open session matching `sessions.client_key_id = ?1` (line 419-423) — correct when a session has one owner, wrong once a session can have several independent drivers: revoking a *joiner's* key already only soft-deletes that joiner's own session key (the `client_id = ?1` filter on line 414-417 is already correct and needs no change), but revoking the **original creator's** key still nukes the whole session out from under every other still-valid driver/observer. Change the session-closing step to only close the session when, after soft-deleting this client's session key(s), **no other active session key remains for it** (`SELECT count(*) FROM keys WHERE role='session' AND name = <session_id> AND deleted_at IS NULL`) — i.e. the session only auto-closes when its last participant's key is revoked. Update the doc comment on `revoke_client_key` and the admin `DELETE /admin/v1/keys/:id` handler comment (`server/src/api/admin/keys.rs:106-110`) accordingly.

### B. Driver/observer registration + FIFO single-active-message mutex

Today, any connection holding a valid session key can call `session.sendMessage` with no registration step, and there is **no serialization at all** between concurrent `session.sendMessage` calls on the same session (`server/src/api/client/rpc.rs::drive_send_message`, `server/src/engine/session.rs::run_turn`) — two drivers calling it at once would both read `stream_history` concurrently and interleave writes. This work item adds explicit registration and a FIFO turn lock.

**Explicit driver registration.**
- Add a new JSON-RPC method, `session.registerDriver` (params `{}`), dispatched in `rpc()`'s method match (`server/src/api/client/rpc.rs:133-151`) alongside `sendMessage`/`subscribe`/`unsubscribe`. It records `session_id -> client_key_id` in a new in-memory registry on `AppState` (`server/src/api/mod.rs`), mirroring the existing `mcp_sessions`/`broadcaster` pattern:
  ```rust
  pub drivers: Arc<Mutex<HashMap<String, HashSet<String>>>>, // session_id -> registered driver client_key_ids
  ```
  and inserts a new `EventType::SessionDriverRegistered` event (broadcast, so other participants see who is driving).
- `session.sendMessage` requires the calling connection's `client_key_id` (from the authenticated session key's `client_id`, already available via `auth_session`'s returned `KeyRecord`) to be present in `drivers[session_id]`; if not, return the JSON-RPC error `{"code": -32001, "message": "call session.registerDriver before session.sendMessage"}` before touching the turn lock or history. This is a deliberate breaking change to the wire protocol (acceptable per the project's alpha status, as WI 0003 already established for the `/messages` → `/rpc` migration) — SDK harnesses must call `registerDriver` once as part of `connect()`/`join()` (see "Client SDK changes" below) so application code never has to think about it.
- `session.subscribe` is unchanged and **is** the observer registration act, per the summary — no new event is logged for it (it already isn't), but add a small `AppState.observers`-style read via the existing broadcaster (or a simple counter) only if needed for the participants endpoint below; this is optional polish, not required for correctness.
- Add a read-only `GET /api/v1/sessions/{id}/participants` (session-key auth) returning `{"drivers": ["key_…", …]}` from the in-memory registry, for operator/debug visibility and for tests to assert registration took effect. Live-only (not persisted) — restarts lose it, same as `mcp_sessions`/broadcaster state, and callers needing the durable "who ever joined" list already have it in `GET /api/v1/sessions/{id}/events` (`session.open` + `session.join` events).

**FIFO single-active-message mutex.**

The mutex must serialize entire *logical turns*, not single HTTP requests — a turn spans a `Paused` outcome (client-side tool call in flight) through however many follow-up `session.sendMessage` calls it takes to reach `Completed`/`ProvidersFailed`, and per the summary, only the client that submitted the paused message may submit its continuation.

- Add to `AppState`: a per-session FIFO gate and the currently-parked turn (if paused), analogous to `mcp_sessions`:
  ```rust
  pub turn_gates: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,       // lazily created per session
  pub pending_turns: Arc<Mutex<HashMap<String, PendingTurn>>>,                     // set only while Paused
  ```
  ```rust
  struct PendingTurn {
      owner_client_key_id: String,
      guard: tokio::sync::OwnedMutexGuard<()>, // held across HTTP requests
      deadline: tokio::time::Instant,
  }
  ```
  `tokio::sync::Mutex` grants `lock_owned()` acquisitions in the order they were requested (FIFO), which is exactly the "queued in a FIFO queue" requirement — no separate queue data structure is needed.
- In `drive_send_message` (`rpc.rs:161-318`), before subscribing to the broadcast feed:
  1. If `pending_turns[session_id]` exists and its `owner_client_key_id` matches the caller's client key: remove and reuse the stashed `OwnedMutexGuard` (this call already "holds the lock" — no queuing).
  2. If it exists and expired (`Instant::now() > deadline`): treat it as **abandoned** — log a `session.error` event (`{"reason": "driver_turn_abandoned", "owner_client_key_id": …}`) through the broadcast choke point, drop the stashed guard (releasing the gate to the next FIFO waiter), and fall through to (3).
  3. Otherwise (including the now-cleared case, and the ordinary "no turn in flight" case): acquire the session's gate via `gate.clone().lock_owned().await` (creating the gate lazily on first use). This is the point where a second driver's message genuinely waits its turn.
  - Run `run_turn` as today. On `Outcome::Paused`: stash the guard in `pending_turns[session_id]` as a `PendingTurn { owner_client_key_id: <caller>, guard, deadline: now + BAE_TURN_TIMEOUT }` instead of dropping it, then emit the terminal response exactly as today (the NDJSON stream still ends — see "Wire-protocol caveat" below).
  - On `Outcome::Completed` / `Outcome::ProvidersFailed`: drop the guard normally (do not touch `pending_turns` — nothing was stashed, or if it existed because this was a reused continuation, simply don't re-insert it), releasing the gate for the next FIFO waiter.
  - A caller whose client key does **not** match the pending turn's owner never attempts to reuse anything — it falls straight to step 3's `lock_owned().await` and genuinely blocks (its NDJSON response stays open with zero bytes written) until the owner's guard is released, either by completion or by the timeout-driven abandonment in step 2. This is deliberate: FIFO ordering is enforced by whoever calls `lock_owned()` first, and a blocked caller simply has an open, quiet HTTP response until it's dequeued — no polling or retries needed on the client side.
- **Wire-protocol caveat, called out explicitly**: the summary's "the client must remain connected... or else the handling of that message is terminated" describes a persistent-connection model, but `POST /api/v1/sessions/{id}/rpc` is a one-shot request/NDJSON-stream-response call (per WI 0003) — a `Paused` outcome's terminal response necessarily ends that HTTP exchange. "Remaining connected" is therefore operationalized as *returning with the continuation before `BAE_TURN_TIMEOUT` elapses*, not literally holding a socket open. Document this translation plainly in `docs/reference/wire-protocol.md` so it isn't discovered by trial and error.
- New env var `BAE_TURN_TIMEOUT` (default `120` seconds) in `server/src/config.rs`, documented in `docs/reference/configuration.md` next to the other `BAE_*` vars.
- `POST /api/v1/sessions/{id}/join` and driver registration are unaffected by an in-flight turn — a joining client can attach and register mid-turn; it simply queues behind the FIFO gate like any other driver once it calls `session.sendMessage`.

**Per-turn tool scoping.** Only the driver that owns the current turn (the same `owner_client_key_id` the FIFO turn lock above tracks) can have its client-side tool calls dispatched and answered — so the LLM must never be offered a *different* registered driver's private tools on that turn. Otherwise the model could call a tool the turn's actual owner never implements, with no one listening for the result.
- Thread the acting `client_key_id` — available at the top of `drive_send_message` from the authenticated session key's `client_id`, and identical to the `owner_client_key_id` the turn lock records — into `run_turn` as a new parameter: `run_turn(store, http, broadcaster, session, profile, mcp, acting_client_key_id)` (`engine/session.rs:104-111`).
- Change the tool-assembly step (`engine/session.rs:118-139`): instead of reading `session.client_tools` as a flat array, look up only the acting client's entry in the per-client object introduced above — `session.client_tools.get(acting_client_key_id).and_then(Value::as_array).cloned().unwrap_or_default()` — and merge *that* with the session's MCP tools (`McpSession::tools()`, unchanged: MCP servers are shared, session-wide infrastructure, not per-driver, so they are still advertised on every turn regardless of which driver owns it). `client_tool_names` (used to decide whether a `tool_use` dispatches as `"client"` vs `"mcp"`) is built from this same acting-client-only set.
- Replace `log_event`'s use of `session.client_key_id` as the `client_key_id` column on every event a turn inserts (`engine/session.rs:82-99`, currently `cid = session.client_key_id.as_str()`) with the acting client's id — every event a turn produces should be attributed to whichever driver actually triggered it, not always the session's original creator.
- This guarantees, by construction, that a driver's private tools are simply never sent to the provider during another driver's turn — there is no possibility of the model requesting a tool only a non-authoring driver declared, so no runtime rejection path is needed for that case (contrast with the corrected edge case below).

**New event types** (extends the closed `EventType` enum, `server/src/events.rs`, from 12 to 14 variants — update `EventType::ALL`, the exhaustiveness test at `events.rs:131-138`, and `should_broadcast`'s exhaustive match in `engine/broadcast.rs:194-220`, adding both new variants as `true`, i.e. always forwarded to live watchers — visibility into who joined/registered is the point of the feature):
- `session.join` — a second (or further) client key minted a session key for an existing session.
- `session.driver.register` — a client key registered as a driver via `session.registerDriver`.

### C. `[providers]` in `bae-config.toml`

Mirrors the `[mcp]` section added in WI 0003 (`server/src/config_file.rs`) almost exactly, plus a request/response translation layer so a registry entry can speak either of two wire formats — **Anthropic Messages API** or **OpenAI Chat Completions API** — at any `base_url`, not just each format's own default SaaS endpoint.

- Extend `BaeConfig` (`config_file.rs:59-64`): add `pub providers: Option<ProvidersConfig>` alongside `pub mcp: Option<McpConfig>`. Shape:
  ```toml
  [providers]

  # Anthropic, default SaaS endpoint (base_url omitted).
  [[providers.entries]]
  name        = "anthropic-sonnet"
  provider    = "anthropic"
  model       = "claude-sonnet-4-6"
  auth_token  = "${ANTHROPIC_API_KEY}"
  max_tokens  = 8096

  # OpenAI, default SaaS endpoint (base_url omitted).
  [[providers.entries]]
  name        = "openai-gpt"
  provider    = "openai"
  model       = "gpt-5"
  auth_token  = "${OPENAI_API_KEY}"
  max_tokens  = 8096

  # Anthropic-*compatible* wire format at a non-Anthropic endpoint (e.g. a
  # self-hosted proxy or gateway that speaks the Messages API) — base_url
  # always wins when set, regardless of `provider`.
  [[providers.entries]]
  name        = "self-hosted-claude-gateway"
  provider    = "anthropic"
  base_url    = "https://llm-gateway.internal.example.com"
  model       = "claude-sonnet-4-6"
  auth_token  = "${INTERNAL_GATEWAY_TOKEN}"
  ```
  Define `ProvidersConfig { pub entries: Vec<NamedProviderConfig> }` and `NamedProviderConfig { pub name: String, #[serde(flatten)] pub config: crate::engine::provider::ProviderConfig }` (reusing the existing `ProviderConfig` struct from `engine/provider.rs:29-37` via `#[serde(flatten)]` rather than duplicating its fields). `${ENV_VAR}` tokens in `auth_token` are preserved raw at parse time and resolved only at call time via the existing `resolve_tokens` — unchanged secret-handling convention.
- **`provider` selects a wire format, not a vendor.** Change `ProviderConfig.provider` (`provider.rs:29-37`) from a free-form `String` to a closed enum, mirroring `McpTransport` (`config_file.rs:104-113`):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
  #[serde(rename_all = "lowercase")]
  pub enum ProviderKind { Anthropic, OpenAi }
  ```
  An unsupported value (e.g. `provider = "cohere"`) is rejected at TOML parse time as an unknown enum variant, exactly like an unsupported `McpTransport` value today — no separate validation step needed. `provider = "anthropic"`/`"openai"` selects *which request/response shape and auth header convention* `engine/provider.rs::call` speaks; it does not restrict `base_url` to that vendor's own service — an OpenAI-compatible self-hosted model server, proxy, or third-party API is exactly the "any LLM provider that uses either OpenAI- or Anthropic-formatted APIs at any base URL" case this section exists for.
- **`base_url` becomes optional with a per-kind default.** Change `ProviderConfig.base_url` to `Option<String>`. Add:
  ```rust
  impl ProviderKind {
      pub fn default_base_url(&self) -> &'static str {
          match self {
              ProviderKind::Anthropic => "https://api.anthropic.com",
              ProviderKind::OpenAi => "https://api.openai.com",
          }
      }
  }
  impl ProviderConfig {
      pub fn effective_base_url(&self) -> &str {
          self.base_url.as_deref().unwrap_or_else(|| self.provider.default_base_url())
      }
  }
  ```
  `effective_base_url()` is what `call()` builds the request URL from (see "Request/response translation" below) — an explicit `base_url` is always used verbatim when present; only its *absence* falls back to the default. Both defaults are bare hosts (no `/v1` suffix), matching today's convention where `call()` appends the versioned path itself (`{base_url}/v1/messages` for Anthropic today) — the OpenAI path appends `/v1/chat/completions` the same way, so both kinds' `base_url` values are directly comparable (host only) whether defaulted or explicit.
- Add `BaeConfig::provider_registry(&self) -> Result<HashMap<String, ProviderConfig>, ConfigFileError>`, mirroring `mcp_registry()` (`config_file.rs:240-279`): duplicate `name` or blank `name` is a fatal startup error (exit code 2), exactly like `ConfigFileError::DuplicateServer`/`EmptyName` (add analogous `DuplicateProvider`/provider-flavored error messages, or generalize the existing variants — implementer's choice, but keep the exit-code-2 "operator authoring error" posture).
- `AppState` (`api/mod.rs`) gets `pub provider_registry: Arc<HashMap<String, ProviderConfig>>`, built at startup in `cli.rs::run_serve` next to `load_mcp_registry` (add a parallel `load_provider_registry`/`load_provider_registry_from`, same missing-file-is-empty-registry / malformed-file-is-fatal semantics as `load_mcp_registry_from`, `cli.rs:149-175`).
- Add `GET /admin/v1/providers` (read-only, loopback admin port), mirroring `GET /admin/v1/mcp-servers` (`server/src/api/admin/mcp.rs`) exactly: list `{name, provider, model, base_url}` sorted by name, where `base_url` is always the *effective* (resolved-default-or-explicit) value so an operator can confirm which endpoint is actually in effect — never `auth_token`.

**Request/response translation (`engine/provider.rs::call`).** `engine/session.rs::run_turn` is, and must remain, provider-agnostic: it builds/reads messages and tool definitions in one canonical shape — the existing Anthropic Messages API shape already used internally today (`content` arrays of `{"type":"text"|"tool_use"|"tool_result", …}` blocks, tools as `{name, description, input_schema}`). `call()` is the **only** place that knows about wire-format differences; it translates in both directions for `ProviderKind::OpenAi` and passes everything through unchanged (as today) for `ProviderKind::Anthropic`:
- **Outgoing, OpenAI kind**: build a Chat Completions request to `{effective_base_url}/v1/chat/completions` with `Authorization: Bearer <resolved auth_token>` (no `x-api-key`/`anthropic-version` headers). Translate the canonical `tools` array (`{name, description, input_schema}`) into OpenAI's function-calling shape (`{"type": "function", "function": {"name", "description", "parameters": input_schema}}`). Translate canonical `messages`: a plain `{role, content: "text"}` passes through as-is; a message whose `content` is an array containing `tool_result` blocks must be split into OpenAI's separate `{"role": "tool", "tool_call_id": …, "content": …}` message(s), since OpenAI does not embed tool results inside a user message the way the canonical/Anthropic shape does; an assistant message containing `tool_use` blocks (from prior history) becomes an assistant message with an OpenAI `tool_calls` array.
- **Incoming, OpenAI kind**: read `choices[0].message` from the OpenAI response and translate it back into the canonical shape *before returning it from `call()`* — a plain `content` string becomes a single `{"type": "text", "text": …}` block; each entry in `tool_calls` becomes a `{"type": "tool_use", "id": tool_call.id, "name": tool_call.function.name, "input": <parsed JSON of tool_call.function.arguments>}` block. The value `call()` returns is always shaped `{"content": [ …canonical blocks… ]}` regardless of which `ProviderKind` served the request, so `run_turn`'s existing `tool_use_blocks`/content handling (`engine/session.rs:246-266`, `444-468`) requires **no changes** — the translation is fully contained in `provider.rs`.
- **Anthropic kind**: unchanged behavior from today, just reading `cfg.effective_base_url()` instead of `cfg.base_url` directly (`provider.rs:183`).
- The raw, untranslated wire response is still what gets logged in `provider.response` events for OpenAI-kind calls (so the event log is a faithful record of what the provider actually said); only the value handed back to `run_turn` for history/tool-dispatch purposes is the canonical-shape translation. Document this distinction (raw-logged vs. canonical-returned) in `docs/reference/message-types.md`.
- **Mixed-kind fallback chains work with no special-casing, by construction.** The fallback walk in `run_turn` (`engine/session.rs:170-229`) already calls `provider::call(http, cfg, &history_value, &tools_value)` once per attempt, where `history_value`/`tools_value` are always the canonical shape and `cfg` is that attempt's own `ProviderConfig` (with its own `provider: ProviderKind`). Because translation happens *inside* `call()`, keyed only off the `cfg` it was given, a profile with `primary_provider` set to an `anthropic`-kind entry and a `fallback_providers` entry pointing at an `openai`-kind entry (or vice versa) is translated correctly per attempt with no changes to the fallback loop itself: attempt 0 translates canonical → Anthropic wire format (or passes through), attempt 1 translates canonical → OpenAI wire format, and whichever attempt succeeds returns canonical content that gets persisted (`server.message.send`) and folded into `history` (`engine/session.rs:313`) exactly the same way regardless of which kind actually served it. This also holds *within* a single turn's multiple provider round-trips (the `for _ in 0..MAX_ITERATIONS` loop, `engine/session.rs:165`) — each iteration re-walks primary→fallbacks independently, so even a turn that needed several MCP round-trips can have different iterations served by different provider kinds without the canonical history ever being kind-specific. `stream_history` (`store/sessions.rs:173-198`), which reconstructs history at the start of a turn from persisted events, is unaffected since what it reads back was already canonical when persisted.

**Profile schema change** (mirrors the WI 0003 `mcp_servers` opaque-blob → name-array change, applied to the two provider fields):
- `profiles.provider_config` (TEXT column, unchanged schema — migration `0003_profiles.sql` stays as-is) now holds a JSON **string** (the primary provider's registry name) instead of an inline config object; `profiles.fallback_configs` now holds a JSON **array of strings** instead of an array of config objects. Rename the corresponding request/response JSON fields to `primary_provider: string` and `fallback_providers: string[]` in the admin API (`server/src/api/admin/profiles.rs`) — the old field names described "a config," which would now be misleading. This is a bigger alpha-breaking change than the `mcp_servers` one; document it prominently in `docs/profiles.md` and the changelog/release notes the same way the `mcp_servers` shape change was called out.
- `CreateProfile::into_input` (`admin/profiles.rs:34-64`): replace `validate_provider_config`/`validate_fallbacks` (which today deserialize `ProviderConfig` directly) with a non-empty-string check for `primary_provider` and `require_string_array` (already exists, reused verbatim) for `fallback_providers` — registry resolution happens later, not at admin-write time, exactly like `mcp_servers`.
- `profile_view` / `public_profile` (`admin/profiles.rs:109-120`, `client/sessions.rs:118-129`): update field names; `public_profile`'s `provider` sub-object (currently reading `p.provider_config.get("provider")`/`.get("model")` off the inline blob) must instead resolve `p.primary_provider` against `state.provider_registry` to surface `{provider, model}` — still no `auth_token`, still no registry name env-var leakage.
- `engine::provider::configs_from_profile` (`provider.rs:46-55`): change its signature to accept the registry and the profile's name references — `fn resolve_from_profile(registry: &HashMap<String, ProviderConfig>, primary_provider: &str, fallback_providers: &[String]) -> Result<(ProviderConfig, Vec<ProviderConfig>), ProviderConfigError>` — replacing direct `serde_json::from_value` deserialization of inline blobs with registry lookups. Add a `ProviderConfigError::PrimaryProviderMissing(String)` variant, distinct from today's `Malformed`.

**Fatal-primary / logged-and-skipped-fallback semantics** (the one asymmetry vs. MCP, called out explicitly in the summary):
- At `POST /api/v1/sessions` (`client/sessions.rs::create`) and `POST /api/v1/sessions/{id}/join`: immediately after loading the profile, resolve `profile.primary_provider` against `state.provider_registry`. Missing → `tracing::error!` (logged on **every** attempt, never deduplicated — same posture as the MCP "not found" logging) and reject with a new `422 primary_provider_unavailable`, inserting a `session.error` event exactly like the existing `profile_unavailable_at_open` path (`client/sessions.rs:316-349`) does for a deleted profile. **No session is created and no session key is issued** — this is the "any client associated with that profile must not be allowed to create any sessions" requirement.
- At `session.sendMessage` (`run_turn`, `engine/session.rs:141-157`): the existing malformed-primary-config branch already ends the turn via `finish_failed` on a parse error — extend it to also catch `ProviderConfigError::PrimaryProviderMissing` (the profile could theoretically be edited, or the server restarted with a changed `bae-config.toml`, between session-open and a later message) and log/terminate the same way. This is a defensive re-check, not the primary enforcement point.
- Missing **fallback** provider names: resolve each independently, log-and-skip per name (never short-circuiting the rest of the list, matching `resolve_registry_names`'s existing MCP behavior in `client/sessions.rs:299-312`), and proceed with whatever subset resolved (including zero) — this never blocks session creation or message sending.

### Client SDK changes (`client-rust/`, `client-typescript/`, `client-python/`)

- Add `Harness::join(session_id, tools)` (and TS/Python equivalents) alongside the existing `connect()`, calling `POST /api/v1/sessions/{id}/join` and returning a `Session` handle identically shaped to `connect()`'s.
- `connect()`/`join()` must issue `session.registerDriver` once as part of session setup (before the caller's first `send()`), so `session.registerDriver` is never something application code has to call directly — this keeps Principle 2 ("thin protocol, customizable harness," `aspec/architecture/design.md`) intact: the registration handshake is a harness-internal transport detail.
- Extend `session.subscribe(...)`'s documented purpose in `docs/guides/building-a-client.md` (unchanged code path) to explicitly frame it as "the observer registration act" per this work item, and document `join()`/`registerDriver` next to it.
- Update each SDK's `types.{rs,ts,py}` for `session.join`/`session.driver.register` event payloads and the renamed `primary_provider`/`fallback_providers` profile fields.
- Extend the `harness-smoke`/`reference-assistant` agents (per `aspec/genai/agents.md`) to exercise a two-driver scenario: connect as driver A, join as driver B (different client key, same profile), have both send messages, and assert the FIFO ordering and cross-visibility of events.

### Documentation

- `docs/profiles.md` — rewrite the "Provider config" and "Fallback configs" sections around the new `primary_provider`/`fallback_providers` name-reference shape (mirroring how the existing "MCP servers" section already documents opt-in-by-name), and prominently flag the breaking change.
- `docs/reference/configuration.md` — add a `[providers]` section to the `bae-config.toml` schema reference, a `BAE_TURN_TIMEOUT` row, and a `GET /admin/v1/providers` section mirroring the existing `GET /admin/v1/mcp-servers` one.
- `docs/reference/client-api.md` — document `POST /api/v1/sessions/{id}/join`, `GET /api/v1/sessions/{id}/participants`, and the new JSON-RPC methods/errors (`session.registerDriver`, `-32001 driver_not_registered`).
- `docs/reference/wire-protocol.md` — add the FIFO-mutex/turn-ownership semantics and the "remaining connected" → timeout translation called out above.
- `docs/reference/message-types.md` — add `session.join` and `session.driver.register` to the event catalog.
- `docs/guides/` — add a short new guide (e.g. `docs/guides/multi-client-sessions.md`) walking through: create a session as driver A, join as driver B with a different client key (same profile), both send messages, observe FIFO ordering and full cross-visibility via `session.subscribe`.
- `examples/bae-config/` — add a `[providers]` section to one existing example file (or a new `examples/bae-config/providers.toml`) so the new section has a runnable reference, referenced from `docs/reference/configuration.md`.


## Edge Case Considerations:

- **Different client key, different profile, same session id**: rejected at `POST /api/v1/sessions/{id}/join` with `403 profile_mismatch`, before any event is logged or session key minted — this is the hard boundary the summary requires.
- **Joining a session that is `closed` or `error`**: `409 session_closed`, matching the existing `close` endpoint's error shape — a joiner cannot resurrect a terminal session.
- **Revoking the session-creating client key while other drivers/observers are still active**: per the `revoke_client_key` change above, the session now only auto-closes when the revoked key's session key was the *last* active one for that session — revoking a joiner (or even the original creator, if others remain) no longer force-closes a session out from under everyone else.
- **A driver never calls `session.registerDriver` before `session.sendMessage`**: rejected with JSON-RPC `-32001`, before the turn lock or broadcast subscription is touched — this must not silently auto-register, since explicit registration is the mechanism the server uses to know who to log/track as a driver.
- **Two drivers submit messages concurrently**: the second blocks on the per-session `tokio::sync::Mutex`'s FIFO ordering; its NDJSON stream stays open with no bytes written until dequeued. Document that a client should apply its own request timeout if it wants to give up waiting rather than hold the connection indefinitely — the server itself does not time out a *queued* (not yet started) message, only an *abandoned in-flight* one.
- **The owning driver of a paused turn disconnects and never returns**: per `BAE_TURN_TIMEOUT`, the next arrival (a same-owner retry or a different queued driver) triggers lazy expiry — a `session.error` (`reason: "driver_turn_abandoned"`) is logged, the gate is released, and the session stays `open` (unlike a provider failure, which moves the session to `error`) so other drivers are unaffected.
- **The owning driver of a paused turn sends a brand-new message instead of a `tool_result` continuation**: allowed — ownership is checked by `client_key_id`, not by the shape of `content`; the owner may abandon the pending tool call voluntarily by sending anything, and the turn proceeds with that as the next `run_turn` input.
- **Two drivers declare different private tool sets**: per "Per-turn tool scoping" in section B, each turn only ever advertises the *acting* driver's own declared tools (plus the session's shared MCP tools) to the provider — a different driver's private tools are never sent during another driver's turn, so the model cannot request a tool the current turn's owner doesn't implement. This is enforced by construction (the tool list built for each `run_turn` call), not by a runtime rejection path, so there is no "wrong driver's tool was called" failure mode to handle.
- **A profile's `primary_provider` is missing from the registry**: fatal for every client on that profile — `POST /api/v1/sessions` and `POST /api/v1/sessions/{id}/join` both reject with `422 primary_provider_unavailable`, logged on **every** attempt (never deduplicated), matching the "must not be allowed to create any sessions or send any messages" requirement. An already-open session on that profile whose next `session.sendMessage` hits the same missing-primary condition (e.g. after a server restart with a changed `bae-config.toml`) ends that turn via `finish_failed` rather than serving a message.
- **A profile's `fallback_providers` entry is missing**: non-fatal — logged and skipped per name (never short-circuiting the rest of the list), exactly like a missing MCP server name; the primary and any still-resolvable fallbacks are used.
- **`bae-config.toml` has a `[providers]` section with a duplicate `name`, or a `name` shared between `[providers]` and `[mcp]`**: duplicate names *within* `[[providers.entries]]` are a fatal startup error (exit 2), same as MCP; names may safely collide *across* the two sections since they are different registries (`provider_registry` vs `mcp_registry`) with no shared namespace — call this out explicitly in the config reference so it isn't assumed to be an error.
- **Existing profiles with the old inline `provider_config`/`fallback_configs` object shape**: silently broken by the rename/shape change (alpha status, no migration) — document plainly, same posture as the WI 0003 `mcp_servers` breaking change; do not attempt to auto-migrate old blobs into registry-name references (there is no reliable way to infer a name for an inline config that was never registered).
- **A registry entry omits `base_url`**: defaults to that entry's `provider` kind's own SaaS endpoint (`https://api.anthropic.com` / `https://api.openai.com`); when `base_url` is present it is always used verbatim, regardless of `provider` — the two are independent knobs, not a validated pairing (a `provider = "openai"` entry with a non-OpenAI `base_url` is fully supported and expected, per the summary's "any LLM provider that uses either OpenAI- or Anthropic-formatted APIs at any base URL").
- **A profile's `primary_provider` and a `fallback_providers` entry resolve to *different* `ProviderKind`s** (e.g. primary is `anthropic`, fallback is `openai`, or vice versa): fully supported with no extra configuration — each attempt is translated independently inside `call()` off its own `cfg.provider`, so a mixed-kind fallback chain behaves exactly like a same-kind one from `run_turn`'s perspective (see "Mixed-kind fallback chains" above). There is no requirement that a profile's primary and fallbacks share a kind.
- **An OpenAI-kind response's `tool_calls[].function.arguments` is not valid JSON** (a malformed/truncated function-call argument string from the provider): treated as a provider-side protocol error for that attempt — surface it the same way a non-JSON Anthropic response body is handled today (`ProviderCallError::Transport`, `provider.rs:207-210`), i.e. log the failure and continue the fallback walk rather than panicking or silently passing through a broken `input`.


## Test Considerations:

- **Unit — join profile-match guard**: same profile → succeeds and returns a usable session key; different profile → `403 profile_mismatch`, no event logged, no session key row inserted.
- **Unit — `sessions::set_client_tools`**: upserts only the given client's entry in the per-client JSON object, leaving every other client's entry byte-for-byte untouched; a second call for the same client *replaces* (does not merge with) that client's own prior list.
- **Integration — per-turn tool scoping**: driver A declares tool `only_a`, driver B joins (same profile) and declares tool `only_b`; assert the `provider.request` event logged during A's turn advertises `only_a` plus the session's MCP tools and never `only_b`, and the reverse for a subsequent turn owned by B — asserted against the mock provider's received tool list, not just the persisted event.
- **Integration — multi-key session lifecycle**: create as client A → join as client B (same profile) → both authenticate successfully via `session.sendMessage`/`session.subscribe` using their respective session keys → `GET /api/v1/sessions/{id}/events` shows both `session.open` and `session.join`.
- **Integration — revoke no longer nukes shared sessions**: two joined clients on one session; revoke the joiner's key → session stays `open`, the other driver's session key still authenticates; revoke the last remaining active session key → session auto-closes.
- **Unit — driver registration gate**: `session.sendMessage` before `session.registerDriver` → `-32001`; after registering → succeeds; a `session.subscribe`-only connection never needs to register.
- **Integration — FIFO ordering**: two drivers submit messages back-to-back against a mock provider with an artificial delay; assert driver A's full event sequence (through its terminal response) completes before driver B's turn's first event appears, regardless of send order at the transport layer.
- **Integration — same-owner continuation reuses the lock without queuing**: driver A gets `Paused`, sends the `tool_result` continuation itself → no other driver's queued message is allowed to run in between (assert via a concurrently-queued driver B's turn only starting after A's continuation completes).
- **Integration — cross-driver continuation rejected mid-turn**: driver A gets `Paused`; driver B (different client key, already registered as driver) attempts `session.sendMessage` — B's call blocks (does not error) until A's turn reaches a terminal state or the abandonment timeout fires.
- **Integration — abandoned turn timeout**: driver A gets `Paused` and never returns; after `BAE_TURN_TIMEOUT` (use a short test override), a queued driver B's message proceeds; assert a `session.error` (`reason: "driver_turn_abandoned"`) was logged and the session is still `open`.
- **Unit — provider registry parsing**: valid `[providers]` with 2+ entries parses; duplicate `name` within `providers.entries` is a fatal startup error; a name shared between `[providers]` and `[mcp]` is accepted (no cross-section collision check); missing/absent `[providers]` table yields an empty registry with no error, symmetric with the existing MCP tests in `config_file.rs`.
- **Unit — profile provider resolution**: `primary_provider` resolves → `ProviderConfig` returned; `primary_provider` missing → `ProviderConfigError::PrimaryProviderMissing`; a `fallback_providers` entry missing is individually skipped without affecting the primary or other fallbacks (mirrors the existing `resolve_registry_names` unit tests for MCP in `client/sessions.rs`).
- **Integration — fatal primary blocks session creation and messaging**: profile with an unresolvable `primary_provider`; `POST /api/v1/sessions` → `422 primary_provider_unavailable`, no session row created with `state='open'` (a `state='error'` audit row is fine, matching `profile_unavailable_at_open`'s existing pattern); the error is logged on every repeated attempt (assert no dedup).
- **Integration — non-fatal fallback**: profile with a valid `primary_provider` and one valid + one missing `fallback_providers` entry; session creation succeeds; a message that forces fallback use only tries the valid one; the missing one is logged every session, never fatal.
- **Unit — `effective_base_url`**: `base_url` omitted → each `ProviderKind`'s documented default; `base_url` present → used verbatim regardless of `provider` (including an `openai`-kind entry pointed at a non-OpenAI host and an `anthropic`-kind entry pointed at a non-Anthropic host).
- **Unit — OpenAI outgoing translation**: canonical `tools` (`{name, description, input_schema}`) → OpenAI function-calling shape (`{"type":"function","function":{...}}`); a canonical message containing `tool_result` blocks splits into separate `{"role":"tool", "tool_call_id", "content"}` message(s); a canonical assistant message containing `tool_use` blocks becomes an OpenAI `tool_calls` array; a plain text message passes through unchanged.
- **Unit — OpenAI incoming translation**: a `choices[0].message` with plain `content` → single canonical `{"type":"text",...}` block; a `tool_calls` array → one canonical `{"type":"tool_use",...}` block per entry, with `input` parsed from the `function.arguments` JSON string; a malformed (non-JSON) `arguments` string surfaces as a provider call error rather than a malformed `input` value silently reaching `run_turn`.
- **Integration — OpenAI-kind end-to-end tool round trip**: profile whose `primary_provider` is an `openai`-kind entry against a mock OpenAI-shaped HTTP server that returns a `tool_calls` response, then a plain-text response; assert the session log's `provider.request`/`provider.response` events contain the raw OpenAI-shaped payloads, while `tool.call`/`server.message.send`/history all use the canonical shape identically to an equivalent Anthropic-kind run — same assertions as the existing Anthropic-only tool-dispatch integration test, run a second time against an OpenAI-kind config.
- **Integration — mixed-kind fallback chain**: profile with `primary_provider` = an `openai`-kind entry that fails (mock 500) and `fallback_providers` = an `anthropic`-kind entry that succeeds (and the reverse pairing in a second test case); assert the successful attempt's canonical content is what's persisted/returned regardless of which kind served it, and that each attempt's own `provider.response` event carries that attempt's raw (untranslated) wire payload.
- **Regression — `EventType`**: `EventType::ALL` grows from 12 to 14 and its exhaustiveness test (`events.rs:131-138`) and `should_broadcast`'s exhaustive match (`broadcast.rs:194-220`) are updated together, so a future omission fails to compile rather than silently defaulting.
- **Cross-SDK**: extend the `harness-smoke` agents in all three client SDKs to exercise `join()` + auto-`registerDriver` + a two-driver FIFO scenario, asserting identical event sequences across Rust/TypeScript/Python for the same scripted inputs (same parity convention as WI 0002/0003).
- All new tests remain offline (`make test-server`, etc.) — FIFO/timeout tests use a mock provider and a short `BAE_TURN_TIMEOUT` override, not real wall-clock waits against a live LLM.


## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- New in-memory per-session registries (`turn_gates`, `pending_turns`, `drivers`) belong on `AppState` (`server/src/api/mod.rs`) alongside the existing `mcp_sessions`/`broadcaster` fields, follow the same `Arc<Mutex<HashMap<SessionId, _>>>` shape, and are torn down on session close (`DELETE /api/v1/sessions/{id}`, `server/src/api/client/sessions.rs::close`) exactly like `mcp_sessions`/the broadcast channel are today — add the two new cleanups to that same function rather than a separate teardown path.
- Keep the provider-registry parsing/config-file concern in `server/src/config_file.rs` (mirroring how `McpServerConfig`/`mcp_registry()` live there, not in `engine/provider.rs`), and keep the outbound-call concern (the existing `ProviderConfig`/`resolve_tokens`/`call`) in `engine/provider.rs` — the same separation WI 0003 established between `config_file.rs` (parsing/registry) and `engine/mcp.rs` (connection/dispatch).
- Reuse `require_string_array` (`admin/profiles.rs`) verbatim for `fallback_providers` instead of writing a near-duplicate validator.
- Keep the OpenAI↔canonical translation functions private to `engine/provider.rs` and pure (`Value -> Value`, no I/O), mirroring how `resolve_tokens_with` (`provider.rs:96-123`) is already split out as a pure, independently unit-testable function from the impure `call()` — this is what makes the translation unit-testable without a mock HTTP server and keeps `run_turn` (`engine/session.rs`) unaware that more than one wire format exists.
- The FIFO gate and driver registry are per-process, in-memory, and rebuilt/lost on restart by design — same "no new persistence" posture already established for `mcp_sessions` and the broadcast registry (`aspec/work-items/0003-full-message-passing.md`'s Codebase Integration section); a restarted server has no open connections to serialize against anyway, and any client must reconnect (and re-`registerDriver`) regardless.
- No new SQLite migration: `profiles.provider_config`/`profiles.fallback_configs` stay `TEXT` columns (migration `0003_profiles.sql`); only their JSON *shape* and the request/response field names change — an application-layer contract change, exactly the precedent set by the `mcp_servers` opaque-blob-to-name-array change in WI 0003, not a schema change.
- Update `aspec/architecture/design.md`'s high-level diagram/description only if the "one driver, multiple observers" phrasing appears anywhere describing the session model (grep for it) — this work item is the correction if so.
- Verify `make image` and `make test` still pass across all four components (`server/`, `client-rust/`, `client-typescript/`, `client-python/`) after this work item, since it changes the profile JSON contract, adds two `EventType` variants, and touches the client-facing router (`join`, `participants`) and the JSON-RPC dispatch table (`registerDriver`).
