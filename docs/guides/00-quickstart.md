# Quickstart

The fastest way in is three `baectl` commands — see
[Fastest path](#fastest-path-three-commands) right below. If you'd rather see
each layer explained (the server, a harness talking to it, a browser-based
launcher), the manual walkthrough that follows it covers the same ground one
step at a time:

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
- **This repo checked out** — Part 1 builds `baectl` from it, and Parts 2 and 3
  build and run code from it.
- **A Rust toolchain** ([rustup](https://rustup.rs)) **and `make`** to build
  `baectl` in Part 1. This goes away once the one-line installer ships (see
  Part 1).
- **For Part 2 only**, the toolchain for the language you pick: Node.js ≥ 20
  (TypeScript), Python ≥ 3.10 + [uv](https://docs.astral.sh/uv/) (Python), or a
  Rust toolchain (Rust).

---

## Fastest path (three commands)

`baectl build`/`ready`/`run` turn a running server plus some harness code
into a working agent conversation with no manual profile/key/env-var wiring.
You need two things first: your provider key exported (from
[Prerequisites](#prerequisites)) and a host `baectl` on `PATH` — that's the
one build step, the same two lines
[Part 1](#part-1--start-the-server) opens with:

```sh
make build-baectl
export PATH="$PWD/baectl/target/host/release:$PATH"
```

Then, from the **repo root**:

```sh
export ANTHROPIC_API_KEY="sk-ant-…"
baectl setup                           # once — server + provider, running
baectl build reference-assistant       # package the example harness
baectl run reference-assistant-rust-local   # check + fix + launch, in one step
```

- `baectl setup` writes `docker-compose.yml`/`.env`/`bae-config.toml` and
  launches the server — you only run this once per directory. It's the same
  command Part 1 below walks through by hand.
- `baectl build reference-assistant` resolves the bundled Rust example under
  `client-rust/examples/reference-assistant/` and records how to run it. It
  prints the exact id to use next (`reference-assistant-rust-local` for the
  Rust example over `--launcher local`, the default) — you don't need to
  memorize the `<name>-<sdk>-<launcher>` pattern, just copy what it prints.
- `baectl run <id>` re-checks that a profile/key exist that satisfy the
  harness's requirements — the wizard's `default` profile allows no
  client-side tools, while the example declares four, so `run` widens the
  profile (additively, never dropping a name) and mints a key automatically,
  with no prompt — then runs the example in the foreground. You'll see the
  assistant's reply on stdout, the same round trip as
  [Part 2](#part-2--run-a-client-harness-example) below. This is precisely the
  `create profile --allowed-tool …` / `create key` bookkeeping Part 1 walks
  through by hand.

To inspect *what* `run` would fix without applying anything, run
`baectl ready reference-assistant-rust-local` first — it prints the same
six-check report and exits non-zero without launching.

### The other two SDKs

Swap `--sdk typescript`/`--sdk python` on the `build` step, and use the
matching `-typescript-`/`-python-` id on `run`:

```sh
baectl build reference-assistant --sdk python
baectl run reference-assistant-python-local
```

`--launcher local` (the default) runs the harness on your host with the
toolchain it needs, so the language prerequisites above still apply. Python
and Rust prepare themselves on first `run` (`uv run` syncs the virtualenv;
`cargo run` compiles). **TypeScript does not** — its manifest's run command is
`npm run example`, which won't install dependencies, so do this once first:

```sh
(cd client-typescript && npm install)
```

### Packaging it into a container instead

Swap `--launcher local` for `schedule`, `api`, or `webapp` and `build`
packages the same harness into a runnable launcher image — `run` then starts
it **detached** and prints where to reach it (a clickable URL for `webapp`, a
ready-to-copy `curl` for `api`, the cron expression for `schedule`):

```sh
baectl build reference-assistant --launcher webapp
baectl run reference-assistant-rust-webapp     # prints: open the chat UI:  http://localhost:9090
```

This works for all three SDKs — `baectl` compiles the harness inside Docker
and provisions the interpreter the launcher base image needs — and is the
wired-up-to-`baesrv` counterpart to the standalone echo example in
[Part 3](#part-3--serve-an-agent-in-the-browser-webapp-launcher) below.

> **Security.** `baectl` does not set `BAE_LAUNCHER_API_TOKEN` for you, so the
> `api`/`webapp` trigger routes on the launched container are **open** — fine on
> `localhost`, never on a network-reachable host. Put that token in the `.env`
> that `setup` wrote (it is forwarded into the container) and terminate TLS
> upstream before exposing it; see the
> [Harness Launchers security section](11-harness-launchers.md#loudly-before-anything-else-bae_launcher_api_token).

### Contributors

Every one of `setup`, `build`, `ready`, and `run` also takes `--dev`, so a BAE
contributor iterating on a local client/launcher/server build can run this
exact same loop against locally built images/binaries instead of published
ones. The [developer quickstart](developer/00-quickstart.md#fastest-path-three-commands-all---dev)
walks that version through end to end. See the
[`baectl` reference](../reference/03-baectl.md#baectl-build) for the full flag
set, what each command writes to disk, and exit codes; the
[harness manifest reference](../reference/07-harness-manifest.md) documents
`bae-harness.toml`, the file a harness (bundled or your own) uses to describe
itself to `build`/`ready`/`run`.

---

## Part 1 — Start the server

The quickest way to a running, configured server is the **`baectl setup`**
wizard. It runs on your **host** — it drives your container engine from the
outside — so you need a `baectl` built for your own platform. Build it from the
checkout; this is the one step that wants a [Rust toolchain](https://rustup.rs)
on the host:

```sh
make build-baectl
export PATH="$PWD/baectl/target/host/release:$PATH"
```

> **Coming soon: a one-line installer**, so `baectl` needs no toolchain and no
> checkout:
>
> ```sh
> # PLACEHOLDER — not published yet; use the source build above.
> curl -fsSL https://<install-host-tbd>/baectl/install.sh | sh
> ```

Run the wizard from the repo root and **press Enter through every prompt** to
accept the defaults:

```sh
baectl setup
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
`reference-assistant` in Part 2 declares **four**: `get_current_time` plus the
three builtin file tools (`read_file`, `write_file`, `explore_files`) scoped to
the example's own `workspace/` directory. The server rejects the whole session
open with `403 tool_not_allowed` if *any* declared tool is missing from the
profile, so the profile has to allow all four. Create it, plus a client key
bound to it. `baectl` runs *inside* the container the wizard launched (whose
admin API is loopback-only), reached with `docker compose exec` from the
directory `setup` wrote its files to:

```sh
# Use the provider name you chose in the wizard (default: anthropic-default).
docker compose exec baesrv baectl create profile assistant anthropic-default \
  --allowed-tool get_current_time \
  --allowed-tool read_file \
  --allowed-tool write_file \
  --allowed-tool explore_files
```

> The example's fifth tool, `run_shell_command`, is a **sandbox** tool. Those
> are declared separately and are deliberately *not* checked against
> `allowed_tools` — the sandbox trust boundary is the profile's allowed image
> list — so it needs no `--allowed-tool` entry here. This is exactly the
> bookkeeping the [fastest path](#fastest-path-three-commands) does for you.

Copy the printed `id: pro_…` into the next command:

```sh
docker compose exec baesrv baectl create key assistant pro_…   # paste the profile id
```

The client key is printed **once** as `key: bae_…`. Export it — Part 2 reads it:

```sh
export BAE_CLIENT_KEY="bae_…"      # paste the key
```

> On Apple's `container` CLI, run `baectl setup --apple`; admin commands are
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
  --allowed-tool get_current_time \
  --allowed-tool read_file \
  --allowed-tool write_file \
  --allowed-tool explore_files
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
three SDKs: it registers `get_current_time` and the three builtin file tools,
opens a session, drives the tool-call loop, and prints the assistant's reply. Pick **one** language — each
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

That's the launcher end to end — but note this example agent just echoes; it
never talks to `baesrv`.

To serve a **real** agent — a harness like Part 2's, talking to `baesrv` —
you have two options:

- **`baectl build <harness> --launcher webapp`** does the whole thing for you:
  it writes the extending Dockerfile and the `bae-app.toml`, compiles the
  harness inside Docker, builds the image, and then `baectl run <id>` launches
  it wired up to your server with a resolved profile and key. See
  [Packaging it into a container instead](#packaging-it-into-a-container-instead)
  above.
- **By hand**, when you want full control: `FROM`-extend the base image, `COPY`
  in your harness binary/script and a `bae-app.toml`, and never redeclare
  `ENTRYPOINT`/`CMD`.

The [Harness Launchers guide](11-harness-launchers.md) covers the by-hand
route, plus the cron (`bae-launcher-schedule`) and plain-HTTP
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
rm -f docker-compose.yml .env bae-config.toml   # the generated files
rm -rf .baectl/                 # build artifacts — see the warning below
```

> **`.baectl/` holds secrets.** `baectl ready`/`run` write
> `.baectl/builds/<id>/resolved.json` (which carries a **plaintext client
> key**) and `harness.env` (any harness secrets you were prompted for), both at
> mode `0600`. The repo's `.gitignore` already covers `.baectl/`, but delete
> the directory when you're done rather than leaving the key on disk.

If you took the [container launcher path](#packaging-it-into-a-container-instead),
`baectl run` left a **detached** container and `baectl build` left two images
behind (the packaged image and its intermediate harness-build image):

```sh
docker rm -f reference-assistant-rust-webapp                     # the running agent
docker rmi reference-assistant-rust-webapp:latest \
           reference-assistant-rust-webapp-harness-build:latest  # its images
```

(The webapp container in Part 3 used `--rm`, so it's already gone. If you used
the by-hand `docker run` path instead, tear it down with
`docker rm -f bae && docker volume rm bae-data`. The `baectl` you built in Part 1
is a build artifact under `baectl/target/` — `make clean-baectl` drops it.)

---

## Troubleshooting

- **The container starts, then exits with `cannot open database at
  /var/lib/bae/bae.db: unable to open database file`** — the `bae-data` volume is
  root-owned, so the image's non-root `bae` user can't create the SQLite file.
  Apple's `container` (unlike docker) doesn't seed a fresh volume's ownership
  from the image. Launchers `setup` generates now fix this themselves; if you
  have an older `bae-setup.sh`, re-run `baectl setup` and choose **Edit** to
  regenerate it, or repair the volume once by hand:
  ```sh
  container run --rm --user 0:0 --entrypoint /bin/chown \
    --volume bae-data:/var/lib/bae \
    ghcr.io/prettysmartdev/better-agent-engine:latest -R bae:bae /var/lib/bae
  ```
- **`403 tool_not_allowed` when the example opens a session** — the profile is
  missing one of the four tools the example declares. The error names the
  offending tool; the profile needs `--allowed-tool` for **all** of
  `get_current_time`, `read_file`, `write_file`, and `explore_files` (Part 1).
  Rather than recreating it by hand, `baectl ready <id> --fix` widens an
  existing profile additively to cover exactly what the harness declares.
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
