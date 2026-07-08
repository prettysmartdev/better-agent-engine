# Quickstart

Get a BAE server running and send your first message in five steps.

## Prerequisites

- Docker (for the production image), or a Rust toolchain to build from source.
- `curl` for the examples below.
- A provider API key — e.g. `ANTHROPIC_API_KEY` — for any profile that calls a
  real LLM.

---

## 1. Start the server

### Production image (recommended)

```sh
docker run -d \
  --name bae \
  -p 8080:8080 \
  -v bae-data:/var/lib/bae \
  -e ANTHROPIC_API_KEY="sk-ant-…" \
  better-agent-engine
```

The container exposes port **8080** (client API). The admin port (8081) binds
to loopback **inside** the container and is never exposed — reach it via
`docker exec` (see [step 2](#2-create-a-profile)) or a local SSH tunnel.

> **TLS note.** The container always speaks plain HTTP internally, and there
> is no setting to change that. TLS terminates upstream — at nginx, Caddy, a
> cloud load balancer, etc.

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `BAE_ADDR` | `0.0.0.0:8080` | Client-facing listen address (plain HTTP). |
| `BAE_ADMIN_ADDR` | `127.0.0.1:8081` | Admin-only listen address. Must be a loopback address; the server refuses to start otherwise. |
| `BAE_DB_PATH` | `/var/lib/bae/bae.db` | SQLite database file. Mount a volume here to persist data. |
| `BAE_LOG` | `info` | Tracing filter, e.g. `baesrv=debug,tower=warn`. |
| `BAE_SHUTDOWN_TIMEOUT` | `30` | Seconds to drain in-flight requests on SIGTERM. |
| `BAE_CONFIG` | _(none)_ | Path to a `bae-config.toml` file (MCP server and LLM provider registries). |
| `BAE_TURN_TIMEOUT` | `120` | Seconds a paused turn's owner has to return with its continuation before it's considered abandoned. |

See [Configuration](../reference/configuration.md) for the full reference.
Provider credentials (e.g. `ANTHROPIC_API_KEY`) are passed through the
environment and referenced from profile configs using `${ANTHROPIC_API_KEY}`
syntax — they are never written to the database.

### Verify the server is up

```sh
curl http://localhost:8080/healthz
# 200 OK, empty body

curl http://localhost:8080/api/v1/meta
# {"version":"0.1.0","api_versions":["v1"]}
```

---

## 2. Create a profile

Profiles are managed through the **admin API**, which binds to loopback only.
From inside the container:

This assumes the server was started with a `bae-config.toml` that declares an
`anthropic-sonnet` entry under `[providers]` — e.g.
[`examples/bae-config/providers.toml`](../../examples/bae-config/providers.toml)
(`BAE_CONFIG=examples/bae-config/providers.toml`). See
[Configuration — `[providers]`](../reference/configuration.md#providers).

```sh
docker exec -i bae sh << 'EOF'
curl -s -X POST http://127.0.0.1:8081/admin/v1/profiles \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "main",
    "primary_provider": "anthropic-sonnet",
    "allowed_tools": ["get_current_time"]
  }' | tee /dev/stderr | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['id'])"
EOF
```

Response (`201 Created`):
```json
{
  "id": "pro_a1b2c3d4e5f6…",
  "name": "main",
  "created_at": "2026-07-06T18:26:01.123Z"
}
```

Save the profile id — you need it in the next step.

> The `allowed_tools` list controls which client-side tools agents may declare.
> An **empty `allowed_tools` list permits no client-side tools**. Tools not in
> the list cause session open to fail with `403 tool_not_allowed`.

---

## 3. Create a client key

```sh
docker exec bae curl -s -X POST http://127.0.0.1:8081/admin/v1/keys \
  -H 'Content-Type: application/json' \
  -d '{"name": "my-agent", "profile_id": "pro_a1b2c3d4e5f6…"}'
```

Response (`201 Created`):
```json
{
  "id": "key_…",
  "name": "my-agent",
  "key": "bae_1a2b3c4d…",
  "prefix": "bae_1a2b",
  "profile_id": "pro_…",
  "created_at": "2026-07-06T18:26:05.000Z"
}
```

> **The `key` field is shown exactly once.** Copy it now — it cannot be
> retrieved again. Only an Argon2id hash is stored.

```sh
CLIENT_KEY="bae_1a2b3c4d…"
```

---

## 4. Open a session

Sessions are created on the **client port** (8080) using the client key.

```sh
SESSION=$(curl -s -X POST http://localhost:8080/api/v1/sessions \
  -H "Authorization: Bearer $CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "client_version": "1.0.0",
    "tools": [
      {
        "name": "get_current_time",
        "description": "Return the current UTC time",
        "input_schema": {"type": "object", "properties": {}}
      }
    ]
  }')
echo "$SESSION"
```

Response (`201 Created`):
```json
{
  "session_id": "ses_…",
  "session_key": "bae_ses_…",
  "profile": {
    "id": "pro_…",
    "name": "main",
    "allowed_tools": ["get_current_time"],
    "mcp_servers": [],
    "provider": {"provider": "anthropic", "model": "claude-sonnet-4-6"}
  }
}
```

> **The `session_key` is shown exactly once.** The returned `profile` is
> sanitized — no `auth_token`, no env var names.

```sh
SESSION_ID=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_id'])")
SESSION_KEY=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_key'])")
```

---

## 5. Send a message

Message sending uses `POST /api/v1/sessions/{id}/rpc` with a JSON-RPC 2.0
envelope. The response is a stream of newline-delimited JSON objects
(`application/x-ndjson`): zero or more `session.event` notifications, followed
by a terminal result object.

**Register as a driver first.** Every session key must call
`session.registerDriver` once before its first `session.sendMessage` — SDK
harnesses do this automatically in `connect()`, but raw HTTP callers must do
it explicitly or `session.sendMessage` fails with `-32001`:

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.registerDriver","params":{}}'
# {"jsonrpc":"2.0","id":1,"result":{"registered":true}}
```

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "session.sendMessage",
    "params": {"message": {"role": "user", "content": "What time is it?"}}
  }'
```

The response streams multiple lines. Each line is a complete JSON object:

```
{"jsonrpc":"2.0","method":"session.event","params":{"id":"evt_…","event_type":"client.message.send",…}}
{"jsonrpc":"2.0","method":"session.event","params":{"id":"evt_…","event_type":"provider.request",…}}
{"jsonrpc":"2.0","method":"session.event","params":{"id":"evt_…","event_type":"provider.response",…}}
{"jsonrpc":"2.0","method":"session.event","params":{"id":"evt_…","event_type":"server.message.send",…}}
{"jsonrpc":"2.0","id":2,"result":{"message":{"role":"assistant","content":[{"type":"text","text":"It is currently …"}]},"events":[…]}}
```

Objects without `"id"` are live event notifications. The last object (carrying
`"id":2`) is the terminal response; its `result` contains the final
`message` and the full `events` array for the turn.

To extract just the assistant's reply (the session key is already registered
as a driver from the `session.registerDriver` call above):

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"session.sendMessage","params":{"message":{"role":"user","content":"What time is it?"}}}' \
  | tail -1 \
  | python3 -c "import sys,json; r=json.load(sys.stdin); print(r['result']['message']['content'][0]['text'])"
```

See [Wire Protocol](../reference/wire-protocol.md) for the full envelope
specification and [Client API](../reference/client-api.md) for method params
and result shapes.

---

## Closing a session

```sh
curl -s -X DELETE "http://localhost:8080/api/v1/sessions/$SESSION_ID" \
  -H "Authorization: Bearer $SESSION_KEY"
# {"session_id":"ses_…","state":"closed"}
```

---

## Next steps

- [Admin API reference](../reference/admin-api.md) — manage profiles and keys.
- [Client API reference](../reference/client-api.md) — full session and message endpoints.
- [Profiles](../profiles.md) — provider config, env var references, fallbacks, MCP wiring.
- [Message types](../reference/message-types.md) — all fourteen `event_type` values and their payloads.
- [MCP Servers](mcp-servers.md) — connect real MCP tools to a profile.
- [Event Streaming](event-streaming.md) — live progress notifications and observer subscriptions.
- [Multi-Client Sessions](multi-client-sessions.md) — join a session as a second driver.
