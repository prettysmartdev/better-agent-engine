# Work Item: Feature

Title: Server skeleton, authentication, session management, and client SDKs
Issue: issuelink

## Summary:
- Stand up the `server/` crate as a running service: an axum HTTP server with `GET /healthz` and `GET /api/v1/meta`, environment-driven configuration (`BASE_ADDR`, `BASE_DB_PATH`, `BASE_LOG`), and SQLite initialization with an embedded, forward-only migration runner (migration 0001 creating the schema-version table).

- authentication and session creation for client/server: base-server exposes two ports. The first (443 if TLS enabled, 8080 if not) for client interactions, serving JSON-RPC over HTTP/2. The second for admin-only interactions, and ONLY binds to localhost.

- admin port exposes a simple HTTP REST API for managing the server. First endpoint(s) to implement are: create client key, list client keys, delete client key. localhost client (via curl) can hit create client key endpoint to create a new key, hashed key stored in sqlite alongside client name provided by request. list endpoint lists clients with active keys, created date, last used date. hashed key value is not returned ever. delete endpoint deletes the indicated client key

- client port exposes JSON-RPC API for client interactions. first actions to implement are session management: client creates session by exchanging client key (plus metadata about itself including client version, list of available tools, etc.) for a session ID and key. client key is ALWAYS hashed and validated with constant-time operations on the server.

- each time a client opens a new session connection with the server, it must provide the session ID and key in order to establish the connection. only the session key's hash is stored in sqlite (use the same keys table for clients and sessions), and constant-time key hashing/comparison. with a valid session key, the client connection is opened and persisted for client and server to exchange JSON-RPC messages. Message contents can be arbitrary payloads but have strict types such as client.message.send, provider.message.send, etc.

- every message sent is persisted in SQLite with session ID and client ID included as columns. The server must be able to replicate a full session's history by reading back individual messages. the session_events table must be a full timestamped log of all client messages, requests sent to LLM providers, their responses, messages exchanged with MCP servers, and more. create a `message type` schema which identifies what every event is, even if each event's payload is freeform.

- create the message types for client.message and server.message (the main object exchanged by client and server when the client sends a message destined for an LLM and the server responds with some result from the LLM. also create message types for tool calls, MCP usage, session compaction, etc.

- each client must be associated with a 'profile', which are managed ONLY via the admin API. a profile describes 1) the LLM provider (connection, model name, auth), and one or more fallbacks if the primary is not available 2) the MCP server connections that should be made available to clients associated with that profile 3) an allowlist of client tools that can be provided by the client harness (the actual list of active client tools must be provided when a client creates a connection to connect to a session). There must be admin API endpoints for creating, updating, listing, and deleting profiles. each time a client key is created, a valid profile name must be included.

- create a basic session handling loop which reads messages from client, sets up an appropriate request to the provider configured for the client's profile, sends request to provider, persists response, and sends message back to client. stub out MCP server support and resource calling, but don't fully implement it yet. Ensure every session and connection follows the rules of its configured profile and that EVERYTHING is persisted into the session_events table. create documentation in docs/ showing how to start the server, create profiles, create client keys, etc. using cURL. Allow provider auth tokens to be provided via env vars and referenced in profile config as ${ENV_VAR_NAME} to be substituted at runtime.

- create the basic rust, typescript, and python implementations for creating a session, creating a session connection, and exchanging messages. ensure the client SDKs are simple and include an example project in each client's sub-codebase showing how to set up a project, instantiate the client harness, and connect to a running server. the client SDKs should be AGENT HARNESSES not just simple REST client wrappers. They should allow defining and providing client-tool definitions and implementations, and handle the full loop of sending and receiving messages from the server, and including hooks where developers can optionally insert custom logic at key points in the agent loop. Ensure the functionality is the same between the 3 client languages, but follows each language's canonical style and idiomatic practices.

## User Stories

### User Story 1:
As a: Platform Operator

I want to:
run the server Docker image with a data volume, verify it is alive via `/healthz`, then use the admin API on localhost to create a profile and issue a client key

So I can:
confirm the instance is healthy and correctly configured, and hand credentials to agent developers without ever exposing admin surface to the network

### User Story 2:
As a: Agent Developer

I want to:
exchange my client key for a session ID and session key using the client harness in Rust, TypeScript, or Python, then send and receive messages over a persistent connection — with the harness handling tool dispatch, provider calls, and retry automatically

So I can:
build an agent loop in my language of choice without managing auth, persistence, or provider calls myself

### User Story 3:
As a: Platform Operator

I want to:
replay any session in full by reading back the session_events log, including every client message, provider request and response, tool call, and MCP exchange, and query `GET /api/v1/meta` to check server and API version

So I can:
audit behavior, debug agent failures, satisfy compliance requirements with a complete tamper-evident history, and ensure client libraries remain compatible after upgrades


## Implementation Details:

### Server — dependencies and module structure
- Add `axum`, `tokio`, `rusqlite`, `serde`, `tracing`, `tracing-subscriber`, `argon2`, `rand`, `subtle` to `server/Cargo.toml`.
- Structure per `aspec/architecture/design.md`: `api/` (router, handlers), `store/` (SQLite open + migration runner, key ops), `engine/` (session loop); `main.rs` is a thin entrypoint that loads config from env and calls into the library.
- `/healthz` returns 200 with no auth. `/api/v1/meta` returns `{version, api_versions}`.

### Server — dual-port listener
- On startup, bind two TCP listeners: `BASE_ADDR` (default `0.0.0.0:8080`) for the client-facing axum router, and `BASE_ADMIN_ADDR` (default `127.0.0.1:8081`) for the admin-only axum router. Both are started before the runtime hands off to axum. The admin and client routers are separate `Router` instances — never shared.
- TLS termination is upstream (nginx/caddy/cloud LB); the container always speaks plain HTTP internally. Document this clearly.

### SQLite — migration runner and schema
- On startup: open or create the database at `BASE_DB_PATH`, apply pending migrations transactionally, refuse to start if the schema version is newer than the binary knows about.
- **Migration 0001 — schema_version table**: `schema_version(version INTEGER PK, applied_at TEXT)`. The migration runner inserts a row per applied migration inside a single transaction.
- **Migration 0002 — keys table**: `keys(id TEXT PK, name TEXT, key_hash TEXT, key_prefix TEXT, role TEXT CHECK(role IN ('client','session')), profile_id TEXT REFERENCES profiles(id), client_id TEXT REFERENCES keys(id), created_at TEXT, last_used_at TEXT, deleted_at TEXT)`. `key_prefix` stores the first 8 chars for display; `key_hash` is Argon2id. `key_hash` is never returned in any API response.
- **Migration 0003 — profiles table**: `profiles(id TEXT PK, name TEXT UNIQUE, provider_config TEXT, fallback_configs TEXT, mcp_servers TEXT, allowed_tools TEXT, created_at TEXT, updated_at TEXT, deleted_at TEXT)`. Complex fields stored as JSON blobs.
- **Migration 0004 — sessions table**: `sessions(id TEXT PK, client_key_id TEXT REFERENCES keys(id), profile_id TEXT REFERENCES profiles(id), state TEXT CHECK(state IN ('open','closed','error')), client_version TEXT, client_tools TEXT, created_at TEXT, closed_at TEXT)`.
- **Migration 0005 — session_events table**: `session_events(id TEXT PK, session_id TEXT REFERENCES sessions(id), client_key_id TEXT, event_type TEXT NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL)`. `event_type` is one of the defined message type strings; `payload` is freeform JSON. Append-only: no updates or deletes.

### Message type schema
Define a closed enum of event type strings (used as `event_type` in `session_events`):
- `client.message.send` — client sends a user turn to the server
- `server.message.send` — server sends the LLM's response back to the client
- `provider.request` — full request payload about to be sent to the LLM
- `provider.response` — raw response received from the LLM provider
- `tool.call` — server or harness invokes a tool (client-side or MCP)
- `tool.result` — result returned from a tool call
- `mcp.request` — request sent to an MCP server (stub; full impl later)
- `mcp.response` — response from an MCP server (stub)
- `session.open` — session connection established
- `session.close` — session connection closed normally
- `session.error` — session terminated due to error
- `session.compaction` — session history was compacted (summary event)

Use an exhaustive match / discriminated union in each language so adding a new type without handling it is a compile or type error.

### Admin API endpoints (localhost only, no auth required initially)

**Profiles**
- `POST /admin/v1/profiles` — body: `{name, provider_config, fallback_configs?, mcp_servers?, allowed_tools?}`. Returns `{id, name, created_at}`.
- `GET /admin/v1/profiles` — cursor-paginated list (`?cursor=&limit=`). Returns `{items: [...], next_cursor}`.
- `GET /admin/v1/profiles/:id` — single profile.
- `PUT /admin/v1/profiles/:id` — full replacement.
- `DELETE /admin/v1/profiles/:id` — soft-delete (`deleted_at`); reject if any active (non-deleted) client keys reference it.

**Client Keys**
- `POST /admin/v1/keys` — body: `{name, profile_id}`. Validates profile exists and is not deleted. Returns `{id, name, key, prefix, profile_id, created_at}` — `key` is the plaintext `base_<random>` shown exactly once.
- `GET /admin/v1/keys` — list active keys (no `key_hash`). Returns `{id, name, prefix, profile_id, created_at, last_used_at}` per item.
- `DELETE /admin/v1/keys/:id` — revoke key (sets `deleted_at`; all open sessions for this key are invalidated).

### Client API (client port)

- `POST /api/v1/sessions` — auth: `Authorization: Bearer <client_key>`. Body: `{client_version, tools: [{name, description, input_schema}]}`. Validates client key (Argon2id, constant-time, `deleted_at IS NULL`). Validates declared tools against profile's `allowed_tools`. Creates session row, generates `ses_…` session ID and `base_ses_…` session key (hash immediately; return plaintext once). Inserts `session.open` event. Returns `{session_id, session_key, profile}`.
- `POST /api/v1/sessions/:id/messages` — auth: `Authorization: Bearer <session_key>`. Body: `{message: {role, content}}`. Inserts `client.message.send` event. Runs the session message loop. Returns `{message: {role, content}, events: [...]}`.
- `GET /api/v1/sessions/:id/events` — cursor-paginated replay. Auth: session key. Returns full `session_events` rows for the session.
- `DELETE /api/v1/sessions/:id` — close session. Auth: session key. Inserts `session.close` event.

### Session message loop (`server/src/engine/session.rs`)
- Load profile for the session; resolve `${ENV_VAR_NAME}` tokens in provider auth config at call time only — never persist resolved values.
- Reconstruct conversation history by streaming (not loading all at once) `client.message.send` and `server.message.send` events from `session_events`.
- Insert `provider.request` event before calling the provider.
- Call provider HTTP API; on failure insert `session.error` and attempt fallback configs in order, inserting a `provider.response` event for each attempt (including failures).
- Insert `provider.response` event with the raw response on success.
- If response contains tool calls, insert `tool.call` events; dispatch to client (return in response for client-side tools) or MCP stubs; insert `tool.result` events. MCP stub path inserts `mcp.request` / `mcp.response` with `{status: "stub"}`.
- Insert `server.message.send` event with the final assistant turn.

### Provider config schema
```json
{
  "provider": "anthropic",
  "base_url": "https://api.anthropic.com",
  "model": "claude-sonnet-4-6",
  "auth_token": "${ANTHROPIC_API_KEY}",
  "max_tokens": 8096
}
```
`fallback_configs` is an array of the same shape. Resolve env vars immediately before the HTTP call; discard the resolved value after the call returns.

### Client SDKs (Rust / TypeScript / Python)

All three SDKs expose the same conceptual surface, named idiomatically per language:
1. **Config** — server URL, client key, client version.
2. **Tool definition** — name, description, JSON schema for input, a callable handler.
3. **Harness / Agent** — holds config + tool registry; exposes `connect()` (async where applicable) which creates a session and returns a `Session` handle.
4. **Session handle** — exposes `send(message)` → response (drives the full round-trip until no pending tool calls) and `close()`.
5. **Hooks** — optional callbacks: `before_send`, `after_receive`, `before_tool_call`, `after_tool_call`. Each receives the relevant event and may mutate or log it; an error return aborts the loop.

Tool calls: when the server returns a tool call, the harness dispatches to the registered handler by name, sends the result back, and continues until a non-tool-call response is received.

Each client includes `examples/reference-assistant/` implementing the `reference-assistant` agent (per `aspec/genai/agents.md`): register a simple tool (e.g. `get_current_time`), open a session, run a message loop, print responses. Fails with a clear message if the configured provider key env var is absent.

### Documentation (`docs/`)
- `docs/quickstart.md` — start the server, create a profile, create a client key, send a message; all curl examples.
- `docs/admin-api.md` — full admin endpoint reference with example request/response bodies.
- `docs/client-api.md` — full client endpoint reference.
- `docs/message-types.md` — catalog of all `event_type` values with payload schemas.
- `docs/profiles.md` — how to write provider config, reference env vars, configure MCP stubs, and set tool allowlists.


## Edge Case Considerations:

- **Startup — missing/unwritable `BASE_DB_PATH`**: clear error message, non-zero exit before attempting to bind ports.
- **Startup — invalid `BASE_ADDR` or `BASE_ADMIN_ADDR`**: usage error (exit code 2) per `aspec/uxui/cli.md`; if `BASE_ADMIN_ADDR` port is already in use, refuse to start rather than silently skipping the admin port.
- **Startup — database newer than binary**: if `schema_version` is ahead of the highest known migration, refuse to start with a clear message rather than silently ignoring unknown migrations.
- **Concurrent startup**: transactional migration runner prevents double-applying migrations if two processes start against the same database simultaneously.
- **Graceful shutdown**: on SIGTERM, stop accepting new connections on both ports, drain in-flight requests (configurable timeout), then close the database.
- **Key entropy**: client and session keys must use a CSPRNG with ≥ 128 bits of entropy (`rand::rngs::OsRng` in Rust; `crypto.randomBytes` in Node; `secrets.token_bytes` in Python).
- **Argon2id parameters**: choose parameters that make brute-force infeasible (memory ≥ 64 MiB, iterations ≥ 3, parallelism = 1). Document the chosen params so operators can tune per deployment.
- **Constant-time comparison**: all key comparisons must use a constant-time equality function (`subtle::ConstantTimeEq` in Rust; `hmac.compare_digest` in Python; a manual XOR loop in TypeScript). Timing oracles on partial-match are a real attack surface.
- **Deleted keys**: a key with `deleted_at` set must be treated as non-existent. Check `deleted_at IS NULL` in the lookup query before hash comparison — do not short-circuit on the hash alone.
- **Session key vs client key collision**: both live in `keys`; always filter by `role` in queries to prevent a session key being accepted as a client key or vice versa.
- **Profile deleted between key creation and session open**: return a clear `profile_unavailable` error and insert a `session.error` event.
- **Env var not set**: if `${ENV_VAR_NAME}` is absent at call time, return a provider config error rather than passing a blank token — a blank token silently produces 401s from the provider.
- **Tool allowlist enforcement**: validate the client's declared tool list against the profile's `allowed_tools` at session-open time. Reject creation if any declared tool is not in the allowlist. An empty allowlist means no tools are permitted.
- **Concurrent session opens on the same client key**: allowed; each produces an independent session row. No limit enforced here; mark as a future rate-limiting concern.
- **Large payloads**: `session_events.payload` is unbounded TEXT; provider responses can be large. Stream the event query when reconstructing conversation history — do not load the full log into memory.
- **Provider fallback tracing**: insert a `provider.response` event for every attempt (including failures) so the full retry trace is preserved in the log.


## Test Considerations:

- **Unit — config parsing**: env var loading for `BASE_ADDR`, `BASE_ADMIN_ADDR`, `BASE_DB_PATH`, `BASE_LOG`; invalid values produce the correct error type.
- **Unit — migration runner**: fresh DB applies all migrations in order; already-up-to-date DB is a no-op; future-versioned DB refuses to start.
- **Unit — key generation and hashing**: byte length meets entropy requirement; hash round-trip passes; wrong key fails; constant-time comparison rejects mismatched input without early-return.
- **Unit — env var substitution**: `${VAR}` present → substituted; `${VAR}` absent → error; literal `$` without braces → passed through unchanged.
- **Unit — tool allowlist validation**: empty allowlist rejects all tools; exact name match required; undeclared tool name in request is rejected.
- **Unit — message type enum**: exhaustive match/discriminated union so a new type added without handling is a compile or type error.
- **Integration — server bootstrap**: start server on ephemeral port pair with a temp DB; assert `/healthz` 200 and `/api/v1/meta` response shape; assert admin port refuses connections from non-loopback (if testable in CI).
- **Integration — admin API**: full CRUD lifecycle for profiles and keys; `key_hash` never returned in any response; deleting a profile blocked while active keys reference it; list pagination returns correct cursor.
- **Integration — session lifecycle**: create key → open session (exchange key for session ID + session key) → send message via mock provider → assert all event types inserted in order → close session → assert `session.close` event → replay via `GET /api/v1/sessions/:id/events` and assert history matches.
- **Integration — auth rejection**: deleted client key → 401; wrong session key → 401; session key on wrong session → 401; `base_admin_addr` inaccessible from client port path.
- **Integration — provider fallback**: profile with broken primary and working fallback; assert fallback used; assert both `provider.response` events (failure + success) are in the log.
- **Integration — tool dispatch (harness)**: register a tool in the harness; mock provider returns a tool-call response; assert harness calls handler and sends result; assert `tool.call` and `tool.result` events in the log.
- **Cross-SDK parity**: run the `harness-smoke` agent (per `aspec/genai/agents.md`) from all three client implementations against a real server; assert identical event type sequences for the same scripted inputs.
- All tests run offline (`make test-server`, `make test-client-rust`, etc.); mock provider must not require real API keys.


## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- The admin router and client router must be registered on separate axum `Router` instances bound to their respective listeners — do not share a router and filter admin routes via middleware.
- All SQLite migrations are embedded via `include_str!` macros and applied by the migration runner; migrations are forward-only and check `schema_version` before applying to be safe against concurrent starts.
- Module layout: key generation/hashing/comparison in `server/src/store/keys.rs`; admin handlers in `server/src/api/admin/`; client API handlers in `server/src/api/client/`; session engine loop in `server/src/engine/session.rs`.
- Client SDK harness logic lives in `src/harness.{rs,ts,py}` (or idiomatic equivalent) and re-exports a clean top-level API. Each SDK's `examples/reference-assistant/` exercises every hook point at least once, serving as living documentation.
- All new `BASE_*` env vars (`BASE_ADMIN_ADDR`, `BASE_TLS_ENABLED`) must be documented in the existing env-var reference and validated at startup per `aspec/uxui/cli.md`.
- Never persist or log resolved env var token values; resolve `${ENV_VAR_NAME}` immediately before the provider HTTP call and discard after.
- Verify the production image still builds (`make image`) after this work item since it introduces the first real binary behavior and dual-port listener.
