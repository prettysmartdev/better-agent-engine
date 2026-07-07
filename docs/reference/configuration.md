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

Provider credentials (e.g. `ANTHROPIC_API_KEY`) are not BAE variables — they
are referenced from profile configs using `${ANTHROPIC_API_KEY}` syntax and
resolved by the server at call time. See [Profiles](../profiles.md).

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
