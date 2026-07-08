# MCP Servers

BAE can connect to MCP (Model Context Protocol) servers and make their tools
available to sessions. This guide walks through connecting the
`@modelcontextprotocol/server-filesystem` server end to end, then shows how the
same steps generalize to other servers.

---

## Prerequisites

- A running BAE server (see [Quickstart](quickstart.md)).
- **Node.js** with `npx` on PATH — used by the filesystem server.
- `curl` for the admin API calls.

---

## Step 1 — Copy the example config and start the server

The repo ships a ready-to-run config file at
[`examples/bae-config/filesystem.toml`](../../examples/bae-config/filesystem.toml):

```toml
[mcp]

[[mcp.servers]]
name      = "filesystem"
transport = "stdio"
command   = "npx"
args      = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

Point the server at it in one of two ways:

```sh
# Via the CLI flag (wins over BAE_CONFIG when both are set):
baesrv --config examples/bae-config/filesystem.toml

# Via environment variable:
BAE_CONFIG=examples/bae-config/filesystem.toml baesrv
```

For Docker, pass it as an environment variable and mount the config into the
container:

```sh
docker run -d \
  --name bae \
  -p 8080:8080 \
  -v bae-data:/var/lib/bae \
  -v $(pwd)/examples/bae-config:/cfg:ro \
  -e BAE_CONFIG=/cfg/filesystem.toml \
  -e ANTHROPIC_API_KEY="sk-ant-…" \
  better-agent-engine
```

> **The `args` path `/data` is the directory the filesystem server can access.**
> Change it to a directory you actually want the agent to read and write — e.g.
> `/home/user/projects` or a mounted data volume.

---

## Step 2 — Restart and confirm registration

After (re)starting, verify the server loaded the config:

```sh
curl http://127.0.0.1:8081/admin/v1/mcp-servers
```

Expected response:

```json
{
  "items": [
    {"name": "filesystem", "transport": "stdio"}
  ]
}
```

If `items` is empty, the server started without finding the config file. Check
that the path is correct and that the process has permission to read it.

> The endpoint shows names and transport types only — no secrets, no paths.
> See [Admin API](../reference/admin-api.md#get-adminv1mcp-servers).

---

## Step 3 — Create or update a profile with `mcp_servers`

`mcp_servers` is now an array of **server names** (strings that must match an
entry in `bae-config.toml`):

This assumes `bae-config.toml` also declares an `anthropic-sonnet` entry
under `[providers]` (see [Configuration — `[providers]`](../reference/configuration.md#providers))
— `[mcp]` and `[providers]` coexist freely in one file, as
[`examples/bae-config/providers.toml`](../../examples/bae-config/providers.toml)'s
header comment notes.

```sh
curl -s -X POST http://127.0.0.1:8081/admin/v1/profiles \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "fs-assistant",
    "primary_provider": "anthropic-sonnet",
    "mcp_servers": ["filesystem"],
    "allowed_tools": []
  }'
```

To update an existing profile (full replacement):

```sh
curl -s -X PUT http://127.0.0.1:8081/admin/v1/profiles/pro_… \
  -H 'Content-Type: application/json' \
  -d '{ …, "mcp_servers": ["filesystem"] }'
```

> **Name-matching happens at session-creation time**, not when the profile is
> saved. If a name in `mcp_servers` is not in the current registry when a
> session is opened, BAE logs an error and skips that server — session creation
> still succeeds. See [Non-fatal skips](#non-fatal-skips) below.

---

## Step 4 — Issue a client key

```sh
curl -s -X POST http://127.0.0.1:8081/admin/v1/keys \
  -H 'Content-Type: application/json' \
  -d '{"name": "fs-agent", "profile_id": "pro_…"}'
```

Copy the `key` field — it is shown exactly once.

```sh
CLIENT_KEY="bae_…"
```

---

## Step 5 — Open a session

```sh
SESSION=$(curl -s -X POST http://localhost:8080/api/v1/sessions \
  -H "Authorization: Bearer $CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"client_version": "1.0.0", "tools": []}')

SESSION_ID=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_id'])")
SESSION_KEY=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_key'])")
```

At this point BAE connected to `npx @modelcontextprotocol/server-filesystem`,
ran the MCP `initialize` handshake, and fetched its tool list — those tools are
now advertised to the provider alongside any client-declared tools.

---

## Step 6 — Send a message that triggers the filesystem tool

Register as a driver first (required once per session key, before its first
`session.sendMessage`):

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.registerDriver","params":{}}'
```

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "session.sendMessage",
    "params": {
      "message": {
        "role": "user",
        "content": "List the files in /data and tell me what you find."
      }
    }
  }'
```

The NDJSON stream will include MCP events as the tool runs, followed by the
terminal result:

```
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"client.message.send",…}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"provider.request",…}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"provider.response",…}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"tool.call","payload":{"name":"list_directory","dispatch":"mcp","server_name":"filesystem",…}}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"mcp.request","payload":{"method":"tools/call","server_name":"filesystem","tool":"list_directory",…}}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"mcp.response","payload":{"server_name":"filesystem","ok":true,"result":{…}}}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"tool.result","payload":{"dispatch":"mcp","server_name":"filesystem","is_error":false,…}}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"provider.request",…}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"provider.response",…}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"server.message.send",…}}
{"jsonrpc":"2.0","id":1,"result":{"message":{"role":"assistant","content":[…]},"events":[…]}}
```

---

## Step 7 — Confirm the MCP events

The `mcp.request` and `mcp.response` events confirm that BAE actually invoked
the filesystem server rather than returning a stub. Key fields:

```json
{
  "event_type": "mcp.request",
  "payload": {
    "method": "tools/call",
    "server_name": "filesystem",
    "tool": "list_directory",
    "input": {"path": "/data"}
  }
}
```

```json
{
  "event_type": "mcp.response",
  "payload": {
    "server_name": "filesystem",
    "ok": true,
    "result": { "content": [{"type":"text","text":"README.md\ndata.csv"}] }
  }
}
```

You can also retrieve the full event history after the fact:

```sh
curl "http://localhost:8080/api/v1/sessions/$SESSION_ID/events" \
  -H "Authorization: Bearer $SESSION_KEY"
```

---

## Adding the fetch server

The same steps apply to any MCP server. For `mcp-server-fetch` (requires
Python + `uv`):

```sh
# Point to the fetch config:
BAE_CONFIG=examples/bae-config/fetch.toml baesrv

# Confirm:
curl http://127.0.0.1:8081/admin/v1/mcp-servers
# {"items":[{"name":"fetch","transport":"stdio"}]}

# Create a profile that opts into it:
curl -X POST http://127.0.0.1:8081/admin/v1/profiles \
  -H 'Content-Type: application/json' \
  -d '{"name":"fetch-assistant","primary_provider":"anthropic-sonnet","mcp_servers":["fetch"]}'

# Open a session and ask the agent to fetch a URL:
# (follow steps 4–6 as above, with the fetch profile)
```

See [`examples/bae-config/fetch.toml`](../../examples/bae-config/fetch.toml)
for the full config file.

---

## `bae-config.toml` reference

### File shape

```toml
[mcp]

[[mcp.servers]]
name      = "filesystem"
transport = "stdio"
command   = "npx"
args      = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]

[[mcp.servers]]
name      = "remote-search"
transport = "sse"
url       = "https://mcp.example.com/sse"
headers   = { Authorization = "Bearer ${SEARCH_MCP_TOKEN}" }
```

`[providers]` (the LLM provider registry — see
[Configuration](../reference/configuration.md#providers)) coexists with
`[mcp]` in the same file with no naming conflict — the two are separate
registries, so a provider and an MCP server may even share a `name`. Other
future top-level sections (e.g. `[logging]`) follow the same pattern; unknown
top-level keys are silently ignored.

### Profile `mcp_servers` field

```json
"mcp_servers": ["filesystem", "fetch"]
```

Each string must be the `name` of an `[[mcp.servers]]` entry. The field is a
plain JSON array of strings — not an array of objects.

> **Breaking change from previous behavior.** Prior to work item 0003,
> `mcp_servers` accepted objects (`[{"name":"filesystem"}]`). It now accepts
> strings only (`["filesystem"]`). This is an alpha API change per the README:
> "APIs and SDKs will likely change."

---

## Non-fatal skips

If a profile's `mcp_servers` list names a server that is not in the current
registry (e.g. a typo, or the config file was not loaded), BAE logs an error
and skips that server — **session creation still succeeds**. The error is logged
every time a session is opened against that profile (not deduplicated):

```
ERROR configured MCP server not found in bae-config.toml; skipping
  profile_id="pro_…" profile_name="fs-assistant"
  mcp_server_name="filesytem" session_id="ses_…"
```

Similarly, if a server is found in the registry but fails to connect (missing
binary, unreachable endpoint, unset auth variable), it is also skipped
non-fatally and logged as an error. Sessions with no successfully connected MCP
servers simply have no MCP tools available — the provider call proceeds with
client-declared tools only.

---

## Closing the session

```sh
curl -s -X DELETE "http://localhost:8080/api/v1/sessions/$SESSION_ID" \
  -H "Authorization: Bearer $SESSION_KEY"
```

On close, BAE terminates the spawned stdio subprocess and drops the session's
MCP connections and broadcast channel.
