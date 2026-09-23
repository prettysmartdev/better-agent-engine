# Quickstart: step by step

The [Quickstart](00-quickstart.md) gets you to a working agent in three
`baectl` commands. This page shows the same ground **one layer at a time, by
hand** — a plain server, a client harness talking to it directly, and the
webapp launcher without `baectl build --launcher webapp` — useful if you want
to understand (or customize) what each `baectl` command automates.

1. **[Part 1 — the server (`baesrv`)](#part-1--start-the-server)** — one container, running and healthy, with a hand-created profile and key.
2. **[Part 2 — a client harness example](#part-2--run-a-client-harness-example)** — the `reference-assistant` agent, in TypeScript, Python, or Rust (your choice), run directly.
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

(Or run `baectl setup --yes` to accept every default non-interactively, the
way the [Quickstart](00-quickstart.md#fastest-path-three-commands) does — the
walkthrough below assumes the interactive prompts so you can see each
question.)

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
`reference-assistant` in Part 2 declares **five**: `get_current_time`, the
three builtin file tools (`read_file`, `write_file`, `explore_files`) scoped to
the example's own `workspace/` directory, and `run_shell_command` bound to a
**local** sandbox. The server rejects the whole session open with
`403 tool_not_allowed` if *any* declared tool is missing from the profile, so
the profile has to allow all five. Create it, plus a client key
bound to it. `baectl` runs *inside* the container the wizard launched (whose
admin API is loopback-only), reached with `docker compose exec` from the
directory `setup` wrote its files to:

```sh
# Use the provider name you chose in the wizard (default: anthropic-default).
docker compose exec baesrv baectl create profile assistant anthropic-default \
  --allowed-tool get_current_time \
  --allowed-tool read_file \
  --allowed-tool write_file \
  --allowed-tool explore_files \
  --allowed-tool run_shell_command
```

> `run_shell_command` here is a sandbox tool with a **local** target: the
> harness itself runs the command in a local container, so the server sees it
> as an ordinary client-dispatched tool and checks it against
> `allowed_tools` like the others. (A **remote** sandbox tool is different:
> it is declared in `sandbox_tools`, is not checked against `allowed_tools`,
> and is gated by the profile's `--available-sandbox` image list instead.)
> This is exactly the bookkeeping the
> [Quickstart's fastest path](00-quickstart.md#fastest-path-three-commands)
> does for you.

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
  --allowed-tool explore_files \
  --allowed-tool run_shell_command
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
  it wired up to your server with a resolved profile and key. See the
  [Quickstart's webapp step](00-quickstart.md#next-try-the-webapp-launcher).
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
rm -rf .baectl/                 # build artifacts, if you also used baectl build/run
```

If you took the by-hand `docker run` path in Part 1 instead, tear it down
with `docker rm -f bae && docker volume rm bae-data`. The `baectl` you built
in Part 1 is a build artifact under `baectl/target/` — `make clean-baectl`
drops it. The webapp container in Part 3 used `--rm`, so it's already gone.

---

## Troubleshooting

Manual-path-specific issues; see the [Quickstart's
Troubleshooting](00-quickstart.md#troubleshooting) for the common `setup`/
`ready`/`run` cases too:

- **`403 tool_not_allowed` when the example opens a session** — the profile is
  missing one of the five tools the example declares. The error names the
  offending tool; the profile needs `--allowed-tool` for **all** of
  `get_current_time`, `read_file`, `write_file`, `explore_files`, and
  `run_shell_command` (Part 1).
  Rather than recreating it by hand, `baectl ready <id> --fix` widens an
  existing profile additively to cover exactly what the harness declares.
- **`422 primary_provider_unavailable` creating the profile** — the provider
  name you passed to `create profile` isn't declared in the generated
  `bae-config.toml`. Use the name the wizard used (default `anthropic-default`);
  if unsure, check the `name` under `[[providers.entries]]` in the
  `bae-config.toml` that `setup` wrote.
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
</content>
