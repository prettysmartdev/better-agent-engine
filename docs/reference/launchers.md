# Harness Launchers Reference

Precise schema for `bae-schedules.toml`, `bae-api.toml`/`bae-app.toml`, every
new `BAE_*` env var the three launcher base images read, and `baeapi`'s fixed
routes — at the same precision level as
[Configuration](configuration.md). For a walkthrough see
[Harness Launchers](../guides/harness-launchers.md).

Launchers are **base images** (`FROM`-extended by your own Dockerfile, never
run standalone) that package one or more of your own agent harnesses and
trigger them on a cron schedule (`bae-launcher-schedule`) or an HTTP POST
(`bae-launcher-api`, `bae-launcher-webapp`). See
[Architecture — Major Components](../../aspec/architecture/design.md) for how
they fit into the rest of the project.

---

## Shared conventions

- **`[[agents]]` is always an array of tables**, in all three config files,
  from a single agent up to as many as you configure — there is no
  single-agent shorthand and no upgrade path to hit.
- **`name` must be unique** within the file. It is simultaneously the
  cron-log/HTTP-log prefix, the API launcher's URL path segment
  (`/agents/{name}/trigger`), and the webapp's card key. A duplicate or blank
  `name` is a fatal startup error, exit code **2**.
- **A missing config file is not fatal.** Both `baesched` and `baeapi` start
  with zero configured agents and log a warning — an image built before its
  config is dropped in still starts cleanly.
- **A config file that exists but is malformed** — bad TOML, an unknown field,
  an invalid cron expression, an invalid JSON Schema, or a duplicate/blank
  `name` — is a **fatal startup error, exit code 2**. A config file that
  *exists but cannot be read* (permissions/I/O, as opposed to missing) is
  also fatal, but the two binaries disagree on the exit code: `baesched`
  treats it as exit **2**, `baeapi` treats it as exit **1** — see each
  launcher's own section below.
- **`${VAR}` secrets**: any `env` map value (in either config file), and any
  `env_template`/`arg_template` entry's own `env`/`flag` string
  (API/webapp launcher), may contain a `${VAR}` token, resolved against the
  launcher process's own environment immediately before spawning the child —
  never logged, never persisted. An unset referenced variable is a **hard
  failure for that one invocation** (schedule launcher: logged and skipped;
  API/webapp launcher: `500` RFC 7807 body) — never a silent empty-string or
  literal substitution. A literal `$` not followed by `{` passes through
  unchanged; an opened `${` with no closing `}` is a fatal usage error at
  load/spawn time. Values arriving in a **request body** are never
  `${VAR}`-resolved — see the `env_template` row below.
- **Per-agent log-line attribution.** Every line of a child's captured
  stdout/stderr is prefixed `[name] ` before being forwarded. This applies
  even to a single-agent config, since the 1-agent and N-agent cases share one
  code path. Each captured line is capped at 8192 characters (with a trailing
  `…`) so one chatty invocation cannot flood `docker logs` or a trigger
  response.
  - **Schedule launcher**: forwarded to `baesched`'s own stdout (child stdout)
    and stderr (child stderr) — `docker logs` shows every agent's output.
  - **API/webapp launcher**: forwarded to **both** places at once — to
    `baeapi`'s own stdout/stderr (matching the stream each line came from), so
    `docker logs` remains the common attributed log surface for every
    launcher, **and** into that request's own streamed
    `POST /agents/{name}/trigger` response body, so the caller (or the webapp
    chat view) sees the same lines live. The response is a copy, not the only
    record.
- **A hung or crashed child never takes the launcher down.** A bad `command`,
  a non-zero exit, or a wedged process is logged/reported per-invocation; the
  scheduler's other timers and the API server's other routes keep working.
  This is a deliberate divergence from `bae-max` (see
  [Infrastructure](../../aspec/devops/infrastructure.md)).
- **`BAE_LOG`** (default `info`) controls both binaries' own stderr log
  filter, exactly like `baesrv`'s `BAE_LOG` — does not affect forwarded child
  output, which is never filtered.

---

## `bae-schedules.toml` (`bae-launcher-schedule`, binary `baesched`)

### Env vars

| Variable | Default | Description |
|---|---|---|
| `BAE_SCHEDULES_CONFIG` | `/etc/bae/bae-schedules.toml` | Path to the config file. |
| `BAE_SCHEDULES_SHUTDOWN_TIMEOUT` | `30` | Whole seconds of grace given to in-flight invocations after `SIGTERM`/`SIGINT` before they are force-killed. A non-integer value is a fatal startup usage error (exit 2). `0` requests immediate force-kill. |
| `BAE_LOG` | `info` | Tracing filter for `baesched`'s own stderr logs. |

There is **no HTTP surface at all** for this launcher — no port, no
`/healthz`. Liveness is purely process-level (`docker ps`, the container's own
exit status); this is deliberate, not a gap relative to the other two
launchers.

### `[[agents]]` field reference

```toml
[[agents]]
name        = "nightly-report"
command     = "/usr/local/bin/nightly-report-harness"
args        = ["--mode", "report"]
env         = { AGENT_MODE = "scheduled", API_TOKEN = "${MY_HARNESS_TOKEN}" }
working_dir = "/app"
schedule    = "0 0 3 * * *"
```

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | yes | — | Unique agent name; log-prefix key. |
| `command` | string | yes | — | Executable to spawn. PATH-resolved if not absolute, the same resolution `tokio::process::Command::new` gives for free. |
| `args` | array of strings | no | `[]` | Command-line arguments, passed through verbatim. |
| `env` | table of string→string | no | `{}` | Extra env vars layered onto (not replacing) the launcher's own environment for the child. Values may contain `${VAR}` tokens. |
| `working_dir` | string | no | _(inherits launcher's cwd)_ | Working directory for the child. |
| `schedule` | string | yes | — | **Exactly six** whitespace-separated cron fields: `sec min hour day month day-of-week`. A schedule with any other field count, or an invalid expression, is a fatal startup error (exit 2) naming the offending agent. |

The top-level document is `deny_unknown_fields` — a singular `[agent]` block
(instead of `[[agents]]`) is rejected at startup rather than silently
producing zero jobs.

### Runtime semantics

- Each `[[agents]]` entry gets its own `tokio-cron-scheduler` job, registered
  in one `O(N)` startup pass with no hard cap on the number of agents.
- **Same-agent overlap policy: skip, not queue.** If a schedule fires while
  that *same* agent's previous invocation is still running, the new fire is
  skipped and logged: `agent "<name>" skipped: previous invocation still
  running`. A very short interval combined with a slow harness can mean an
  agent effectively misses most of its scheduled fires — this is expected,
  not silently masked.
- **Different agents always run fully concurrently**, with no throttling or
  shared resource limit — two agents' schedules coinciding is normal and
  expected, and is a completely separate policy from the same-agent
  overlap-skip above; don't conflate the two.
- On `SIGTERM`/`SIGINT`: new fires stop being accepted, the cron scheduler
  stops, and any in-flight invocation gets up to
  `BAE_SCHEDULES_SHUTDOWN_TIMEOUT` seconds before being force-killed. The
  process then always exits **0** (a received termination signal is a clean
  shutdown, not a failure).

### Loading semantics (`baesched`)

A missing config file is a warning, zero agents, not fatal. Every other
startup failure is fatal and exits **2** — including a config file that
*exists but cannot be read* (permissions/I/O): unlike `baeapi` below,
`baesched` maps this case to a usage error (2), not a runtime error (1), on
the reasoning that an unreadable file at a fixed, operator-controlled path is
still a config-authoring problem. An internal `tokio-cron-scheduler`
initialization failure (rare) exits **1**.

---

## `bae-api.toml` / `bae-app.toml` (`bae-launcher-api` / `bae-launcher-webapp`, binary `baeapi`)

Both base images ship the exact same `baeapi` binary. `bae-app.toml` is
`bae-api.toml`'s identical schema **plus** optional presentation fields used
only by the webapp UI (`display_name`, `description`, `icon`,
`chat_input_field`, `[[agents.prompts]]`) — the plain API launcher parses and
ignores them.

### Env vars

| Variable | Default | Description |
|---|---|---|
| `BAE_LAUNCHER_API_CONFIG` | `/etc/bae/bae-api.toml` in `bae-launcher-api`; `/etc/bae/bae-app.toml` in `bae-launcher-webapp` | Path to the config file. Same env var name in both images — only the baked-in default differs, so your own Dockerfile only has to `COPY` the right filename to the default path and never has to touch this env var. |
| `BAE_LAUNCHER_API_ADDR` | `0.0.0.0:9090` | Listen address. Overrides `[server] addr` from the config file. Distinct from `baesrv`'s `8080`/`8081` so a launcher and a `baesrv`/`bae-max` container can coexist on one host. |
| `BAE_LAUNCHER_API_TOKEN` | _(unset)_ | Bearer token. **Unset by default — every `/agents/*` route is then open to any caller who can reach the port**, and a loud startup warning is logged. When set, every `/agents/*` route (never `/healthz` or `/_launcher/*`) requires `Authorization: Bearer <token>`, checked in constant time. **Put this port behind a TLS-terminating reverse proxy on an internal network regardless** — see [Security](../../aspec/architecture/security.md) — a bearer token alone is not a substitute for TLS. |
| `BAE_LAUNCHER_WEBAPP_STATIC_DIR` | _(unset in `bae-launcher-api`)_ / a built `web/dist` path (baked in by `bae-launcher-webapp`'s own Dockerfile) | When set to a non-empty, non-whitespace value, `baeapi` additionally serves that directory as a static single-page app at `/`, falling back to `index.html` for any unmatched path (client-side routing) — existing API routes keep priority. Left unset in the plain API image, so it never serves any UI. |
| `BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT` | `30` | Whole seconds the graceful drain may take after `SIGTERM`/`SIGINT` before `baeapi` exits anyway, force-killing any still-running child invocations — a hung child holding a trigger request open never blocks shutdown indefinitely. A non-integer value is a fatal startup usage error (exit 2). `0` requests an immediate exit once the signal arrives. Mirrors `BAE_SCHEDULES_SHUTDOWN_TIMEOUT`. |
| `BAE_LOG` | `info` | Tracing filter for `baeapi`'s own stderr logs. Forwarded child output (which goes to `baeapi`'s stdout/stderr as well as the response body) is never filtered by it. |

### `[server]` field reference

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `addr` | string | no | _(none — falls through to `BAE_LAUNCHER_API_ADDR`/`0.0.0.0:9090`)_ | Listen address. Overridden by `BAE_LAUNCHER_API_ADDR` if set. |

The top-level document, `[server]`, and every sub-table are all
`deny_unknown_fields`: a singular `[agent]` block (instead of `[[agents]]`) or
a typo'd top-level key is a fatal startup error (exit 2), never a
silently-ignored key that degrades the launcher to zero agents — the same
posture as `bae-schedules.toml`.

### `[[agents]]` field reference

```toml
[[agents]]
name    = "daily-digest"       # becomes POST /agents/daily-digest/trigger
command = "/usr/local/bin/daily-digest-harness"
args    = ["--mode", "digest"]
working_dir = "/app"
env     = { API_TOKEN = "${MY_HARNESS_TOKEN}" }

# webapp-only presentation fields (parsed and ignored by bae-launcher-api).
# NOTE: every scalar [[agents]] field, including these, must come before any
# nested table below ([agents.request_schema], [[agents.env_template]], ...)
# — once a nested table header opens, a bare `key = value` line belongs to
# that nested table, not back to the parent [[agents]] entry.
display_name     = "Daily Digest"
description      = "Summarizes the day's activity."
icon             = "📋"
chat_input_field = "prompt"

[agents.request_schema]
type = "object"
required = ["prompt"]
[agents.request_schema.properties.prompt]
type = "string"

[[agents.env_template]]
field = "prompt"
env   = "AGENT_PROMPT"

[[agents.arg_template]]
field = "priority"
flag  = "--priority"

[[agents.prompts]]
label  = "Summarize today"
prompt = "Summarize today's activity."
```

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | yes | — | Unique agent name. Becomes the `POST /agents/{name}/trigger` path segment, the log-prefix key, and (webapp) the card key. |
| `command` | string | yes | — | Executable to spawn. PATH-resolved if not absolute. |
| `args` | array of strings | no | `[]` | Base CLI arguments, always passed; any `arg_template` values are appended after these. |
| `working_dir` | string | no | _(inherits launcher's cwd)_ | Working directory for the child. |
| `env` | table of string→string | no | `{}` | Static per-agent env vars — the natural home for `${VAR}` secrets, resolved at spawn time, never logged. An unset reference is a `500` (never spawned, never a blank env value). |
| `request_schema` | JSON Schema object | no | _(none — any JSON body accepted)_ | The POST body is validated against this schema **before** templating or spawning. A schema-invalid body never reaches the child. Compiled once at startup; an invalid schema itself is a fatal startup error (exit 2) naming the agent. |
| `env_template` | array of `{field, env}` | no | `[]` | Copies validated body field `field`'s value into child env var `env`. The operator-authored `env` string may itself carry `${VAR}` tokens, resolved per invocation (unset → `500`); the **body-derived value** is copied **verbatim** and never `${VAR}`-resolved (a request body is untrusted input — resolving it would let any caller exfiltrate the launcher's environment). A field absent from a schema-valid body is silently skipped. |
| `arg_template` | array of `{field, flag}` | no | `[]` | Appends `flag` then validated body field `field`'s value to `args`. The `flag` string may carry `${VAR}` tokens like `env_template.env`; the body-derived value is verbatim, same untrusted-input rule. |
| `display_name` | string | no | _(falls back to `name` in the UI)_ | Webapp card/detail header. Ignored by `bae-launcher-api`. |
| `description` | string | no | _(none)_ | Webapp card one-line description. Ignored by `bae-launcher-api`. |
| `icon` | string | no | _(none)_ | An emoji or an `http(s)://` image URL, rendered on the webapp card. Ignored by `bae-launcher-api`. |
| `chat_input_field` | string | no | `"prompt"` | Which `request_schema` field the webapp chat box's free-text input fills. Ignored by `bae-launcher-api`. |
| `prompts` | array of `{label, prompt}` | no | `[]` | Pre-defined-prompt buttons in the webapp chat view; each click triggers the same route as free-form text. Ignored by `bae-launcher-api`. |

Every sub-table above (`AgentConfig`, `[[agents.env_template]]`,
`[[agents.arg_template]]`, `[[agents.prompts]]`) is `deny_unknown_fields` —
typo protection.

**Value rendering for `env_template`/`arg_template`:** a JSON string becomes
its raw contents (no surrounding quotes); `null` becomes the empty string;
every other JSON value (number, bool, array, object) becomes its compact JSON
form (e.g. `3`, `true`, `["a"]`).

### `baeapi` fixed routes

| Method | Path | Auth\* | Behavior |
|---|---|---|---|
| `GET` | `/healthz` | never | `200 OK`, empty body. Unauthenticated liveness probe present on every instance regardless of agent count. |
| `GET` | `/_launcher/agents` | never | JSON array of every configured agent, in config order — see shape below. |
| `GET` | `/_launcher/agents/{name}` | never | Single-agent detail, same shape as one array element; `404` if `name` is unknown. |
| `POST` | `/agents/{name}/trigger` | Bearer, if `BAE_LAUNCHER_API_TOKEN` is set | Validate the JSON body against `request_schema` → template into env/args → spawn → stream the response. `404` for an unknown `name`. |

\* One shared `{name}` route handles every configured agent — there are not N
separately-registered literal routes. An unknown agent name is a handler-level
`404`, behaviorally identical to a per-agent route not existing.

#### `GET /_launcher/agents` response shape

```json
[
  {
    "name": "daily-digest",
    "display_name": "Daily Digest",
    "description": "Summarizes the day's activity.",
    "icon": "📋",
    "request_schema": { "type": "object", "required": ["prompt"], "properties": { "prompt": { "type": "string" } } },
    "chat_input_field": "prompt",
    "prompts": [{ "label": "Summarize today", "prompt": "Summarize today's activity." }]
  }
]
```

Optional fields serialize as `null` when unset; `prompts` defaults to `[]`.
**Never included, on any route:** `command`, `args`, `working_dir`, `env`,
`env_template`, `arg_template`, or any `${VAR}`-resolved value — matching
`bae-config.toml`'s "secrets never persisted, never echoed" posture. Env-var
*names* wired up by `env_template` are not secrets and would be safe to show,
but the introspection route omits `env_template` entirely rather than
partially exposing it.

#### `POST /agents/{name}/trigger` request/response

- Request body: any JSON object satisfying `request_schema` (or any JSON body
  at all, if `request_schema` is omitted). An empty body is treated as `{}`.
- Response: `Content-Type: application/x-ndjson`, **streamed** — each captured
  child output line is forwarded as its own chunk (`[name] <content>\n`,
  already prefixed and length-capped), stdout and stderr interleaved in
  arrival order. The response headers are sent once the child's *first* output
  line (or its exit, if silent) arrives — this lets a spawn failure become a
  clean `500` instead of a `200` with the error buried in the body, at the
  cost of headers waiting on a completely silent child's first output. The
  stream always ends with a trailing NDJSON object reporting the exit code:
  `{"exit_code": 0}` (or `null` if the child was killed by a signal rather
  than exiting normally).
- No server-side history of any kind is kept — a caller that wants the full
  output just buffers the response body (`curl`'s default behavior works
  fine); there is nothing to query afterward.
- **Concurrent triggers — same agent or different agents — spawn
  independently, with no locking or de-duplication.** This is intentionally
  asymmetric with the schedule launcher's same-agent overlap-skip: an HTTP
  caller retrying, or two legitimate simultaneous requests, is a different
  situation from one timer double-firing on itself.

#### Error responses (RFC 7807 `{type, title, status, detail}`)

Every error response carries `Content-Type: application/problem+json`, the
RFC 7807 media type for JSON problem details.

| Situation | Status | `type` |
|---|---|---|
| Body is not valid JSON | `400` | `bad_request` |
| Body fails `request_schema` | `400` | `bad_request` — `detail` names every failing instance path (e.g. `/prompt: "prompt" is a required property`); child never spawned |
| Unknown agent `name` (trigger or introspection) | `404` | `not_found` |
| Missing/invalid `Authorization` header (auth enabled) | `401` | `unauthorized` |
| Unset `${VAR}` referenced in static `env` or in an applied `env_template.env`/`arg_template.flag` string | `500` | `internal` — never a blank or literal value |
| Spawn failure (bad `command`, not executable, etc.) | `500` | `internal` |

### Shutdown semantics (`baeapi`)

On `SIGTERM`/`SIGINT`, `baeapi` drains in-flight requests gracefully, bounded
by `BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT` (default 30s). If the drain finishes in
time — the normal case — the process exits **0** immediately. If a hung child
is still holding a trigger request open when the bound elapses, the process
exits **0** anyway and the hung child is force-killed on the way out; without
the bound, one wedged agent could keep the launcher alive forever.

### Loading semantics (`baeapi`)

Same missing-vs-malformed posture as `bae-schedules.toml` above: a missing
file is a warning + zero agents (not fatal); a file that fails to parse, has
an invalid `request_schema`, or has a duplicate/blank `name` is a fatal
startup error, exit code **2**. A file that exists but cannot be *read*
(permissions/I/O) is exit code **1** — unlike `baesched`, which maps the same
unreadable-file case to exit 2 (see [`baesched`'s own loading
semantics](#loading-semantics-baesched) above; the two binaries deliberately
disagree here). A bind failure at startup (port already in use, etc.) is also
exit code 1.
