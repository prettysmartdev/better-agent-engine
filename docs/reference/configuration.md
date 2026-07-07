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
| `BAE_ADMIN_KEY_FILE` | `/var/lib/bae/admin-key.pem` | Plaintext admin-key file. Written by the server only when it self-generates a key (first boot or `--rotate-admin-key`); read by `baectl`. Overridden by `--admin-key-file`. |
| `BAE_ADMIN_KEY_HASH_FILE` | `/var/lib/bae/admin-key-hash.pem` | Pre-provisioned Argon2id admin-key hash file (read-only input — the server never writes it). Used for the multi-replica pre-provisioning flow. Overridden by `--admin-key-hash-file`. |
| `BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH` | _(unset)_ | Truthy (`1` or case-insensitive `true`) disables admin-port authentication entirely — the pre-this-feature zero-auth behavior. Also settable via `--dangerously-disable-admin-auth`. **Do not use in production**; see [Admin authentication](../guides/admin-authentication.md#disabling-admin-auth). |

Provider credentials (e.g. `ANTHROPIC_API_KEY`) are not BAE variables — they
are referenced from profile configs using `${ANTHROPIC_API_KEY}` syntax and
resolved by the server at call time. See [Profiles](../profiles.md).

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
currently holds the MCP server registry; the top-level table is designed to
accommodate future sections (e.g. `[logging]`, `[providers]`) without
restructuring the MCP section.

### Top-level layout

```toml
[mcp]
# ...
```

A file with no `[mcp]` table is valid. Unknown top-level keys are ignored
(forward-compatibility).

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

## Example configs

Ready-to-run examples are in [`examples/bae-config/`](../../examples/bae-config/):

| File | What it runs |
|---|---|
| [`filesystem.toml`](../../examples/bae-config/filesystem.toml) | `@modelcontextprotocol/server-filesystem` over stdio via `npx` (requires Node.js). |
| [`fetch.toml`](../../examples/bae-config/fetch.toml) | `mcp-server-fetch` over stdio via `uvx` (requires Python + uv). |
| [`multi-server.toml`](../../examples/bae-config/multi-server.toml) | filesystem + fetch + git stdio servers plus a placeholder SSE entry. |

For a hands-on walkthrough using these files see [MCP Servers](../guides/mcp-servers.md).

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
