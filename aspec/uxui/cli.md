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
- `setup` — interactive quickstart wizard; local scaffolding (generates a launcher, `.env`, and `bae-config.toml`) with an optional final step that launches the deployment and creates a first profile/key. See [Setup wizard](#setup-wizard) below and [baectl reference — `baectl setup`](../../docs/reference/baectl.md#baectl-setup) for the full question list.

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
- `setup`-only flags, not shared with the rest of `baectl` (no admin-API
  call to make, so no `--json`/auto-configuration flags apply): `--dev`
  (use locally-built `make image`/`make image-max` tags instead of the
  published GHCR tags), `--apple` (emit a `bae-setup.sh` script driving
  Apple's `container` CLI instead of `docker-compose.yml`), `--dir <DIR>`
  (directory to read/write the generated files in, default `.`, mirroring
  `auth create key`'s `--out-dir`).

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

##### Setup wizard
`setup` is the **one** `baectl` command whose "stdin: unused" line above does
not apply — it reads interactive stdin/stdout Q&A (each question defaulted,
so a bare enter walks the whole wizard) to build a deployment before a server
exists to talk to. When stdin isn't a TTY (piped/CI), every question falls
back to its default with nothing printed, and the launch question
specifically defaults to declining rather than the interactive default — see
[baectl reference — `baectl setup`](../../docs/reference/baectl.md#baectl-setup)
for the full question list, generated-file shapes, and exit codes. Every
other `baectl` command's "stdin: unused" line stays accurate.

See [baectl reference](../../docs/reference/baectl.md) for the complete,
implementation-verified command surface, and
[Admin authentication](../../docs/guides/admin-authentication.md) for the
key lifecycle it auto-discovers.

## baesched and baeapi (work item 0014 — harness launchers)

Two more binaries ship, one per launcher base image family, never alongside
`baesrv`/`baectl` in the same image: `baesched` (`bae-launcher-schedule`) and
`baeapi` (`bae-launcher-api` and, unmodified, `bae-launcher-webapp`). Both are
**base-image binaries an agent developer's own Dockerfile is expected to run
unmodified** — they take no subcommands and no CLI flags of their own at all;
every setting is an env var, matching the Docker-first, flag-free posture
`baesrv` itself uses for its own env-first configuration surface. See
[Harness Launchers](../../docs/guides/harness-launchers.md) and
[Harness Launchers reference](../../docs/reference/launchers.md) for the full
walkthrough and schema.

### baesched

Binary name: baesched
Install path: `/usr/local/bin` inside the `bae-launcher-schedule` base image
Storage location: none of its own — reads `bae-schedules.toml` once at startup; no database, no local persistence of run history or output.

#### Environment variables

| Variable | Default | Description |
|---|---|---|
| `BAE_SCHEDULES_CONFIG` | `/etc/bae/bae-schedules.toml` | Path to the config file. A missing file is not an error (starts with zero agents); a malformed one is. |
| `BAE_SCHEDULES_SHUTDOWN_TIMEOUT` | `30` | Whole seconds of grace given to in-flight invocations after `SIGTERM`/`SIGINT` before they're force-killed. A non-integer value is a usage error at startup. |
| `BAE_LOG` | `info` | Tracing filter for `baesched`'s own stderr logs, same variable name and posture as `baesrv`'s `BAE_LOG`. |

#### Inputs and outputs

- stdin: unused.
- stdout: forwarded child stdout only, each line prefixed `[name] ` — never `baesched`'s own logs.
- stderr: `baesched`'s own tracing logs, plus forwarded child stderr (also `[name] `-prefixed).
- Exit codes: **0** success (including a clean signal-triggered shutdown — a received `SIGTERM`/`SIGINT` is never itself a failure), **1** runtime error (a `tokio-cron-scheduler` internal initialization failure), **2** usage error (malformed TOML, an unknown top-level field, a duplicate/blank agent `name`, a schedule that isn't exactly six cron fields or doesn't parse, an invalid `BAE_SCHEDULES_SHUTDOWN_TIMEOUT` — **and, unlike `baeapi` below, also an existing-but-unreadable config file**: `baesched` maps that case to a usage error, not a runtime one).

#### No HTTP surface

`baesched` opens no port and has no `/healthz` — by design, not omission; see
[Infrastructure — the three launcher base images](../devops/infrastructure.md#the-three-launcher-base-images-extended-not-run-standalone).
Liveness is process-level only (`docker ps`, the container's exit status).

### baeapi

Binary name: baeapi
Install path: `/usr/local/bin` inside both the `bae-launcher-api` and `bae-launcher-webapp` base images (the exact same binary in both)
Storage location: none of its own — reads `bae-api.toml`/`bae-app.toml` once at startup; no database, no persisted run history — a trigger's output is streamed to the caller and forwarded to `baeapi`'s own stdout/stderr (`docker logs`), never written to a file.

#### Environment variables

| Variable | Default | Description |
|---|---|---|
| `BAE_LAUNCHER_API_CONFIG` | `/etc/bae/bae-api.toml` in `bae-launcher-api`; `/etc/bae/bae-app.toml` in `bae-launcher-webapp` | Path to the config file. Same variable name in both images; only the baked-in default path differs per image. |
| `BAE_LAUNCHER_API_ADDR` | `0.0.0.0:9090` | Listen address; overrides `[server] addr` from the config file. |
| `BAE_LAUNCHER_API_TOKEN` | _(unset)_ | Bearer token gating every `/agents/*` route (never `/healthz`/`/_launcher/*`). **Unset by default — open port, loud startup warning.** See [Security](../architecture/security.md). |
| `BAE_LAUNCHER_WEBAPP_STATIC_DIR` | _(unset in `bae-launcher-api`)_ | When set to a non-empty value, additionally serves that directory as a static SPA at `/` with an `index.html` client-side-routing fallback. Baked in by `bae-launcher-webapp`'s own Dockerfile `ENV`; left unset in `bae-launcher-api`. |
| `BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT` | `30` | Whole seconds the post-signal graceful drain may take before the process exits anyway, force-killing still-running child invocations. A non-integer value is a usage error at startup. Mirrors `BAE_SCHEDULES_SHUTDOWN_TIMEOUT`. |
| `BAE_LOG` | `info` | Tracing filter for `baeapi`'s own stderr logs. |

#### Inputs and outputs

- stdin: unused.
- stdout: forwarded child stdout only, each line prefixed `[name] ` — the same posture as `baesched`, so `docker logs` carries every agent's attributed output. The same lines are *also* streamed into that trigger's own HTTP response body (`POST /agents/{name}/trigger`, `application/x-ndjson`).
- stderr: `baeapi`'s own tracing logs (startup, one line per HTTP request, `/healthz` at DEBUG so health checks don't drown the log), plus forwarded child stderr (also `[name] `-prefixed).
- Exit codes: **0** success (clean shutdown after a `SIGTERM`/`SIGINT` — the graceful drain is bounded by `BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT`, so even a hung child's open trigger request only delays exit, never prevents it), **1** runtime error (an unreadable-but-present config file, or a listener bind failure), **2** usage error (malformed TOML, an unknown/wrong-shape top-level key such as a singular `[agent]` table, an invalid `request_schema`, a duplicate/blank agent `name`, an invalid `BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT`).

#### Fixed routes

One shared `axum` router serves every configured agent's trigger route plus
three routes present on every instance regardless of agent count:

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `POST` | `/agents/{name}/trigger` | Bearer, if `BAE_LAUNCHER_API_TOKEN` is set | Validate the JSON body against the agent's `request_schema`, template it into env/args, spawn, and stream the response as chunked NDJSON. |
| `GET` | `/healthz` | never | `200 OK`, empty body. |
| `GET` | `/_launcher/agents` | never | Every configured agent's safe presentation fields and `request_schema`, in config order — never `command`/`args`/`env`/`env_template`/`arg_template` or a resolved `${VAR}` value. |
| `GET` | `/_launcher/agents/{name}` | never | Single-agent detail, same shape; `404` for an unknown name. |

Full field-by-field schema and per-route request/response detail:
[Harness Launchers reference](../../docs/reference/launchers.md).
