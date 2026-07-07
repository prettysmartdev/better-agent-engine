# MCP-Attached Profile

Configure an MCP server, create a profile that uses it, open a session, and
trigger the tool. Shown in raw curl.

For the full guided walkthrough see [MCP Servers](../guides/mcp-servers.md).

---

## Prerequisites

- BAE server running with a `bae-config.toml` that includes the `filesystem`
  server (use [`examples/bae-config/filesystem.toml`](../../examples/bae-config/filesystem.toml)).
- Node.js + `npx` installed (used by the filesystem server).
- Verify registration:

```sh
curl http://127.0.0.1:8081/admin/v1/mcp-servers
# {"items":[{"name":"filesystem","transport":"stdio"}]}
```

---

## Create a profile with `mcp_servers`

```sh
PROFILE=$(curl -s -X POST http://127.0.0.1:8081/admin/v1/profiles \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "fs-assistant",
    "provider_config": {
      "provider":   "anthropic",
      "base_url":   "https://api.anthropic.com",
      "model":      "claude-sonnet-4-6",
      "auth_token": "${ANTHROPIC_API_KEY}",
      "max_tokens": 8096
    },
    "mcp_servers":    ["filesystem"],
    "allowed_tools":  []
  }')
PROFILE_ID=$(echo "$PROFILE" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
echo "profile: $PROFILE_ID"
```

`mcp_servers` is an array of **name strings** from `bae-config.toml`.

---

## Create a client key and open a session

```sh
KEY=$(curl -s -X POST http://127.0.0.1:8081/admin/v1/keys \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"fs-agent\",\"profile_id\":\"$PROFILE_ID\"}")
CLIENT_KEY=$(echo "$KEY" | python3 -c "import sys,json; print(json.load(sys.stdin)['key'])")

SESSION=$(curl -s -X POST http://localhost:8080/api/v1/sessions \
  -H "Authorization: Bearer $CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"client_version":"1.0.0","tools":[]}')
SESSION_ID=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_id'])")
SESSION_KEY=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_key'])")
```

---

## Send a message that triggers the filesystem tool

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "session.sendMessage",
    "params": {
      "message": {
        "role": "user",
        "content": "List the files in /data."
      }
    }
  }'
```

Watch for the MCP events in the notification stream:

```
…
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"tool.call","payload":{"name":"list_directory","dispatch":"mcp","server_name":"filesystem",…}}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"mcp.request","payload":{"method":"tools/call","server_name":"filesystem","tool":"list_directory",…}}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"mcp.response","payload":{"server_name":"filesystem","ok":true,"result":{…}}}}
{"jsonrpc":"2.0","method":"session.event","params":{"event_type":"tool.result","payload":{"dispatch":"mcp","server_name":"filesystem","is_error":false,…}}}
…
{"jsonrpc":"2.0","id":1,"result":{"message":{…},"events":[…]}}
```

`mcp.request` and `mcp.response` confirm the tool ran against the real
filesystem server, not a stub.

---

## Fetch the full event history

```sh
curl "http://localhost:8080/api/v1/sessions/$SESSION_ID/events" \
  -H "Authorization: Bearer $SESSION_KEY"
```

---

## Close the session

```sh
curl -s -X DELETE "http://localhost:8080/api/v1/sessions/$SESSION_ID" \
  -H "Authorization: Bearer $SESSION_KEY"
```
