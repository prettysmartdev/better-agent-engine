# Work Item: Feature

Title: baectl CLI
Issue: issuelink

## Summary:
- Add a new baectl CLI (Rust) as an additional sub-codebase alongside server and the clients. This should be an HTTP/REST wrapper around the bae server's admin API. It should be built and bundled into both the dev and production images so that users can `container exec baectl create profile...` instead of using CURL with the raw API. It should automatically be configured to use the localhost port and auth method. Create commands for profile, and key management, using positional arguments for required information and flags for optional fields. Use the best available CLI crate for Rust, and ensure it compiles to a static binary. Document it fully in docs/ and update all examples/guides to use the CLI as the primary method of interaction, with curl as an optional alternative inside a collapsable 'details' block for each example.

- **Scope note on "auth method" — this work item adds real admin-port authentication.** The admin port (`BAE_ADMIN_ADDR`) has **no authentication today**: `docs/reference/admin-api.md` currently states this explicitly and `server/src/api/admin/mod.rs` has no auth middleware. `aspec/uxui/setup.md` and `aspec/architecture/security.md` already describe a bootstrap-admin-key vision that hadn't shipped — this work item **implements** it: the server self-provisions (or ingests a pre-provisioned) `role=admin` key at startup, persists only its hash, and enforces `Authorization: Bearer <token>` on every `/admin/v1/*` route unless explicitly disabled. "Automatically configured... auth method" means `baectl` transparently discovers and uses that admin key with **zero operator configuration** when run the documented way (`docker exec`/`container exec` inside the same container as `baesrv`), because it reads the same stable on-disk file the server wrote. See "Admin authentication (bootstrap key lifecycle)" under Implementation Details for the full design, including the `baesrv --dangerously-disable-admin-auth` / `--rotate-admin-key` flags and the `baectl auth create key` pre-provisioning command for multi-replica deployments. This is a breaking change to the previously-open admin port, acceptable per the project's alpha status (README: "Status: alpha... APIs and SDKs will likely change").

## User Stories

### User Story 1:
As a: Platform Operator

I want to:
run `docker exec bae baectl create profile main anthropic claude-sonnet-4-6 --allowed-tool get_current_time` instead of hand-assembling a curl command with a JSON body

So I can:
create profiles and client keys without memorizing the admin API's JSON field names, quoting rules, or the `${ENV_VAR}` auth-token template syntax

### User Story 2:
As a: New bae User

I want to:
follow the quickstart and every other guide/example using one consistent CLI, with the raw curl equivalent available but tucked away in a collapsed `<details>` block if I want to see the underlying HTTP call

So I can:
get a profile and key set up quickly without first learning the admin API's request/response shapes, while still being able to peek at the wire format when I need to debug or script against it directly

### User Story 3:
As a: Platform Operator

I want to:
have `baectl` already present as a small static binary inside both the dev container and the production image, with zero extra install step and zero extra configuration to reach the local server

So I can:
administer a running container (`docker exec`/`container exec`) the same way in development and production, without a Rust toolchain, network access to crates.io, or manually pointing the tool at the right host/port

## Implementation Details:

### New component: `baectl/`
- Add a new top-level, independently-buildable crate `baectl/` (Cargo package name `baectl`), following the same shape as `client-rust/` (its own `Cargo.toml`, `Makefile` with the standard `build`/`test`/`lint`/`fmt`/`clean` verbs, `src/`, no shared workspace with `server/` or `client-rust/` — matches today's pattern where every Rust component already stands alone with its own `Cargo.toml`/`target/`).
- Add `baectl` to the root `Makefile`'s `COMPONENTS` list so `make build`/`test`/`lint`/`fmt`/`clean` iterate over it exactly like `server`, `client-rust`, `client-typescript`, `client-python`.
- Module layout: `src/main.rs` (thin entrypoint calling into the library, mirroring `server/src/main.rs` + `cli.rs`'s split), `src/cli.rs` (clap derive command tree), `src/admin_client.rs` (a small `reqwest`-based wrapper over `/admin/v1/*`, with typed request/response structs mirroring the JSON bodies documented in `docs/reference/admin-api.md` and implemented in `server/src/api/admin/{profiles,keys,mcp}.rs`), `src/output.rs` (human-readable vs `--json` rendering), `src/error.rs` (maps RFC 7807 bodies and transport failures to exit codes).
- **Does not depend on `client-rust` (`bae-rs`).** That crate's scope is the client-port session harness — tool dispatch, hooks, the agent loop (`aspec/architecture/design.md` Component 2). baectl only talks to the admin port and has none of those concerns; depending on it would pull in harness types that don't apply here. baectl gets its own minimal, purpose-built admin HTTP client instead.

### CLI crate and static binary
- Use `clap` (v4, derive macros) — the de facto standard, actively maintained Rust CLI crate, giving `--help` generation, subcommands, and validation for free (satisfies "best available CLI crate").
- HTTP transport: `reqwest` with `default-features = false, features = ["rustls-tls", "json"]` — same convention already used in `server/Cargo.toml` and `client-rust/Cargo.toml` — so the TLS stack is pure-Rust and the crate compiles cleanly under a musl target with no OpenSSL. **This choice is load-bearing for the static-binary requirement**: pulling in `native-tls`/OpenSSL (directly or transitively) would break static linking under musl, so pin `default-features = false` and comment why, matching `server/Cargo.toml`'s existing comment style.
- Static binary target: `x86_64-unknown-linux-musl`.
  - `Dockerfile.dev`: in the existing Rust-toolchain `RUN` block, add `rustup target add x86_64-unknown-linux-musl` and the `musl-tools` apt package, so `make build-baectl`/`test-baectl`/`lint-baectl` work in the dev loop and a developer can reproduce the exact static build locally.
  - `baectl/Makefile`'s `build` target compiles the release artifact with `cargo build --release --target x86_64-unknown-linux-musl`, so `make build-baectl` produces the same binary shape that ships in the image. `test`/`lint` continue to use the native host target (faster, no cross-compilation needed for `cargo test`/`clippy`).
- Production `Dockerfile` changes (this restructures the existing `build` stage, which currently does `COPY server/ ./` — flattening `server/`'s contents into `/build` — then a single `cargo build --release`):
  ```dockerfile
  FROM rust:1-bookworm AS build
  WORKDIR /build
  COPY server/ ./server/
  COPY baectl/ ./baectl/
  RUN cargo build --release --manifest-path server/Cargo.toml
  RUN rustup target add x86_64-unknown-linux-musl \
      && apt-get update && apt-get install -y --no-install-recommends musl-tools \
      && rm -rf /var/lib/apt/lists/* \
      && cargo build --release --target x86_64-unknown-linux-musl --manifest-path baectl/Cargo.toml
  ```
  then in the runtime stage:
  ```dockerfile
  COPY --from=build /build/server/target/release/baesrv /usr/local/bin/baesrv
  COPY --from=build /build/baectl/target/x86_64-unknown-linux-musl/release/baectl /usr/local/bin/baectl
  ```
  Call this restructuring out explicitly in the PR — the `COPY server/ ./server/` (not `./`) change is a subtle but necessary fix to make room for a second crate in the same build stage.
  - The musl static binary runs fine on the `debian:bookworm-slim` runtime base (statically linked, no libc dependency on the host) — no additional runtime packages needed for baectl specifically.

### Command structure
Verb-first subcommands, each taking a resource-type positional, matching the shape used in the work item's own example (`baectl create profile ...`) and mapping 1:1 onto the admin API's actual CRUD surface (profiles have full create/list/get/replace/delete; keys have create/list/delete only — there is no single-key-get endpoint and keys are immutable besides revocation, so `baectl` does not invent a `get key`/`update key` that the API can't back):

| Command | Admin endpoint | Positional (required) | Flags (optional) |
|---|---|---|---|
| `baectl create profile <name> <provider> <model>` | `POST /admin/v1/profiles` | `name`, `provider`, `model` | `--base-url <url>` (default `https://api.anthropic.com` when `provider=anthropic`, otherwise required), `--auth-token-env <VAR>` (expands to `provider_config.auth_token = "${VAR}"`; default `ANTHROPIC_API_KEY` when `provider=anthropic`), `--max-tokens <n>` (default 4096, matches server default), `--fallback <provider:model:auth_token_env>` (repeatable), `--mcp-server <name>` (repeatable), `--allowed-tool <name>` (repeatable) |
| `baectl list profiles` | `GET /admin/v1/profiles` | — | `--limit <n>`, `--cursor <c>`, `--json` |
| `baectl get profile <id>` | `GET /admin/v1/profiles/{id}` | `id` | `--json` |
| `baectl update profile <id> <provider> <model>` | `PUT /admin/v1/profiles/{id}` | `id`, `provider`, `model` | same optional flags as `create profile` (full replacement — mirrors the API) |
| `baectl delete profile <id>` | `DELETE /admin/v1/profiles/{id}` | `id` | — |
| `baectl create key <name> <profile_id>` | `POST /admin/v1/keys` | `name`, `profile_id` | `--json` |
| `baectl list keys` | `GET /admin/v1/keys` | — | `--limit <n>`, `--cursor <c>`, `--json` |
| `baectl delete key <id>` | `DELETE /admin/v1/keys/{id}` | `id` | — |
| `baectl auth create key` | *(local only — no API call)* | — | `--name <name>` (default `provisioned-admin`), `--out-dir <dir>` (default `.`) |

  - `baectl auth create key` is a distinct category from every other command above: it never talks to the admin API at all. It is a local key-generation utility for pre-provisioning a shared admin credential across multiple independent server replicas — see "Admin authentication" below for its exact output and how the server consumes it.
  - `--auth-token-env` is a deliberate ergonomic addition over the raw API: the API's `auth_token` field is a literal string that must already be the `${ENV_VAR_NAME}` template (`docs/profiles.md`); baectl builds that template string from a bare env var name so an operator never has to hand-type `${...}` quoting.
  - `--fallback` accepts a compact `provider:model:auth_token_env` triple (repeatable) rather than requiring a nested flag per fallback field, since `fallback_configs` is a list of full provider configs and a verbose flag set per fallback would be unwieldy; document the triple's exact grammar and separator escaping in `docs/reference/baectl.md`.
  - No client-side validation of `--mcp-server` names against the live registry at `create`/`update profile` time — the registry is runtime/config-file-driven (`bae-config.toml`) and can differ across restarts; a typo'd name is caught (non-fatally, logged) at session-creation time per `aspec/work-items/0003-full-message-passing.md`'s "Profile → MCP wiring" behavior, not here. (Optional, non-blocking nicety: `baectl` may fetch `GET /admin/v1/mcp-servers` and print a warning — not an error — if a given name isn't currently registered.)

### Auth / addressing (auto-configuration)
- Default admin address: `BAE_ADMIN_ADDR` env var if set, else `127.0.0.1:8081` (matches `server/src/config.rs`'s own default) — since `baectl` is invoked via `docker exec`/`container exec` inside the same container as `baesrv`, this "just works" with zero flags in the documented deployment model. Overridable with `--admin-addr <host:port>` for the rare case of running `baectl` outside the container (e.g. over an SSH tunnel), matching the flag-beats-env-var precedence already established for `baesrv --config`/`BAE_CONFIG`.
- Auth precedence, highest to lowest: (1) explicit `--admin-token <token>` / `BAE_ADMIN_TOKEN` env var — sent verbatim as `Authorization: Bearer <token>`, for scripting or an operator-held key not backed by a local file; (2) `--admin-key-file <path>` / `BAE_ADMIN_KEY_FILE` env var — reads the plaintext admin key from a file and sends it the same way; (3) the default path for that same file (see below) — auto-probed with no flag/env var needed at all. If none resolve to a usable token and the server enforces admin auth, requests fail `401` with a message pointing at all three options.
- Add a short "baectl" section to `aspec/uxui/cli.md` (today only documents `baesrv`'s own CLI) covering this command tree, its flag conventions, and its exit codes, so the CLI-design spec covers both binaries that now ship in the image.

### Admin authentication (bootstrap key lifecycle)

This is the mechanism that makes "auth method" auto-configuration real rather than aspirational. It implements the bootstrap-admin-key vision already sketched in `aspec/uxui/setup.md` and `aspec/architecture/security.md`, refined here from "print to stdout once" to "write to a stable file" — printing a secret to stdout means it only ever lives in container logs (which are frequently shipped to a log aggregator — a worse exposure surface than a single file on the data volume), and a log line can't be read programmatically by `baectl` the moment the container starts. A file at a well-known, persistent path can be.

**Server-side (`baesrv`), on every `serve` startup, after the store opens and before either listener binds:**
1. Query `keys` for an active (`deleted_at IS NULL`) row with `role = 'admin'`.
2. **If one exists and `--rotate-admin-key` was not given:** nothing to do — enforcement (below) uses whatever active admin key row(s) are present.
3. **If `--rotate-admin-key` was given:** soft-delete every active `role='admin'` row and delete the plaintext key file at `BAE_ADMIN_KEY_FILE` if it exists, then fall straight into step 4's self-generate path — a rotation **always** mints brand-new key material and ignores any pre-provisioned hash file that happens to be sitting at `BAE_ADMIN_KEY_HASH_FILE` (rotating while silently re-ingesting stale hash material would defeat the purpose of rotating). Log `INFO "admin key rotated"` with the new file's path — never the key value.
4. **If no active admin key exists (first boot, or just rotated) and no key was minted in step 3 yet:**
   - If a file exists at `BAE_ADMIN_KEY_HASH_FILE` (resolved path, see below): parse it and insert its `key_hash`/`prefix`/`name` directly into `keys` as a new `role='admin'` row. **The server never learns the plaintext in this path** — this is the multi-replica pre-provisioning flow (see `baectl auth create key` below). Log `INFO "admin key hash loaded from pre-provisioned file at <path>"`.
   - Else: self-generate — same CSPRNG + Argon2id path already used for client keys (`store::keys::generate_client_key`, generalized to take a role and prefix), but with a `bae_admin_` prefix (distinguishing admin keys from `bae_` client keys and `bae_ses_` session keys by sight). Insert the hash into `keys` (`role='admin'`), then **write the plaintext to `BAE_ADMIN_KEY_FILE`** with `0600` permissions (owner-only — the container already runs as the non-root `bae` user). Log `INFO "no admin key found; generated new admin key, written to <path>"` — never the key value.
5. **If `--dangerously-disable-admin-auth` was given:** skip steps 1–4 entirely (no key is created or rotated) and the admin router is built with no auth middleware layered on it — today's current zero-auth behavior, preserved as an explicit, loudly-logged opt-out (`WARN "admin API authentication is DISABLED (--dangerously-disable-admin-auth) — anyone able to reach the admin port has full control"` on every boot with this flag set, so it's never silently forgotten in a long-lived deployment). **Passing both `--dangerously-disable-admin-auth` and `--rotate-admin-key` together is a usage error (exit 2)** — rotating a key that won't be enforced is a contradiction the CLI should catch rather than silently accept.
6. Unless disabled, layer a new auth-enforcement middleware on the admin router (`server/src/api/admin/mod.rs`, alongside the existing `log_requests` layer): extract `Authorization: Bearer <token>`, hash-compare (Argon2id, constant-time, same `subtle::ConstantTimeEq` pattern as client/session key verification) against every active `role='admin'` row — there is normally exactly one, but the check does not assume that (a pre-provisioned-hash replica and a manually recovered replica could briefly have more than one) — `401` on no match or missing header.

**File locations and format** (both live under `/var/lib/bae`, the existing documented data volume — no new Docker volume needed, but operators should know this volume now also holds live credential material, relevant to `aspec/devops/infrastructure.md`'s "restrict volume/file access" guidance):
- `BAE_ADMIN_KEY_FILE` (default `/var/lib/bae/admin-key.pem`) — plaintext, single line, the `bae_admin_<random>` token. Written by the server only when it self-generates (step 4's second branch); deleted and rewritten on `--rotate-admin-key`. This is the file `baectl` auto-reads.
- `BAE_ADMIN_KEY_HASH_FILE` (default `/var/lib/bae/admin-key-hash.pem`) — a small JSON document, e.g. `{"key_hash": "$argon2id$v=19$m=65536,t=3,p=1$...", "prefix": "bae_admin_1a2b", "name": "provisioned-admin"}`. Read-only input to the server (it never writes this file itself) — an operator-supplied artifact produced by `baectl auth create key` (below). Argon2id's PHC string encoding embeds its own salt and cost parameters, so this hash is independently verifiable by the server with no coordination beyond both sides implementing the standard PHC format — `baectl` and `baesrv` don't need to share code or agree on out-of-band parameters.
- Both paths are also settable via `--admin-key-file <path>` / `--admin-key-hash-file <path>` flags on `baesrv`, parsed the same way as the existing `--config` flag.

**`baectl auth create key`** — the multi-replica pre-provisioning tool. Generates the *same two artifacts* described above, locally, with no network call:
- `<out-dir>/admin-key.pem` — a freshly generated plaintext `bae_admin_<random>` token (same CSPRNG as the server).
- `<out-dir>/admin-key-hash.pem` — the Argon2id PHC hash of that same token (same cost parameters as the server: memory 64 MiB, iterations 3, parallelism 1, output 32 bytes — matching `docs/reference/admin-api.md`'s "Key security" table) plus its prefix and `--name`.

  This lets an operator generate one admin credential once, then: drop `admin-key-hash.pem` onto the persistent volume of **every** replica (at `BAE_ADMIN_KEY_HASH_FILE`'s path) before/at first boot so each independently-running server (each with its own SQLite database, per `aspec/devops/infrastructure.md`'s "one server instance per database" rule) ingests the identical hash, and keep `admin-key.pem` centrally (or copy it to wherever `baectl` itself runs, at `BAE_ADMIN_KEY_FILE`'s path) so **one** plaintext key authenticates against all of them — without ever needing to log into any individual replica to read its self-generated key. `baectl auth create key` requires `argon2`/`rand` dependencies mirroring `server/Cargo.toml`'s choices (only this one command needs them; every other `baectl` command is a pure HTTP client).

### Output and conventions
- Follows `aspec/uxui/cli.md`'s existing conventions (established for `baesrv`, extended here to `baectl`): kebab-case long flags, `--help` on every command, stdout = command results only (scriptable), stderr = errors/diagnostics, exit codes `0` success / `1` runtime error (e.g. connection refused, API error) / `2` usage error (e.g. missing required positional).
- `--json` on every read/list/create/update command prints the single raw JSON document the admin API returned (or an array for list); default (no `--json`) prints a compact human-readable table/summary. `key create`'s plaintext `key` field is shown exactly once in both modes, with a stderr warning ("copy this now — it cannot be retrieved again"), mirroring the API's own one-time-display semantics — never written to a log file or cached.
- `list profiles`/`list keys` auto-paginate by default (follow `next_cursor` until it's `null`) and print the full result set — a human running `baectl list profiles` should not need to know the API is cursor-paginated. `--limit`/`--cursor` opt back into raw single-page behavior for scripting.

## Edge Case Considerations:

- **`--dangerously-disable-admin-auth` set**: docs must state plainly that anyone able to `docker exec`/`container exec` into the container (or reach `BAE_ADMIN_ADDR` directly) can then run any admin command with no credential — this is identical to today's (pre-this-work-item) reality and is why the flag is named "dangerously"; the server logs a loud `WARN` on every boot with it set so it's never silently forgotten in a long-running deployment.
- **`--dangerously-disable-admin-auth` and `--rotate-admin-key` both given**: usage error, exit `2` — contradictory (no point rotating a key nothing will check); caught at flag-parsing time, not left to run silently.
- **No admin key present and no hash file, first ever boot**: expected/normal — the server self-generates and writes `BAE_ADMIN_KEY_FILE`; not an error.
- **`BAE_ADMIN_KEY_HASH_FILE` present but malformed** (invalid JSON, missing `key_hash`/`prefix`, or a `key_hash` that isn't a valid Argon2id PHC string): startup usage error (exit 2) — an operator authoring/transfer mistake caught once at boot, analogous to `bae-config.toml`'s duplicate-name/bad-transport failure mode in `aspec/work-items/0003-full-message-passing.md`, not something to silently ignore or fall back from.
- **`BAE_ADMIN_KEY_FILE` already exists but no admin key row exists in `keys`** (e.g. operator manually deleted the DB row, or restored a DB backup from before the file existed): the server does not trust a pre-existing plaintext file as a source of truth for re-inserting a hash (it never re-hashes a file it finds) — it always follows the ordinary bootstrap decision (hash-file-if-present, else self-generate), so this stale file would be silently overwritten by a freshly generated (different) key on the next self-generate; document this explicitly so it isn't a surprise, and recommend operators delete stale files themselves.
- **`baectl` finds no usable token** (no `--admin-token`, no `BAE_ADMIN_KEY_FILE` at any resolved path) **and the server enforces auth**: every request fails `401`; `baectl` surfaces one clear message listing all three ways to supply a token, not a generic "unauthorized."
- **`baectl auth create key` output files land somewhere world-readable**: `admin-key.pem` is live credential material — write it (and instruct the operator to keep it) at restrictive permissions; `admin-key-hash.pem` is lower-sensitivity (one-way hash, can't be turned back into the plaintext) but still deserves care, since possessing it lets someone make *a* server accept a specific credential as admin if they can also plant the file on that server's volume before its first boot.
- **Multiple active `role='admin'` rows simultaneously** (e.g. a pre-provisioned-hash replica later also has a manually recovered key inserted): the enforcement middleware checks the bearer token against every active admin row, not just the first — any match succeeds; this is allowed, not an error.
- **Migrating the `keys` table's `role` `CHECK` constraint**: SQLite cannot `ALTER TABLE ... DROP CONSTRAINT`, so the new migration (below) must rebuild the table (create-copy-drop-rename) to widen `CHECK(role IN ('client','session'))` to include `'admin'` — existing `client`/`session` rows must be preserved byte-for-byte across the rebuild.
- **Server unreachable** (wrong `--admin-addr`, server not running, admin port not yet bound): a clear "could not connect to admin API at <addr>" message on stderr, exit `1` — not a raw `reqwest` error/backtrace.
- **Duplicate profile name** (`409 duplicate_name`): clean stderr message using the API's `detail`, exit `1`.
- **Delete profile with active keys** (`409 profile_in_use`): surface the API's `detail` (which includes the active-key count `n`) and suggest `baectl list keys` to find and `baectl delete key` them first, since the admin API does not return which specific keys reference a profile.
- **Key create against a missing/deleted profile** (`422 profile_unavailable`): clear message naming the profile id that wasn't found/active.
- **Get/delete/update on a nonexistent id** (`404 not_found`): clear message, exit `1`; not treated as a usage error since the id itself was well-formed.
- **Missing required positional** (e.g. `baectl create profile main` with no `provider`/`model`): usage error, exit `2`, `--help`-style message — clap's derive gives this for free but the exit-code mapping must be verified explicitly against `aspec/uxui/cli.md`'s convention.
- **Empty list results**: `baectl list profiles`/`list keys` with zero items print a clear "no profiles found"/"no keys found" rather than an empty table with only headers.
- **Static binary regression risk**: an accidental transitive `native-tls`/OpenSSL dependency (e.g. from a future crate addition) would silently break musl static linking — pin `reqwest`'s `default-features = false` and verify the build stays fully static (see Test Considerations).
- **Version skew**: since `baectl` and `baesrv` ship from the same image build, they're always in lockstep in the documented deployment; if someone runs a locally-built `baectl` against a differently-versioned server and gets an unexpected response shape, fail with "unexpected response from admin API — check baectl and server versions match" rather than a raw JSON-parse panic.
- **`--fallback`/`--mcp-server`/`--allowed-tool` repeated flags with zero occurrences**: correctly produce an empty array (`fallback_configs: []`, `mcp_servers: []`, `allowed_tools: []`), matching the API's own optional-field defaults — an empty `allowed_tools` still means "no client-side tools permitted," exactly as documented in `docs/reference/admin-api.md`.

## Test Considerations:

- **Unit — argument parsing**: every subcommand's required positionals reject omission (usage error, exit 2); repeatable flags (`--fallback`, `--mcp-server`, `--allowed-tool`) collect correctly into arrays including the zero-occurrence case; `--auth-token-env VAR` expands to the literal string `${VAR}`; the `--fallback` triple grammar parses and rejects malformed input with a usage error.
- **Unit — admin request building**: the JSON bodies built for `create profile`/`update profile`/`create key` match `docs/reference/admin-api.md`'s documented shapes field-for-field (`name`, `provider_config`, `fallback_configs`, `mcp_servers`, `allowed_tools` / `name`, `profile_id`).
- **Unit — response mapping**: 201/200/204 successes map to correct human and `--json` output; RFC 7807 error bodies (`400`/`404`/`409`/`422`) map to the right stderr message and exit code for each `type` slug (`bad_request`, `not_found`, `duplicate_name`, `profile_in_use`, `profile_unavailable`).
- **Integration — full CRUD lifecycle**: boot the real admin router (`server` crate's existing test harness, ephemeral port, temp DB) and drive `baectl` as a subprocess against it: create profile → get → list → update → delete; create key → list → delete. Assert output at each step. Fully offline, no real network, no real provider keys.
- **Integration — error surfaces**: duplicate profile name, delete-with-active-keys, key-create-against-missing-profile, get/delete-of-bogus-id — assert the correct exit code and a clean (non-JSON-dump) message for each.
- **Integration — pagination**: seed more than one page of profiles/keys against the test admin router; assert `baectl list profiles`/`list keys` auto-paginates and returns the full set by default, and that `--limit`/`--cursor` opt back into single-page raw behavior.
- **Regression — static binary**: a CI/Make check that builds `--target x86_64-unknown-linux-musl` and inspects the resulting binary (e.g. `file`, `ldd` expecting "not a dynamic executable" or musl-only linkage) to catch a transitive OpenSSL/glibc dependency regressing the static-binary requirement.
- **Regression — image smoke test**: `make image` builds successfully; `docker run --rm <image> baectl --help` (and the Apple `container` equivalent) succeeds with no Rust toolchain present, proving the bundled binary is genuinely static; same check against `make dev-image`.
- **Unit — migration**: the new `keys`-table-rebuild migration preserves every existing `client`/`session` row (id, hashes, timestamps) unchanged and afterward accepts a `role='admin'` insert that the old `CHECK` constraint would have rejected.
- **Unit — admin key generation**: generated plaintext has the `bae_admin_` prefix and ≥128 bits of entropy (matching the existing client/session key entropy test pattern in `server/src/store/keys.rs`); the Argon2id hash round-trips (verifies against its own plaintext, rejects a wrong one).
- **Integration — first-boot bootstrap**: fresh temp DB, no hash file, no existing admin row; start the server; assert exactly one `role='admin'` row is inserted, `BAE_ADMIN_KEY_FILE` is written with `0600` permissions, and the plaintext in that file authenticates successfully against a real admin endpoint.
- **Integration — second boot is a no-op**: restart against the same DB/file; assert no new admin row is inserted and the existing file is untouched (same content, same mtime-independent check via content comparison).
- **Integration — pre-provisioned hash file**: seed `BAE_ADMIN_KEY_HASH_FILE` (as `baectl auth create key` would produce) before first boot with no existing admin row; start the server; assert the given hash is ingested verbatim (not regenerated), `BAE_ADMIN_KEY_FILE` is **not** written (server never learns the plaintext), and the plaintext from the paired `admin-key.pem` `baectl auth create key` produced authenticates successfully.
- **Integration — `--rotate-admin-key`**: start with an existing admin key; restart with `--rotate-admin-key` (and, separately, with a hash file also present); assert the old plaintext no longer authenticates, a new admin row/file exists, and — critically — the rotation ignored the hash file and generated fresh material rather than re-ingesting it.
- **Integration — `--dangerously-disable-admin-auth`**: start with the flag; assert no admin key row is created, no file is written, and every `/admin/v1/*` route succeeds with **no** `Authorization` header at all (today's zero-auth behavior, preserved).
- **Integration — usage-error combination**: `--dangerously-disable-admin-auth` plus `--rotate-admin-key` together exits `2` before touching the DB or filesystem.
- **Integration — enforcement**: with auth enabled, every `/admin/v1/*` route (`profiles`, `keys`, `mcp-servers`) returns `401` with no/garbage bearer token and succeeds with the correct one; a client/session-role key must **not** be accepted on the admin port (role-scoped, mirroring the existing client-vs-session role check).
- **Integration — `baectl` auto-discovery**: with `BAE_ADMIN_KEY_FILE` present at the default path, `baectl` commands succeed with zero auth flags; with `--admin-token`/`BAE_ADMIN_TOKEN` also set, the explicit token takes precedence over the file (verify by pointing the file at a stale/wrong key and the explicit token at the real one).
- **Integration — `baectl auth create key` round-trip**: generate a pair with `baectl auth create key`; feed the hash file into a freshly booted test server via `BAE_ADMIN_KEY_HASH_FILE`; assert the plaintext file authenticates against that server — proving the two independent Argon2id implementations (server's and baectl's) are cross-compatible with no shared code.
- All new tests remain offline per existing convention (`make test-baectl`), matching `server`'s and `client-rust`'s existing test posture — no real network calls.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- New crate lives at `baectl/`, structured like `client-rust/` (own `Cargo.toml`, `Makefile` with `build`/`test`/`lint`/`fmt`/`clean`, `src/main.rs` + `src/cli.rs` + `src/admin_client.rs` + `src/output.rs` + `src/error.rs`), added to the root `Makefile`'s `COMPONENTS` list.
- Reuses the `reqwest` `rustls-tls`-only convention already established in `server/Cargo.toml` and `client-rust/Cargo.toml`, for the same static-binary-friendly reason — comment why, as those crates already do.
- Does **not** depend on `client-rust`/`bae-rs` — see "New component" above for why that scope boundary matters. `baectl` does add its own `argon2`/`rand` dependencies (mirroring `server/Cargo.toml`) for the `auth create key` local keygen path only.
- Update `aspec/architecture/design.md` with a new "Component 5: baectl (baectl/)" entry — a thin admin-API CLI bundled into both images, explicitly distinguished from Components 2–4 (which are published client libraries) since baectl is not published to crates.io/npm/PyPI.
- Update `aspec/devops/cicd.md`'s "Publishing" section: baectl has no independent `<component>-v<semver>` release tag or publish job — it ships only inside the Docker image build, so its version tracks the image tag, not its own SemVer line.
- Update `aspec/uxui/cli.md` with a baectl command-tree/flag-conventions section (see "Auth / addressing" above) and the two new `baesrv` flags (`--dangerously-disable-admin-auth`, `--rotate-admin-key`), since it currently documents only `baesrv`'s pre-existing `serve`/`migrate`/`key`/`version` surface. Note explicitly that `--rotate-admin-key` is a deliberate exception to the doc's own "every flag has an environment-variable equivalent" rule — an env var equivalent would rotate the key on **every** restart of a long-lived deployment (env vars tend to be baked into compose/k8s manifests and persist across restarts), which is exactly the surprising, unwanted behavior a one-shot operator action must avoid; `--dangerously-disable-admin-auth` does get a `BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH` env equivalent since leaving auth off is a standing deployment choice, not a one-shot action.
- Update `aspec/uxui/setup.md`'s "Initial configuration"/"Superuser access" sections to match the file-based mechanism actually built here (replacing "prints a one-time bootstrap admin API key to stdout" with "writes the bootstrap admin key to `BAE_ADMIN_KEY_FILE` on the data volume"), and note the recovery/rotation path is now `baesrv --rotate-admin-key` plus `baectl`'s automatic pickup, rather than an unimplemented `baesrv key create --role admin` — that CLI recovery subcommand `aspec/uxui/cli.md` already lists remains a distinct, not-yet-built path (direct DB/filesystem recovery when even the key file is lost) and is out of scope for this work item; call out that gap explicitly rather than silently leaving it unaddressed.
- Update `aspec/architecture/security.md`'s "Authentication" bullet to reflect the shipped mechanism (file-based, not stdout-printed) and reference this work item.
- Update `Dockerfile` (restructure the `build` stage's `COPY`/`cargo build` steps to build both `baesrv` and the musl-target `baectl`) and `Dockerfile.dev` (musl target + `musl-tools`) exactly as described above.
- New SQLite migration (`server/src/store/migrations/0006_admin_key_role.sql` or similar) rebuilding the `keys` table to widen its `role` `CHECK` constraint to `IN ('client','session','admin')` — SQLite requires a create-copy-drop-rename for a `CHECK` change, not an in-place `ALTER`; preserve every existing column and row exactly.
- New/extended store functions (e.g. `server/src/store/keys.rs` or a new `server/src/store/admin_key.rs`): `find_active_admin_key`, `insert_generated_admin_key` (generalizing the existing client-key generator to take a role + prefix), `insert_admin_key_from_hash`, `revoke_active_admin_keys` — plus the startup bootstrap/rotation sequencing (likely a new `server/src/admin_auth.rs` or folded into `cli.rs`'s `run_serve`, alongside the existing MCP-registry-loading step) and the new auth-enforcement middleware layered on `server/src/api/admin/mod.rs`'s router (next to the existing `log_requests` layer).
- Update `docs/reference/admin-api.md`: remove the "No authentication is required on the admin port" line, replace with the `Authorization: Bearer <admin_key>` requirement, a short description of the bootstrap-key file, and a mention of `baectl` as the recommended way to exercise these endpoints (curl remains fully documented, now with the required header added to every example — that page is the API reference, so it does not need `<details>` collapsing itself, unlike the guides/examples below).
- Add **`docs/reference/baectl.md`** — full command reference: every subcommand (including `auth create key`), its positional args and flags, exit codes, `--json` output shape per command, and the `--admin-addr`/`BAE_ADMIN_ADDR`, `--admin-token`/`BAE_ADMIN_TOKEN`, `--admin-key-file`/`BAE_ADMIN_KEY_FILE` settings and their precedence. Link it from `docs/README.md`'s Reference section.
- Add a **new `docs/guides/admin-authentication.md`** guide covering: how the bootstrap key is created and where to find it (`docker exec bae cat /var/lib/bae/admin-key.pem` as the manual/curl equivalent of what `baectl` does automatically), rotating with `--rotate-admin-key`, disabling with `--dangerously-disable-admin-auth` (and why not to in production), and the full multi-replica `baectl auth create key` walkthrough (generate → distribute the hash file to each replica's volume → distribute the plaintext file to wherever `baectl`/operators run). Link it from `docs/README.md`.
- Update `docs/reference/configuration.md` to add the new env vars: `BAE_ADMIN_KEY_FILE`, `BAE_ADMIN_KEY_HASH_FILE`, `BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH` (server); `BAE_ADMIN_ADDR` (reused), `BAE_ADMIN_TOKEN`, `BAE_ADMIN_KEY_FILE` (reused, client-side read) for baectl.
- Update every existing admin-API curl example across `docs/` (`docs/guides/quickstart.md`, `docs/guides/mcp-servers.md`, `docs/examples/mcp-profile.md`) to include the `Authorization: Bearer $ADMIN_KEY` header, with a one-line note on how to fetch `$ADMIN_KEY` (`docker exec bae cat /var/lib/bae/admin-key.pem`) — this is on top of, not instead of, the `<details>`-collapsed curl-alternative pattern described below.
- Update `docs/guides/quickstart.md` steps "2. Create a profile" and "3. Create a client key": show the `baectl create profile ...` / `baectl create key ...` command as the primary example, with the existing `docker exec ... curl ...` block moved inside a collapsed alternative:
  ```markdown
  ```sh
  docker exec bae baectl create profile main anthropic claude-sonnet-4-6 \
    --allowed-tool get_current_time
  ```

  <details>
  <summary>curl (alternative)</summary>

  ```sh
  docker exec -i bae sh << 'EOF'
  curl -s -X POST http://127.0.0.1:8081/admin/v1/profiles ...
  EOF
  ```
  </details>
  ```
  Apply the same primary-command-plus-collapsed-curl pattern to every profile/key-creation example in `docs/guides/mcp-servers.md` (its profile-creation step) and `docs/examples/mcp-profile.md`. **Do not** touch the session-open/message-send/session-close curl examples in `quickstart.md`, `docs/examples/session-basics.md`, or `docs/examples/live-events.md` — those hit the client port with a client/session key (JSON-RPC session loop), which `baectl` deliberately does not wrap (out of scope per the summary: profile and key management only).
- Verify `make image` and `make dev-image` still build, and `make build`/`test`/`lint`/`fmt`/`clean` (now iterating five components) all pass, since this changes the production `Dockerfile`'s build-stage structure and the root `Makefile`'s `COMPONENTS` list.
