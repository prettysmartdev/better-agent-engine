# CLI Design

Two binaries ship in the image: `baesrv` (the server) and `baectl` (an admin
API CLI). They share the same flag/exit-code/I-O conventions below except
where noted.

## baesrv

Binary name: baesrv
Install path: /usr/local/bin (inside the Docker image; the image is the primary distribution)
Storage location: /var/lib/bae/ (SQLite database and server data; overridable via BAE_DB_PATH)

### Design principles:

#### Command structure
Top level command groups:
- `serve` — run the HTTP server (the default when no subcommand is given)
- `migrate` — apply pending database migrations and exit (for operators who want migrations separate from serving)
- `key` — bootstrap/admin key operations (e.g. `key create --role admin`) for recovery without API access. **Not yet built** — see uxui/setup.md's "Superuser access" for the currently-shipped recovery path (`--rotate-admin-key`) and this gap's scope.
- `version` — print version and supported API versions

#### Flag structure
Flag guidance:
- Long flags in kebab-case (`--db-path`, `--addr`); every flag has an environment-variable equivalent (`BAE_DB_PATH`, `BAE_ADDR`) and flags take precedence over env vars.
- No required flags: every option has a sensible default so `baesrv` alone starts a working server.
- `--help` on every command; global `--json` for machine-readable output.

`serve`'s admin-port-authentication flags (work item 0004):
- `--admin-key-file <path>` (env `BAE_ADMIN_KEY_FILE`, default `/var/lib/bae/admin-key.pem`) — plaintext admin-key file the server writes on self-generate and `baectl` reads.
- `--admin-key-hash-file <path>` (env `BAE_ADMIN_KEY_HASH_FILE`, default `/var/lib/bae/admin-key-hash.pem`) — pre-provisioned Argon2id hash file to ingest (read-only input).
- `--rotate-admin-key` — revoke the current admin key and mint a fresh one this boot. **Deliberate exception to the "every flag has an env-var equivalent" rule above: no env-var equivalent exists.** An env var would rotate the key on every restart of a long-lived deployment (env vars tend to be baked into compose/k8s manifests and persist across restarts) — exactly the surprising, unwanted behavior a one-shot operator action must avoid. Combining this with `--dangerously-disable-admin-auth` is a usage error (exit 2): rotating a key that won't be enforced is a contradiction.
- `--dangerously-disable-admin-auth` (env `BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH`) — serve the admin port with no authentication (the pre-0004 zero-auth behavior). **Does** get an env-var equivalent, unlike `--rotate-admin-key` above: leaving auth off is a standing deployment choice (e.g. baked into a dev-only compose file), not a one-shot action, so the usual flag/env pairing applies.

See [Admin authentication](../../docs/guides/admin-authentication.md) and
[Admin API reference](../../docs/reference/admin-api.md) for the full
lifecycle these flags control.

#### Inputs and outputs
I/O Guidance:
- stdin: unused; the server is configured entirely via flags/env, not piped input.
- stdout: command results only (e.g. a created key, version info) so output is scriptable; with `--json`, results are single JSON documents.
- stderr: all logs (tracing output), human-readable by default, JSON lines when `BAE_LOG_FORMAT=json`.
- Exit codes: 0 success, 1 runtime error, 2 usage error.

#### Configuration
Global config:
- Environment variables are the configuration surface (`BAE_ADDR`, `BAE_DB_PATH`, `BAE_LOG`); no config file is required, matching the Docker-first deployment model.
- If a config file is ever added it will be explicitly opted into via `--config <path>`, with env/flags still taking precedence.

## baectl

Binary name: baectl
Install path: /usr/local/bin (inside both the dev and production Docker images, alongside `baesrv`; compiled as a static `x86_64-unknown-linux-musl` binary — see devops/cicd.md and architecture/design.md's Component 5)
Storage location: none of its own — a pure HTTP client over the admin API. `auth create key` writes two local files (`admin-key.pem`, `admin-key-hash.pem`) into an operator-chosen `--out-dir`, not a fixed storage location.

### Design principles:

#### Command structure
Verb-first, resource-typed positional, mapping 1:1 onto the admin API's CRUD surface:
- `create profile <name> <provider> <model>` / `create key <name> <profile_id>`
- `list profiles` / `list keys` (cursor-paginated; auto-paginates by default)
- `get profile <id>`
- `update profile <id> <provider> <model>` (full replacement, mirroring the API's `PUT`)
- `delete profile <id>` / `delete key <id>`
- `auth create key` — local-only admin-key-pair generation (no API call); pre-provisions a shared admin credential across multiple server replicas.

Profiles get the full CRUD set; keys get create/list/delete only — there is
no single-key-get or key-update endpoint on the admin API (keys are
immutable besides revocation), so `baectl` does not invent commands the API
can't back. Session management (open/send-message/close) is explicitly out
of scope — that hits the client port with a client/session key and is left
to the published client libraries.

#### Flag structure
Flag guidance (same conventions as `baesrv` above):
- Long flags in kebab-case; required information is positional, optional
  fields are flags (per the work item's own guidance).
- Global flags valid on every subcommand: `--admin-addr <host:port>`,
  `--admin-token <token>`, `--admin-key-file <path>` — the auto-configuration
  surface (see below).
- `--json` on every read/list/create/update command: prints the exact JSON
  document the admin API returned (an array for an auto-paginated list);
  default is a compact human-readable summary/table.
- `--help` on every command and subcommand.

#### Auto-configuration
Unlike `baesrv`, `baectl` is a client with nothing to bind — its
"configuration" is discovering where the server is and how to authenticate,
with zero flags required in the documented `docker exec`/`container exec`
deployment:
- Admin address: `--admin-addr` > `BAE_ADMIN_ADDR` env var > default
  `127.0.0.1:8081` (matches `baesrv`'s own default).
- Admin token: `--admin-token`/`BAE_ADMIN_TOKEN` > `--admin-key-file`/
  `BAE_ADMIN_KEY_FILE` (explicit) > the default probed path
  `/var/lib/bae/admin-key.pem` (the same file `baesrv` writes on
  self-generate; absence at the default path is not an error).

#### Inputs and outputs
I/O Guidance (identical to `baesrv`'s conventions):
- stdin: unused.
- stdout: command results only (scriptable); `--json` results are single
  JSON documents (or a flat array for auto-paginated lists).
- stderr: diagnostics, one-off warnings (e.g. "copy this key now"), and
  error messages, prefixed `baectl: `.
- Exit codes: 0 success, 1 runtime error (connection failure, any admin API
  error response, an unexpected/unparseable response body — version skew),
  2 usage error (a missing required positional or a value `baectl` rejects
  itself, e.g. a malformed `--fallback` spec).

See [baectl reference](../../docs/reference/baectl.md) for the complete,
implementation-verified command surface, and
[Admin authentication](../../docs/guides/admin-authentication.md) for the
key lifecycle it auto-discovers.
