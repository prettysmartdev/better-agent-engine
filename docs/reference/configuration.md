# Configuration Reference

BAE is configured through three independent mechanisms with distinct failure
semantics:

1. **CLI flags** — `baesrv --config <path>` (or `baesrv serve --config <path>`).
2. **Environment variables** — `BAE_*` vars control the server's runtime behavior.
3. **`bae-config.toml`** — optional file for MCP server registry and future
   operator-level settings.

Flag-beats-env-var precedence: when `--config` and `BAE_CONFIG` are both set,
`--config` wins.

---

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `BAE_ADDR` | `0.0.0.0:8080` | Client-facing listen address (plain HTTP). |
| `BAE_ADMIN_ADDR` | `127.0.0.1:8081` | Admin-only listen address. Must be a loopback address; the server refuses to start otherwise. |
| `BAE_DB_PATH` | `/var/lib/bae/bae.db` | SQLite database file path. Mount a volume here to persist data. |
| `BAE_LOG` | `info` | Tracing filter string, e.g. `baesrv=debug,tower=warn`. |
| `BAE_SHUTDOWN_TIMEOUT` | `30` | Seconds to drain in-flight requests on SIGTERM. |
| `BAE_CONFIG` | _(none)_ | Path to a `bae-config.toml` file. Overridden by `--config`. Absence is not an error. |
| `BAE_TURN_TIMEOUT` | `120` | Seconds a paused turn's owner has to return with its continuation before the turn is considered abandoned and the FIFO gate is released to the next queued driver. See [Wire Protocol — FIFO turn ownership](wire-protocol.md#fifo-turn-ownership-and-driver-registration). |
| `BAE_ADMIN_KEY_FILE` | `/var/lib/bae/admin-key.pem` | Plaintext admin-key file. Written by the server only when it self-generates a key (first boot or `--rotate-admin-key`); read by `baectl`. Overridden by `--admin-key-file`. |
| `BAE_ADMIN_KEY_HASH_FILE` | `/var/lib/bae/admin-key-hash.pem` | Pre-provisioned Argon2id admin-key hash file (read-only input — the server never writes it). Used for the multi-replica pre-provisioning flow. Overridden by `--admin-key-hash-file`. |
| `BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH` | _(unset)_ | Truthy (`1` or case-insensitive `true`) disables admin-port authentication entirely — the pre-this-feature zero-auth behavior. Also settable via `--dangerously-disable-admin-auth`. **Do not use in production**; see [Admin authentication](../guides/admin-authentication.md#disabling-admin-auth). |
| `BAE_SANDBOX_DRIVER` | `docker` | Which `SandboxDriver` implementation the server uses to launch sandboxes for [`session.startRemoteSandbox`](client-api.md#sessionstartremotesandbox): `docker` or `apple-container`. Any other value is a startup usage error (exit code 2, `ConfigError::InvalidSandboxDriver`). One driver is chosen server-wide, not per-profile — it reflects what container engine is actually installed on *this host*; `available_sandboxes` (see [Profiles — Available sandboxes](../profiles.md#available-sandboxes)) is the per-profile *image allowlist* layered on top of it. See [Sandboxes](../guides/sandboxes.md). |

Provider credentials (e.g. `ANTHROPIC_API_KEY`) are not BAE variables — they
are referenced from `[providers]` registry entries in `bae-config.toml` using
`${ANTHROPIC_API_KEY}` syntax and resolved by the server at call time. See
[Profiles](../profiles.md).

### `baectl` environment variables

`baectl` is a separate binary (see [baectl reference](baectl.md)) with its
own, client-side, environment variables:

| Variable | Default | Description |
|---|---|---|
| `BAE_ADMIN_ADDR` | `127.0.0.1:8081` | Admin API address to connect to. Same variable name as the server's listen address — since `baectl` runs inside the same container by default, the value that makes the server listen is also the value that makes `baectl` connect. Overridden by `--admin-addr`. |
| `BAE_ADMIN_TOKEN` | _(unset)_ | Admin bearer token, sent verbatim. Highest-precedence auth source. Overridden by `--admin-token`. |
| `BAE_ADMIN_KEY_FILE` | `/var/lib/bae/admin-key.pem` | Path `baectl` reads the plaintext admin key from, if `BAE_ADMIN_TOKEN`/`--admin-token` is not set. Same variable name and default path as the server's own `BAE_ADMIN_KEY_FILE` — `baectl` reads the exact file the server wrote. Overridden by `--admin-key-file`. |

See [baectl reference → Auto-configuration](baectl.md#auto-configuration)
for the full precedence order on both the address and the token.

---

## CLI flags

```
baesrv [--config <path>] [serve] [SERVE OPTIONS]   # start the server (default subcommand)
baesrv [--config <path>] migrate                   # run DB migrations and exit
```

`--config <path>` and `--config=<path>` are both accepted; the flag may appear
before or after the subcommand. When `--config` is given it overrides `BAE_CONFIG`.
A config path pointing to a file that does not exist is **not an error** — the
server starts with an empty MCP registry.

### `serve` options: admin-port authentication

| Flag | Env var | Description |
|---|---|---|
| `--admin-key-file <path>` | `BAE_ADMIN_KEY_FILE` | Plaintext admin-key file path (default `/var/lib/bae/admin-key.pem`). |
| `--admin-key-hash-file <path>` | `BAE_ADMIN_KEY_HASH_FILE` | Pre-provisioned hash file path (default `/var/lib/bae/admin-key-hash.pem`). |
| `--rotate-admin-key` | _(none — see below)_ | Revoke the current admin key and mint a fresh one this boot. |
| `--dangerously-disable-admin-auth` | `BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH` | Serve the admin port with no authentication. |

`--rotate-admin-key` is a deliberate exception to this doc's usual
flag/env-var pairing: it has **no** environment-variable equivalent. An env
var would rotate the key on every restart of a long-lived deployment (env
vars persist in compose/Kubernetes manifests across restarts), which is the
opposite of the one-shot action a rotation should be. Passing
`--rotate-admin-key` together with `--dangerously-disable-admin-auth` (flag
or env) is a usage error (exit `2`). See
[Admin authentication](../guides/admin-authentication.md) for the full
lifecycle these flags control.

---

## `bae-config.toml` schema

`bae-config.toml` is a TOML file pointed to via `--config` or `BAE_CONFIG`. It
holds the MCP server registry and the LLM provider registry; the top-level
table is designed to accommodate future sections (e.g. `[logging]`) without
restructuring either.

### Top-level layout

```toml
[mcp]
# ...

[providers]
# ...
```

A file with no `[mcp]` table, no `[providers]` table, or neither is valid —
each absent table yields an empty registry with no error. Unknown top-level
keys are ignored (forward-compatibility).

### `[[mcp.servers]]` entries

Each MCP server is an entry in the `[[mcp.servers]]` array. `name` must be
unique within the file — duplicate names cause a startup error (exit code 2).

**stdio server (subprocess):**

```toml
[[mcp.servers]]
name      = "filesystem"
transport = "stdio"
command   = "npx"
args      = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

**http/sse server (remote endpoint):**

```toml
[[mcp.servers]]
name      = "remote-search"
transport = "sse"
url       = "https://mcp.example.com/sse"
headers   = { Authorization = "Bearer ${SEARCH_MCP_TOKEN}" }
```

### Field reference

| Field | Required | Description |
|---|---|---|
| `name` | yes | Unique name; profiles reference this string in `mcp_servers`. |
| `transport` | yes | `"stdio"`, `"sse"`, or `"http"`. |
| `command` | for stdio | Executable to spawn (e.g. `"npx"`, `"uvx"`). |
| `args` | for stdio | Argument list for the subprocess. |
| `url` | for sse/http | Remote endpoint URL. |
| `headers` | for sse/http | HTTP headers map. Values may contain `${ENV_VAR}` tokens resolved at connect time, never persisted. |

### Startup validation errors

The following are fatal startup errors (exit code 2):

- Two `[[mcp.servers]]` entries share the same `name`.
- A `stdio` entry is missing `command`.
- An `sse` or `http` entry is missing `url`.
- `transport` is an unsupported value.
- The TOML is malformed.

A missing or unreadable config file (permission error) is also fatal. A missing
file path (file does not exist) is **not** an error — the server starts with an
empty registry.

### `${ENV_VAR}` substitution in headers

Header values may contain `${VAR_NAME}` tokens:

```toml
headers = { Authorization = "Bearer ${SEARCH_MCP_TOKEN}" }
```

Tokens are resolved immediately before each MCP connection attempt. The
resolved value is held only for that connection and never written to logs,
events, or the database. Unset variables cause that MCP server to fail to
connect and be skipped non-fatally (logged as an error). The raw token string
(e.g. `"Bearer ${SEARCH_MCP_TOKEN}"`) is what is stored in memory and returned
by `GET /admin/v1/mcp-servers` — never the resolved value.

---

## `[providers]`

`[providers]` is the LLM provider registry — declared once in
`bae-config.toml` and referenced by profiles by name
(`primary_provider`/`fallback_providers`), the same opt-in-by-name model as
`[mcp]`/`mcp_servers`. See [Profiles — Provider config](../profiles.md#provider-config)
for how profiles reference these entries, and the breaking change from the
prior inline `provider_config`/`fallback_configs` shape.

### `[[providers.entries]]` entries

Each provider connection is an entry in the `[[providers.entries]]` array.
`name` must be unique **within `providers.entries`** — duplicate or blank
names cause a startup error (exit code 2).

```toml
[providers]

[[providers.entries]]
name        = "anthropic-sonnet"
provider    = "anthropic"
model       = "claude-sonnet-4-6"
auth_token  = "${ANTHROPIC_API_KEY}"
max_tokens  = 8096

[[providers.entries]]
name        = "openai-gpt"
provider    = "openai"
model       = "gpt-5"
auth_token  = "${OPENAI_API_KEY}"
max_tokens  = 8096

# Any endpoint speaking the Anthropic Messages API (self-hosted gateway,
# proxy, etc.) — base_url always wins over the provider-kind default.
[[providers.entries]]
name        = "self-hosted-claude-gateway"
provider    = "anthropic"
base_url    = "https://llm-gateway.internal.example.com"
model       = "claude-sonnet-4-6"
auth_token  = "${INTERNAL_GATEWAY_TOKEN}"
```

### Field reference

| Field | Required | Description |
|---|---|---|
| `name` | yes | Unique name (within `providers.entries`); profiles reference this string in `primary_provider`/`fallback_providers`. |
| `provider` | yes | Wire format, not a vendor: `"anthropic"` (Messages API) or `"openai"` (Chat Completions API). A value outside this closed set is a TOML parse error (unknown enum variant) at startup. |
| `base_url` | no | Base URL for the provider's API. When omitted, defaults to the `provider` kind's own SaaS endpoint (see below). When set, it is always used **verbatim**, regardless of `provider` — a `provider = "openai"` entry pointed at a non-OpenAI host (or vice versa) is fully supported; any endpoint that speaks the selected wire format works. |
| `model` | yes | Model identifier. |
| `auth_token` | yes | API key or `${ENV_VAR}` reference, resolved at call time (see [`${ENV_VAR}` substitution](#env_var-substitution-in-headers) — same convention as MCP `headers`). |
| `max_tokens` | no | Max tokens per response. Default `4096`. |

`base_url` defaults, when omitted:

| `provider` | Default `base_url` |
|---|---|
| `"anthropic"` | `https://api.anthropic.com` |
| `"openai"` | `https://api.openai.com` |

Both defaults are bare hosts — the server appends the versioned path itself
(`/v1/messages` for `anthropic`, `/v1/chat/completions` for `openai`).

### No cross-namespace collision with `[mcp]`

**`[providers]` and `[mcp]` names are separate registries.** A `[providers]`
entry and an `[[mcp.servers]]` entry may share the same `name` with no error
— only duplicates *within* `providers.entries` (or *within* `mcp.servers`)
are rejected. Don't assume a name used in one section reserves it in the
other.

### Startup validation errors

The following are fatal startup errors (exit code 2), mirroring the `[mcp]`
posture:

- Two `[[providers.entries]]` entries share the same `name`.
- A `[[providers.entries]]` entry has a blank `name`.
- `provider` is not `"anthropic"` or `"openai"`.
- The TOML is malformed.

A missing or unreadable config file (permission error) is fatal for the whole
file (both `[mcp]` and `[providers]`). A missing file *path* (file does not
exist) is **not** an error — the server starts with empty registries for
both.

### Fatal-primary / logged-and-skipped-fallback

Resolution happens per-profile at session creation
(`POST /api/v1/sessions`) and join (`POST /api/v1/sessions/{id}/join`), not
at startup — a `[providers]` entry can be added, removed, or renamed and only
affects profiles the next time they're used to open or join a session. The
one asymmetry versus `[mcp]`/`mcp_servers`:

- A profile's `primary_provider` name not found in the registry is **fatal**
  for every client key on that profile: `422 primary_provider_unavailable`,
  logged on every attempt, no session created.
- A profile's `fallback_providers` entry not found in the registry is
  **logged and skipped**, independently per name — never fatal, exactly like
  an unresolvable `mcp_servers` name.

See [Profiles — Fatal primary / non-fatal fallback](../profiles.md#fatal-primary--non-fatal-fallback)
for the full behavior and [Admin API](admin-api.md#post-adminv1profiles--create) for
the profile request/response shape.

---

## Sandbox driver

`BAE_SANDBOX_DRIVER` selects which `SandboxDriver` implementation the server
uses to launch containers for `session.startRemoteSandbox` — see
[Sandboxes](../guides/sandboxes.md) for the full feature and
[Profiles — Available sandboxes](../profiles.md#available-sandboxes) for the
per-profile image allowlist layered on top of it.

| Value | Driver |
|---|---|
| `docker` (default) | Shells out to the `docker` CLI: `docker image inspect`/`pull`, `docker run -d --rm <image> sleep infinity`, `docker exec <id> sh -c <command>`, `docker stop <id>`. |
| `apple-container` | Shells out to the `container` CLI (`container images inspect`/`pull`/`run`/`exec`/`stop`). Only usable on macOS. |

Any other value is a fatal startup usage error (exit code 2,
`ConfigError::InvalidSandboxDriver`).

The driver is chosen **once, server-wide** — never per-profile — because it
reflects what container engine is actually installed on *this host*.
`available_sandboxes` (a profile field) is a separate, per-profile concern:
the *image allowlist* on top of whichever host-wide driver is configured.

### `apple-container` on a non-macOS host

Selecting `apple-container` on a host that isn't macOS is only fatal at
startup **if at least one profile declares a non-empty
`available_sandboxes`** — a driver that cannot function is only a problem
once something actually depends on it. If no profile declares any sandbox
images, the server starts anyway with a driver that fails every call as
`Unsupported` (so a later profile update that adds `available_sandboxes`
would need `BAE_SANDBOX_DRIVER` corrected and the server restarted). This
mirrors the exit-code-2 usage-error posture already used for other
operator-authoring mistakes (e.g. a malformed `bae-config.toml`).

### Abandoned containers are not automatically cleaned up

**A server that is killed (not gracefully closed) leaves no record of the
sandbox containers it started, and does not clean them up.** Both the
in-memory map of running remote sandboxes and the per-profile image-status
cache are process-local state — the same posture as the MCP session
registry — so a restarted server has no way to know what a *prior* process
started. There is **no startup reconciliation pass** that inspects the host
for stray containers from a previous run.

Operators running the Docker or Apple Containers driver in production should
rely on `--rm`-equivalent-on-crash host-level hygiene (the containers this
work item starts are themselves launched with `--rm`, so they are removed
when they stop cleanly — the gap is specifically containers left **running**
by an ungracefully-killed server) or a periodic external sweep. Do not
assume the server will ever clean these up on your behalf.

---

## Example configs

Ready-to-run examples are in [`examples/bae-config/`](../../examples/bae-config/):

| File | What it runs |
|---|---|
| [`filesystem.toml`](../../examples/bae-config/filesystem.toml) | `@modelcontextprotocol/server-filesystem` over stdio via `npx` (requires Node.js). |
| [`fetch.toml`](../../examples/bae-config/fetch.toml) | `mcp-server-fetch` over stdio via `uvx` (requires Python + uv). |
| [`multi-server.toml`](../../examples/bae-config/multi-server.toml) | filesystem + fetch + git stdio servers plus a placeholder SSE entry. |
| [`providers.toml`](../../examples/bae-config/providers.toml) | `[providers]` registry: an Anthropic entry, an OpenAI entry, and an Anthropic-wire-format entry at a self-hosted `base_url`. |

For a hands-on walkthrough using the MCP examples see
[MCP Servers](../guides/mcp-servers.md); for a multi-driver session walkthrough
that opens a session against a `[providers]`-backed profile see
[Multi-Client Sessions](../guides/multi-client-sessions.md).

---

## Admin endpoint: `GET /admin/v1/mcp-servers`

Returns the currently loaded MCP registry — useful to confirm what a running
server has available without reading the config file:

```sh
ADMIN_KEY=$(docker exec bae cat /var/lib/bae/admin-key.pem)
curl http://127.0.0.1:8081/admin/v1/mcp-servers -H "Authorization: Bearer $ADMIN_KEY"
```

```json
{
  "items": [
    {"name": "filesystem", "transport": "stdio"},
    {"name": "fetch",      "transport": "stdio"}
  ]
}
```

Items are sorted by name. Secrets (`command`, `args`, `url`, `headers`) are
never returned. See [Admin API](admin-api.md#get-adminv1mcp-servers).

---

## Admin endpoint: `GET /admin/v1/providers`

Returns the currently loaded provider registry — the set of entries parsed
from `[providers]` in `bae-config.toml` at startup:

```sh
curl http://127.0.0.1:8081/admin/v1/providers
```

```json
{
  "items": [
    {"name": "anthropic-sonnet", "provider": "anthropic", "model": "claude-sonnet-4-6", "base_url": "https://api.anthropic.com"},
    {"name": "openai-gpt",       "provider": "openai",    "model": "gpt-5",             "base_url": "https://api.openai.com"}
  ]
}
```

Items are sorted by name. `base_url` is always the **effective** value
(resolved default when the entry omitted it, or the explicit value
otherwise) — never `auth_token`. See
[Admin API](admin-api.md#get-adminv1providers).

---

## Admin endpoint: `GET /admin/v1/sandbox-status`

Returns the in-memory sandbox-image provisioning status for every profile
that has declared `available_sandboxes` — useful to confirm an image finished
pulling without grepping server logs:

```sh
curl http://127.0.0.1:8081/admin/v1/sandbox-status
```

```json
{
  "items": [
    {
      "profile_id": "pro_…",
      "images": [
        {"name": "python:3.12", "status": "available"},
        {"name": "node:22", "status": "error", "detail": "pull failed: unauthorized"}
      ]
    }
  ]
}
```

One item **per profile** — never a flat, cross-profile image list, the same
per-profile scoping [`session.sandbox.available`](message-types.md#sessionsandboxavailable)
and `session.startRemoteSandbox` enforce (see
[Sandboxes — The profile-scoping guarantee](../guides/sandboxes.md#the-profile-scoping-guarantee)).
Items are sorted by `profile_id`, then by image name within each profile.
`status` is one of `pending`/`available`/`error`; `detail` is present only on
`error`. Rebuilt from a fresh `pending` state for every declared image at
server restart (see [Abandoned containers](#abandoned-containers-are-not-automatically-cleaned-up)
above — this endpoint reflects the current process's view only).
