# Quickstart (from source, for developers)

> **This is the contributor / build-from-source version of the
> [Quickstart](../00-quickstart.md).** It covers the exact same ground, but
> every image is **built from your local checkout** instead of pulled from
> `ghcr.io/prettysmartdev/better-agent-engine`. If you just want to *use* BAE,
> follow the [normal Quickstart](../00-quickstart.md) — it's shorter and needs
> no toolchain. Come here when you're iterating on the server, `baectl`, an
> SDK, or a launcher and want to run *your* code end to end.

The fastest inner loop is three `baectl` commands with `--dev` — see
[Fastest path](#fastest-path-three-commands-all---dev) right below. If you'd
rather see each layer built and explained separately, the walkthrough that
follows covers the same ground one step at a time, all built locally:

1. **[Part 1 — the server (`baesrv`)](#part-1--build-and-start-the-server)** —
   `make image`, then the `baectl setup --dev` wizard pointed at your local tag.
2. **[Part 2 — a client harness example](#part-2--run-a-client-harness-example-from-source)** —
   the `reference-assistant` agent, exercising your local SDK source.
3. **[Part 3 — the webapp launcher](#part-3--serve-an-agent-in-the-browser-webapp-launcher)** —
   an agent you chat with from a browser, on a locally-built base image.

Do them in order the first time — Part 2 uses the key you mint in Part 1.
Commands run from the **repo root** unless noted (Part 2 `cd`s into the client
you pick), and a couple of environment variables are set once in your shell and
reused throughout — keep the same terminal open.

## Prerequisites

- **Docker** (or Apple's `container` CLI) **and `make`** — that's all the host
  needs to *build*. The dev image ([`Dockerfile.dev`](../../../Dockerfile.dev))
  carries the Rust, Node, and Python/uv toolchains, so `make image` and friends
  don't require any of them on your host. See [DEVELOPING.md](../../../DEVELOPING.md)
  for the full build loop.
- **A Rust toolchain** ([rustup](https://rustup.rs)) **for Part 1, step 2 only**
  — `baectl` runs on the host, not in a container, so `make build-baectl` builds
  it with your toolchain for your platform. There's a Docker-only fallback on
  Linux hosts (see that step).
- **A provider API key** — this guide uses `ANTHROPIC_API_KEY`. Export it now;
  the server and the local example both read it:
  ```sh
  export ANTHROPIC_API_KEY="sk-ant-…"
  ```
- **This repo checked out** — you're building and running its code.
- **For Part 2 only**, the toolchain for the language you pick, *on the host*:
  Node.js ≥ 20 (TypeScript), Python ≥ 3.10 + [uv](https://docs.astral.sh/uv/)
  (Python), or a Rust toolchain (Rust). (Or run the example inside `make shell`,
  which has all three — see Part 2.)

---

<a id="fastest-path-three-commands-all---dev"></a>

## Fastest path (three commands, all `--dev`)

Before the walkthrough: the same three-command loop as the
[normal Quickstart](../00-quickstart.md#fastest-path-three-commands), with
`--dev` on every verb. **`--dev` is what guarantees no step silently falls back
to a published GHCR tag or a released SDK package** — the whole loop runs
against your checkout.

Build the two things it needs — the server image, and a host `baectl` — then
run it from the **repo root**:

```sh
make image                                          # better-agent-engine:latest
make build-baectl
export PATH="$PWD/baectl/target/host/release:$PATH"

export ANTHROPIC_API_KEY="sk-ant-…"
baectl setup --dev                            # once — your local image, running
baectl build reference-assistant --dev        # your local SDK source
baectl run reference-assistant-rust-local --dev
```

What each `--dev` actually does — they are **not** all the same:

| Command | Effect of `--dev` |
|---|---|
| `setup --dev` | Writes your local `better-agent-engine:latest`/`:max` tag into `docker-compose.yml` instead of the GHCR tag, and pre-answers the wizard's "use locally-built image tags?" question. |
| `build --dev` | For a container launcher, bases the packaged image on your local `better-agent-engine:launcher-<type>` tag instead of the GHCR one. Records the flag in the build's `manifest.json`. For `--launcher local` there is nothing to switch — the harness is compiled from your checkout either way — so it only records the flag. |
| `ready --dev` / `run --dev` | **Nothing functional.** The artifact was already fixed at `build` time. The flag is a consistency guard: passing it when the build recorded the opposite (or omitting it when the build recorded `--dev`) prints a warning naming the mismatch, so you don't accidentally mix a locally built artifact with published-artifact assumptions. |

`baectl build` always compiles the bundled example against the SDK source in
your checkout — for `--launcher local` on the host, and for a container
launcher inside Docker — so a change you make under `client-rust/src/` shows up
on the next `build`/`run` with no publish step.

### Packaging your local launcher base

Container launchers need the matching base image built locally first:

```sh
make image-launcher-webapp                     # or -api / -schedule
baectl build reference-assistant --launcher webapp --dev
baectl run reference-assistant-rust-webapp --dev   # prints its http://localhost:9090 URL
```

Note what you **don't** have to do here: `baectl build --dev` references the
local `better-agent-engine:launcher-webapp` tag directly, so none of the
`docker tag …` retagging that [Part 3](#part-3--serve-an-agent-in-the-browser-webapp-launcher)
needs for the hand-written example Dockerfile applies. This is the shortest
loop for iterating on a launcher base image.

Container packaging works for all three SDKs — `baectl` compiles the harness
inside Docker and provisions the interpreter the Debian-based launcher base
needs. Swap `--sdk typescript`/`--sdk python` on `build` and use the matching
`-typescript-`/`-python-` id on `run`. For `--launcher local` the harness runs
on your host instead, so its toolchain applies: Python and Rust prepare
themselves on first `run`, but TypeScript needs `(cd client-typescript && npm
install)` once first (its run command is `npm run example`, which does not
install dependencies).

> **Security.** `baectl` does not set `BAE_LAUNCHER_API_TOKEN` for you, so the
> `api`/`webapp` trigger routes on the launched container are **open** — fine
> on `localhost`, never on a network-reachable host. See the
> [Harness Launchers security section](../11-harness-launchers.md#loudly-before-anything-else-bae_launcher_api_token).

The rest of this guide is the same ground one step at a time, plus the
by-hand alternatives for when you need them.

---

## Part 1 — Build and start the server

Everything below mirrors the [normal Quickstart's Part 1](../00-quickstart.md#part-1--start-the-server),
with two developer differences: you **build** the image instead of pulling it,
and you pass **`--dev`** to the wizard so the generated launcher references your
local tag.

### 1. Build the server image

`make image` builds [`Dockerfile`](../../../Dockerfile) and tags it
`better-agent-engine:latest` (a **local** tag — no registry prefix):

```sh
make image
```

> Building the `bae-max` variant instead? Use `make image-max` (tags
> `better-agent-engine:max`) and answer `max` to the wizard's **Image variant?**
> question below.

### 2. Get a `baectl` binary

The wizard is a host-side tool, so you need a `baectl` that runs on **your**
machine. Build it from source and put it on `PATH` for the rest of this guide:

```sh
make build-baectl
export PATH="$PWD/baectl/target/host/release:$PATH"
```

`build-baectl` is the one component verb that runs on the **host** rather than in
the dev image: it compiles with your own Rust toolchain for your own platform
(macOS included), so the binary is directly runnable. It's the only step in this
guide that wants [Rust](https://rustup.rs) on the host — everything else still
needs just Docker and `make`.

> **Coming soon: a one-line installer**, so a host `baectl` needs no toolchain:
>
> ```sh
> # PLACEHOLDER — not published yet; use `make build-baectl` above.
> curl -fsSL https://<install-host-tbd>/baectl/install.sh | sh
> ```
>
> It will install the *released* binary, so keep building from source whenever
> you're iterating on `baectl` itself.

### 3. Run the wizard with `--dev`

`--dev` makes `setup` write your **local** `better-agent-engine:latest` (or
`:max`) tag into `docker-compose.yml` instead of the published GHCR tags — and
pre-answers the wizard's "use locally-built image tags?" question. Run it from
the repo root and **press Enter through every prompt** to accept the defaults:

```sh
baectl setup --dev
```

It writes `docker-compose.yml`, `.env`, and `bae-config.toml` into the current
directory, launches the server from your local image, and creates a first
`default` profile and client key. A few things to know as you Enter through it:

- Because you exported `ANTHROPIC_API_KEY` in the prerequisites, the wizard
  captures it automatically (no prompt) and writes it into `.env` for the
  server.
- Accepting the defaults names your provider **`anthropic-default`** — note
  whatever you pick, the next step needs it.
- `--dev` only swaps the image tags. `setup` does **not** verify the tag exists
  locally, so if you skipped step 1 the launch will fail with the engine's own
  "no such image" error — see [Troubleshooting](#troubleshooting).

Check it's up:

```sh
curl -s http://localhost:8080/healthz && echo "  ← server is up"
```

### 4. Create a profile and key for the example

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
> list — so it needs no `--allowed-tool` entry here. The
> [fastest path](#fastest-path-three-commands-all---dev) above does this whole
> step for you.

Copy the printed `id: pro_…` into the next command:

```sh
docker compose exec baesrv baectl create key assistant pro_…   # paste the profile id
```

The client key is printed **once** as `key: bae_…`. Export it — Part 2 reads it:

```sh
export BAE_CLIENT_KEY="bae_…"      # paste the key
```

> On Apple's `container` CLI, run `baectl setup --dev --apple`; admin commands
> are then `container exec bae baectl …` instead of `docker compose exec baesrv
> …`. See [`baectl setup`](../../reference/03-baectl.md#baectl-setup) for the
> full question list and flags (including `--dev`), and
> [Configuration](../../reference/05-configuration.md) for every `BAE_*`
> variable.

<details>
<summary>Prefer to skip the wizard? (<code>make run/baesrv</code> or <code>make run</code>)</summary>

The Makefile has two build-and-run shortcuts that launch the server directly —
handy when you're iterating and don't need the generated compose files. Both are
documented in [DEVELOPING.md](../../../DEVELOPING.md#building-and-running-the-server).

**`make run/baesrv`** builds `Dockerfile` and runs the *production* image
detached as a container named `bae` on port 8080, mounting
[`bae-max-demo/config.toml`](../../../bae-max-demo/config.toml) (which declares
the provider **`anthropic-sonnet`**) and forwarding `ANTHROPIC_API_KEY`:

```sh
make run/baesrv
docker exec bae baectl create profile assistant anthropic-sonnet \
  --allowed-tool get_current_time \
  --allowed-tool read_file \
  --allowed-tool write_file \
  --allowed-tool explore_files
docker exec bae baectl create key assistant pro_…      # paste the profile id
export BAE_CLIENT_KEY="bae_…"                           # paste the key
```

Note the provider name is **`anthropic-sonnet`** here (from the demo config),
not `anthropic-default`, and admin commands use `docker exec bae` in place of
`docker compose exec baesrv` for the rest of this guide.

**`make run`** instead runs `baesrv` *inside the dev container* (named
`better-agent-engine-dev`) without building the production image — the fastest
inner loop while editing server code. Its admin API is loopback-only inside that
container on port 8081; reach it with `docker exec better-agent-engine-dev
baectl …`.

</details>

---

## Part 2 — Run a client harness example (from source)

The `reference-assistant` is the canonical BAE agent, shipped identically in all
three SDKs: it registers `get_current_time` and the three builtin file tools,
opens a session, drives the tool-call loop, and prints the assistant's reply. Because these examples build
against the SDK source **in this repo**, running one exercises *your* local SDK
changes — no published package involved. Pick **one** language — each reads the
`BAE_CLIENT_KEY` (and `ANTHROPIC_API_KEY`) you exported above.

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

> No host toolchain, or want a clean environment? `make shell` drops you into
> the dev container (Rust + Node 22 + Python/uv) with the repo mounted at
> `/workspace`. Export `BAE_CLIENT_KEY`/`ANTHROPIC_API_KEY` inside it and run
> the same commands. The server is reachable at `http://localhost:8080` from the
> host; from inside a *separate* container you may need to point the client at
> the host — see the SDK example READMEs for the base-URL env var.

You'll see the assistant's answer on stdout and `[hook …]` lines on stderr as
each of the five hooks fires. That's a full round-trip: your local harness
declared a tool, the server called the model, the model called your tool, and
your harness answered.

From here, [Building a Client](../01-building-a-client.md) walks through the
harness API in each language, and the example's own README
([TypeScript](../../../client-typescript/examples/reference-assistant/README.md),
[Python](../../../client-python/examples/reference-assistant/README.md),
[Rust](../../../client-rust/examples/reference-assistant/README.md)) documents
its environment variables and failure modes. The
[`issue-triage`](../08-issue-triage-agent.md) example composes file tools,
sandboxes, and an MCP server on one session.

---

## Part 3 — Serve an agent in the browser (webapp launcher)

The **webapp launcher** (`bae-launcher-webapp`) wraps a harness in a browser
chat UI — a card grid and a chat view — with no HTTP-server code of your own.
The repo ships a ready-to-run example (a trivial echo harness, so you can see
the whole UI in two commands).

> **Shortcut.** If what you want is *your* harness on *your* launcher base,
> use `baectl build --launcher webapp --dev` — see
> [Packaging your local launcher base](#packaging-your-local-launcher-base)
> above. It references the local tag directly, so none of the retagging below
> applies. The rest of this part is the by-hand route, and the standalone echo
> example that comes with it.

The one developer wrinkle: the example's
[`Dockerfile`](../../../examples/launchers/webapp/Dockerfile) does
`FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-webapp` — the
**published** base tag. `make image-launcher-webapp` builds that base but tags
it locally as `better-agent-engine:launcher-webapp` (no registry prefix), so the
`FROM` won't find it. Build the base, then **retag it to match the `FROM`** so
`docker build` resolves to your local image instead of pulling:

```sh
make image-launcher-webapp
docker tag better-agent-engine:launcher-webapp \
  ghcr.io/prettysmartdev/better-agent-engine:launcher-webapp
```

Now build and run the example on top of your local base. Back at the **repo
root**:

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
`ENTRYPOINT`/`CMD`. The [Harness Launchers guide](../11-harness-launchers.md)
covers that, plus the cron (`bae-launcher-schedule`) and plain-HTTP
(`bae-launcher-api`) variants — both of which have their own
`make image-launcher-schedule` / `make image-launcher-api` targets you'd retag
the same way.

> **Security.** The example leaves `BAE_LAUNCHER_API_TOKEN` unset, so every
> trigger route is open — fine on `localhost`, never on a network-reachable
> host. Set that token and terminate TLS upstream before exposing it; see the
> [guide's security section](../11-harness-launchers.md#loudly-before-anything-else-bae_launcher_api_token).

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
> mode `0600`. `.gitignore` already covers `.baectl/`, but delete the directory
> when you're done rather than leaving the key on disk.

If you used a container launcher, `baectl run` left a **detached** container and
`baectl build` left two images (the packaged image and its intermediate
harness-build image):

```sh
docker rm -f reference-assistant-rust-webapp
docker rmi reference-assistant-rust-webapp:latest \
           reference-assistant-rust-webapp-harness-build:latest
```

(The webapp container in Part 3 used `--rm`, so it's already gone. If you used
`make run/baesrv` instead, tear it down with `docker rm -f bae && docker volume
rm bae-data`; for `make run`, `Ctrl-C` stops the dev container, which was
started with `--rm`.)

The `baectl` you built is a build artifact under `baectl/target/` —
`make clean-baectl` drops it.

Optionally drop the locally-built images and the Part 3 retag:

```sh
docker rmi my-webapp-launcher \
  ghcr.io/prettysmartdev/better-agent-engine:launcher-webapp \
  better-agent-engine:launcher-webapp \
  better-agent-engine:latest
```

---

## Troubleshooting

Developer-specific first, then the failures shared with the
[normal Quickstart](../00-quickstart.md#troubleshooting):

- **`no such image: better-agent-engine:latest` when the wizard launches** — you
  passed `--dev` but haven't built the image yet. Run `make image` (or
  `make image-max` for the max variant), then re-run `baectl setup --dev` in the
  same directory and choose **Launch**.
- **The Part 3 build pulls from GHCR (or fails on `pull access denied`)** — the
  local base tag doesn't match the example's `FROM`. Run
  `make image-launcher-webapp` and the `docker tag …` retag above so
  `ghcr.io/prettysmartdev/better-agent-engine:launcher-webapp` resolves locally.
- **The compose file still references a GHCR tag** — you ran `baectl setup`
  without `--dev`. Re-run `baectl setup --dev` and choose **Edit** (accepting
  every default) to rewrite `docker-compose.yml` with the local tag.
- **`403 tool_not_allowed` when the example opens a session** — the profile is
  missing one of the four tools the example declares. The error names the
  offending tool; the profile needs `--allowed-tool` for **all** of
  `get_current_time`, `read_file`, `write_file`, and `explore_files` (Part 1,
  step 4). `baectl ready <id> --fix` widens an existing profile additively to
  cover exactly what the harness declares, rather than recreating it by hand.
- **`baectl ready`/`run` warns about a `--dev` mismatch** — the build recorded
  a different `--dev` setting than this invocation used. It's a warning, never
  a failure: the artifact was fixed at `build` time, so the flag has no
  functional effect here. Re-`build` with the flag you actually want if the
  warning is telling you something real.
- **A container launcher build fails on a missing base image** — `build --dev`
  references your local `better-agent-engine:launcher-<type>` tag. Run the
  matching `make image-launcher-schedule` / `-api` / `-webapp` first.
- **The example exits complaining a provider key is unset** — export
  `ANTHROPIC_API_KEY` in the shell running the example (it fails fast locally,
  even though the key is only used server-side).
- **`ProvidersFailedError` / an all-providers-failed result** — the *server*
  couldn't reach the provider. Confirm `ANTHROPIC_API_KEY` was set when you ran
  `baectl setup` (so it landed in `.env`) and that the key is valid; re-run
  `baectl setup --dev` to fix `.env` if needed.
- **`422 primary_provider_unavailable` creating the profile** — the provider
  name you passed to `create profile` isn't declared in the generated
  `bae-config.toml`. Use the name the wizard used (default `anthropic-default`;
  or `anthropic-sonnet` if you took the `make run/baesrv` path).

---

## Next steps

- [DEVELOPING.md](../../../DEVELOPING.md) — the full build/test/lint loop, per-component
  verbs, and every `make` target.
- [Normal Quickstart](../00-quickstart.md) — the pull-from-GHCR version, for when
  you want to reproduce a user's experience.
- [RELEASING.md](../../../RELEASING.md) — how the images and SDKs you built locally get
  published.
- [Building a Client](../01-building-a-client.md) — the harness API in Rust, TypeScript, and Python.
- [baectl reference](../../reference/03-baectl.md) — every `baectl` subcommand, flag (including `--dev`), and exit code.
- [Harness Launchers](../11-harness-launchers.md) — cron, HTTP, and webapp triggers for your agents.
