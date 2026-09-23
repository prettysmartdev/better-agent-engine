# Quickstart

The fastest way in is three `baectl` commands, below. Want to see each layer
by hand instead — the server, a harness talking to it, a browser-based
launcher — one step at a time? See
[Quickstart: step by step](00a-quickstart-step-by-step.md), linked again at
the bottom of this page.

## Prerequisites

- **Docker** (or Apple's `container` CLI) to run the images.
- **A provider API key** — this guide uses `ANTHROPIC_API_KEY`. Export it now;
  the server and the local example both read it:
  ```sh
  export ANTHROPIC_API_KEY="sk-ant-…"
  ```
- **This repo checked out.**
- **A Rust toolchain** ([rustup](https://rustup.rs)) **and `make`** — needed
  today to build `baectl` itself (see [Install `baectl`](#install-baectl)
  below) and to run the bundled Rust example this page uses. This toolchain
  requirement goes away for `baectl` once the one-line installer ships.
- *Only for the other two SDKs:* **Node.js 22 with `npm`** (TypeScript) or
  **[`uv`](https://docs.astral.sh/uv/)** (Python) on the host.

## Install `baectl`

`baectl` drives your container engine from the **host**, so you need a
`baectl` built for your own platform — the in-image Linux binary can't run on
macOS. Build one from this checkout:

```sh
make build-baectl
export PATH="$PWD/baectl/target/host/release:$PATH"
```

> **Installer coming.** A one-line `curl | sh` installer (no checkout, no Rust
> toolchain) is planned but not published yet — this source build is the only
> path for now.

## Fastest path (three commands)

From the **repo root**:

<!-- quickstart-commands:start -->
```sh
baectl setup --yes
baectl build reference-assistant
baectl run reference-assistant-rust-local
```
<!-- quickstart-commands:end -->

That's it — no prompts, no manual profile/key/env wiring between the three
commands.

> **Apple `container` instead of Docker?** Use `baectl setup --yes --apple`
> as the first command; `build` and `run` are unchanged (their hints then
> print `container exec bae baectl …` instead of `docker compose exec …`).
> The flag matters on the very first `setup`: a directory already set up for
> the other engine makes `setup --yes` abort without touching anything — see
> [Troubleshooting](#troubleshooting).

- **`baectl setup --yes`** picks your provider from the environment
  (`ANTHROPIC_API_KEY` first, then `OPENAI_API_KEY`), writes
  `docker-compose.yml`/`.env`/`bae-config.toml`, and launches the server —
  once per directory. See [`baectl setup`](../reference/03-baectl.md#baectl-setup).
- **`baectl build reference-assistant`** packages the bundled Rust example
  under `client-rust/examples/reference-assistant/` and prints the exact id
  to use next (`reference-assistant-rust-local`) — you don't need to memorize
  the `<name>-<sdk>-<launcher>` pattern, just copy what it prints.
- **`baectl run <id>`** re-checks that a profile/key exist that satisfy the
  harness's requirements — the wizard's `default` profile allows no
  client-side tools, while the example declares five, so `run` widens the
  profile (additively, never dropping a name) and mints a key automatically,
  with no prompt — then runs the example in the foreground.

### What you'll see

`setup --yes` prints which provider it picked (never the key value), then
streams the engine's own startup output, then a ready-to-copy
`BAE_URL`/`BAE_API_KEY` example. `build` prints
`` built `reference-assistant-rust-local` ``. `run` prints a couple of
`fixed: …` lines (the automatic profile-widen and key-create — this is the
only run where you'll see those; later runs reuse them) and then a small
header:

```
── running reference-assistant (local) ──────────────
profile:  default (pro_…)
key:      key_…
server:   http://localhost:8080
(Ctrl-C to stop)
```

followed by the assistant's answer on stdout and `[hook …]` progress lines on
stderr — the same round trip
[Building a Client](01-building-a-client.md) walks through in each SDK.

Curious what `run` would have fixed, without applying anything? Run
`baectl ready reference-assistant-rust-local` first — it prints the same
six-check report. On a truly fresh `setup --yes`, expect two `⚠` lines
(profile needs widening, key needs creating) and exit code `3` — meaning
every remaining issue is auto-fixable, which is exactly what `run` (or
`ready --fix`) then does with no prompting. See [`baectl ready` exit
codes](../reference/03-baectl.md#baectl-ready).

### The other two SDKs

Swap `--sdk typescript`/`--sdk python` on `build`, and use the matching
`-typescript-`/`-python-` id on `run`:

```sh
baectl build reference-assistant --sdk python
baectl run reference-assistant-python-local
```

All three SDKs prepare themselves automatically on first `run` — Rust
compiles (`cargo run`), Python syncs its virtualenv (`uv run`), and
TypeScript installs its dependencies (`npm install`, via the manifest's
`prepare` command) — no manual install step first. They do need their own
toolchain on the host, just as Rust needs `cargo`: **Node.js 22 with `npm`**
for TypeScript, and **[`uv`](https://docs.astral.sh/uv/)** for Python.

## Next: try the webapp launcher

Package the same harness behind a browser chat UI instead of running it on
your host — `build` compiles it inside Docker, `run` starts it **detached**
and prints a clickable URL:

```sh
baectl build reference-assistant --launcher webapp
baectl run reference-assistant-rust-webapp     # prints: open the chat UI:  http://localhost:9090
```

On Apple's `container` CLI, the default server address given to the
container (`http://host.docker.internal:8080`) is a Docker-only alias. Pass
`--server-url http://<your-mac's-address>:8080` to `run` with an address of
your Mac that containers can reach (see [`baectl run`](../reference/03-baectl.md#baectl-run)).

This works for all three SDKs, and is the wired-up-to-`baesrv` counterpart to
the standalone echo example in
[Quickstart: step by step — Part 3](00a-quickstart-step-by-step.md#part-3--serve-an-agent-in-the-browser-webapp-launcher).
`schedule` and `api` (a cron trigger and a plain HTTP trigger) package the
same way — see [Harness Launchers](11-harness-launchers.md).

> **Security.** `baectl` does not set `BAE_LAUNCHER_API_TOKEN` for you, so the
> `api`/`webapp` trigger routes on the launched container are **open** — fine
> on `localhost`, never on a network-reachable host. Put that token in the
> `.env` that `setup` wrote (it is forwarded into the container) and
> terminate TLS upstream before exposing it; see the
> [Harness Launchers security section](11-harness-launchers.md#loudly-before-anything-else-bae_launcher_api_token).

## Contributors

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

## Clean up

From the directory `baectl setup` wrote its files to:

```sh
docker compose down -v          # stop the server and drop its data volume
rm -f docker-compose.yml .env bae-config.toml   # the generated files (in this checkout,
                                                # only docker-compose.yml shows in `git status`)
rm -rf .baectl/                 # build artifacts — see the warning below
```

> **`.baectl/` holds secrets.** `baectl ready`/`run` write
> `.baectl/builds/<id>/resolved.json` (which carries a **plaintext client
> key**) and `harness.env` (any harness secrets you were prompted for), both at
> mode `0600`. The repo's `.gitignore` already covers `.baectl/`, but delete
> the directory when you're done rather than leaving the key on disk.

If you took the container-launcher path above, `baectl run` left a
**detached** container and `baectl build` left two images behind (the
packaged image and its intermediate harness-build image):

```sh
docker rm -f reference-assistant-rust-webapp                     # the running agent
docker rmi reference-assistant-rust-webapp:latest \
           reference-assistant-rust-webapp-harness-build:latest  # its images
```

(The `baectl` you built above is a build artifact under `baectl/target/` —
`make clean-baectl` drops it.)

---

## Troubleshooting

- **`baectl setup --yes` exits with `no provider API key found in the
  environment`** — export `ANTHROPIC_API_KEY` (or `OPENAI_API_KEY`) *before*
  running it; `setup --yes` checks before writing anything.
- **`baectl setup --yes` exits with `aborted; no files were changed.`** — the
  directory already holds a partial setup, or one made for the other engine
  (for example a `docker-compose.yml` from a Docker run when you now pass
  `--apple`, or `bae-setup.sh` the other way round). `--yes` never overwrites
  that. The line above the error names the files present: remove them (after
  `docker compose down -v` if a server is running from them) and re-run, or run
  `baectl setup` without `--yes` to choose interactively.
- **`baectl ready`/`run` report `⚠` items and exit `3`** — that's expected on
  a fresh setup, not a failure: every listed issue is auto-fixable, and
  `run` (or `ready --fix`) resolves them without prompting. A `✗` line is the
  one that actually blocks — its hint tells you what to fix.
- **The container starts, then exits with `cannot open database at
  /var/lib/bae/bae.db: unable to open database file`** — the `bae-data` volume is
  root-owned, so the image's non-root `bae` user can't create the SQLite file.
  Apple's `container` (unlike docker) doesn't seed a fresh volume's ownership
  from the image. Re-run `baectl setup` and choose **Edit** to regenerate
  `bae-setup.sh`, or repair the volume once by hand:
  ```sh
  container run --rm --user 0:0 --entrypoint /bin/chown \
    --volume bae-data:/var/lib/bae \
    ghcr.io/prettysmartdev/better-agent-engine:latest -R bae:bae /var/lib/bae
  ```
- **`403 tool_not_allowed` when the example opens a session** (only possible
  with `--no-ready`, or on the manual walkthrough) — the profile is missing
  one of the tools the example declares. `baectl ready <id> --fix` (or a
  plain `run`) widens an existing profile additively to cover exactly what
  the harness declares.
- **The example exits complaining a provider key is unset** — export
  `ANTHROPIC_API_KEY` (or whichever variable `BAE_PROVIDER_KEY_ENV` names) in
  the shell running the example — it fails fast locally, even though the key
  is only used server-side.
- **`ProvidersFailedError` / an all-providers-failed result** — the *server*
  couldn't reach the provider. Confirm the provider key was set when you ran
  `baectl setup` (so it landed in `.env`) and that it's valid; re-run
  `baectl setup` to fix `.env` if needed.
- **`422 primary_provider_unavailable` creating a profile by hand** — the
  provider name you passed isn't declared in the generated
  `bae-config.toml`. Use the name the wizard used (default `anthropic-default`);
  if unsure, check the `name` under `[[providers.entries]]` in the
  `bae-config.toml` that `setup` wrote.
- **Docker/`container` isn't running** — start it before `baectl setup`;
  `setup`'s launch step reports a clean "not found on PATH" error rather than
  a raw shell error, but it still needs the engine itself running.

---

## Next steps

- [Quickstart: step by step](00a-quickstart-step-by-step.md) — the same ground
  covered by hand: a manually created profile/key, each SDK's example
  directly, and the webapp launcher without `baectl build --launcher webapp`.
- [Building a Client](01-building-a-client.md) — the harness API in Rust, TypeScript, and Python.
- [baectl reference](../reference/03-baectl.md) — every `baectl` subcommand, flag, and exit code.
- [Admin authentication](09-admin-authentication.md) — how the admin key is created, rotated, and disabled.
- [Client API reference](../reference/00-client-api.md) — full session and message endpoints.
- [Wire Protocol](../reference/01-wire-protocol.md) and [Session Basics](../examples/session-basics.md) — drive a session over raw HTTP/curl.
- [Profiles](../profiles.md) — provider config, env var references, fallbacks, MCP wiring.
- [Message types](../reference/04-message-types.md) — every `event_type` value and its payload.
- [MCP Servers](02-mcp-servers.md) — connect real MCP tools to a profile.
- [Event Streaming](06-event-streaming.md) — live progress notifications and observer subscriptions.
- [Multi-Client Sessions](07-multi-client-sessions.md) — join a session as a second driver.
- [Harness Launchers](11-harness-launchers.md) — cron, HTTP, and webapp triggers for your agents.
</content>
