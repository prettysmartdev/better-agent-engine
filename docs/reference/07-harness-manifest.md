# Harness Manifest Reference (`bae-harness.toml`)

The local, per-harness manifest an author drops next to their harness code so
[`baectl build`, `baectl ready`, and `baectl run`](03-baectl.md#baectl-build)
can act on it without guessing — a bundled example
(`client-{rust,typescript,python}/examples/{issue-triage,reference-assistant}/`)
or a fully external harness, inside this repo or not.

**`baectl` is its only consumer — `baesrv` never reads this file.** It plays
the same "small, explicit, TOML" role for a *harness* that
`bae-schedules.toml`/`bae-api.toml`/`bae-app.toml` play for a *launcher config*
(see the [Harness Launchers reference](06-launchers.md)), but it is read
entirely on the host, before any container exists.

A missing or malformed `bae-harness.toml` is a fatal **usage error, exit
`2`**, whose message names the offending field or file:

```
baectl: could not read bae-harness.toml at examples/foo/bae-harness.toml: No such file or directory (os error 2)
baectl: invalid bae-harness.toml: missing field `run`
baectl: invalid bae-harness.toml: unknown variant `golang`, expected one of `rust`, `typescript`, `python`
```

Every `bae-harness.toml` struct (`[harness]`, `[harness.requires]`,
`[harness.launcher]`, `[harness.container]`) is strict —
`#[serde(deny_unknown_fields)]` — so a typo such as `allowed_tool` (missing
the trailing `s`) is this same exit-`2` usage error naming the unknown key,
never a silently ignored requirement, one line, with the offending line
number and its source text appended:

```
baectl: invalid bae-harness.toml: unknown field `allowed_tool`, expected one of `allowed_tools`, `mcp_servers`, `env`, `sandboxes` (line 6: `allowed_tool = ["x"]`)
```

(`manifest.json`/`resolved.json`, the files `baectl` itself writes, stay
lenient for forward compatibility — this strictness is only for the
hand-authored `bae-harness.toml`.)

**`name` and `--id` must be valid Docker tag/container-name components** —
lowercase `^[a-z0-9][a-z0-9_.-]*$`, at most 128 characters — since both end
up in image tags (`<id>:latest`) and container names (`--name <id>`):

```
baectl: invalid harness name "My Agent": must match [a-z0-9][a-z0-9_.-]* (Docker tag/container-name rules)
baectl: invalid --id "Bad/Id": must match [a-z0-9][a-z0-9_.-]* (Docker tag/container-name rules)
```

`--id` is only named in the message when `--id` was actually passed on the
command line; a derived `<name>-<sdk>-<launcher>` id that ends up too long
gets its own message naming the derivation:
`` invalid build id "…" derived from the harness name: … — shorten the harness name or pass --id ``.
An over-length name or id appends `, at most 128 characters` to the same
message.

---

## Schema

The file has one top-level table, `[harness]`.

### `[harness]`

| Field | Type | Required | Default | Consumed by |
|---|---|---|---|---|
| `name` | string | **yes** | — | `build` (the id stem, the packaged binary name, the `[[agents]]` entry name in generated launcher config); `ready`/`run` (display, key naming). Must be a valid Docker tag/container-name component — see above. |
| `sdk` | string enum: `"rust"` \| `"typescript"` \| `"python"` | **yes** | — | `build` (selects the default build toolchain for `--launcher schedule/api/webapp`; recorded in the build id and `manifest.json`). |
| `run` | string | **yes** | — | `build --launcher local` records it verbatim; `run` executes it as `sh -c "<run>"`. **Required even for a harness that only supports container launchers** — the field has no default in the parser regardless of which `--launcher` you actually intend to use. |
| `working_dir` | string | no | `"."` | `build`/`run`, resolved relative to this file's own directory — both for `--launcher local`'s `cd` before running, and (when `baectl` synthesizes the build Dockerfile) as the generated Dockerfile's build context, where `build` also writes a `.dockerignore` if none exists (see [`baectl build` — harness build stage](03-baectl.md#baectl-build)). |
| `prepare` | string | no | none | `run --launcher local` only (see below) — a command executed before `run`, only when needed. Never used for a container launcher. |
| `[harness.requires]` | table | no | all fields empty | `ready`/`run`'s six compatibility checks. May be omitted entirely for a harness with no requirements. |
| `[harness.launcher]` | table | no | absent (`None`) | `build`'s container-packaging step. **Omitting this table restricts the harness to `--launcher local`** — see below. |
| `[harness.container]` | table | no | absent (`None`) | `build`'s *generated* per-SDK Dockerfile only — overrides its build command and entrypoint. See below. |

### `prepare` (local launcher only)

An optional shell command `baectl run` (`--launcher local` only) executes via
`sh -c` in `working_dir`, before `run`, and **only when needed** — e.g.
`prepare = "npm install"` for the bundled TypeScript examples, which have no
other install step. Never runs for a container launcher (its generated or
harness-supplied Dockerfile already installs dependencies at build time), and
never for Rust/Python examples, which prepare themselves (`cargo run`
compiles, `uv run` syncs the virtualenv).

"Needed" is decided by the command's own shape:

- A command starting with `npm ` or `npx `: needed when `node_modules/` is
  missing under `working_dir`, or `package-lock.json` is newer (by mtime)
  than `node_modules/`.
- Any other command: needed when `<dir>/.baectl/builds/<id>/prepared` is
  missing, or older (by mtime) than this `bae-harness.toml`.

The marker (or, for `npm`/`npx`, `node_modules/`'s own mtime) is
touched/updated after a successful run, so a `run` immediately after `build`
doesn't reinstall on every invocation. `run` prints
`` prepare: <cmd>   (in <workdir>) `` before running it; a non-zero exit
aborts `run` with the child's own exit code and
`` baectl: prepare command failed (exit N): <cmd> ``.

### `[harness.requires]`

| Field | Type | Required | Default | Consumed by |
|---|---|---|---|---|
| `allowed_tools` | array of string | no | `[]` | `ready` check #2 — the resolved profile's `allowed_tools` must be a superset. List every tool the harness declares in the session's `tools`, **including** a sandbox tool with a *local* target (for example the bundled examples' `run_shell_command`): the harness dispatches it itself, so the server checks it against `allowed_tools` like any client tool. A *remote* sandbox tool does not belong here — it is declared in `sandbox_tools`, which the server does not check against `allowed_tools`; list its image under `sandboxes` instead. |
| `mcp_servers` | array of string | no | `[]` | `ready` check #2 (profile's `mcp_servers` superset) **and** check #3 (each name must also exist in `bae-config.toml`'s `[[mcp.servers]]` — a distinct, always-print-only check, since a name missing from the registry can't be fixed by any profile edit). |
| `env` | array of string | no | `[]` | `ready`/`run` check #5, **in addition to** the resolved profile's own provider auth-token env var (which `ready`/`run` resolve automatically — do not list it here). Do not list `BAE_SERVER_URL`/`BAE_CLIENT_KEY` either; `run` sets both itself. |
| `sandboxes` | array of string | no | `[]` | `ready` check #2 — the resolved profile's `available_sandboxes` (see [Sandboxes](../guides/03-sandboxes.md)) must also be a superset. A harness that declares no sandbox image here still keeps whatever `available_sandboxes` the widened profile already had — `ready --fix`/`run` union, never replace, that list (see [`baectl create/update profile --available-sandbox`](03-baectl.md#baectl-create-profile)). The fix runs the **server image's** own `baectl` (via `docker compose exec`/`container exec`), so widening a profile that has, or needs, sandbox images requires a server image whose `baectl` supports `--available-sandbox`. Against an older image the in-container command fails with an `unexpected argument '--available-sandbox'` error; pull a current image (`docker compose pull`) and re-run. |

### `[harness.launcher]`

Present only for a harness that supports being packaged behind a trigger
(`--launcher schedule/api/webapp`). Omit the whole table for a harness that
only supports `--launcher local` — e.g. a two-phase list-then-per-item control
loop with no single "prompt" to template, like the bundled `issue-triage`
example. Requesting a container launcher for a harness with no
`[harness.launcher]` section is a usage error, exit `2`:

```
baectl: harness 'issue-triage' has no [harness.launcher] section; --launcher api requires it
```

| Field | Type | Required | Default | Consumed by |
|---|---|---|---|---|
| `dockerfile` | string | no | none — `build` synthesizes a per-SDK default (see below) | `build`, as the `-f` argument to the harness's build-stage `docker build`. Path is relative to the harness directory. |
| `target` | string | no | none | `build`, as `docker build`'s `--target`, only meaningful if `dockerfile` is itself multi-stage. |
| `binary_path` | string | conditionally — **required whenever `dockerfile` is set**; optional (and defaulted) when it isn't | a per-SDK default when `dockerfile` is omitted (see [`baectl build`](03-baectl.md#baectl-build)) | `build`'s generated launcher Dockerfile, as the `COPY --from=<harness-build image>` source path. |
| `prompt_env` | string | **yes, whenever `[harness.launcher]` is present at all** | — | `build`'s generated `bae-api.toml`/`bae-app.toml` (`request_schema`/`env_template` are keyed on this name). Required even if you only ever intend `--launcher schedule`, which doesn't itself use it — the struct has no default. |
| `default_schedule` | string (six-field cron expression) | no in the schema, but **`build` fails if it's absent and you request `--launcher schedule`** | none | `build`'s generated `bae-schedules.toml`. Validated at build time (after the harness build stage has already run), not at manifest-parse time. |

### `[harness.container]`

Optional overrides for **`baectl`'s own generated** per-SDK build Dockerfile
(the one it synthesizes when `[harness.launcher].dockerfile` is *not* set —
see [`baectl build`](03-baectl.md#baectl-build)). **Ignored, with a stderr
warning, whenever `[harness.launcher].dockerfile` is set** — a harness that
supplies its own build Dockerfile already owns these decisions.

| Field | Type | Required | Default | Consumed by |
|---|---|---|---|---|
| `build` | string | no | the per-SDK default (`cargo build --release --example <name>` / `npm ci && npm run build` / `pip install --no-cache-dir .`) | `build`'s generated Dockerfile's build `RUN` line. Must be a non-empty, single-line command. |
| `entrypoint` | string | no | the per-SDK default (see below) | `build`'s generated launcher shim/Dockerfile. Must be a non-empty, single-line command. |

Both fields are optional individually. A multi-line or blank value for
either is a usage error, exit `2`:
`` [harness.container].<field> must be a non-empty single-line command ``.

**`entrypoint`'s meaning is SDK-specific:**

- **Rust:** the image path of the already-built binary, replacing
  `/build/target/release/<name>` in the `COPY --from=<harness-build>`
  instruction.
- **TypeScript:** the command the generated shim `exec`s in place of
  `./node_modules/.bin/tsx examples/<name>/main.ts`, run from
  `/opt/bae-harness`.
- **Python:** the command the generated shim `exec`s in place of
  `/opt/bae-harness/venv/bin/python examples/<name>/main.py`, run from
  `/opt/bae-harness/app` with the venv's `bin/` prepended to `PATH`.

**A harness outside the bundled `examples/<name>/main.*` layout must set
these** (or supply its own `[harness.launcher].dockerfile`) — `baectl`'s
generated defaults assume the bundled examples' exact directory and entry-file
conventions, and have no way to discover an arbitrary harness's real build
command or entry point. See [Interpreted SDKs in a
container](#interpreted-sdks-in-a-container) below for what the TypeScript/
Python defaults actually do, so you know what you're overriding.

### `binary_path`/`dockerfile`/`target` are Docker-image concepts, never host paths

`binary_path` is deliberately a path **inside a Docker image** — never a path
on your machine — and `dockerfile`/`target` name a **Docker build stage**,
never a host toolchain invocation. This is not a stylistic choice: your host's
OS, CPU architecture, and libc essentially never match the launcher base
images, which are Debian-based Linux (`Dockerfile.launcher-*`, matching the
root `Dockerfile`'s own runtime base — see the
[Harness Launchers reference](06-launchers.md)). A binary compiled on macOS
can't run there at all, and even a Linux-built binary can mismatch glibc/musl
or CPU architecture depending on how and where it was built. That's why
container-mode `build` always compiles **inside Docker** (§"`--launcher
schedule|api|webapp`" in [`baectl build`](03-baectl.md#baectl-build)) rather
than `COPY`ing something you built on the host — whether via your own
`[harness.launcher].dockerfile` or `baectl`'s generated per-SDK default, the
compile step runs in Docker's own Linux build environment, producing a binary
that's guaranteed compatible with the base image the next step extends. This
removes the OS/libc mismatch class of failure entirely; it does **not** by
itself solve cross-*architecture* deployment — building on an arm64 host still
produces an arm64 image by default.

### The `[harness.launcher].dockerfile` escape hatch

`baectl`'s generated per-SDK default Dockerfile (documented in
[`baectl build`](03-baectl.md#baectl-build)) covers the common case — proven
by all six bundled examples, none of which set `dockerfile`/`target`, and
(for the reference assistants) none of which set `binary_path` either. Set
`[harness.launcher].dockerfile` only when your harness needs something the
generated default can't provide: extra native dependencies, a private package
registry, a non-default toolchain version, a musl target, and so on. It is an
escape hatch, not a requirement every harness author must satisfy.

---

## Worked examples

### Rust — `client-rust/examples/reference-assistant/bae-harness.toml`

The manifest actually shipped for the bundled Rust reference assistant. It
omits `dockerfile`/`target`/`binary_path` entirely to exercise `baectl`'s
generated Rust default (`rust:1-bookworm`, `cargo build --release --example
reference-assistant`, artifact at `/build/target/release/reference-assistant`):

```toml
[harness]
name = "reference-assistant"
sdk = "rust"
run = "cargo run --release --example reference-assistant"
working_dir = "../.."

[harness.requires]
allowed_tools = ["get_current_time", "read_file", "write_file", "explore_files", "run_shell_command"]
mcp_servers = []
env = []

[harness.launcher]
prompt_env = "AGENT_PROMPT"
default_schedule = "0 0 3 * * *"
```

`working_dir = "../.."` because the manifest lives two directories below the
`client-rust/` project root (`examples/reference-assistant/`), and `run`/the
generated Dockerfile's build context both need to be issued from that root.
`AGENT_PROMPT`, when set, overrides the example's hardcoded default prompt
(`main.rs` reads it before falling back to a CLI arg, then the literal
default `"What time is it?"`) — the minimum change needed to make packaging
this example behind an HTTP trigger or chat UI demonstrate something real.

### TypeScript — `client-typescript/examples/reference-assistant/bae-harness.toml`

```toml
[harness]
name = "reference-assistant"
sdk = "typescript"
run = "npm run example"
working_dir = "../.."
prepare = "npm install"

[harness.requires]
allowed_tools = ["get_current_time", "read_file", "write_file", "explore_files", "run_shell_command"]
mcp_servers = []
env = []

[harness.launcher]
prompt_env = "AGENT_PROMPT"
default_schedule = "0 0 3 * * *"
```

Same shape as the Rust manifest, `run` naming the package's existing `npm run
example` script. `prepare = "npm install"` is what lets `baectl run` work
from a clean checkout with no manual `npm install` step first — Rust/Python
prepare themselves, so their manifests omit it. Omitting `dockerfile`/`binary_path` here means `build`
generates a `node:22-bookworm` build stage (`npm ci && npm run build`) that
also stages the whole project — sources, build output and `node_modules` —
under `/opt/bae-harness` and writes an executable shim at
`/opt/bae-harness/bae-harness-entrypoint`, which becomes the default
`binary_path`. The generated launcher image installs Node 22 before copying
that tree in. See [interpreted SDKs in a container](#interpreted-sdks-in-a-container).

### Python — `client-python/examples/reference-assistant/bae-harness.toml`

```toml
[harness]
name = "reference-assistant"
sdk = "python"
run = "uv run python examples/reference-assistant/main.py"
working_dir = "../.."

[harness.requires]
allowed_tools = ["get_current_time", "read_file", "write_file", "explore_files", "run_shell_command"]
mcp_servers = []
env = []

[harness.launcher]
prompt_env = "AGENT_PROMPT"
default_schedule = "0 0 3 * * *"
```

Generates a `debian:bookworm-slim` build stage that installs `python3`/
`python3-venv`, `pip install`s the project into a virtualenv at
`/opt/bae-harness/venv`, stages the project under `/opt/bae-harness/app`, and
writes the same executable shim at `/opt/bae-harness/bae-harness-entrypoint`
(the default `binary_path`) when `dockerfile`/`binary_path` are omitted, as
here. Debian's own `python3` rather than the `python:3.12` image, so the
staged virtualenv's interpreter symlink still resolves against the `python3`
the generated launcher image installs. See
[interpreted SDKs in a container](#interpreted-sdks-in-a-container).

### A `--launcher local`-only harness — `client-rust/examples/issue-triage/bae-harness.toml`

`issue-triage` runs a two-phase, list-then-per-issue control loop with no
single "prompt" to template, so its manifest omits `[harness.launcher]`
entirely — `--launcher schedule/api/webapp` against it is the usage error
shown above; `--launcher local` (the default) remains fully supported:

```toml
[harness]
name = "issue-triage"
sdk = "rust"
run = "cargo run --release --example issue-triage"
working_dir = "../.."

[harness.requires]
allowed_tools = ["read_file", "write_file", "explore_files", "run_shell_command"]
mcp_servers = ["github"]
env = ["GITHUB_TOKEN", "TRIAGE_REPO", "TRIAGE_EXEC_MODE"]
```

The TypeScript and Python `issue-triage` manifests are the same shape, with
`sdk`/`run` adjusted per SDK (`npx tsx examples/issue-triage/main.ts` /
`uv run python examples/issue-triage/main.py`); the TypeScript one also adds
`prepare = "npm install"`, same as its `reference-assistant` sibling.

### Authoring your own build Dockerfile (the escape hatch)

A harness with unusual build needs supplies its own `dockerfile` and, since
`binary_path` then can't be derived, must set it explicitly:

```toml
[harness]
name = "reference-assistant"
sdk = "rust"
run = "cargo run --release --example reference-assistant"
working_dir = "."

[harness.requires]
allowed_tools = ["get_current_time"]
mcp_servers = []
env = []

[harness.launcher]
dockerfile = "Dockerfile.build"
target = "build"
binary_path = "/build/target/release/reference-assistant"
prompt_env = "AGENT_PROMPT"
default_schedule = "0 0 3 * * *"
```

`build` then runs
`docker build -f Dockerfile.build --target build -t <id>-harness-build:latest <harness-dir>`
instead of writing a generated Dockerfile — `baectl` does not pre-validate
that `Dockerfile.build` actually has a `build` target or that its `COPY
--from` source matches `binary_path`; a mismatch surfaces as the underlying
`docker build` failure (an unknown `--target` name, or a missing `COPY
--from` source path).

---

<a id="interpreted-sdks-in-a-container"></a>
## Interpreted SDKs in a container

The launcher base images (`bae-launcher-{schedule,api,webapp}`) are
`debian:bookworm-slim` with only `ca-certificates` installed, and the launcher
spawns the packaged harness as the plain command `/usr/local/bin/<name>`
running as the unprivileged `bae` user. A Rust harness satisfies that
directly: `cargo build --example` produces a self-contained ELF binary.

TypeScript and Python have no equivalent single artifact, so `baectl`'s
generated default splits the work across the two builds it already runs:

1. the **build stage** assembles everything the harness needs — the project
   tree plus `node_modules` (TypeScript) or a virtualenv (Python) — under
   `/opt/bae-harness`, and writes a small `sh` shim at
   `/opt/bae-harness/bae-harness-entrypoint` that `cd`s into that tree and
   `exec`s the entry module. That shim path is the generated `binary_path`;
2. the **launcher build** installs the matching interpreter (Node 22 via the
   same NodeSource pattern `Dockerfile.max` uses; Debian's `python3`), copies
   `/opt/bae-harness` in `--chown`ed to `bae`, then copies the shim to
   `/usr/local/bin/<name>` and drops back to `USER bae`.

Nothing extra is required of the harness author **for a harness that matches
the bundled `examples/<name>/main.*` layout**: all six bundled examples use
this default with no committed Dockerfile. A harness that doesn't match that
layout — a different entry-file path, a different build command, a monorepo
with its own script names — **does** need to say so, one of two ways:

- Set `[harness.container].build`/`entrypoint` (see above) to override just
  the generated Dockerfile's build command and entry point, keeping
  `baectl`'s interpreter provisioning.
- Or supply its own `[harness.launcher].dockerfile` (with an explicit
  `binary_path`) for full control. `baectl` injects **no** interpreter
  provisioning in that case — a harness that owns its build Dockerfile owns
  its runtime requirements too.

Two further consequences are worth knowing, regardless of which override you
use:

- The generated launcher build needs **network access** for its `apt-get`/
  NodeSource step, on top of the network the build stage already needs for
  dependency installation.
- A harness that needs a different interpreter version, a private package
  registry, or native dependencies still needs the `dockerfile` escape
  hatch above — `[harness.container]` only swaps the build/entry
  *commands*, not the base image or its installed packages.

`--launcher local` is unaffected for all three SDKs: it runs `harness.run` on
the host, against whatever toolchain is already installed there.

---

## See also

- [`baectl build`/`ready`/`run`](03-baectl.md#baectl-build) — the commands
  that read this file.
- [Harness Launchers reference](06-launchers.md) — the `bae-schedules.toml`/
  `bae-api.toml`/`bae-app.toml` schema `build` generates from
  `[harness.launcher]`, and the base images it packages against.
- [Quickstart](../guides/00-quickstart.md#fastest-path-three-commands) —
  `setup` → `build` → `run` end to end.
