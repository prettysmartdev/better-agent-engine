# Work Item: Feature

Title: baectl quickstart
Issue: issuelink

## Summary:
- This work item aims to create a 'baectl setup` command which is an interactive CLI wizard that helps the user set up their baesrv, first profile and key, etc.

The wizard should progressively ask questions via stdout/stdin Q&A such as:

- Max or Standard image?
- Which provider(s) to use
- Which MCP servers to use
- Various other BAE_* env var values the user would prefer

There should always be defaults available, or the user can ovveride with an answer

The result output by baectl should be 1) a docker-compose.yml file which launches baesrv (max or standard) using all the settings the user chose 2) A .env file that encodes all the user's choices (that the docker-compose file references) 3) The option to immediately launch the configuration

Two flags should exist:

- `--dev` flag which causes `baectl setup` to use dev image names (i.e. what `make` in the repo creates)
- `--apple` flag which causes `baectl setup` to produce a shell script that launches `container` directly INSTEAD of a docker-compose.yml file. It should still work with the .env file.

The command should be idempotent so if the files it creates already exist, it should offer to either edit them (and ask questions again to overwrite what already exists) or launch the saved configuration.

## User Stories

### User Story 1:
As a: New bae User

I want to:
run `baectl setup` and answer a short series of Q&A prompts (image variant, provider, MCP servers, a first profile/key) instead of hand-writing `bae-config.toml`, a `docker-compose.yml`, and a `.env` file from the docs

So I can:
go from a clean checkout to a running `baesrv` with a working profile and client key in one command, with every choice defaulted so I can just hit enter through the whole wizard if I don't have opinions yet

### User Story 2:
As a: Platform Operator

I want to:
re-run `baectl setup` against a directory that already has a `docker-compose.yml`/`bae-config.toml`/`.env` from a previous run, and be offered a clear choice between re-launching the saved configuration as-is or walking back through the wizard to change specific answers

So I can:
adjust my setup (add an MCP server, switch provider, flip from Standard to Max) without hand-editing three generated files or risking `baectl setup` silently clobbering a configuration I already have running

### User Story 3:
As an: Agent Developer on macOS using Apple's `container` CLI instead of Docker

I want to:
pass `--apple` to `baectl setup` and get a runnable shell script that launches `baesrv`/`bae-max` via `container run` instead of a `docker-compose.yml`

So I can:
follow the same one-command quickstart as a Docker user, using the container engine already installed on my machine, without hand-translating a compose file into `container run` flags myself

## Implementation Details:

### New command: `baectl setup`

- `setup` is a new top-level subcommand alongside `create`/`list`/`get`/`update`/`delete`/`auth` in `baectl/src/cli.rs`'s clap derive tree. Like `auth create key` (`aspec/uxui/cli.md`'s "baectl" section, `docs/reference/baectl.md#baectl-auth-create-key`), **it never calls the admin API to do its file-generation work** — it is a local scaffolding tool that runs *before* a server exists to talk to. It differs from `auth create key` in one way: at the very end, if the user opts to launch, `setup` *does* start talking to the admin API (see "Launch and first profile/key" below), making it the one `baectl` command that both generates local files and drives a live server in the same invocation.
- New module `baectl/src/setup.rs` holding the wizard state machine, prompt helpers, and file templates; `main.rs`/`cli.rs` route `baectl setup` into it exactly like `auth::run_create_key` is routed today. No new crate dependency for prompting — read lines from `std::io::stdin().lock()` after writing the prompt (with its default shown in brackets) to `std::io::stdout()`/flush, matching the project's stated preference for minimal dependencies (baectl's only non-`clap`/`reqwest`/`serde` deps today are `argon2`/`rand`, both scoped to `auth create key`). An empty line (bare enter) accepts the shown default; anything else is validated per-question and re-prompted on invalid input rather than aborting the wizard.

### Flags

| Flag | Effect |
|---|---|
| `--dev` | Use the image tags `make image`/`make image-max` produce locally (`better-agent-engine:latest` / `better-agent-engine:max`, per the root `Makefile`'s `IMAGE`/`MAX_IMAGE` variables) instead of the published GHCR tags (`ghcr.io/prettysmartdev/better-agent-engine:latest` / `:max`, per `aspec/uxui/setup.md`'s "User installation"). Intended for contributors iterating on a local `make image` build. |
| `--apple` | Emit `bae-setup.sh` (a shell script driving `container run`, Apple's container CLI) instead of `docker-compose.yml`. Both output modes still read the same `.env` file — see "Two output modes" below. |
| `--dir <path>` | Directory to read/write the three generated files in. Default `.` (current directory), mirroring `auth create key`'s `--out-dir` convention. |

Both flags are answerable interactively too (`--dev`/`--apple` just pre-fill/skip the corresponding wizard questions), so `baectl setup` with no flags at all still produces a complete, working setup — consistent with `aspec/uxui/cli.md`'s "no required flags, every option has a sensible default" principle already established for `baesrv`/`baectl`.

### Wizard flow

Runs top to bottom, one question at a time, each with `[default]` shown inline (e.g. `Image variant? [standard/max] (default: standard): `):

1. **Idempotency check (first, before any question)** — see "Idempotency" below; may short-circuit the rest of the wizard entirely (re-launch path) or seed subsequent defaults from the existing files (edit path).
2. **Image variant** — `standard` (`Dockerfile`) or `max` (`Dockerfile.max`, bundles the MAX web dashboard on port 3000). Default `standard`.
3. **Provider(s)** — repeatable "add a provider? [y/N]" loop building `[[providers.entries]]`. Each entry asks: registry `name` (default `<kind>-default`, e.g. `anthropic-default`), `provider` kind (`anthropic`/`openai` — the only two `ProviderKind` values, `server/src/engine/provider.rs:57`), `model` (default `sonnet-5` for anthropic, `gpt-5.6-luna` for openai — placeholders only; not asserted against a live model list), `auth_token` env var name (default `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` per kind, stored as the literal `${VAR}` template — same ergonomic transform work item 0004 established for `--auth-token-env`), and — only if that env var isn't already exported in the wizard's own process environment — the **actual secret value** to write to `.env` (never echoed back to stdout as it's typed is out of scope for a first cut; note this explicitly in Edge Cases). At least one provider is required to proceed (a profile needs a `primary_provider`); the loop keeps offering "add another?" until the user declines.
4. **MCP servers** — repeatable "add an MCP server? [y/N]" loop building `[[mcp.servers]]`. Offers a short curated pick-list drawn from `docs/guides/mcp-servers.md`'s worked examples plus a free-form "custom" option:
   - `filesystem` (stdio, `command=npx`, `args=["-y","@modelcontextprotocol/server-filesystem","/data"]`, prompts for the mounted directory)
   - `fetch` (stdio, `command=uvx`, `args=["mcp-server-fetch"]`, per the guide's "Adding the fetch server" section)
   - `github` (http, `url=https://api.githubcopilot.com/mcp/`, `headers.Authorization=Bearer ${GITHUB_TOKEN}`, prompts for the `GITHUB_TOKEN` value the same way providers prompt for secrets)
   - `custom` — freeform `name`/`transport`/`command`+`args` or `url`+`headers`
   No servers is a valid answer (`mcp_servers: []`, matching the API's own empty-is-valid convention).
5. **Other `BAE_*` env vars** — a short fixed list, each optional with the documented server default shown as the default answer (`docs/reference/configuration.md`'s Environment Variables table): `BAE_ADDR` (`0.0.0.0:8080`), `BAE_LOG` (`info`), `BAE_SHUTDOWN_TIMEOUT` (`30`), `BAE_TURN_TIMEOUT` (`120`), `BAE_SANDBOX_DRIVER` (`docker`). If image variant is `max`, also prompt for the MAX web port (default `3000`) and `BAE_MAX_PASSWORD` (default: leave blank → MAX self-generates and writes its own password file, per `docs/guides/max-webapp.md`; a non-blank answer is written to `.env` and passed through). `BAE_DB_PATH`/`BAE_ADMIN_ADDR`/`BAE_CONFIG`/`BAE_ADMIN_KEY_FILE`/`BAE_ADMIN_KEY_HASH_FILE` are **not** asked — they're wired to fixed container-internal paths/volumes by the generated compose file itself (mirroring `run/baesrv`'s/`run/baemax`'s Makefile targets), and exposing them as wizard questions would let a user break the file layout `setup` itself controls.
6. **Launch now?** — `[y/N]`, default `N` in non-interactive-looking contexts (see Edge Cases) else `Y`. If yes, see "Launch and first profile/key" below.

### Idempotency (step 1)

- Before asking anything, check whether `<dir>/docker-compose.yml` **or** `<dir>/bae-setup.sh` (whichever the `--apple` flag/its wizard question implies) plus `<dir>/.env` and `<dir>/bae-config.toml` already exist.
- **None exist:** proceed straight into the normal wizard (fresh setup).
- **All three (or the relevant compose-or-script variant + the other two) exist:** print a summary of the saved configuration (parsed back out of the existing `.env`/`bae-config.toml` — image variant, provider names, MCP server names) and offer exactly two choices:
  - **Launch** — skip the entire wizard and go straight to "Launch and first profile/key" below, reusing the files verbatim.
  - **Edit** — re-run the full wizard, but every question's default is pre-filled from the existing files' current values (not the hardcoded server defaults) rather than starting blank, so accepting every default reproduces the current setup unchanged and only the fields the user actively changes differ. Regenerating overwrites all three files; **the existing files are backed up first** (`docker-compose.yml.bak` / `bae-setup.sh.bak`, `.env.bak`, `bae-config.toml.bak`, one backup generation deep — a second consecutive edit overwrites the `.bak` from the first) so a wizard mistake doesn't destroy the last known-good config with no recovery path.
- **Partial state** (e.g. `.env` exists but `bae-config.toml` was deleted, or vice versa): treat as corrupted/incomplete, warn explicitly which file(s) are missing, and require the user to confirm before proceeding into a fresh wizard run that will overwrite whatever partial state remains (see Edge Case Considerations).

### Two output modes

- **Default (compose):** `docker-compose.yml` with one service (`baesrv` or `bae-max` depending on the image-variant answer), `image: <resolved tag>`, `env_file: .env`, `volumes: [bae-data:/var/lib/bae]`, a bind mount of the generated `bae-config.toml` to `/etc/bae/config.toml` read-only (mirroring `run/baesrv`'s `--volume "$(CURDIR)/bae-max-demo:/etc/bae:ro"` pattern) plus `BAE_CONFIG=/etc/bae/config.toml` baked into the service's `environment:` (not `.env`, since it's a fixed path the compose file itself controls, not a user choice), `ports: ["${BAE_ADDR_PORT:-8080}:8080"]` (and `3000:3000` too when `max`), a named volume declaration for `bae-data`, and `restart: unless-stopped`. No `container_name` pinned (compose derives one from the directory/project name, avoiding a collision with the Makefile's own unrelated `bae`/`bae-max` container names used by `run/baesrv`/`run/baemax`).
- **`--apple` (script):** `bae-setup.sh` — a `#!/usr/bin/env bash` script (`set -euo pipefail`) that: sources `.env`, creates the `bae-data` volume if absent (`container volume inspect ... || container volume create ...`, mirroring the Makefile's `ensure-engine` Apple-container branch), stops/removes any prior container of the same name (mirroring `run/baesrv`'s `-$(ENGINE) stop`/`rm` idempotent-rerun pattern), then runs a single `container run -d --name <name> --publish <port>:8080 [--publish 3000:3000] --volume bae-data:/var/lib/bae --volume "$(pwd)/bae-config.toml:/etc/bae/config.toml:ro" --env-file .env --env BAE_CONFIG=/etc/bae/config.toml <resolved tag>`. Marked executable (`0755`) when written. Functionally equivalent to the compose file, not a wrapper around `docker compose` — Apple's `container` CLI has no compose equivalent today.
- Both modes reference the **same** `.env` and `bae-config.toml` — only the launcher artifact differs, per the summary's "It should still work with the .env file."

### `.env` file

- Holds every secret and every user-chosen override as `KEY=value` lines, sourced by both output modes: the provider auth-token env vars (`ANTHROPIC_API_KEY`, etc.), MCP server secret env vars (`GITHUB_TOKEN`, etc.), and the non-default `BAE_*` values from step 5 (only lines the user actually changed from the shown default are written, keeping the file short and self-documenting — an unset key means "use the image's built-in default," not "unset the variable").
- `bae-config.toml`'s `${VAR}` references are resolved from the **container's** environment at connect time (`config_file.rs`'s existing "Secrets" contract) — `.env`'s job is purely to get those values into the container via `env_file:`/`--env-file`, `setup` does not resolve or template them itself.
- Written with `0600` permissions (it holds live provider/MCP credentials), matching `auth create key`'s existing file-permission convention for `admin-key.pem`.

### `bae-config.toml` generation

- Serialized from the `[[providers.entries]]`/`[[mcp.servers]]` structures built during steps 3–4, using the exact TOML shape documented in `config_file.rs`'s module doc comment and `docs/reference/configuration.md`'s `bae-config.toml` schema section (`[providers]`/`[[providers.entries]]`, `[mcp]`/`[[mcp.servers]]`) — reuse `server::config_file::BaeConfig`'s `Serialize` derive if `baectl` can cheaply depend on it (see Codebase Integration for the dependency-boundary question this raises), else hand-build the TOML string field-by-field in `setup.rs` matching that shape exactly, with a unit test asserting round-trip parseability against `server`'s own `BaeConfig` deserializer (test-only cross-crate dependency, not a runtime one, sidesteps the boundary question for shipping code while still catching drift).
- File header comment states it was generated by `baectl setup` and the timestamp/flags used, mirroring `bae-max-demo/config.toml`'s existing header-comment convention, so a user who later hand-edits it understands its provenance.

### Launch and first profile/key (step 6, or the idempotent re-launch path)

- Runs `docker compose up -d` (or `bae-setup.sh`, in `--apple` mode) via `std::process::Command`, streaming its stdout/stderr through so the user sees real container-engine output, not a swallowed result.
- Polls `GET /healthz` on the resolved client address (from `BAE_ADDR`/the port choice) with a short backoff/timeout (a few seconds, generous enough for first-boot migrations) before proceeding — launching the container doesn't mean the server is accepting connections yet.
- Once healthy, **only on a fresh setup** (not the "Launch" idempotent path, which assumes a profile/key already exist from the prior run — see Edge Cases): creates one profile (name `default`, `primary_provider` = the first provider entry added in step 3, no MCP servers/allowed tools unless the user wants to hand-edit afterward) and one client key (name `default`). **Critically, this does not connect to `127.0.0.1:8081` from the host** — the admin port is loopback-only *inside* the container by design (`aspec/devops/infrastructure.md`, reinforced by work item 0004) and the generated compose file/script deliberately never publishes it (see "Admin port reachability" in Edge Case Considerations), so `setup` instead shells out via `docker exec <container> baectl create profile ...` / `docker exec <container> baectl create key ...` (or the `container exec` equivalent in `--apple` mode) — the exact same binary and admin-auto-configuration path (`BAE_ADMIN_ADDR` default `127.0.0.1:8081`, admin key auto-read from `/var/lib/bae/admin-key.pem`) that already works with zero flags when run *inside* the container, per `docs/reference/baectl.md`'s "Auto-configuration" section. `setup` parses that subprocess's `--json` output rather than reimplementing request-building against `admin_client.rs` from the host, since the host has no route to the admin port to build a request against in the first place.
- Prints the created client key plaintext **exactly once**, with the same "copy it now" stderr warning `create key` already gives, plus a ready-to-copy example of exporting `BAE_URL`/`BAE_API_KEY` for a client library, per `aspec/uxui/experience.md`'s documented client-config convention.
- If `--launch` is declined (or the wizard's launch question is answered `N`), print the exact command the user would run manually (`docker compose up -d` or `./bae-setup.sh`) plus a reminder that `baectl setup` (re-run, same directory) will offer the launch step again without redoing the Q&A.

## Edge Case Considerations:

- **Re-running with no local changes intended.** Covered by the idempotency "Launch" path (see Implementation Details) — must not regenerate/overwrite any file when the user just wants to relaunch.
- **Re-running to launch, but the previously-created profile/key were since deleted via `baectl delete profile`/`delete key` or a wiped data volume.** The "Launch" idempotent path does not attempt to detect this (it has no way to know without an extra round-trip that would slow down the common case) — document that a user in this state should use the "Edit" path (even with zero answer changes) to re-trigger profile/key creation, or run `baectl create profile`/`create key` by hand.
- **Partial file state** (one or two of the three generated files present, not all three). Treated as corrupted/incomplete per "Idempotency" above — never silently treated as either "fresh" or "existing," since guessing wrong in either direction risks clobbering something or crashing on a missing file mid-wizard.
- **`--dir` doesn't exist or isn't writable.** Usage/runtime error before any prompting begins (exit `1`, matching `auth create key`'s existing "`--out-dir` doesn't exist" error), not a wizard question — mirrors `auth create key`'s existing behavior for the same failure mode.
- **`--apple` passed but the host has no `container` CLI, or no flag passed but the host has no `docker`.** `setup` does not hard-require the engine to be present to *generate* files (a user might generate on one machine, deploy on another), but the "Launch now?" step, if reached, must detect the missing binary before invoking it and fail with a clear "`docker`/`container` not found on PATH" message (exit `1`) rather than a raw "command not found" from the shell — same spirit as the Makefile's own `ensure-engine` target.
- **Non-interactive stdin** (`setup` run with stdin redirected from `/dev/null` or a CI pipe, no TTY). Detect via `stdin().is_terminal()`; if not a TTY, every question falls back to its default with no prompt printed (equivalent to "hit enter through the whole wizard") **except** the launch question, which defaults to `N` in this mode specifically — auto-launching a container from a non-interactive invocation with defaulted, unreviewed provider/secret choices is a footgun `setup` should not default into. Document this explicitly since it's the one place the "always safe to accept every default" framing doesn't hold.
- **Secret entry echoed to the terminal.** A first cut reading via plain `stdin` echoes typed characters (no `rpassword`-style masking) — call this out explicitly as a known limitation in both the work item and `docs/reference/baectl.md`'s `setup` section, since operators should be aware the token appears in their terminal scrollback/history the same way a curl-with-inline-secret command already does today (not a new class of exposure, but worth stating plainly).
- **User declines to provide a value for a provider's secret env var, and it isn't already exported in `setup`'s own process environment either.** `setup` still writes the `${VAR}` reference into `bae-config.toml` (a provider is still useful to have registered) but leaves that line out of `.env` entirely and warns at the end of the wizard, once, listing every such variable — resolution then fails at connect time with the server's existing "unresolved `${ENV_VAR}`" error (`config_file.rs`'s documented resolve-at-call-time contract), which is an acceptable, already-documented failure mode rather than something `setup` needs to prevent.
- **Two providers share the same `provider`/`model` but the user gives them different registry `name`s (or, conversely, tries to reuse a `name` for a second entry).** `setup` enforces uniqueness of `name` within the wizard loop itself (re-prompt on collision) — matching `config_file.rs`'s registry-build-time duplicate-name rejection — rather than writing a `bae-config.toml` guaranteed to fail server startup.
- **`--dev` and image not actually built locally yet** (user passes `--dev` without ever running `make image`). `setup` does not verify the tag exists locally (`docker image inspect`) at file-generation time — only the eventual `docker compose up -d`/`container run` invocation would fail, with the engine's own "no such image" error surfacing verbatim; note this in `docs/reference/baectl.md` rather than adding a preflight check that duplicates what `make image-smoke` already exists to verify.
- **Admin port reachability during "Launch and first profile/key."** Unlike every other `baectl` command (documented to run via `docker exec`/`container exec` **inside** the container, reaching `127.0.0.1:8081` from there), `setup` runs on the **host** right after starting the container — so it must reach the admin port through whatever the compose file/script actually publishes. Since `aspec/devops/infrastructure.md` deliberately does **not** expose 8081 to the host network (admin-port-over-network is explicitly called out as a security boundary in work item 0004 and infrastructure.md), `setup`'s post-launch profile/key creation must run `baectl create profile`/`create key` **inside** the container via `docker exec`/`container exec` (shelling out, the same pattern every other documented `baectl` invocation uses) rather than connecting to `127.0.0.1:8081` from the host — the generated compose file/script must therefore **not** publish port 8081, preserving the existing "admin port never touches the network" guarantee, and `setup`'s own admin-API calls for profile/key creation go through `docker exec bae baectl create profile ...`, not a direct host-side HTTP client call.
- **`docker compose` vs the legacy standalone `docker-compose` binary.** `setup` invokes `docker compose up -d` (the `v2` plugin subcommand form, matching current Docker distribution norms) and surfaces a clear error if neither `docker compose` nor `docker` itself resolves, rather than silently trying both and masking which one actually ran.
- **MAX password handling when `image=max` and the user leaves `BAE_MAX_PASSWORD` blank.** `setup` must not silently assume a password exists — it should tell the user, in the post-launch summary, exactly how to retrieve MAX's self-generated password (mirroring how the admin key retrieval instructions already work: `docker exec ... cat <path>`, per `docs/guides/max-webapp.md`), since it did not set one itself.
- **Re-running `setup --apple` in a directory whose existing config was generated in compose mode (or vice versa).** The idempotency check in "Idempotency" above is keyed on the launcher file matching the flag actually passed this run; if `docker-compose.yml` exists but `--apple` is now passed (or `bae-setup.sh` exists but `--apple` is absent), treat this as **partial state** (per that edge case) rather than silently ignoring the mismatched flag or silently generating a second, inconsistent launcher artifact alongside the first.

## Test Considerations:

- **Unit — wizard defaults and validation**: every step's default value matches the documented server default (cross-checked against `docs/reference/configuration.md`'s table so the two never drift silently); invalid answers (unknown provider kind, malformed MCP transport, duplicate provider/MCP name) re-prompt rather than crashing or silently accepting bad input.
- **Unit — non-interactive fallback**: with stdin not a TTY, every question resolves to its default with zero prompts printed, and the launch question specifically resolves to `N` (not the interactive default) — assert this by piping `/dev/null` (or an equivalent mock) as stdin in a test harness.
- **Unit — `bae-config.toml` generation round-trips**: for a range of wizard answer combinations (zero/one/many providers, zero/one/many MCP servers of each transport), the generated TOML string parses successfully via `server`'s own `BaeConfig` deserializer (test-only dependency) and every field matches what was entered — the strongest guard against silent schema drift between `baectl setup`'s hand-built TOML and `config_file.rs`'s actual shape.
- **Unit — `.env` generation**: only user-changed (non-default) `BAE_*` values are written; every provider/MCP secret the user supplied a value for appears; a declined secret is correctly *absent* from `.env` while still present as a `${VAR}` reference in `bae-config.toml`.
- **Unit — `docker-compose.yml`/`bae-setup.sh` generation**: for both `standard`/`max` and both compose/`--apple` combinations (four cases), assert the emitted file references the correct image tag (dev vs. published, per `--dev`), correct published ports (8080 only, or 8080+3000 for `max`), the `bae-config.toml` bind mount, `env_file`/`--env-file` wiring, and — critically — that port `8081` (admin) never appears in any generated ports/publish list, regression-guarding the admin-port-never-exposed edge case above.
- **Unit — idempotency detection**: fresh (no files) → wizard; all three present → launch/edit choice; partial (any proper subset of the three) → the corrupted-state path; launcher-file/flag mismatch (compose file exists but `--apple` passed, or vice versa) → also the corrupted-state path.
- **Unit — edit path pre-fills from existing files**: seed a directory with a known-answer `.env`/`bae-config.toml`, choose "Edit," accept every default with no changes — assert the regenerated files are byte-identical to the originals (modulo the timestamp header comment) and that `.bak` copies of the pre-edit files were written first.
- **Integration — full fresh-setup-then-launch lifecycle**: drive `baectl setup` as a subprocess with a scripted stdin transcript (image=standard, one anthropic provider with a fixture API key, no MCP servers, launch=yes) against the real `docker`/`container` engine available in CI, assert the container comes up healthy, and that exactly one profile and one client key exist afterward via `baectl list profiles`/`list keys`. Skippable/conditional on an engine being present in the test environment, matching the existing `make image-smoke`/`check-static` posture of requiring a container engine only for the tests that genuinely need one.
- **Integration — idempotent re-launch**: run the fresh-setup flow above, tear the container down (`docker compose down`, files left in place), run `baectl setup` again choosing "Launch," and assert no new profile/key is created (the "Launch" path does not touch the admin API for profile/key creation, per Implementation Details) and the same container comes back up against the same data volume with the original profile/key still present and usable.
- **Integration — `--apple` output is actually runnable**: on a host with Apple's `container` CLI available (conditional/skippable elsewhere, matching the project's existing Apple-container test posture), generate with `--apple`, run the resulting `bae-setup.sh` directly (not through `baectl`), and assert the container reaches a healthy state — proving the script is self-contained and not secretly dependent on `baectl` being re-invoked.
- **Regression — admin port never published**: for every generated compose/script variant in the unit tests above, assert `8081`/the admin address never appears in any `--publish`/`ports:` entry, tying directly back to the host-vs-in-container admin-reachability edge case.
- All new tests remain offline-by-default per `baectl`'s existing test posture (`make test-baectl`) — only the explicitly engine-gated integration tests above touch a real container engine, and they must be skippable in environments without one, matching `check-static`/`image-smoke`'s existing conditional-on-engine posture.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- New `baectl/src/setup.rs`, routed from `cli.rs`'s clap tree alongside the existing `create`/`list`/`get`/`update`/`delete`/`auth` command groups — same file-per-command-group shape `auth.rs` (if that's where `auth create key` currently lives) or `cli.rs`'s existing subcommand dispatch already establishes; do not fold this into `admin_client.rs`, which is scoped to admin-API request/response types, not local file generation or process orchestration.
- For the post-launch step, `setup` deliberately **does** shell out to `docker exec`/`container exec` running `baectl create profile ...`/`baectl create key ...` **inside** the container (see Implementation Details and the "Admin port reachability" edge case) rather than linking `admin_client.rs`'s request-building functions into a host-side call — the host process has no network route to the loopback-only admin port to call them against. This is the one place `setup` invokes `baectl` as a subprocess of itself, and it is intentional, not a shortcut: it's the same `docker exec bae baectl ...` pattern every other documented `baectl` usage already relies on.
- **Dependency-boundary decision needed on `bae-config.toml` serialization**: either (a) depend on `server`'s `config_file::BaeConfig` types directly for `Serialize` (weigh against `baectl`'s deliberate `bae-rs`-independence precedent from work item 0004 — `server` is a different kind of dependency than `client-rust`, but pulling in the `server` crate at all is a bigger boundary crossing than baectl has taken before) or (b) hand-build the TOML string with a test-only cross-crate parseability check (see Test Considerations) and no runtime dependency on `server`. Document whichever is chosen in this file's own module doc comment, mirroring how `config_file.rs` explains *why* it's structured the way it is.
- No new runtime crate dependencies beyond what's needed for TOML serialization if option (a) above isn't taken (`server/Cargo.toml` already depends on a TOML crate — check its exact choice, likely `toml`, and match it rather than introducing a second TOML library into the workspace's dependency set) — `std::io`/`std::process` cover prompting and process orchestration with no new crate.
- Update `docs/reference/baectl.md`: new `### baectl setup` section (in the Commands table and its own subsection, matching every other command's documentation shape — flags, exact wizard question list with defaults, both output-mode file shapes, exit codes) plus updates to the top-of-file Commands table.
- Update `aspec/uxui/cli.md`'s "baectl" section: add `setup` to the "Command structure" list, `--dev`/`--apple`/`--dir` to "Flag structure," and a short "Setup wizard" note under "Inputs and outputs" clarifying `setup` is the one `baectl` command that reads interactive stdin (every other command's "stdin: unused" line stays accurate for the rest of the surface).
- Update `docs/guides/quickstart.md`'s very first steps: offer `baectl setup` as the recommended fastest path to a running server + profile + key, ahead of (not replacing) the existing manual `docker run` + `baectl create profile`/`create key` walkthrough — keep the manual path documented too, since `setup` is a convenience wrapper around the same underlying primitives, not a replacement API.
- Update `aspec/architecture/design.md`'s "Component 5: baectl" entry to mention `setup` as part of baectl's scope (it remains "not a published library," same distinction already documented for the rest of baectl).
- `docker-compose.yml`/`bae-setup.sh`/`.env`/`bae-config.toml` written by `setup` into an operator's working directory are exactly the kind of generated, secret-bearing local artifacts `.gitignore` already excludes (`.env`, `.env.*` per the existing entries) — verify `docker-compose.yml`/`bae-setup.sh`/`bae-config.toml` are **not** ignored by default (a generated compose file is often intentionally committed for a team, unlike `.env`), and note this distinction explicitly rather than leaving it to guesswork.
- Verify `make build`/`test`/`lint`/`fmt` (baectl component) and `make test-baectl` continue passing with the new module and its tests.
