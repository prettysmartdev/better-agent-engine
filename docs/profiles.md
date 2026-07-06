# Profiles

A **profile** packages everything the server needs to run a session on behalf
of an agent: which LLM provider to call, how to authenticate with it, which
fallback providers to try if the primary fails, which MCP servers are
available, and which client-side tools agents are allowed to declare.

Profiles are managed via the admin API — see [admin-api.md](admin-api.md).

---

## Provider config

The `provider_config` field of a profile describes the primary LLM:

```json
{
  "provider":   "anthropic",
  "base_url":   "https://api.anthropic.com",
  "model":      "claude-sonnet-4-6",
  "auth_token": "${ANTHROPIC_API_KEY}",
  "max_tokens": 8096
}
```

| Field | Required | Description |
|---|---|---|
| `provider` | yes | Provider name (currently `"anthropic"`). |
| `base_url` | yes | Base URL for the provider's API. |
| `model` | yes | Model identifier. |
| `auth_token` | yes | API key or `${ENV_VAR}` reference (see below). |
| `max_tokens` | no | Max tokens per response. Default `4096`. |

---

## Referencing environment variables in `auth_token`

`auth_token` may contain `${ENV_VAR_NAME}` tokens:

```json
"auth_token": "${ANTHROPIC_API_KEY}"
```

The server resolves these **immediately before** each provider call, holds
the resolved value only for the duration of that HTTP request, and discards
it immediately afterward. Resolved values are never written to the database,
logs, or event payloads. The stored config retains the literal template string
(e.g. `"${ANTHROPIC_API_KEY}"`).

Rules:

- Multiple tokens are allowed: `"Bearer ${MY_PREFIX}_${MY_SUFFIX}"`.
- A literal `$` not followed by `{` is passed through unchanged.
- An unterminated `${` is a provider config error and causes the attempt to
  fail.
- If the referenced variable is **not set** at call time, the attempt fails
  with a `provider.response` failure event (`ok: false`) and the fallback walk
  begins. This is surfaced to the client as a `502` if no fallback succeeds.

The admin surface returns the literal template string (e.g.
`"${ANTHROPIC_API_KEY}"`), not the resolved value. The client-facing session
open response strips `auth_token` entirely.

---

## Fallback configs

`fallback_configs` is an ordered array of provider configs tried if the
primary fails:

```json
"fallback_configs": [
  {
    "provider":   "anthropic",
    "base_url":   "https://api.anthropic.com",
    "model":      "claude-haiku-4-5-20251001",
    "auth_token": "${ANTHROPIC_API_KEY}",
    "max_tokens": 4096
  },
  {
    "provider":   "anthropic",
    "base_url":   "https://api-fallback.example.com",
    "model":      "claude-sonnet-4-6",
    "auth_token": "${FALLBACK_API_KEY}",
    "max_tokens": 8096
  }
]
```

- Each entry has the same shape as `provider_config`.
- Fallbacks are tried in order after the primary fails. The first successful
  response ends the walk.
- If all providers fail, the session moves to `error` state and the client
  receives a `502` with the normal `{message, events}` body containing the
  failure trail.
- `"provider_call_failed"` events are recorded for each failing attempt;
  `"all_providers_failed"` is recorded if every attempt fails.
- Omit or pass `[]` for no fallbacks (default).

---

## MCP servers

`mcp_servers` lists MCP server stubs available to sessions on this profile:

```json
"mcp_servers": [
  {"name": "filesystem"},
  {"name": "web-search"}
]
```

Full MCP implementation is a later work item. Currently:

- MCP server entries are stored on the profile and returned at session open.
- When the LLM calls a tool that was **not** declared by the client at session
  open, the server dispatches it as an MCP stub: it records `tool.call`
  (dispatch: mcp), `mcp.request`, `mcp.response` (with `{"status":"stub"}`),
  and `tool.result` events, and sends a stub result back to the provider.
- The session loop continues with the stub result until the LLM produces a
  final text turn.

---

## Tool allowlists

`allowed_tools` lists the names of client-side tools agents are permitted to
declare when opening a session:

```json
"allowed_tools": ["get_current_time", "read_file", "write_file"]
```

Behavior:

- When a client calls `POST /api/v1/sessions`, every tool name in the `tools`
  array must appear in the profile's `allowed_tools`.
- A tool name not in the list causes `403 tool_not_allowed` and the session
  is not created.
- **An empty `allowed_tools` list (`[]`) permits no client-side tools.** A
  client may still open sessions (with `"tools": []`), but any non-empty tool
  declaration will be rejected.
- MCP tools are not declared by the client and are not subject to this check.

### Example: no client-side tools allowed

```json
{
  "name": "server-only",
  "provider_config": { … },
  "allowed_tools": []
}
```

Clients connecting with this profile must pass `"tools": []` at session open.

### Example: specific tools allowed

```json
{
  "name": "assistant",
  "provider_config": { … },
  "allowed_tools": ["get_current_time", "search_web"]
}
```

A client declaring `"tools": [{"name": "get_current_time"}, {"name": "search_web"}]`
is accepted. A client declaring an additional tool not in the list is rejected.

---

## Full profile example

```json
{
  "name": "production-assistant",
  "provider_config": {
    "provider":   "anthropic",
    "base_url":   "https://api.anthropic.com",
    "model":      "claude-sonnet-4-6",
    "auth_token": "${ANTHROPIC_API_KEY}",
    "max_tokens": 8096
  },
  "fallback_configs": [
    {
      "provider":   "anthropic",
      "base_url":   "https://api.anthropic.com",
      "model":      "claude-haiku-4-5-20251001",
      "auth_token": "${ANTHROPIC_API_KEY}",
      "max_tokens": 4096
    }
  ],
  "mcp_servers": [
    {"name": "filesystem"}
  ],
  "allowed_tools": ["get_current_time", "read_file"]
}
```

Create it:

```sh
curl -X POST http://127.0.0.1:8081/admin/v1/profiles \
  -H 'Content-Type: application/json' \
  -d @profile.json
```

Update it (full replacement):

```sh
curl -X PUT http://127.0.0.1:8081/admin/v1/profiles/pro_… \
  -H 'Content-Type: application/json' \
  -d @profile.json
```
