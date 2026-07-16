# Harness Launchers

Launchers are **base Docker images** you extend with your own agent
harness — a binary or script you build, in any language, using none of bae's
SDKs if you don't want to — to get it triggered in a specific way without
writing any scheduling, HTTP-server, or process-supervision code yourself.
Three are available:

| Image | Binary | Triggered by | Has an HTTP surface? |
|---|---|---|---|
| `bae-launcher-schedule` | `baesched` | cron schedule | **No** — none at all, by design |
| `bae-launcher-api` | `baeapi` | `POST /agents/{name}/trigger` | Yes, `0.0.0.0:9090` by default |
| `bae-launcher-webapp` | `baeapi` (same binary) | the webapp's chat UI, which itself calls `POST /agents/{name}/trigger` | Yes, same as above, plus a static SPA at `/` |

All three are **base images meant to be extended, never run standalone in
production** — `docker run` on a bare base image starts a launcher with zero
configured agents. Your Dockerfile only ever `COPY`s in your harness and a
config file; it must never redeclare `ENTRYPOINT`/`CMD`, or the launcher
binary never runs and the container does nothing.

For the exact config schema and env var reference, see
[Harness Launchers Reference](../reference/06-launchers.md). Ready-to-run
examples for all three live in
[`examples/launchers/`](../../examples/launchers/).

---

## Shared conventions before you start

- **One image, one or more agents.** Every config file's `[[agents]]` is
  always an array — a single Dockerfile extending a single base image is
  expected to bundle a whole family of related harnesses (e.g.
  `nightly-report` + `weekly-digest` in one `bae-schedules.toml`). `name` must
  be unique within the file; it is simultaneously the log-prefix, the URL path
  segment, and (webapp) the card key.
- **Secrets use `${VAR}`.** Any `env`/`agents.env` value may reference
  `${MY_SECRET}`, resolved from the launcher process's own environment
  immediately before spawning the child — never logged. An unset reference
  fails that one invocation loudly rather than silently passing an empty
  string.
- **A hung or crashed agent never takes the launcher down.** This is a
  deliberate divergence from `bae-max`, this project's other multi-process
  container, where **either process dying takes the whole container down**
  (see
  [Infrastructure — the `bae-max` variant](../../aspec/devops/infrastructure.md#the-bae-max-variant-a-deliberate-exception-to-one-process-per-container)).
  A launcher typically hosts many independent agents/schedules in one
  container, so one misbehaving agent must not stop the rest — a hung
  `nightly-report` invocation never blocks `weekly-digest`'s next fire, and a
  hung `POST /agents/summarize/trigger` never blocks `POST
  /agents/translate/trigger`.
- **No local persistence of run history or output, in V1, for any launcher —
  `docker logs` is the one shared, attributed log surface.** Every captured
  line is prefixed `[name] ` and forwarded to the launcher's own
  stdout/stderr: `baesched` forwards every scheduled invocation's output
  there, and `baeapi` forwards every triggered invocation's output there
  **in addition to** streaming the same lines back in that request's own
  trigger response (`curl --no-buffer`, or the webapp's chat view, sees them
  live). Nothing is ever written to a file, and there is no run history to
  query afterward — once a response stream is consumed, `docker logs` is the
  only remaining record.

---

## Loudly, before anything else: `BAE_LAUNCHER_API_TOKEN`

Both `bae-launcher-api` and `bae-launcher-webapp` run `baeapi`, and
**`BAE_LAUNCHER_API_TOKEN` is unset by default** — meaning every
`/agents/*` trigger route is open to any caller who can reach the port, no
authentication at all. `baeapi` logs a loud warning on every startup where
this is the case. This matches the posture the admin port itself shipped with
before [work item 0004](../../aspec/work-items/0004-baectl-cli.md) added its
own auth — ship the simplest thing, and the operator opts into hardening it.

Before running either image anywhere network-reachable:

1. Set `BAE_LAUNCHER_API_TOKEN` to a strong random value, and pass
   `Authorization: Bearer <token>` on every `/agents/*` call (this does **not**
   gate `/healthz` or `/_launcher/*`, which stay open for liveness/
   introspection). The webapp keeps working with a token set: the dashboard
   and agent cards load as before (introspection is ungated), and the first
   chat message that gets a `401` surfaces a token prompt in the chat view —
   the operator pastes the token once per browser tab, it is held in memory
   only (a refresh clears it, like the transcript), and subsequent messages
   send it as the Bearer header.
2. **Regardless of whether a token is set**, put a TLS-terminating reverse
   proxy in front and keep the container on an internal network — a bearer
   token alone is not a substitute for TLS. This is exactly the guidance
   [`aspec/architecture/security.md`](../../aspec/architecture/security.md)
   already gives for every other bae port; nothing about this port is
   special-cased to be safer to expose directly.

The schedule launcher has no equivalent warning because it has no HTTP
surface to warn about — see the next section.

---

## Schedule launcher (`bae-launcher-schedule` / `baesched`)

### No HTTP surface, on purpose

`baesched` opens **no port at all** — no `/healthz`, nothing. This is
deliberate, not an oversight relative to the other two launchers: the whole
job is running cron-triggered child processes, and there is nothing an HTTP
listener would add. Liveness is purely process-level: `docker ps`, or the
container's own exit status.

### Extending the base image

```dockerfile
FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-schedule

COPY --chmod=755 my-harness /usr/local/bin/my-harness
COPY bae-schedules.toml /etc/bae/bae-schedules.toml
# No ENTRYPOINT/CMD — inherited from the base image.
```

`BAE_SCHEDULES_CONFIG` already defaults to `/etc/bae/bae-schedules.toml`, so
dropping your config at that exact path (as above) needs no env var at all.

### Config shape

```toml
[[agents]]
name        = "nightly-report"
command     = "/usr/local/bin/nightly-report-harness"
args        = ["--mode", "report"]
env         = { API_TOKEN = "${MY_HARNESS_TOKEN}" }   # ${VAR} resolved at spawn time
working_dir = "/app"
schedule    = "0 0 3 * * *"   # 6-field cron: sec min hour day month day-of-week

[[agents]]
name     = "weekly-digest"
command  = "/usr/local/bin/weekly-digest-harness"
schedule = "0 0 8 * * MON"
```

Full field reference: [Reference — `bae-schedules.toml`](../reference/06-launchers.md#bae-schedulestoml-bae-launcher-schedule-binary-baesched).

### The two overlap policies — don't conflate them

- **Same agent, same timer firing again while still running → skipped**, not
  queued, logged as `agent "<name>" skipped: previous invocation still
  running`. A very short schedule paired with a slow harness can mean an
  agent effectively misses most of its fires — this is expected V1 behavior,
  not silently masked.
- **Different agents' schedules coinciding → both run, fully concurrently**,
  with no throttling or coordination between them. `nightly-report` and
  `weekly-digest` firing at the exact same instant is normal and unthrottled.

These are two separate rules about two separate things (one agent's own
timer vs. two different agents), not one rule read two ways.

### Walkthrough: cron-trigger a script

Using [`examples/launchers/schedule/`](../../examples/launchers/schedule/):

```sh
docker build -t my-schedule-launcher examples/launchers/schedule/
docker run --rm my-schedule-launcher
```

The example's `bae-schedules.toml` configures a `hello-cron` agent on a
30-second schedule and an `hourly-check` agent on an hourly one, both running
the same trivial `example-harness.sh` (which just echoes its args and `env`).
Within the first 30 seconds you'll see log lines like:

```
[hello-cron] [example-harness] args: --mode cron
[hello-cron] [example-harness] GREETING=hello from baesched
```

`Ctrl-C` sends `SIGINT` (Docker's normal `stop` sends `SIGTERM`); `baesched`
handles both identically — it stops accepting new fires, gives any in-flight
invocation up to `BAE_SCHEDULES_SHUTDOWN_TIMEOUT` seconds (default 30) to
finish, then exits 0.

---

## API launcher (`bae-launcher-api` / `baeapi`)

### Extending the base image

```dockerfile
FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-api

COPY --chmod=755 my-harness /usr/local/bin/my-harness
COPY bae-api.toml /etc/bae/bae-api.toml
# No ENTRYPOINT/CMD — inherited from the base image.
```

### Config shape

```toml
[server]
addr = "0.0.0.0:9090"        # optional; BAE_LAUNCHER_API_ADDR overrides it

[[agents]]
name    = "daily-digest"     # becomes POST /agents/daily-digest/trigger
command = "/usr/local/bin/daily-digest-harness"

[agents.request_schema]      # the POST body must satisfy this JSON Schema
type = "object"
required = ["prompt"]
[agents.request_schema.properties.prompt]
type = "string"

[[agents.env_template]]      # validated body field -> child env var
field = "prompt"
env   = "AGENT_PROMPT"

[[agents.arg_template]]      # validated body field -> appended CLI flag+value
field = "priority"
flag  = "--priority"
```

Full field reference and every route: [Reference — `bae-api.toml`/`bae-app.toml`](../reference/06-launchers.md#bae-apitoml--bae-apptoml-bae-launcher-api--bae-launcher-webapp-binary-baeapi).

### The HTTP-trigger overlap asymmetry

The schedule launcher skips a same-agent overlapping fire (above). The API
launcher does **not** have an equivalent rule: two concurrent
`POST /agents/daily-digest/trigger` requests — whether that's a retrying
caller or two genuinely simultaneous callers — both spawn independently, with
no locking or de-duplication. This is deliberate, not an inconsistency: a
timer firing twice on itself and an external caller making two HTTP requests
are different situations, and the HTTP case has no good way to distinguish
"this is a retry" from "this is a second, legitimate request."

### Walkthrough: curl-trigger an agent

Using [`examples/launchers/api/`](../../examples/launchers/api/):

```sh
docker build -t my-api-launcher examples/launchers/api/
docker run --rm -p 9090:9090 my-api-launcher
```

In another shell:

```sh
curl --no-buffer -X POST http://localhost:9090/agents/echo/trigger \
  -H 'Content-Type: application/json' \
  -d '{"prompt": "hello from curl"}'
```

Streams back (NDJSON, chunked):

```
[echo] [example-harness] args:
[echo] [example-harness] AGENT_PROMPT=hello from curl
{"exit_code":0}
```

Try an invalid body to see schema validation reject it before anything spawns:

```sh
curl -i -X POST http://localhost:9090/agents/echo/trigger \
  -H 'Content-Type: application/json' -d '{}'
# HTTP/1.1 400 Bad Request
# {"type":"bad_request","title":"Bad Request","status":400,
#  "detail":"request body failed schema validation: /prompt: \"prompt\" is a required property"}
```

And introspection:

```sh
curl http://localhost:9090/_launcher/agents
curl http://localhost:9090/healthz   # 200, empty body
```

---

## Webapp launcher (`bae-launcher-webapp`)

### The same `baeapi` binary — no second process

`bae-launcher-webapp` bundles the **exact same `baeapi` binary** as
`bae-launcher-api`, unmodified, plus a built static webapp
(`launchers/webapp/web/`). There is no second Node/webapp backend process —
`baeapi` itself serves the static SPA (via `BAE_LAUNCHER_WEBAPP_STATIC_DIR`,
baked into this image's own Dockerfile) alongside its usual JSON routes, on
one shared port. That means this image is structurally *simpler* than
`bae-max`: one process, no dual-process entrypoint script, no
signal-forwarding between two processes, and no "either process dying kills
the container" logic to reason about — there is only ever one process here.

### Extending the base image

```dockerfile
FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-webapp

COPY --chmod=755 my-harness /usr/local/bin/my-harness
COPY bae-app.toml /etc/bae/bae-app.toml
# No ENTRYPOINT/CMD — inherited from the base image.
```

### Config shape

`bae-app.toml` is `bae-api.toml`'s identical schema plus optional
presentation fields used only by the UI:

```toml
[[agents]]
name    = "daily-digest"
command = "/usr/local/bin/daily-digest-harness"

display_name     = "Daily Digest"        # webapp card/detail header
description      = "Summarizes the day's activity."
icon             = "📋"                  # emoji or an http(s):// image URL
chat_input_field = "prompt"              # which request_schema field the chat box fills

[agents.request_schema]
type = "object"
required = ["prompt"]
[agents.request_schema.properties.prompt]
type = "string"

[[agents.prompts]]                       # pre-defined-prompt buttons
label  = "Summarize today"
prompt = "Summarize today's activity."
```

`GET /_launcher/agents`/`GET /_launcher/agents/{name}` — the routes the
frontend calls directly via `fetch`, same-origin, no separate backend to
broker them — never return `command`, `args`, `env`, `env_template`,
`arg_template`, `working_dir`, or any resolved `${VAR}` value, only the
presentation fields and the `request_schema` itself.

### Walkthrough: click through the grid → chat

Using [`examples/launchers/webapp/`](../../examples/launchers/webapp/):

```sh
docker build -t my-webapp-launcher examples/launchers/webapp/
docker run --rm -p 9090:9090 my-webapp-launcher
```

Open `http://localhost:9090/` in a browser:

1. **Home grid** — one card per configured agent (here, one: "Echo Agent",
   🔊). The grid wraps for however many agents you configure — tens of cards
   scroll and wrap rather than breaking a fixed layout. Building an image with
   **zero** configured agents shows an explanatory empty state instead of a
   blank page.
2. Click the card to open its **detail/chat page**, scoped to that one agent.
3. Type a free-form message, or click one of the pre-defined prompt buttons
   (**"Say hello"**, **"Tell a joke"**) — either path `POST`s
   `{"prompt": "<text>"}` to `/agents/echo/trigger` and renders the streamed
   response into a chat bubble as it arrives.
4. **Refresh the page.** The transcript is gone — there is no client- or
   server-side chat history persistence in V1, by design, matching every
   other "no local persistence" guarantee in this feature.

---

## Common mistakes

- **Overriding `ENTRYPOINT`/`CMD` in your own Dockerfile.** The image builds
  fine but the launcher binary never runs, and the container does nothing.
  Only ever `COPY` — see every example above and every file under
  [`examples/launchers/`](../../examples/launchers/).
- **Two `[[agents]]` entries with the same `name`**, most often from
  copy-pasting a block and forgetting to change the name. Fatal at startup
  (exit 2), naming the offending agent.
- **Assuming a missing config file is an error.** It isn't — both launchers
  start cleanly with zero agents so an image built mid-iteration (harness
  copied in, config not yet added) still runs. A config file that *exists*
  but is malformed is the one that's fatal.
- **Pointing `command` at another agent's binary**, or two harnesses `COPY`'d
  to the same path. The launcher has no notion of which binary "belongs" to
  which agent and cannot detect this — keep each agent's `command` distinct
  and correct.
