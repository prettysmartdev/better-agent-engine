<p align="center">
  <img src="./docs/images/bae_logo.svg" width="620" alt="bae logo">
</p>

<p align="center">
  <strong>Build multiplayer AI agents that combine the powers of Local and Cloud.<br>
  Ultra customizable harnesses, parallel clients, durable sessions, and builtin tools.</strong>
</p>

<p align="center">
  <img src="https://github.com/prettysmartdev/better-agent-engine/actions/workflows/test.yml/badge.svg">
</p>

---

`better-agent-engine` (a.k.a. BAE) is a "hybrid agent system" which allows combining remote and local execution to develop powerful AI agents. BAE client harness libraries (TypeScript, Python, and Rust) are ultra-customizable with both custom and builtin tools such as files, sandboxes, subagents, and more. BAE is natively multi-client which allows many users to connect simultaneously to work together when driving an agent session.

## Combine the powers of Local and Cloud to build multiplayer agents that solve real problems for you and your team.

## What you get

- **Durable, replayable sessions out of the box.** Every message, provider call,
  tool call, and result is persisted server-side in SQLite. Restart a client,
  swap it for another, or reconnect after a crash — the full history is still
  there, and you can replay it from any point.
- **True multiplayer built in.** Multiple client harnesses and users can connect
  to the same session simultaneously, each bringing their own set of local tools,
  and all able to drive prompts while receiving a live stream of the entire session.
- **A harness you actually control.** The agent loop, tool dispatch, and prompting
  strategy are code *you* compose and override — not a black box. The wire
  protocol is extremely simple, allowing client harnesses to remain extremely lightweight.
- **Your language, your patterns.** First-class client libraries for **TypeScript,
  Python, and Rust**, at feature parity. Every capability exists in all three,
  designed for each language's own conventions.
- **Batteries included.** Built-in sandboxes, scoped file tools, server-managed
  MCP servers, and native CLI subagents mean you're wiring together capabilities,
  not reimplementing them for every agent.
- **Any model, any endpoint — with a fallback.** Providers are declared by *wire
  format* (Anthropic Messages API or OpenAI Chat Completions), not by vendor, so
  any compatible endpoint works through its own `base_url`. Each profile names a
  primary provider plus an ordered chain of fallbacks that are tried automatically
  when one fails.
- **Deploy it the way you run.** Ship agents as **cron jobs, HTTP APIs, or chat
  web apps** with `harness launchers`, administer everything from the **MAX**
  browser dashboard, or script it all with the **`baectl`** admin CLI.
- **Observability included.** Live event streaming and read-only observers let you
  watch any session as it runs, and OpenTelemetry is wired through both the client
  SDKs (zero-config spans) and the server (OTLP trace and metric export).

**→ Ready to try it? The [Quickstart](docs/guides/00-quickstart.md) takes you from a
running server, through a client harness example in your language, to an agent
you chat with in the browser.**


## The hybrid cloud/local model

BAE splits an agent into two cooperating halves and lets you deploy each half where
it makes sense:

- **`baesrv` (the server, remote)** owns everything durable — session logs, the
  tool-call loop, LLM provider connections, MCP servers, and auth. It's a single
  tiny rust binary with SQLite as its only datastore. One process to deploy, one
  database to back up.
- **Your custom harness (local)** connects to `baesrv` and runs *your* tools in *your*
  environment — with access to your filesystem, your network, your secrets, your
  local sandbox engine.

**Why this matters:** a tool call from the model can execute on your laptop or cluster
(reading a private repo, hitting an internal API) *in the same turn* as a
server-side MCP tool running in the cloud. You get the durability, centralized
auth, and always-on availability of a hosted service **and** the trust boundary,
local access, and iteration speed of code running on your own machines. Nothing
durable lives in the client, so clients stay trivially simple, disposable, and
interchangeable.

```mermaid
graph LR
    subgraph "Your environment (local)"
        H[Your harness<br/>TS / Python / Rust]
        LT[Local tools<br/>files · shell · local sandbox]
    end
    subgraph "BAE Server (cloud)"
        S[baesrv<br/>sessions · tool loop · auth]
        DB[(SQLite)]
        MCP[MCP servers]
        RS[Remote sandboxes]
    end
    H -->|HTTP + bearer key| S
    S --> DB
    S --> MCP
    S --> RS
    S -.->|provider SDKs| LLM[LLM Providers]
    H --- LT
```


## The sandbox model: real execution on your terms

Agents that can run code are powerful and dangerous. BAE makes the security
boundary something *you* declare, not something you hope the model respects.

- **Two execution targets, chosen per call.** Bind `run_shell_command` or
  `run_shell_named` to your harness and decide — at the moment the LLM actually
  calls the tool — whether it runs in the **server's** sandbox (a container
  `baesrv` starts and execs into on your behalf) or your **harness's own local**
  container engine. Entirely controlled by your custom harness and `baesrv` profile.
- **Operator-controlled images.** A profile declares exactly which container
  images a session may launch. The server pre-pulls and verifies them in the background
  the instant the profile is created, so an agent's first sandbox call never stalls on
  a multi-hundred-megabyte image pull. Any image you didn't explicitly allow simply can't run.
- **Scoped file access.** Mount built-in `read_file` / `write_file` /
  `explore_files` tools with directory allowlists, extension rules, and
  filename regexes. Path-traversal-safe validation is vetted once and consistent
  across all three languages — an LLM-driven file tool can never touch a path you
  didn't permit.
- **Modular by design.** Docker and Apple Containers sit behind one common driver
  interface, so the same agent runs on a Linux server or a Mac laptop unchanged.

**The result**: you can give an agent genuine shell and filesystem power while keeping
the trust boundary (which images, which commands, local vs. remote, which
directories) entirely in your control.


## Multiplayer: many drivers, many observers, one session

A BAE session isn't a private one-to-one chat. Multiple clients can attach to the
**same** live session at once, each getting a complete real-time stream of
everything that happens.

- **Drivers** send messages and dispatch tool calls. The server serializes turns
  through a FIFO mutex — everyone's messages queue in order, each turn runs to
  completion before the next begins, and each driver's own tool calls are handled
  by that driver. Two teammates can pair on a shared debugging session, a
  human-in-the-loop reviewer can steer an agent mid-run, or several specialized
  clients can cooperate — **without anyone keeping a private copy of the
  conversation or coordinating turns out of band.**
- **Observers** subscribe read-only and watch every driver's activity live —
  provider calls, tool calls, results, joins — without any power to alter the
  session. Perfect for monitoring, audit, and "watch the agent work" UIs.
- **Independent toolsets, shared conversation.** Each driver can register its own
  distinct set of tools (as long as they're configured in the profile's allowlist) and still
  participate in one session — each turn only ever exposes the acting driver's
  tools to the model.

Because the server owns all state and streams every event, joining a session
means catching the full history *and* the live tail with no gaps, no double-delivery,
and resumable after a disconnect.


## MAX: a web dashboard for your BAE instance

**MAX** (multi-agent executor) is an optional web UI, shipped in the `bae-max`
container variant, that turns operating and debugging BAE into something you can
do from any browser (desktop, tablet, or phone).

- **Administer without a terminal.** Create, list, and revoke profiles and client
  keys from a dashboard instead of `docker exec`-ing a CLI or hand-assembling
  admin-API calls. LLM provider and MCP server configuration are fully available.
- **Watch any session as a live graph.** Open the Sessions tab, pick any session —
  running or historical — and see its events render live as an interactive graph:
  client turns, provider requests/responses, tool calls, MCP exchanges, joins.
  Click any node for its full JSON payload. Debug or audit an in-flight run
  **without writing your own observer harness.**
- **Observe safely.** MAX joins sessions strictly as an observer — it can *watch*
  everything and *drive* nothing, so pointing it at a live production session is
  never a risk.
- **Works everywhere.** Fully mobile- and tablet-friendly — mint a key, check a
  stuck session, or triage an incident from whatever device is in front of you.

Because MAX is a pure API client, everything it does is also possible via the `baesrv` admin API.


## Ship agents as cron jobs, APIs, or web apps: harness launchers

A finished harness still needs *something* to trigger it. The launcher base
images give you three trigger models with none of the scheduling, HTTP-server,
or process-supervision code you'd otherwise write yourself. You `FROM`-extend
one base image, `COPY` in your harness (any language, with or without a BAE SDK)
and a config file, and ship it.

| Image | Triggered by | HTTP surface |
|---|---|---|
| `bae-launcher-schedule` | a cron schedule | none, by design |
| `bae-launcher-api` | `POST /agents/{name}/trigger` | JSON in, NDJSON out |
| `bae-launcher-webapp` | a built-in chat UI (which calls the same trigger API) | static SPA **plus** the trigger API |

One image can bundle a whole family of related agents, incoming request bodies
are validated against a per-agent JSON Schema, and secrets are injected as
`${VAR}` at spawn time and never logged. Ready-to-run examples for all three
live in [`examples/launchers/`](examples/launchers/); the full walkthrough is in
the [Harness Launchers guide](docs/guides/11-harness-launchers.md).


## Quickstart

The fastest path is the **`baectl setup`** wizard: extract the bundled `baectl`
binary from the image and run it to scaffold `docker-compose.yml`, `.env`, and
`bae-config.toml`, launch the server, and mint your first profile and client
key. Prefer to start the server by hand?

```sh
docker run -p 8080:8080 -v bae-data:/var/lib/bae ghcr.io/prettysmartdev/better-agent-engine:latest
curl http://localhost:8080/healthz
```

Then create a profile and client key (with `baectl` or the admin API) and drive
it. The full walkthrough takes you from the running server through a client
harness example in the language of your choice to serving an agent in the
browser with the webapp launcher: [`docs/guides/00-quickstart.md`](docs/guides/00-quickstart.md).

Want the dashboard? Run the `bae-max` variant instead —
`ghcr.io/prettysmartdev/better-agent-engine:max` — and open MAX in your browser.
See the [MAX Webapp guide](docs/guides/10-max-webapp.md).

## Project status

Early Beta. The project architecture is in place but libraries, APIs, and wire protocols will likely evolve.

## Learn more

- **[Documentation](docs/README.md)** — guides, API reference, and worked examples.
- **[Quickstart](docs/guides/00-quickstart.md)** — your first session, end to end.
- **[Multi-Client Sessions](docs/guides/07-multi-client-sessions.md)** — the
  multiplayer driver/observer model in practice.
- **[Sandboxes](docs/guides/03-sandboxes.md)** and
  **[File Tools](docs/guides/04-file-tools.md)** — safe, scoped execution.
- **[Native CLI Subagents](docs/guides/05-subagents.md)** — delegate a task to an
  external CLI such as `claude` or `codex`.
- **[MCP Servers](docs/guides/02-mcp-servers.md)** — attach real MCP tools to a profile.
- **[Profiles](docs/profiles.md)** — providers, fallback chains, tool allowlists,
  and sandbox-image and MCP-server wiring.
- **[Event Streaming](docs/guides/06-event-streaming.md)** — consume the live event
  feed and subscribe as an observer.
- **[Harness Launchers](docs/guides/11-harness-launchers.md)** — trigger your agents
  by cron, HTTP, or a chat web app.
- **[Building a Client](docs/guides/01-building-a-client.md)** — the harness API in
  Rust, TypeScript, and Python.
- **[`baectl`](docs/reference/03-baectl.md)** — the admin CLI for profiles and keys.

## Contributing & development

Building from source, running the tests, and the project specification live in
**[DEVELOPING.md](DEVELOPING.md)**. Cutting a release is documented in
**[RELEASING.md](RELEASING.md)**.

## License

Apache-2.0 — see [LICENSE](LICENSE).
