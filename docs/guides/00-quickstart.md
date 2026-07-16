# Quickstart

Get up and running with the three things that make up a BAE deployment:

1. **[Part 1 — the server (`baesrv`)](#part-1--start-the-server)** — one container, running and healthy.
2. **[Part 2 — a client harness example](#part-2--run-a-client-harness-example)** — the `reference-assistant` agent, in TypeScript, Python, or Rust (your choice).
3. **[Part 3 — the webapp launcher](#part-3--serve-an-agent-in-the-browser-webapp-launcher)** — an agent you chat with from a browser.

Do them in order the first time — Part 2 uses the key you mint in Part 1.
Commands run from the **repo root** unless noted (Part 2 `cd`s into the client
you pick), and a couple of environment variables are set once in your shell and
reused throughout — keep the same terminal open.

## Prerequisites

- **Docker** (or Apple's `container` CLI) to run the images.
- **A provider API key** — this guide uses `ANTHROPIC_API_KEY`. Export it now;
  the server and the local example both read it:
  ```sh
  export ANTHROPIC_API_KEY="sk-ant-…"
  ```
- **This repo checked out** — Parts 2 and 3 build and run code from it.
- **For Part 2 only**, the toolchain for the language you pick: Node.js ≥ 20
  (TypeScript), Python ≥ 3.10 + [uv](https://docs.astral.sh/uv/) (Python), or a
  Rust toolchain (Rust).

---

## Part 1 — Start the server

The quickest way to a running, configured server is the **`baectl setup`**
wizard. `baectl` is a small static binary — a one-line `curl | sh` installer is
on the way, but for now copy it out of the published image:

```sh
cid=$(docker create ghcr.io/prettysmartdev/better-agent-engine:latest)
docker cp "$cid":/usr/local/bin/baectl ./baectl
docker rm "$cid" >/dev/null
```

Run the wizard from the repo root and **press Enter through every prompt** to
accept the defaults:

```sh
./baectl setup
```

It writes `docker-compose.yml`, `.env`, and `bae-config.toml` into the current
directory, launches the server, and creates a first `default` profile and
client key. A few things to know as you Enter through it:

- Because you exported `ANTHROPIC_API_KEY` in the prerequisites, the wizard
  captures it automatically (no prompt) and writes it into `.env` for the
  server.
- Accepting the defaults names your provider **`anthropic-default`** — note
  whatever you pick, the next step needs it.

Check it's up:

```sh
curl -s http://localhost:8080/healthz && echo "  ← server is up"
```

The wizard's `default` profile allows **no client-side tools**, and the
`reference-assistant` in Part 2 declares one (`get_current_time`) — so create a
profile that allows it, plus a client key bound to it. `baectl` runs *inside*
the container the wizard launched (whose admin API is loopback-only), reached
with `docker compose exec` from the directory `setup` wrote its files to:

```sh
# Use the provider name you chose in the wizard (default: anthropic-default).
docker compose exec baesrv baectl create profile assistant anthropic-default \
  --allowed-tool get_current_time
```

Copy the printed `id: pro_…` into the next command:

```sh
docker compose exec baesrv baectl create key assistant pro_…   # paste the profile id
```

The client key is printed **once** as `key: bae_…`. Export it — Part 2 reads it:

```sh
export BAE_CLIENT_KEY="bae_…"      # paste the key
```

> On Apple's `container` CLI, run `./baectl setup --apple`; admin commands are
> then `container exec bae baectl …` instead of `docker compose exec baesrv …`.
> The server speaks plain HTTP on port **8080** (the admin port stays
> loopback-only inside the container); terminate TLS upstream. See
> [`baectl setup`](../reference/03-baectl.md#baectl-setup) for the full question
> list and flags, and [Configuration](../reference/05-configuration.md) for
> every `BAE_*` variable.

<details>
<summary>Prefer to start the server by hand (no wizard)?</summary>

Run the image directly against the repo's ready-made provider registry
([`examples/bae-config/providers.toml`](../../examples/bae-config/providers.toml),
which declares `anthropic-sonnet`), then create the profile and key with
`docker exec bae baectl …`:

```sh
docker run -d --name bae \
  -p 8080:8080 \
  -v bae-data:/var/lib/bae \
  -v "$PWD/examples/bae-config/providers.toml:/etc/bae/providers.toml:ro" \
  -e BAE_CONFIG=/etc/bae/providers.toml \
  -e ANTHROPIC_API_KEY \
  ghcr.io/prettysmartdev/better-agent-engine:latest

docker exec bae baectl create profile assistant anthropic-sonnet \
  --allowed-tool get_current_time
docker exec bae baectl create key assistant pro_…      # paste the profile id
export BAE_CLIENT_KEY="bae_…"                           # paste the key
```

This path takes no interactive input and pins a known provider name, which is
handy for scripting. Substitute `docker exec bae` for `docker compose exec
baesrv` in the rest of this guide.

</details>

---

## Part 2 — Run a client harness example

The `reference-assistant` is the canonical BAE agent, shipped identically in all
three SDKs: it registers `get_current_time`, opens a session, drives the
tool-call loop, and prints the assistant's reply. Pick **one** language — each
reads the `BAE_CLIENT_KEY` (and `ANTHROPIC_API_KEY`) you exported above.

**TypeScript**
```sh
cd client-typescript && npm install
npm run example -- "What time is it?"
```

**Python**
```sh
cd client-python && uv sync
uv run python examples/reference-assistant/main.py "What time is it?"
```

**Rust**
```sh
cd client-rust
cargo run --example reference-assistant -- "What time is it?"
```

You'll see the assistant's answer on stdout and `[hook …]` lines on stderr as
each of the five hooks fires. That's a full round-trip: your local harness
declared a tool, the server called the model, the model called your tool, and
your harness answered.

From here, [Building a Client](01-building-a-client.md) walks through the harness
API in each language, and the example's own README
([TypeScript](../../client-typescript/examples/reference-assistant/README.md),
[Python](../../client-python/examples/reference-assistant/README.md),
[Rust](../../client-rust/examples/reference-assistant/README.md)) documents
its environment variables and failure modes. The
[`issue-triage`](08-issue-triage-agent.md) example composes file tools, sandboxes,
and an MCP server on one session.

---

## Part 3 — Serve an agent in the browser (webapp launcher)

The **webapp launcher** (`bae-launcher-webapp`) wraps a harness in a browser
chat UI — a card grid and a chat view — with no HTTP-server code of your own.
The repo ships a ready-to-run example (a trivial echo harness, so you can see
the whole UI in two commands). Back at the **repo root**:

```sh
docker build -t my-webapp-launcher examples/launchers/webapp/
docker run --rm -p 9090:9090 my-webapp-launcher
```

Open **http://localhost:9090/** and:

1. Click the **Echo Agent** card.
2. Type a message, or click **Say hello** / **Tell a joke** — either one
   triggers the agent and streams its output into the chat live.

That's the launcher end to end. To serve a **real** agent — a harness like
Part 2's, talking to `baesrv` — you `FROM`-extend the base image, `COPY` in
your harness binary/script and a `bae-app.toml`, and never redeclare
`ENTRYPOINT`/`CMD`. The [Harness Launchers guide](11-harness-launchers.md) covers
that, plus the cron (`bae-launcher-schedule`) and plain-HTTP
(`bae-launcher-api`) variants.

> **Security.** The example leaves `BAE_LAUNCHER_API_TOKEN` unset, so every
> trigger route is open — fine on `localhost`, never on a network-reachable
> host. Set that token and terminate TLS upstream before exposing it; see the
> [guide's security section](11-harness-launchers.md#loudly-before-anything-else-bae_launcher_api_token).

---

## Clean up

From the directory `baectl setup` wrote its files to:

```sh
docker compose down -v          # stop the server and drop its data volume
rm -f docker-compose.yml .env bae-config.toml baectl   # the generated files
```

(The webapp container in Part 3 used `--rm`, so it's already gone. If you used
the by-hand `docker run` path instead, tear it down with
`docker rm -f bae && docker volume rm bae-data`.)

---

## Troubleshooting

- **`403 tool_not_allowed` when the example opens a session** — the profile
  doesn't allow `get_current_time`. Recreate it with
  `--allowed-tool get_current_time` (Part 1).
- **The example exits complaining a provider key is unset** — export
  `ANTHROPIC_API_KEY` in the shell running the example (it fails fast locally,
  even though the key is only used server-side).
- **`ProvidersFailedError` / an all-providers-failed result** — the *server*
  couldn't reach the provider. Confirm `ANTHROPIC_API_KEY` was set when you ran
  `baectl setup` (so it landed in `.env`) and that the key is valid; re-run
  `baectl setup` to fix `.env` if needed.
- **`422 primary_provider_unavailable` creating the profile** — the provider
  name you passed to `create profile` isn't declared in the generated
  `bae-config.toml`. Use the name the wizard used (default `anthropic-default`);
  if unsure, check the `name` under `[[providers.entries]]` in the
  `bae-config.toml` that `setup` wrote.

---

## Next steps

- [Building a Client](01-building-a-client.md) — the harness API in Rust, TypeScript, and Python.
- [baectl reference](../reference/03-baectl.md) — every `baectl` subcommand, flag, and exit code.
- [Admin authentication](09-admin-authentication.md) — how the admin key is created, rotated, and disabled.
- [Client API reference](../reference/00-client-api.md) — full session and message endpoints.
- [Wire Protocol](../reference/01-wire-protocol.md) and [Session Basics](../examples/session-basics.md) — drive a session over raw HTTP/curl.
- [Profiles](../profiles.md) — provider config, env var references, fallbacks, MCP wiring.
- [Message types](../reference/04-message-types.md) — all 27 `event_type` values and their payloads.
- [MCP Servers](02-mcp-servers.md) — connect real MCP tools to a profile.
- [Event Streaming](06-event-streaming.md) — live progress notifications and observer subscriptions.
- [Multi-Client Sessions](07-multi-client-sessions.md) — join a session as a second driver.
- [Harness Launchers](11-harness-launchers.md) — cron, HTTP, and webapp triggers for your agents.
