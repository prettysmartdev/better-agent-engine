# Work Item: Enhancement

Title: extremely simple first run experience
Issue: issuelink

## Summary:
- There needs to be a way for someone to run BAE with MAX and their client harness of choice using the absolute minimal number of commands/steps. It's currently way too complicated and time consuming to get started.

baectl should be enhanced/expanded to make it dead simple to get BAE (with or without MAX) running, configured for the client harness (and toolset) the user wants to run, and get their client harness of choice packaged with their launcher of choice and running locally with 2-3 baectl commands.

Propose a way to make this work, and ensure that the flow includes a `--dev` flag for all commands so that a developer of BAE can use locally built versions of binaries/container images/etc without needing to pull remote images etc.

The preferable workflow would look something like this:

baectl setup <...> (gets bae with or without MAX installed, running, and configured with a provider connected - ONLY ONCE)

baectl build <some local client harness / launcher combo> (prepares the fully packaged harness/launcher)

baectl ready <built thing's identifier> (ensures that profiles/auth/permissions/tool lists/etc are compatible with the thing you want to run, identifies any issues, suggests how to fix/reconfigure) 

baectl run <built thing's identifer> (launches the built harness/launcher combo and shows the user where/how to run their agent)

This will involve some involved steps/wizards/etc, but it is imperative that the developer experience of setting up BAE, configuring it, and running the "inner loop" of developing an agent is as smooth and convenient as possible. 

**Builds on already-shipped work.** `baectl setup` (`aspec/work-items/0012-baectl-quickstart.md`, `baectl/src/setup.rs`) already gets a `baesrv`/`bae-max` instance running with a provider connected, in one command, with a working `--dev` flag. `launchers/` (`aspec/work-items/0014-harness-launchers.md`) already defines the "package a harness + config into a `FROM bae-launcher-*` image" pattern (`examples/launchers/{schedule,api,webapp}/`). This work item does not replace either — it adds the three missing verbs (`build`, `ready`, `run`) that turn "a running server" plus "some harness code" into "a running, wired-up agent," and a small new local manifest convention (`bae-harness.toml`) that lets any harness — a bundled example or a user's own — describe what it needs so those verbs can act on it without guessing.

## User Stories

### User Story 1:
As a: New bae User

I want to:
run `baectl setup` once, then `baectl build reference-assistant` and `baectl run <id>`, and see the example agent answer a question end to end

So I can:
go from a clean checkout to a working agent conversation in three commands total, with no manual profile/key/env-var wiring and no need to read the admin API docs first

### User Story 2:
As a: BAE Contributor iterating on client-rust/launchers/server locally

I want to:
pass `--dev` to `setup`, `build`, `ready`, and `run` and know every one of them uses my locally built images/binaries — never a published GHCR tag or crates.io/npm release — for the whole loop

So I can:
verify an unreleased change to a client SDK or a launcher base image actually works end to end, without a stray command in the pipeline silently falling back to a stale published artifact

### User Story 3:
As an: Agent Developer packaging my own custom harness

I want to:
drop a `bae-harness.toml` next to my harness code, run `baectl build --harness-dir ./my-agent --launcher webapp`, have `baectl ready <id>` tell me exactly which MCP server I still need to register (and how) rather than fail deep inside a container, then `baectl run <id>` and get a clickable URL

So I can:
package and run my own harness the same way I'd run a bundled example, using bae's own compatibility checks to catch a misconfigured profile before I ever launch a container

## Implementation Details:

> **Amended by `aspec/work-items/0018-compaction-and-first-run-hardening.md`
> (§B/§C).** In particular: §3's "`ready` exits 1 and does not write
> `resolved.json`" below is superseded by 0018 §B7 — a check that `run`/
> `ready --fix` resolves automatically (a widenable profile, a creatable key)
> now reports `⚠ … — will be fixed by run` and, when every failing check is
> auto-fixable, `ready` exits **3**, not 1; a genuinely blocking check (#1,
> #3, #5, or an irreconcilable #2) still exits 1. `bae-harness.toml` (§1)
> gains optional `prepare`, `[harness.requires].sandboxes`, and
> `[harness.container]` fields, and every `bae-harness.toml` struct is now
> `#[serde(deny_unknown_fields)]` (0018 §B9). `setup` (already shipped by
> `aspec/work-items/0012-baectl-quickstart.md`, referenced in this file's
> "Builds on already-shipped work" note) gains a non-interactive `--yes`/`-y`
> flag (0018 §C1). See `docs/reference/03-baectl.md` for the current, exact
> behavior of all three.
>
> **Housekeeping note (0018 §B12):** the MCP client's spawn deadline in
> `server/src/engine/mcp.rs` is **15 seconds**, not the 4 seconds this work
> item's design assumed — that change shipped alongside this work item's own
> commit without being called out in this spec. Recorded here for the
> record; it is not itself part of this work item's scope.

### 1. New local manifest: `bae-harness.toml`

Every harness `build` can act on — a bundled example or a user's own project — declares itself with a `bae-harness.toml` file at its root, the same "small, explicit, TOML" convention `bae-schedules.toml`/`bae-api.toml`/`bae-app.toml` already establish (`aspec/work-items/0014-harness-launchers.md` section A):

```toml
[harness]
name    = "reference-assistant"
sdk     = "rust"                 # "rust" | "typescript" | "python"
run     = "cargo run --release --example reference-assistant"   # used by --launcher local only — runs on the host
working_dir = "."                # relative to this file's directory

[harness.requires]
allowed_tools = ["get_current_time"]
mcp_servers   = []
env           = []               # extra required env vars beyond BAE_SERVER_URL/BAE_CLIENT_KEY/the provider's auth-token var

# Optional — omit entirely for a harness that only supports `--launcher local`
# (e.g. issue-triage's two-phase, non-single-prompt control loop). Everything
# in this section is used only when packaging with --launcher schedule/api/webapp —
# see "Building the harness for container packaging" below for why the build
# step itself always runs inside Docker, never on the host.
[harness.launcher]
dockerfile        = "Dockerfile.build"   # optional — see below; omit to use baectl's generated per-sdk default
target            = "build"              # optional `docker build --target`, if `dockerfile` is itself multi-stage
binary_path       = "/build/target/release/reference-assistant"   # the built artifact's path *inside* the image `dockerfile` produces
prompt_env        = "AGENT_PROMPT"       # env var the packaged harness reads its triggered prompt from
default_schedule  = "0 0 3 * * *"        # only used when packaged with --launcher schedule
```

`baectl build`/`ready`/`run` are the only consumers; the server never reads this file. This work item adds one to each of the six bundled examples (`client-{rust,typescript,python}/examples/{issue-triage,reference-assistant}/bae-harness.toml`) and documents the schema (`docs/reference/07-harness-manifest.md`) so a fully external harness — inside this repo or not — can opt in with no other changes. `issue-triage`'s manifest omits `[harness.launcher]` (its two-phase list-then-per-issue loop has no single "prompt" to template); `--launcher api`/`webapp`/`schedule` against it is a usage error (exit 2) naming the missing section. `reference-assistant`'s three SDK implementations each gain a small, additive change: read an optional `AGENT_PROMPT` env var to override the example's hardcoded user message (default behavior unchanged when unset) — the minimum needed to make packaging it behind an HTTP trigger or chat UI actually demonstrate something.

**Building the harness for container packaging always happens inside Docker, never on the host.** A host-compiled binary (whatever OS/arch/libc the developer's machine happens to be) cannot be reliably `COPY`'d into a `bae-launcher-*` base image — the base images are Debian-based Linux (`Dockerfile.launcher-*`, matching the root `Dockerfile`'s runtime base), so a macOS binary never runs there at all, and even a Linux-built binary can mismatch glibc/musl or CPU architecture depending on how and where it was built. `binary_path` in `[harness.launcher]` is deliberately a path *inside a Docker image*, never a host filesystem path, and `target`/`dockerfile` name a **Docker build stage**, not a host toolchain invocation — see §2 for exactly how `build` uses them.

### 2. `baectl build <harness>` — new subcommand

```
baectl build <harness> [--sdk rust|typescript|python] [--harness-dir <path>]
                        [--launcher local|schedule|api|webapp] [--id <id>]
                        [--dev] [--dir <path>]
```

Like `setup`, `build` (and `ready`/`run` below) is **host-invoked**, not `docker exec`-invoked — it needs `cargo`/`npm`/`uv`/`docker` and, for a bundled example, the repo checkout itself. `aspec/uxui/cli.md`'s "baectl" section already carries a `setup`-shaped exception to the "always run inside the container" framing; this work item widens that exception to `build`/`ready`/`run` explicitly.

- **Harness resolution**: `<harness>` is either a bundled example name (`issue-triage`|`reference-assistant`), resolved to `<sdk-dir>/examples/<harness>/` under `--sdk`'s SDK directory (default `rust`) relative to `--dir` (which must therefore be, or be under, a repo checkout for a bundled harness — a clear error if `client-{rust,typescript,python}/` isn't found), or, with `--harness-dir <path>`, any directory containing its own `bae-harness.toml` — no repo checkout required for this path at all.
- **`--launcher local` (default)**: runs `harness.run`'s implied build step (if the sdk needs one — see below) as a subprocess **on the host**, streaming output; writes `<dir>/.baectl/builds/<id>/manifest.json` (`kind: "local"`, absolute `harness_dir`, `run_command`, `working_dir`, `requires`, `created_at`). No Docker image involved, and building on the host is correct here specifically because `run` will also *execute* on that same host — there is no cross-environment gap to worry about. (`cargo run`/`npm run start`/the sdk's interpreter invocation compiles/prepares lazily on first `run` rather than needing a separate host build step up front for most sdks; where one is needed — e.g. a TypeScript harness's `tsc` step — it's driven by `harness.run` itself, matching how the bundled examples already build via `cargo run --release`/`npm start`, not a `baectl`-invented build command.)
- **`--launcher schedule|api|webapp` — the harness is compiled inside Docker, never on the host:**
  1. Resolve the harness's **build Dockerfile**: `[harness.launcher].dockerfile` if the manifest sets it (path relative to the harness dir); otherwise `baectl` synthesizes one from `harness.sdk` and writes it to `<dir>/.baectl/builds/<id>/Dockerfile.build.generated` for inspection — e.g. for `sdk = "rust"`, `FROM rust:1-bookworm AS build` + `WORKDIR /build` + `COPY . .` + `cargo build --release --example <name>` (mirroring the root `Dockerfile`'s own Rust build-stage conventions), landing the binary at the generated default `binary_path` (`/build/target/release/<name>`); analogous `node:22-bookworm`/`npm ci && npm run build` and `python:3.12-bookworm`/dependency-install defaults for `typescript`/`python`. This default covers all six bundled examples with **zero extra files** — `[harness.launcher].dockerfile` exists purely as an escape hatch for a harness with unusual build needs (private registries, native deps, a musl target, etc.), not a requirement every harness author must satisfy.
  2. `docker build -f <dockerfile> [--target <target>] -t <id>-harness-build <harness_dir>` (streamed) — this is the actual compile step, and it now runs inside Docker's own Linux build environment regardless of the developer's host OS/arch, producing a binary that is guaranteed compatible with the Linux base image the next step extends.
  3. Generate `<dir>/.baectl/builds/<id>/Dockerfile`:
     ```dockerfile
     FROM <base_image_tag>
     COPY --from=<id>-harness-build:latest <binary_path> /usr/local/bin/<harness-name>
     COPY bae-{schedules,api,app}.toml /etc/bae/bae-{schedules,api,app}.toml
     ```
     `COPY --from=<image>` names the harness-build image built in step 2 directly (Docker supports copying from any image reference, not only a same-file stage) — no source code, host toolchain, or host-built artifact ever enters this second build's context at all. `base_image_tag` follows `--dev` exactly as before: the local `make image-launcher-<type>` tag (`better-agent-engine:launcher-<type>`, `Makefile`'s `LAUNCHER_*_IMAGE` vars) or the published `ghcr.io/prettysmartdev/better-agent-engine:launcher-<type>` tag, mirroring `setup.rs`'s `DEV_STANDARD`/`PUBLISHED_STANDARD` selection.
  4. Also generate the matching `bae-{schedules,api,app}.toml` under the same directory, in the exact shape `examples/launchers/{schedule,api,webapp}/` already establishes — one `[[agents]]` entry named after the harness, `command = "/usr/local/bin/<harness-name>"`, and (for `api`/`webapp`) a `request_schema` with one required string property whose name is `harness.launcher.prompt_env`'s field and an `env_template` entry mapping it to that same env var.
  5. `docker build -t <local_tag> <dir>/.baectl/builds/<id>/` (or `container build`, matching the engine `setup` used, detected from `<dir>`'s existing compose file vs. `bae-setup.sh`), then writes `manifest.json` (`kind: "container"`, `image_tag`, `harness_build_image`, `launcher_type`, `port`, `requires`).
- **Id**: default `<harness-name>-<sdk>-<launcher>` (e.g. `reference-assistant-rust-local`), collision-suffixed (`-2`, `-3`, …) only when `--id` is omitted and a different harness/sdk/launcher combination already used the bare name; `--id` always overrides. Re-running `build` with the same id overwrites that build's files/image(s) in place (builds are disposable local artifacts, unlike `setup`'s credential-bearing files — no backup-then-overwrite ceremony needed here, just a printed "rebuilt `<id>`").
- Prints the resolved `<id>` and the next command (`baectl ready <id>` or `baectl run <id>`).

### 3. `baectl ready <id>` — new subcommand

```
baectl ready <id> [--fix] [--dir <path>]
```

Loads `<dir>/.baectl/builds/<id>/manifest.json` (missing → error pointing at `baectl build`). Reaches the admin API exactly the way `setup`'s own launch step already does — `docker compose exec <service> baectl ...` / `container exec <name> baectl ...` subprocess calls, never a direct host connection to the loopback-only admin port (`baectl/src/setup.rs`'s existing helper for this is extracted into a small shared module both files call — see Codebase Integration). Runs six checks, each printed with ✓/✗:

1. **Server reachable** — the exec'd `baectl list profiles --json` succeeds; failure also covers "no `setup` was ever run in `--dir`."
2. **Compatible profile exists** — among `list profiles`, one whose `allowed_tools` ⊇ `requires.allowed_tools` and `mcp_servers` ⊇ `requires.mcp_servers`. ✗ prints the exact `baectl create profile`/`update profile` command that would satisfy it.
3. **Required MCP servers are actually registered** — parses `<dir>/bae-config.toml` (reusing `setup.rs`'s existing read-back parser from its idempotent Edit path) for a `[[mcp.servers]]` entry per name in `requires.mcp_servers`. This is a distinct failure from #2: a profile can satisfy #2 today and still break tomorrow if the underlying registry entry is missing, and a name missing from the registry entirely can't be fixed by any profile edit. ✗ prints the `bae-config.toml` snippet to add plus "then restart: `docker compose restart`" (or the apple-script equivalent) — **never auto-edited**, since it requires a server restart, which `ready`/`--fix` will not take unattended.
4. **Client key exists** for the resolved profile — among `list keys`, one bound to it. ✗ offers to create one.
5. **Required env vars are resolvable** — for `kind: "local"`, checked against the *host* process environment (what `run` will inherit); for `kind: "container"`, checked against `<dir>/.env` or the host environment (what `run` will pass through — see §4). Covers `requires.env` plus the resolved profile's provider auth-token env var (looked up via `primary_provider` in `bae-config.toml`). ✗ lists exactly which vars, and where each needs to be set.
6. **MAX reachability** (informational, never ✗) — if `setup`'s image variant was `max`, prints its URL.

`--fix` applies only to #2/#4 (pure admin-API creates — safe, reversible, and, for #2, **additive**: if a profile already exists but is missing only some tools/servers, the fix reads its current full body and `update profile`s with the union, never dropping names another harness may depend on). It always prompts `Apply the N safe fix(es) above? [y/N]` before mutating anything, even when stdin isn't a TTY (unlike `setup`'s wizard defaults, a state-mutating fix is not something to silently skip past) — `run`'s own auto-fix behavior (§4) is the non-interactive path, not `ready --fix`. Items #3 and #5 are always print-only.

On full success, writes `<dir>/.baectl/builds/<id>/resolved.json` (mode `0600`, like `.env`): `{profile_id, profile_name, key_id, client_key_plaintext, server_url, max_url}`. `client_key_plaintext` is populated **only** when `ready` itself just created the key (the admin API shows it exactly once, `aspec/work-items/0004-baectl-cli.md`'s "shown once, never logged" rule) — this file is the one place that plaintext is persisted locally, exactly parallel to how `.env` persists provider secrets, and is what lets `run` avoid ever needing its own admin-API round trip. If any check remains unresolved, `ready` exits 1 and does not write (or leaves stale) `resolved.json`, so its mere presence is not by itself a green light — `run` re-validates the ids inside it still exist (§4) rather than trusting the file blindly.

### 4. `baectl run <id>` — new subcommand

```
baectl run <id> [--dir <path>] [--no-ready] [--server-url <url>]
```

By default, `run` first performs the same six checks as `ready`, re-validating (not just trusting) any existing `resolved.json`, and **auto-applies the #2/#4 safe fixes with no prompt** — this, not `ready`, is the fully non-interactive fast path that keeps the "2–3 commands" promise (`setup` once, then `build` + `run`). It prints exactly what it fixed. An unfixable issue (#3/#5) aborts with the same guidance `ready` prints, plus a pointer to run `baectl ready <id>` for the full report. `--no-ready` skips straight to launch using the existing `resolved.json` verbatim (fails loudly if it's absent).

- **`kind: "local"`**: exports `BAE_SERVER_URL`/`BAE_CLIENT_KEY` from `resolved.json`, leaves every other required env var exactly as the host already has it set (never overwritten), `cd`s to `harness_dir`/`working_dir`, and runs `harness.run` in the foreground via `std::process::Command::status()` (inherited stdio, so the child's own output streams live and Ctrl-C reaches it directly) — `run` prints a header first (harness name, resolved profile/key, server URL, MAX URL if applicable, "Ctrl-C to stop") and propagates the child's exit code as its own.
- **`kind: "container"`**: resolves `requires.env` values from the host environment, prompting for any still missing (mirroring `setup`'s own secret-prompt step) and writing them to `<dir>/.baectl/builds/<id>/harness.env` (`0600`); `docker rm -f <id>` any prior container of the same name (idempotent re-run, mirroring `bae-setup.sh`'s existing stop/rm pattern) then `docker run -d --name <id> --env-file <dir>/.env --env-file harness.env --env BAE_SERVER_URL=<resolved> --env BAE_CLIENT_KEY=<resolved> [-p <port>:<port> for api/webapp] <image_tag>` (or the Apple `container run` equivalent). This is a **detached**, long-running container (schedule/api/webapp are servers, not one-shot commands) — unlike local mode, `run` does not block; it starts the container and prints the "where/how" summary: webapp → the clickable URL; api → a ready-to-copy `curl --no-buffer -X POST .../agents/<name>/trigger` command; schedule → its cron expression and `docker logs -f <id>` to tail. `--server-url` overrides the auto-derived value for reaching `baesrv` from the launcher's own container network (see Edge Cases).

### 5. `--dev` on `ready`/`run`

The launcher/example artifact a `ready`/`run` invocation acts on was already fixed at `build` time — there is nothing left for `--dev` to switch. Both commands still accept it (fulfilling the summary's "all commands" requirement) purely as a **consistency guard**: if the target's `manifest.json` records it was built with `--dev` and `ready`/`run` is invoked without it (or vice versa), print a warning naming the mismatch rather than silently proceeding — catching the likely mistake of mixing a locally built artifact with published-artifact assumptions (or the reverse) without pretending the flag does anything functionally on these two commands.

## Edge Case Considerations:
- **No prior `baectl setup` in `--dir`.** `ready`/`run`'s check #1 fails with a message pointing at `baectl setup`, not a raw connection error.
- **`--harness-dir` outside any bae repo checkout.** Must work for `build --launcher local` and, under `--dev` off, `--launcher schedule/api/webapp` (they only need `docker` and the published launcher base images) — a bundled-example harness is the only path that requires a repo checkout, and only because the example source itself lives there.
- **Missing or malformed `bae-harness.toml`.** Fatal, exit 2, naming the missing/invalid field — same posture WI 0014 established for malformed `bae-schedules.toml`/`bae-api.toml`.
- **Host OS/arch/libc never matching the target container.** This is the reason container-mode `build` compiles inside Docker at all (§2) rather than `COPY`ing a host-built binary — a developer on macOS or an arch/libc that doesn't match the Debian-based launcher images would otherwise get a container that fails at startup with an exec-format or missing-library error, not a build-time error. Building inside Docker (whether via a harness-supplied `dockerfile` or `baectl`'s generated per-sdk default) removes this class of failure entirely for OS/libc; it does **not** by itself solve cross-*architecture* deployment (building on an arm64 host still produces an arm64 image by default) — that's a real, separately-scoped gap, called out explicitly rather than silently assumed away: a future `--platform`/buildx extension is the natural fix, not built here.
- **A harness's own `[harness.launcher].dockerfile` doesn't produce `binary_path`, or `target` doesn't exist in it.** Surfaced as the underlying `docker build` failure (missing `COPY --from` source path, or an unknown `--target` name) — `baectl` does not pre-validate the harness's Dockerfile beyond confirming the file exists, since it has no way to know its stage names without invoking Docker itself.
- **`baectl`'s generated default build Dockerfile doesn't fit a harness's needs** (extra native deps, a private package registry, a non-default toolchain version). The escape hatch is authoring an explicit `[harness.launcher].dockerfile` — the generated default is a convenience for the common case (proven by all six bundled examples using it with no override), never the only path.
- **Two builds of the same harness name under different SDKs or launchers in one `--dir`.** The default id includes both sdk and launcher type specifically to avoid a silent collision; an explicit `--id` is always available and takes precedence.
- **A profile satisfies `allowed_tools`/`mcp_servers` but its provider's auth-token env var isn't set.** Still caught — check #5 resolves the provider env var via the profile's `primary_provider` lookup in `bae-config.toml`, not just the harness's own declared `requires.env`.
- **`requires.mcp_servers` names a server no profile has, versus one no `bae-config.toml` registry entry defines at all.** Kept as two distinct checks (#2 vs #3) with two distinct fixes (an admin-API update vs. a file edit + restart) — conflating them would either under-report (claiming success when the registry itself is missing the entry) or over-report (asking for a restart when only the profile needed updating).
- **`ready --fix`/`run`'s auto-fix widening an existing profile's `allowed_tools`/`mcp_servers`.** Always additive — read-then-union, never a destructive full replacement that could silently revoke access another harness's key depends on, even though the underlying `update profile` API call is itself a full replacement (`aspec/work-items/0004-baectl-cli.md`).
- **A freshly created client key's plaintext.** Persisted exactly once, into `resolved.json` at `0600` — the one deliberate exception to "a client key's plaintext is never written to disk by baectl," justified the same way `.env` already is: it is the only way `run` can act non-interactively without a second admin-API round trip per invocation.
- **`resolved.json` referencing a profile/key since deleted out-of-band** (mirrors WI 0012's own "profile/key deleted since last setup run" edge case). `ready`/`run` re-validate the referenced ids still exist on every invocation rather than trusting a cached file — a stale reference re-enters the fix flow instead of failing deep inside a launched container.
- **Reaching `baesrv` from a `run`-launched standalone container.** A `build`-produced container image is a portable artifact, not joined to `setup`'s own compose network — it must reach `baesrv` via the **published host port**, whose exact reachable address differs by engine/OS (`host.docker.internal` on Docker Desktop, `172.17.0.1` or `--network host` on Linux, and Apple's `container` CLI has its own model again). `run` best-effort-detects this and exposes `--server-url` as an explicit override for when detection guesses wrong — documented plainly as a known rough edge, not silently papered over.
- **Re-running `run` for an already-running `kind: "container"` id.** `docker run --name <id>` would collide; `run` `docker rm -f`s any prior same-named container first, mirroring `bae-setup.sh`'s existing idempotent stop/rm pattern, rather than surfacing an opaque "name already in use" error.
- **`run` invoked non-interactively (CI) for a `kind: "container"` harness with an unset `requires.env` value.** No TTY to prompt — fails loudly, naming the missing variable, rather than blocking on a prompt that will never be answered; `--no-ready` plus pre-populating `harness.env` is the documented scriptable path.
- **`issue-triage` (or any harness with no `[harness.launcher]` section) built with `--launcher api/webapp/schedule`.** Usage error, exit 2, naming the missing section — `--launcher local` remains fully supported for such a harness.
- **`--dev` mismatch between `build` and a later `ready`/`run`.** Warning, not a hard failure (see §5) — a heuristic catching a likely mistake, not a correctness guarantee baectl can fully verify.

## Test Considerations:
- **Unit — `bae-harness.toml` parsing**: valid fixtures for all three `sdk` values; a missing required field and malformed TOML each produce the documented exit-2 error naming the field; a harness with no `[harness.launcher]` section parses to `None` there without error.
- **Unit — id derivation**: default `<name>-<sdk>-<launcher>` shape, collision suffixing only when `--id` is omitted, `--id` always wins.
- **Unit — manifest/resolved round-trip**: `manifest.json`/`resolved.json` (de)serialize correctly for both `kind` variants; `resolved.json` omits `client_key_plaintext` when no key was freshly created.
- **Unit — readiness checks, pure logic**: profile allowed_tools/mcp_servers superset matching against fixture profile lists; `bae-config.toml` registry-presence parsing distinguishing "no profile has it" from "no registry entry exists" fixtures; missing-env-var detection for both `local` and `container` kinds against fixture environments/`.env` files — all driven through fixed input rather than a live server, matching `setup.rs`'s existing fake-stdin/fake-command unit-test style.
- **Unit — additive profile fix**: given an existing profile's current `allowed_tools`/`mcp_servers`, the computed `update profile` body is the union with `requires`, never a subset of what was already there.
- **Unit — container-mode generation**: for each of `schedule`/`api`/`webapp`, the generated launcher Dockerfile/`bae-{schedules,api,app}.toml` matches `examples/launchers/*`'s shape, uses the correct dev-vs-published base image tag per `--dev`, and (api/webapp) the generated `request_schema`/`env_template` round-trips through the launcher's own config parser (mirroring WI 0012's `BaeConfig`-deserializer round-trip-test posture, here against `launcher-core`'s config types).
- **Unit — default build-Dockerfile generation**: for each `sdk` value, the synthesized `Dockerfile.build.generated` names a base image and build command consistent with that sdk's toolchain, and is skipped entirely (the harness's own `dockerfile` used verbatim) whenever `[harness.launcher].dockerfile` is set.
- **Integration (engine-gated, skippable per the existing `check-static`/`image-smoke` conditional-on-engine posture)**: full `setup --dev` → `build reference-assistant --launcher local --dev` → `ready` → `run` lifecycle against real local images; assert the harness completes and prints its final answer; assert a second `run` against the same, now-fully-provisioned target makes zero further admin-API mutations.
- **Integration (engine-gated)**: `build --launcher api --dev` → `ready --fix` → `run` → drive the printed curl command against the now-running container → assert a streamed response; assert a second `run` cleanly replaces the first container (no name collision).
- **Regression — container build never touches the host toolchain**: run `build --launcher api` (bundled `reference-assistant`, default generated Dockerfile) in an environment with no `cargo`/`rustc` on `PATH` at all — it must still succeed, since the actual compile happens inside the `docker build` in step 2 of §2, not on the host. This is the direct regression guard for the host/target OS-arch mismatch this design exists to avoid.
- **Regression — local state hygiene**: `.baectl/` is covered by `.gitignore`; `resolved.json`/`harness.env` are written at `0600`.
- All new tests remain offline by default (`make test-baectl`) except the explicitly engine-gated integration tests above, matching the component's existing test posture.

## Codebase Integration:
- New module `baectl/src/harness.rs` for `build`/`ready`/`run` logic and the `bae-harness.toml`/`manifest.json`/`resolved.json` types; `cli.rs` gains `Build`/`Ready`/`Run` `Command` variants alongside the existing `Setup`, following its established `--dev`/`--dir` flag conventions exactly.
- Extract the pieces `setup.rs` already implements and this work item needs again — the `docker compose exec`/`container exec` subprocess wrapper, engine/`--apple` detection, and `${VAR}`-secret interactive prompting — into a small shared internal module (e.g. `baectl/src/engine.rs`) rather than duplicating them, since `setup.rs`'s launch step is doing exactly this work today.
- Add `bae-harness.toml` to each of `client-{rust,typescript,python}/examples/{issue-triage,reference-assistant}/`; add the optional `AGENT_PROMPT` env-var override to all three `reference-assistant` implementations (additive, default behavior unchanged). None of the six get a committed `Dockerfile.build` — they deliberately exercise `baectl`'s generated per-sdk default build Dockerfile (§2), which doubles as that default's own regression coverage; a harness author who needs a custom one has the `[harness.launcher].dockerfile` override, documented in `docs/reference/07-harness-manifest.md`.
- No new Makefile targets required beyond WI 0014's existing `image-launcher-*` — `build --dev` documents them as a prerequisite exactly the way `setup --dev` already documents needing `make image`/`make image-max` first.
- `.gitignore`: add `.baectl/` (holds a plaintext client key in `resolved.json` and possibly harness secrets in `harness.env` — same sensitivity class as the existing `.env` entry).
- Docs: extend `docs/reference/03-baectl.md` with `build`/`ready`/`run` sections; add `docs/reference/07-harness-manifest.md` for the `bae-harness.toml` schema; update `docs/guides/00-quickstart.md` to lead with `setup` → `build` → `run` as the fastest path, keeping the existing manual walkthrough documented alongside it, not replaced; update `aspec/uxui/cli.md`'s "baectl" section (command list, and the widened "runs on the host" exception now covering four commands instead of one); update `aspec/architecture/design.md`'s Component 5 (`baectl`) scope description to mention `build`/`ready`/`run`.
- Verify `make build`/`test`/`lint`/`fmt` (baectl component) and `make test-baectl` pass; verify the new engine-gated integration tests are properly skippable in environments without a container engine, matching `check-static`/`image-smoke`'s existing conditional posture.
