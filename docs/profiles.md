# Profiles

A **profile** packages everything the server needs to run a session on behalf
of an agent: which LLM provider to call (by name, from the registry declared
in `bae-config.toml`), which fallback providers to try if the primary fails,
which MCP servers are available, which sandbox container images the agent
may launch, and which client-side tools agents are allowed to declare.

Profiles are managed via the admin API — see [admin-api.md](reference/admin-api.md).

---

## Provider config

> **Alpha breaking change.** Prior to work item 0005, `primary_provider` and
> `fallback_providers` were named `provider_config` and `fallback_configs`,
> and each held an **inline config object** (`{provider, base_url, model,
> auth_token, max_tokens}`). They now hold **registry name references** — a
> single string and an array of strings — resolved against the `[providers]`
> table in `bae-config.toml` at session-creation time, the same
> opt-in-by-name model [MCP servers](#mcp-servers) already use. This is a
> bigger change than the work item 0003 `mcp_servers` shape change (field
> names changed, not just the array element type): existing profile data
> using the old inline-object shape is silently broken (alpha status, no
> migration) and must be recreated against named `[providers]` entries.

The `primary_provider` field of a profile is the **name** of a `[providers]`
entry declared in `bae-config.toml`:

```json
"primary_provider": "anthropic-sonnet"
```

At session creation (`POST /api/v1/sessions`) and at join
(`POST /api/v1/sessions/{id}/join`), BAE resolves this name against the
server's provider registry. Unlike `mcp_servers`, **a missing primary is
fatal**: if the name isn't in the registry, the request is rejected with
`422 primary_provider_unavailable` and no session (and no session key) is
created. The failure is logged on every attempt (never deduplicated) — see
[Fatal primary / non-fatal fallback](#fatal-primary--non-fatal-fallback)
below.

See [Configuration](reference/configuration.md#providers) for the full
`[providers]` `bae-config.toml` schema, and
[`examples/bae-config/providers.toml`](../examples/bae-config/providers.toml)
for a runnable three-entry example.

---

## Referencing environment variables in `auth_token`

`auth_token` in a `[providers]` registry entry may contain `${ENV_VAR_NAME}`
tokens:

```toml
auth_token  = "${ANTHROPIC_API_KEY}"
```

The server resolves these **immediately before** each provider call, holds
the resolved value only for the duration of that HTTP request, and discards
it immediately afterward. Resolved values are never written to the database,
logs, or event payloads. The registry retains the literal template string
(e.g. `"${ANTHROPIC_API_KEY}"`).

Rules:

- Multiple tokens are allowed: `"Bearer ${MY_PREFIX}_${MY_SUFFIX}"`.
- A literal `$` not followed by `{` is passed through unchanged.
- An unterminated `${` is a provider config error and causes the attempt to
  fail.
- If the referenced variable is **not set** at call time, the attempt fails
  with a `provider.response` failure event (`ok: false`) and the fallback walk
  begins. If no fallback succeeds, `session.sendMessage` returns a terminal
  `result` with a "provider unavailable" message (SDKs raise `ProvidersFailedError`).

`GET /admin/v1/providers` (see [Configuration](reference/configuration.md#admin-endpoint-get-adminv1providers))
returns the literal template string (e.g. `"${ANTHROPIC_API_KEY}"`), never
the resolved value. The client-facing session open response strips
`auth_token` entirely — it only ever surfaces `{"provider": "<kind>", "model": "<model>"}`.

---

## Fallback providers

`fallback_providers` is an ordered array of **registry name strings** tried if
the primary fails:

```json
"fallback_providers": ["anthropic-haiku", "openai-gpt"]
```

- Each name is resolved independently against the `[providers]` registry.
- Fallbacks are tried in order after the primary fails. The first successful
  response ends the walk.
- A profile's primary and fallbacks may resolve to **different provider
  kinds** (e.g. an `anthropic`-kind primary with an `openai`-kind fallback,
  or vice versa) — mixed-kind fallback chains work with no extra
  configuration; each attempt is translated independently based on its own
  registry entry's `provider` kind. See
  [Configuration — `[providers]`](reference/configuration.md#providers).
- If all providers fail, the session moves to `error` state. `session.sendMessage`
  returns a terminal `result` (HTTP 200) whose `message` contains a generic
  "provider unavailable" assistant turn and whose `events` include the failure
  trail. SDKs surface this as `ProvidersFailedError`.
- `"provider_call_failed"` events are recorded for each failing attempt;
  `"all_providers_failed"` is recorded if every attempt fails.
- Omit or pass `[]` for no fallbacks (default).

### Fatal primary / non-fatal fallback

This is the one asymmetry between `primary_provider` and `fallback_providers`
(mirroring the summary's requirement in the work item):

| | Missing from `[providers]` registry | Session creation / join |
|---|---|---|
| `primary_provider` | Logged (`tracing::error!`, every attempt, never deduplicated) | **Fatal** — `422 primary_provider_unavailable`, no session created, no session key issued |
| Each `fallback_providers` entry | Logged and skipped, independently per name | **Never fatal** — session creation succeeds with whatever subset resolved (including zero) |

A profile whose `primary_provider` cannot be resolved blocks **every** client
key associated with it from creating or joining sessions on that profile —
not just the request that first surfaces the typo. If an already-open
session's next `session.sendMessage` hits the same missing-primary condition
(e.g. the server restarted with a changed `bae-config.toml`), that turn ends
via the existing `session.error` (`reason: "provider_config"`) path instead
of serving a message — see [Message Types](reference/message-types.md#sessionerror).

---

## MCP servers

`mcp_servers` opts this profile into a subset of the MCP servers declared in
`bae-config.toml`. It is an **array of server name strings**:

```json
"mcp_servers": ["filesystem", "web-search"]
```

At session creation, BAE looks up each name in the registry built from
`bae-config.toml`. For each found server it connects, runs the MCP
`initialize` handshake, and merges the server's tools into the tool list
advertised to the provider. A name not found in the registry is skipped
non-fatally (an error is logged every session creation; session open still
succeeds).

See [MCP Servers guide](guides/mcp-servers.md) for a hands-on walkthrough,
and [Configuration](reference/configuration.md) for the full `bae-config.toml`
schema.

> **Alpha breaking change.** Prior to work item 0003, `mcp_servers` accepted
> objects (`[{"name":"filesystem"}]`). It now accepts strings only
> (`["filesystem"]`). Existing profile data using the old shape should be
> updated.

---

## Available sandboxes

`available_sandboxes` opts this profile into a set of container images its
sessions may launch as **remote sandboxes** via `session.startRemoteSandbox`.
It is an **array of image name strings**:

```json
"available_sandboxes": ["python:3.12", "node:22"]
```

The instant a profile is created or replaced with a non-empty
`available_sandboxes`, the server spawns a background task (never on the
request's critical path) that ensures every named image is present locally —
inspecting, then pulling on a miss — logging success or failure per image and
recording each image's status (`pending`/`available`/`error`) in memory. A
session opened against this profile, once its client key registers as a
driver, receives a `session.sandbox.available` notification listing exactly
these images and their current status — never any other profile's images,
even ones known and successfully pulled on the same running server. See
[Sandboxes guide](guides/sandboxes.md) for the full walkthrough (declaring
the field, the background provisioning task, the driver-connect
notification, starting/stopping a remote sandbox, and binding
`run_shell_command`/`run_shell_named` in a client harness) and
[Configuration — Sandbox driver](reference/configuration.md#sandbox-driver)
for the server-wide `BAE_SANDBOX_DRIVER` selection this sits on top of.

Behavior:

- `session.startRemoteSandbox` validates its requested `image` against
  **this session's own profile's** `available_sandboxes` only — an image
  declared on a different profile is rejected exactly like an image unknown
  anywhere on the server (`sandbox_image_not_allowed`). A profile is a hard
  trust boundary for what its sessions may launch, the same guarantee
  `allowed_tools` provides for client-side tools.
- **An empty `available_sandboxes` list (`[]`, the default) permits no
  remote sandbox at all** — every `session.startRemoteSandbox` call on such a
  profile's sessions fails with `sandbox_image_not_allowed`, regardless of
  image name.
- This field governs the **remote** sandbox lifecycle only. A client
  harness's own **local** sandbox (its local Docker/Apple Containers engine)
  is never validated against `available_sandboxes` — see
  [Sandboxes — Local sandboxes report their own
  lifecycle](guides/sandboxes.md#local-sandboxes-report-their-own-lifecycle).

### Example: no remote sandboxes

```json
{
  "name": "text-only-assistant",
  "primary_provider": "anthropic-sonnet",
  "available_sandboxes": []
}
```

### Example: two images available

```json
{
  "name": "sandboxed-assistant",
  "primary_provider": "anthropic-sonnet",
  "available_sandboxes": ["python:3.12", "node:22"]
}
```

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
  "primary_provider": "anthropic-sonnet",
  "allowed_tools": []
}
```

Clients connecting with this profile must pass `"tools": []` at session open.

### Example: specific tools allowed

```json
{
  "name": "assistant",
  "primary_provider": "anthropic-sonnet",
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
  "primary_provider": "anthropic-sonnet",
  "fallback_providers": ["anthropic-haiku"],
  "mcp_servers": ["filesystem"],
  "available_sandboxes": ["python:3.12"],
  "allowed_tools": ["get_current_time", "read_file"]
}
```

This assumes `bae-config.toml` declares matching `[providers]` entries named
`anthropic-sonnet` and `anthropic-haiku` — see
[Configuration — `[providers]`](reference/configuration.md#providers).

Create it (admin requests need the bootstrap admin key — see the
[admin authentication guide](guides/admin-authentication.md)):

```sh
ADMIN_KEY=$(docker exec bae cat /var/lib/bae/admin-key.pem)
curl -X POST http://127.0.0.1:8081/admin/v1/profiles \
  -H "Authorization: Bearer $ADMIN_KEY" \
  -H 'Content-Type: application/json' \
  -d @profile.json
```

Update it (full replacement):

```sh
curl -X PUT http://127.0.0.1:8081/admin/v1/profiles/pro_… \
  -H "Authorization: Bearer $ADMIN_KEY" \
  -H 'Content-Type: application/json' \
  -d @profile.json
```
