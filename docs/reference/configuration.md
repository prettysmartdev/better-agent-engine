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

Provider credentials (e.g. `ANTHROPIC_API_KEY`) are not BAE variables — they
are referenced from `[providers]` registry entries in `bae-config.toml` using
`${ANTHROPIC_API_KEY}` syntax and resolved by the server at call time. See
[Profiles](../profiles.md).

---

## CLI flags

```
baesrv [--config <path>] [serve]   # start the server (default subcommand)
baesrv [--config <path>] migrate   # run DB migrations and exit
```

`--config <path>` and `--config=<path>` are both accepted; the flag may appear
before or after the subcommand. When `--config` is given it overrides `BAE_CONFIG`.
A config path pointing to a file that does not exist is **not an error** — the
server starts with an empty MCP registry.

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
curl http://127.0.0.1:8081/admin/v1/mcp-servers
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
