# baectl Reference

`baectl` is a command-line HTTP client for the [admin API](02-admin-api.md)
(`/admin/v1/*`). It ships as a static binary at `/usr/local/bin/baectl` inside
both the dev and production images, alongside `baesrv`. Run it with
`docker exec`/`container exec` against a running container — for admin commands
there is nothing to install, and no Rust toolchain or network access is needed.

```sh
docker exec bae baectl create profile main anthropic-sonnet \
  --allowed-tool get_current_time
```

`baectl` covers **profile and key management**, plus four commands that run on
the **host** rather than through `docker exec`, so each needs a host-native
binary (details in their sections below):

- [`baectl setup`](#baectl-setup) — generates a runnable deployment (compose
  file/script, `.env`, `bae-config.toml`) and can launch it, before a server
  exists to talk to.
- [`baectl build`](#baectl-build), [`baectl ready`](#baectl-ready), and
  [`baectl run`](#baectl-run) — the three verbs that turn a `setup`-launched
  server plus some harness code into a running, wired-up agent: package a
  harness (`build`), verify/fix its profile-and-key compatibility with the
  server (`ready`), then launch it (`run`). Each acts on a local
  `bae-harness.toml` manifest — see the
  [Harness manifest reference](07-harness-manifest.md).

`baectl` does not open sessions or send messages — those hit
the client port (8080) with a client/session key and are documented in the
[Client API](00-client-api.md) and the [guides](../guides/00-quickstart.md).

---

## Auto-configuration

When run inside the same container as `baesrv` (the documented deployment —
`docker exec`/`container exec`), `baectl` needs **zero flags**: it finds the
admin port on loopback and reads the admin key the server wrote to disk at
startup.

### Admin address

Precedence, highest to lowest:

1. `--admin-addr <HOST:PORT>`
2. `BAE_ADMIN_ADDR` env var
3. default: `127.0.0.1:8081`

A bare `host:port` value is used as plain HTTP (`http://host:port` — the
admin port never speaks TLS). A value that already contains `://` is used
verbatim, for the rare case of reaching `baectl` over an SSH tunnel or through
a TLS-terminating proxy.

### Admin token

Precedence, highest to lowest:

1. `--admin-token <TOKEN>` / `BAE_ADMIN_TOKEN` env var — sent verbatim as
   `Authorization: Bearer <token>`. Use this for scripting or an
   operator-held key that isn't backed by a local file.
2. `--admin-key-file <PATH>` / `BAE_ADMIN_KEY_FILE` env var — reads the
   plaintext admin key from an explicitly named file (surrounding whitespace
   is trimmed). If this file is named explicitly and cannot be read, that is
   a hard runtime error (exit `1`) — the operator asked for it specifically.
3. The default probed path, `/var/lib/bae/admin-key.pem` — read the same way,
   but a missing file here is **not** an error; `baectl` simply proceeds with
   no token. If the server enforces admin auth, the request then fails with
   `401` and `baectl` prints the guidance in [Errors](#errors) below.

`--admin-token`, `--admin-key-file`, and `--admin-addr` are global flags —
valid before or after the subcommand, on every command.

See [Admin authentication](../guides/09-admin-authentication.md) for how the
server-side key file is created and rotated.

---

## Commands

Verb-first, resource-typed positional, mapping 1:1 onto the admin API's CRUD
surface. Profiles support the full create/list/get/update/delete set; keys
support create/list/delete only — there is no single-key-get or key-update
endpoint (keys are immutable besides revocation).

| Command | Admin endpoint |
|---|---|
| [`baectl create profile <name> <primary_provider>`](#baectl-create-profile) | `POST /admin/v1/profiles` |
| [`baectl list profiles`](#baectl-list-profiles) | `GET /admin/v1/profiles` |
| [`baectl get profile <id>`](#baectl-get-profile) | `GET /admin/v1/profiles/{id}` |
| [`baectl update profile <id> <primary_provider>`](#baectl-update-profile) | `PUT /admin/v1/profiles/{id}` |
| [`baectl delete profile <id>`](#baectl-delete-profile) | `DELETE /admin/v1/profiles/{id}` |
| [`baectl create key <name> <profile_id>`](#baectl-create-key) | `POST /admin/v1/keys` |
| [`baectl list keys`](#baectl-list-keys) | `GET /admin/v1/keys` |
| [`baectl delete key <id>`](#baectl-delete-key) | `DELETE /admin/v1/keys/{id}` |
| [`baectl auth create key`](#baectl-auth-create-key) | *(local only — no API call)* |
| [`baectl setup`](#baectl-setup) | *(local scaffolding — no API call, except post-launch `create profile`/`create key` run **inside** the container)* |
| [`baectl build <harness>`](#baectl-build) | *(local — packages a harness into a build artifact; no API call)* |
| [`baectl ready <id>`](#baectl-ready) | `GET /admin/v1/profiles`, `GET /admin/v1/keys`, and (additively) `POST`/`PUT` on both — all run **inside** the container, same as `setup`'s launch step |
| [`baectl run <id>`](#baectl-run) | same admin-API access as `ready`, then launches the harness on the host or in a new container |

`--help` is available on every command and subcommand (`baectl --help`,
`baectl create --help`, `baectl create profile --help`, …).

### `baectl create profile`

```
baectl create profile <name> <primary_provider> [flags]
```

**Positionals (required):**

| Positional | Description |
|---|---|
| `name` | Unique profile name. |
| `primary_provider` | The **name** of a `[providers]` entry declared in `bae-config.toml` (e.g. `anthropic-sonnet`) — not a provider id or model. See [Configuration — `[providers]`](05-configuration.md#providers). |

**Flags (optional):**

| Flag | Description |
|---|---|
| `--fallback <NAME>` | A fallback `[providers]` registry name, repeatable, tried in order after the primary fails. |
| `--mcp-server <NAME>` | MCP server name to enable, repeatable. Omitted entirely → `mcp_servers: []`. |
| `--allowed-tool <NAME>` | Client-side tool name to allow, repeatable. Omitted entirely → `allowed_tools: []` (no client-side tools permitted). |
| `--available-sandbox <IMAGE>` | Sandbox image this profile permits (see [Sandboxes](../guides/03-sandboxes.md)), repeatable. Omitted entirely → `available_sandboxes: []`. |
| `--json` | Print the raw JSON response instead of a human summary. |

`baectl` does **not** validate `--mcp-server`/`primary_provider`/`--fallback`
names against the live MCP/provider registries — both registries are
config-file-driven and can differ across restarts. A typo'd MCP server name
is caught non-fatally at session-creation time (see
[MCP Servers](../guides/02-mcp-servers.md#non-fatal-skips)); an unresolvable
`primary_provider` is **fatal** at session-creation time (see
[Profiles — Fatal primary / non-fatal fallback](../profiles.md#fatal-primary--non-fatal-fallback)).
`baectl` never builds or sends provider config (URL, auth token, max
tokens) — that is entirely operator-managed `bae-config.toml` on the
server, listable via `GET /admin/v1/providers`.

**Output (human):**

```
created profile
  id:         pro_a1b2c3d4e5f6…
  name:       main
  created_at: 2026-07-06T18:26:01.123Z
```

**Output (`--json`):** the raw `{id, name, created_at}` document the API
returned.

**Errors:** duplicate name (`409 duplicate_name`), malformed body
(`400 bad_request`). See [Errors](#errors).

### `baectl list profiles`

```
baectl list profiles [--limit <N>] [--cursor <C>] [--json]
```

No positionals.

| Flag | Description |
|---|---|
| `--limit <N>` | Fetch a single page of at most `N` items. Opts **out** of auto-pagination. |
| `--cursor <C>` | Fetch a single page starting at this opaque cursor. Opts out of auto-pagination. |
| `--json` | Print raw JSON instead of a human table. |

**Pagination:** with neither `--limit` nor `--cursor`, `baectl` follows
`next_cursor` until it is `null` and returns the **full** result set — a
human running `baectl list profiles` never needs to know the API is
cursor-paginated. Passing either flag opts back into raw single-page
behavior, for scripting.

**Output (human, auto-paginated or single-page):** a fixed-width table,
columns `ID NAME PRIMARY_PROVIDER`. An empty result prints `no profiles found`
(not an empty table with only headers).

**Output (`--json`):**
- Auto-paginated (default): a flat JSON **array** of every profile.
- Single-page (`--limit`/`--cursor` given): the raw page document,
  `{"items": [...], "next_cursor": ...}`.

### `baectl get profile`

```
baectl get profile <id> [--json]
```

| Positional | Description |
|---|---|
| `id` | Profile id. |

| Flag | Description |
|---|---|
| `--json` | Print the raw JSON document instead of a human summary. |

**Output (human):** every field of the profile — `id`, `name`,
`primary_provider` (registry name), `fallback_providers` (registry names),
`mcp_servers`, `allowed_tools`, `available_sandboxes`, `created_at`,
`updated_at`. Empty list fields print `(none)`.

**Output (`--json`):** the full Profile object, same shape as a `list`
item.

**Errors:** `404 not_found` if the id doesn't exist or was deleted.

### `baectl update profile`

```
baectl update profile <id> <primary_provider> [--name <NAME>] [flags]
```

Full replacement (`PUT`) — mirrors the admin API, which always overwrites
every field.

| Positional | Description |
|---|---|
| `id` | Id of the profile to replace. |
| `primary_provider` | The `[providers]` registry name (see [`create profile`](#baectl-create-profile)). |

| Flag | Description |
|---|---|
| `--name <NAME>` | New name. **Optional** — see below. |
| *(same config flags as `create profile`)* | `--fallback`, `--mcp-server`, `--allowed-tool`, `--available-sandbox`, `--json`. |

> **`--name` is optional, filling a gap in the admin API.** `PUT
> /admin/v1/profiles/{id}` always requires a `name` in its body, but
> `update profile`'s positional signature has none. When `--name` is
> omitted, `baectl` first `GET`s the current profile and reuses its existing
> name, so a plain `baectl update profile <id> <primary_provider>` changes
> the provider reference without renaming. Pass `--name` to rename during
> the same replace.

Any repeatable flag left unset (`--fallback`, `--mcp-server`,
`--allowed-tool`, `--available-sandbox`) serializes as an explicit empty
array in the `PUT` body — a full replacement clears fields that aren't
re-specified, exactly like a direct `PUT` call would. This is why
`baectl ready --fix`/`run` always re-send a profile's **entire** existing
list for every list field (unioned with whatever the harness declares) when
they widen it — see [`baectl ready`](#baectl-ready) below.

**Output:** same as `get profile` (human full summary, or `--json` the
replaced Profile object).

**Errors:** `400 bad_request`, `404 not_found`.

### `baectl delete profile`

```
baectl delete profile <id>
```

Soft-deletes the profile. No flags, no `--json` (the API returns
`204 No Content`).

**Output:** `deleted profile <id>` on stdout.

**Errors:**
- `404 not_found` — no profile with this id.
- `409 profile_in_use` — the profile still has active client keys.
  `baectl`'s message names the suggested next steps: run `baectl list keys`
  to find them, then `baectl delete key <id>` for each, then retry.

### `baectl create key`

```
baectl create key <name> <profile_id> [--json]
```

| Positional | Description |
|---|---|
| `name` | Human label for the key. |
| `profile_id` | Id of the profile this key is bound to. Must be a non-deleted profile. |

| Flag | Description |
|---|---|
| `--json` | Print the raw JSON response instead of a human summary. |

**Output (human):**

```
created key
  id:         key_a1b2c3d4e5f6…
  name:       my-agent
  key:        bae_1a2b3c4d…
  prefix:     bae_1a2b
  profile_id: pro_…
  created_at: 2026-07-06T18:26:05.000Z
```

**The plaintext `key` field is shown exactly once**, in both human and
`--json` output, followed by a stderr warning:
`baectl: copy the key now — it cannot be retrieved again`. It is never
logged or cached — copy it immediately.

**Errors:** `400 bad_request` (blank name), `422 profile_unavailable` (the
referenced profile does not exist or is deleted).

### `baectl list keys`

```
baectl list keys [--limit <N>] [--cursor <C>] [--json]
```

Same shape and pagination behavior as [`list profiles`](#baectl-list-profiles).

**Output (human):** table, columns `ID NAME PREFIX PROFILE_ID`. Empty result
prints `no keys found`.

**Output (`--json`):** flat array (auto-paginated) or `{items, next_cursor}`
(single page).

### `baectl delete key`

```
baectl delete key <id>
```

Revokes the client key (cascades to its session keys and open sessions — see
[Admin API → Client Keys](02-admin-api.md#client-keys)). No flags.

**Output:** `revoked key <id>` on stdout.

**Errors:** `404 not_found`.

### `baectl auth create key`

```
baectl auth create key [--name <NAME>] [--out-dir <DIR>]
```

**This command never calls the admin API.** It is a local key-generation
utility for pre-provisioning one shared admin credential across multiple
independent server replicas. See
[Admin authentication → multi-replica walkthrough](../guides/09-admin-authentication.md#multi-replica-pre-provisioning)
for the full flow.

| Flag | Description |
|---|---|
| `--name <NAME>` | Name recorded in the hash file (display only, on the server). Default `provisioned-admin`. |
| `--out-dir <DIR>` | Directory to write the two output files into. Default `.` (current directory). |

**Writes two files**, both with `0600` permissions:

- `<out-dir>/admin-key.pem` — the plaintext `bae_admin_<48 hex chars>` token,
  single line with a trailing newline (readers must trim). This is the
  **live credential** — treat it like a password. Copy it to wherever
  `baectl`/operators run, at the path `BAE_ADMIN_KEY_FILE` resolves to.
- `<out-dir>/admin-key-hash.pem` — a pretty-printed JSON document the server
  ingests at boot:

  ```json
  {
    "key_hash": "b8f15df49ca3acc355c07ed98cc11d61d7e172db85ceab49cc3ef02381f983c5",
    "prefix": "bae_admin_1a2b",
    "name": "provisioned-admin"
  }
  ```

  Drop this file onto **every replica's** data volume at the path
  `BAE_ADMIN_KEY_HASH_FILE` resolves to, before that replica's first boot.

The token is generated with 192 bits of CSPRNG entropy (24 random bytes,
hex-encoded) and hashed with unsalted SHA-256 over its exact bytes, then
encoded as 64 lowercase hexadecimal characters — see
[Key security](02-admin-api.md#key-security). The deterministic format has no
salt or tunable parameters, so `baectl` and the server independently produce
the same digest without shared code or out-of-band configuration.

**Output:** stdout prints the two file paths (scriptable); stderr prints
handling guidance for each file.

**Errors:** a runtime error (exit `1`) if either file cannot be written
(e.g. `--out-dir` doesn't exist or isn't writable).

### `baectl setup`

```
baectl setup [--dev] [--apple] [--yes|-y] [--dir <DIR>]
```

The interactive quickstart wizard: a short series of defaulted stdin/stdout
Q&A questions that produce a runnable deployment — a launcher, a `.env`, and a
`bae-config.toml` — in `--dir`, and can immediately launch it. It is the one
`baectl` command that is a **local scaffolding tool**: it runs *before* a
server exists to talk to, and never links `admin_client.rs` host-side (the
host has no route to the loopback-only admin port). The one exception is the
optional final launch step, where `setup` shells out to `docker exec`/
`container exec` running `baectl create profile`/`baectl create key`
**inside** the just-started container — the same auto-configuration path
every other documented `baectl` invocation already uses. This makes `setup`
the one `baectl` command that both generates local files *and* drives a live
server in the same invocation.

**Run it on the host, not inside the image.** `setup` drives your host's
container engine (`docker compose up -d` / `./bae-setup.sh`), so it must run
where that engine is — not inside the production image, whose entrypoint is
`baesrv` and which carries no `docker`/`container` client. The binary in the
image is a static *Linux* binary built for the image, so it is not the copy to
run on the host — a macOS host cannot execute it at all. Build a host-native
`baectl` from a checkout instead:

```sh
make build-baectl        # → baectl/target/host/release/baectl
baectl/target/host/release/baectl setup
```

> **Coming soon: a one-line installer**, which will be the normal way to get a
> host `baectl` — no checkout or Rust toolchain:
>
> ```sh
> # PLACEHOLDER — not published yet; use the source build above.
> curl -fsSL https://<install-host-tbd>/baectl/install.sh | sh
> ```

**Flags:**

| Flag | Description |
|---|---|
| `--dev` | Use the image tags a local `make image`/`make image-max` produces (`better-agent-engine:latest` / `:max`) instead of the published GHCR tags (`ghcr.io/prettysmartdev/better-agent-engine:latest` / `:max`). For contributors iterating on a local build. |
| `--apple` | Emit `bae-setup.sh` (a shell script driving Apple's `container` CLI) instead of `docker-compose.yml`. Both output modes read the same `.env`. |
| `--dir <DIR>` | Directory to read/write the three generated files in. Default `.` (current directory), mirroring `auth create key`'s `--out-dir` convention. |
| `--yes` / `-y` | Non-interactive: accept every wizard default (even on a TTY) and pick the provider from the environment. See below. |

No flag is required — `baectl setup` with no arguments still produces a
complete, working setup, consistent with every other `baectl`/`baesrv`
command's "no required flags" convention.

#### `--yes` (non-interactive quickstart)

`baectl setup --yes` (or `-y`) is the form the [Quickstart](../guides/00-quickstart.md)
uses: no prompts at all, suitable for a script or a first-run copy-paste.

- Every wizard question takes its default, exactly as if you'd pressed Enter
  through the whole interactive wizard — `--dev`/`--apple` keep their normal
  flag semantics (unasked; default `false` unless passed).
- The provider is picked from the environment, first match wins: **`ANTHROPIC_API_KEY`**,
  then **`OPENAI_API_KEY`** — the wizard's own detection order. The winner
  becomes the single registry entry, using the wizard's defaults for that
  kind (name `anthropic-default`/`openai-default`, default model, auth env
  var = that variable). Stdout prints which one was picked, e.g.
  `using provider anthropic (ANTHROPIC_API_KEY is set)` — **key values are
  never printed**, only the variable name.
- If neither variable is set (or both are empty), `setup --yes` exits `1`
  before writing anything, with exactly:
  ```
  baectl: no provider API key found in the environment.
          export ANTHROPIC_API_KEY="sk-ant-…"   # or OPENAI_API_KEY="sk-…"
          then re-run `baectl setup --yes`
  ```
- Launch is implied (as if you'd answered **Launch now?** yes), and the
  first profile/key are created exactly as on the interactive fresh-setup
  path (see [Launch step](#setup-launch)). A missing engine binary on `PATH`
  still produces the existing exit-`1` message.
- **Idempotent:** if `--dir` already has a complete setup (all three files
  present), `--yes` skips the Edit/Launch question — it always relaunches
  the saved configuration verbatim, printing
  `already set up in <dir> — server launched from the saved configuration`,
  and exits `0`. It never edits or overwrites a working setup.
- **Partial/corrupted state** (one or two of the three files present) is
  *not* silently overwritten by `--yes`: the "Overwrite and run a fresh
  setup?" question resolves to its default **No** under `--yes` too, so
  `setup --yes` exits `1` with `baectl: aborted; no files were changed.`
  rather than guessing at a fresh setup over unreviewed leftover state.
- `--yes --apple` writes `bae-setup.sh`, exactly as `--apple` alone does.
- The generated `docker-compose.yml` (both `--yes` and the interactive path)
  now includes, on the server service:
  ```yaml
      extra_hosts:
        - "host.docker.internal:host-gateway"
  ```
  harmless on Docker Desktop, required for the container to reach a
  host-side service (e.g. a mock provider, or another local server) via
  `host.docker.internal` on Linux.

#### Wizard question list

Runs top to bottom; `[default]` is shown inline and a bare enter accepts it.
On a directory with an existing, complete setup, the wizard is skipped
entirely in favor of a Launch/Edit choice (see
[Idempotency](#setup-idempotency) below); when **Edit** is chosen, every
question's default below is pre-filled from the existing files instead.

The two mode flags are also **answerable interactively**: passing `--apple` or
`--dev` on the command line pre-fills and skips its question, while omitting the
flag makes `setup` ask it (both default to "no", i.e. `docker-compose.yml` and
the published image tags), so `baectl setup` with no flags still produces a
complete setup:

0. **Use Apple's `container` CLI (instead of docker-compose)?** — default `N`;
   skipped when `--apple` is passed. Chosen before anything else because it
   decides which launcher file the idempotency check looks for.
0. **Use locally-built (`make image`) image tags?** — default `N`; skipped when
   `--dev` is passed.
1. **Image variant?** (`standard`/`max`) — default `standard`.
2. **Provider(s)** — at least one is required (a profile needs a
   `primary_provider`). The wizard prints "At least one provider is required"
   and asks for the first provider unconditionally; each subsequent one is
   gated by **Add another provider?** (default `N`). Per provider:
   - **Provider kind?** (`anthropic`/`openai`) — default `anthropic`.
   - **Registry name?** — default `<kind>-default` (e.g. `anthropic-default`).
     Must be unique among providers added this run; re-prompts on collision
     or a blank answer.
   - **Model?** — default `sonnet-5` (anthropic) / `gpt-5.6-luna` (openai).
     Not validated against a live model list — a placeholder you can edit
     later.
   - **Auth token env var name?** — default `ANTHROPIC_API_KEY` /
     `OPENAI_API_KEY`. Stored in `bae-config.toml` as `${VAR}`.
   - **Secret value** — only asked if that env var isn't already exported in
     `setup`'s own process environment (in which case its value is captured
     silently, with no prompt). See
     [Secret handling](#setup-secret-handling) below.
3. **MCP server(s)** — zero is valid. Looped, gated each time by
   **Add an MCP server?** (default `N`). Per server:
   - **Which?** (`filesystem`/`fetch`/`github`/`custom`) — default
     `filesystem`.
     - `filesystem` — stdio, `command=npx`,
       `args=["-y","@modelcontextprotocol/server-filesystem",<dir>]`.
       Asks **Server name?** (default `filesystem`) and
       **Directory to expose?** (default `/data`).
     - `fetch` — stdio, `command=uvx`, `args=["mcp-server-fetch"]`. Asks
       **Server name?** (default `fetch`) only.
     - `github` — http, `url=https://api.githubcopilot.com/mcp/`,
       `headers.Authorization=Bearer ${GITHUB_TOKEN}`. Asks
       **Server name?** (default `github`) and prompts for `GITHUB_TOKEN`
       the same way a provider secret is collected.
     - `custom` — asks **Server name?**, **Transport?**
       (`stdio`/`http`/`sse`, default `stdio`), then either **Command?**
       (default `npx`) + **Args? (space-separated)** for `stdio`, or
       **URL?** for `http`/`sse`.
   - Server names must be unique within this run; re-prompts on collision or
     a blank answer.
4. **Other `BAE_*` env vars** — each optional, its documented server default
   shown as the default answer; only an answer that differs from the default
   is written to `.env` (an unset key means "use the image's built-in
   default," not "unset"). Asked in this fixed order:
   `BAE_ADDR` (`0.0.0.0:8080`), `BAE_LOG` (`info`),
   `BAE_SHUTDOWN_TIMEOUT` (`30`), `BAE_TURN_TIMEOUT` (`120`),
   `BAE_SANDBOX_DRIVER` (`docker`).
   If the image variant is `max`, two more questions follow:
   - **MAX web port?** — default `3000`.
   - **MAX password? (blank = MAX generates one on first boot)** — default
     blank. A non-blank answer is written to `.env` as `BAE_MAX_PASSWORD`.

   `BAE_DB_PATH`, `BAE_ADMIN_ADDR`, `BAE_CONFIG`, `BAE_ADMIN_KEY_FILE`,
   `BAE_ADMIN_KEY_HASH_FILE`, and `BAE_OTEL_LOG` are **not** asked — they are
   wired to fixed container-internal paths by the generated launcher itself,
   or (for `BAE_OTEL_LOG`) only matter once `[telemetry]` is enabled, which
   `setup` does not configure (see [`[telemetry]` is never generated](#setup-no-telemetry) below).
5. **Launch now?** — default `Y` when the wizard is running interactively,
   `N` otherwise. See [Launch step](#setup-launch) below.

<a id="setup-secret-handling"></a>
**Secret handling.** For each secret env var (a provider's auth token,
`GITHUB_TOKEN` for the `github` MCP server), `setup` resolves a value in this
order:

1. Already captured earlier in this same run (two providers sharing a var) →
   reused silently.
2. Already exported and non-empty in `setup`'s own process environment →
   captured into `.env` with no prompt.
3. On the **Edit** path with an existing `.env` value → asks
   **Keep the existing value for VAR?** (default `Y`); the existing value is
   never re-echoed to the terminal.
4. Otherwise prompts **Value for VAR? (blank to skip)**. A blank answer
   leaves the variable out of `.env` — the `${VAR}` reference is still
   written to `bae-config.toml`, and the variable is listed in a one-time
   warning printed at the end of the run. Resolution then fails at connect
   time with the server's existing "unresolved `${ENV_VAR}`" error.

**Known limitation:** typed secret values are **echoed to the terminal as
you type them** — this first cut has no `rpassword`-style masking. The token
appears in your terminal scrollback/history the same way an inline `curl`
secret already would; be aware of this if your terminal session is logged or
shared.

<a id="setup-idempotency"></a>
#### Idempotency

Before asking anything, `setup` checks `--dir` for the launcher matching this
run's mode (`docker-compose.yml`, or `bae-setup.sh` with `--apple`), `.env`,
and `bae-config.toml`:

- **None present** → the normal wizard runs (fresh setup).
- **All three present** → prints a summary (image variant, provider names,
  MCP server names) and asks
  **Edit this configuration? (No = launch the saved config as-is)**
  (default `N`):
  - **No (Launch)** — reuses the three files verbatim; does **not** run the
    wizard, does **not** create a profile/key (it assumes the ones from the
    original run still exist — see the note under [Launch step](#setup-launch)).
  - **Yes (Edit)** — backs up the current files to `<file>.bak` (one
    generation deep — a second consecutive edit overwrites the `.bak`), then
    re-runs the wizard with every default pre-filled from the existing
    files, regenerates all three files, and offers to launch.
- **The launcher for the *other* mode is present** (e.g. `docker-compose.yml`
  exists but `--apple` was passed this run) → treated as a
  launcher/flag mismatch, handled the same as partial state below.
- **One or two of the three files present** (partial/corrupted state) →
  warns which file(s) are missing/mismatched and asks
  **Overwrite and run a fresh setup?** (default `N`) before proceeding; a
  decline leaves every file untouched and exits successfully.

**Non-interactive stdin** (`stdin` is not a TTY — e.g. piped from
`/dev/null` or a CI job): every question above resolves to its default with
*no prompt printed at all*, as if you hit enter through the entire wizard —
**except** the launch question, which defaults to `N` in this mode
specifically (auto-launching from unreviewed, defaulted answers is a
footgun `setup` avoids). On an existing complete setup, the Edit-vs-Launch
choice also resolves to **Launch** (reuse verbatim, never an unattended
overwrite).

#### Generated files

Both output modes reference the same `.env` and `bae-config.toml` — only the
launcher differs.

**`bae-config.toml`** (mode `0644`) — a provenance header comment, then
`[mcp]` (always present, even with zero servers) followed by any
`[[mcp.servers]]` entries, then `[providers]` followed by the
`[[providers.entries]]` entries:

```toml
# Generated by `baectl setup` (variant: standard, flags: (none), unix: 1752684000).
# Re-run `baectl setup` in this directory to launch or edit it.

[mcp]

[[mcp.servers]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]

[providers]

[[providers.entries]]
name = "anthropic-default"
provider = "anthropic"
model = "sonnet-5"
auth_token = "${ANTHROPIC_API_KEY}"
```

<a id="setup-no-telemetry"></a>
**`[telemetry]` is never emitted** — an absent section keeps telemetry
disabled per `config_file.rs`'s contract. Add it by hand afterward if you
want OpenTelemetry export (see
[Configuration — `[telemetry]`](05-configuration.md#telemetry)).

**`.env`** (mode `0600`) — a provenance header, then secrets (provider auth
vars in provider order, then MCP secret vars, then any remaining secret such
as `BAE_MAX_PASSWORD`), then non-default `BAE_*` overrides in the fixed
step-4 order. Only values you supplied or changed appear:

```sh
# Generated by `baectl setup` (variant: standard, flags: (none), unix: 1752684000).
# Holds secrets and non-default BAE_* overrides. Sourced by the launcher.

ANTHROPIC_API_KEY=sk-ant-...
BAE_LOG=debug
```

**`docker-compose.yml`** (default mode, `0644`) — one service (`baesrv` for
`standard`, `bae-max` for `max`), publishing `8080` (and the chosen MAX port
too, for `max`); **`8081` (the admin port) is never published**:

```yaml
# Generated by `baectl setup` (variant: standard, flags: (none), unix: 1752684000).
# Re-run `baectl setup` in this directory to launch or edit it.
services:
  baesrv:
    image: ghcr.io/prettysmartdev/better-agent-engine:latest
    env_file: .env
    environment:
      BAE_CONFIG: /etc/bae/config.toml
    volumes:
      - bae-data:/var/lib/bae
      - ./bae-config.toml:/etc/bae/config.toml:ro
    ports:
      - "${BAE_ADDR_PORT:-8080}:8080"
    extra_hosts:
      - "host.docker.internal:host-gateway"
    restart: unless-stopped
volumes:
  bae-data:
```

**`bae-setup.sh`** (`--apple` mode, `0755`) — functionally equivalent, driving
Apple's `container` CLI directly (no compose equivalent exists for it);
container named `bae` (`bae-max` for `max`), stopped/removed first so a
re-run is idempotent; same "never publish `8081`" rule:

```sh
#!/usr/bin/env bash
set -euo pipefail
# Generated by `baectl setup` (variant: standard, flags: --apple, unix: 1752684000).
# Re-run `baectl setup` in this directory to launch or edit it.
cd "$(dirname "$0")"

# Read only BAE_ADDR_PORT from .env, without evaluating the file.
BAE_ADDR_PORT="$(sed -n 's/^BAE_ADDR_PORT=//p' .env 2>/dev/null | tail -n1)"
BAE_ADDR_PORT="${BAE_ADDR_PORT:-8080}"

container volume inspect bae-data >/dev/null 2>&1 || container volume create bae-data
container stop bae >/dev/null 2>&1 || true
container rm bae >/dev/null 2>&1 || true

container run -d --name bae \
  --publish "${BAE_ADDR_PORT}:8080" \
  --volume bae-data:/var/lib/bae \
  --volume "$(pwd)/bae-config.toml:/etc/bae/config.toml:ro" \
  --env-file .env \
  --env BAE_CONFIG=/etc/bae/config.toml \
  ghcr.io/prettysmartdev/better-agent-engine:latest
```

The script never `source`s `.env` — its values (provider/MCP secrets, arbitrary
overrides) could contain shell metacharacters that sourcing would evaluate. It
reads only the one host-port override it needs (`BAE_ADDR_PORT`) literally, and
hands every variable to the container through `--env-file .env` (which the
`container` CLI parses itself, never evaluating it as shell). If you chose a
non-default `BAE_ADDR`, the publish/health-check port above is that address's
port rather than `8080`.

Inside this repository's checkout (any directory of it), the root
`.gitignore` already ignores `.env` (it holds live secrets), `bae-config.toml`
and `bae-setup.sh`. `docker-compose.yml` is the only generated file it does
**not** ignore, so after a Docker-mode `setup` in the checkout `git status`
shows it as untracked until you remove it (an Apple-mode `setup` leaves no
untracked file). Outside this repo nothing is ignored for you: a team that
wants to commit its generated deployment can do so (keep `.env` out of it);
`setup` does not force that choice either way.

<a id="setup-launch"></a>
#### Launch step

If you answer **Launch now?** with yes (or accept the saved config's default
Launch choice on a re-run):

1. Warns once about any declined/unresolved secrets.
2. Checks that the required engine binary (`docker` for the default mode,
   `container` for `--apple`) is on `PATH` — a clean
   "`docker`/`container` not found on PATH" runtime error (exit `1`) if not,
   rather than a raw shell "command not found."
3. Runs `docker compose up -d` (or executes `./bae-setup.sh`), streaming its
   output.
4. Polls `GET /healthz` (up to ~30 tries, 2s timeout + 0.5s backoff) before
   proceeding, on the port the server actually listens on — `8080` by default,
   or the port half of a non-default `BAE_ADDR`. The launcher publishes and
   `setup` polls that same port, so choosing e.g. `BAE_ADDR=0.0.0.0:9090`
   yields a coherent `9090:9090` mapping rather than an unlaunchable one.
5. **On a fresh setup only** (not the verbatim re-launch path), creates a
   profile named `default` (`primary_provider` = the first provider you
   added) and a client key named `default` — by running
   `baectl create profile`/`baectl create key` **inside** the container
   (`docker compose exec`/`container exec`), since the admin port is
   loopback-only inside the container and is never published to the host.
   The plaintext key is printed **exactly once**, with the same
   `baectl: copy the key now — it cannot be retrieved again` stderr
   warning `create key` gives directly, followed by a ready-to-copy
   `BAE_URL`/`BAE_API_KEY` export example.

   The **Launch**-only re-run path (existing, unedited config) does **not**
   repeat this step — it assumes the profile/key from the original run still
   exist. If they were deleted, re-run `baectl setup` and choose **Edit**
   (even with no answer changes) to recreate them, or run
   `baectl create profile`/`create key` by hand inside the container.
6. If `max` and the MAX password was left blank, prints the retrieval
   command for MAX's self-generated password file.

If you decline to launch, `setup` prints the exact manual command
(`docker compose up -d` or `./bae-setup.sh`) — re-running `baectl setup` in
the same directory offers the launch step again without redoing the Q&A.

**`--dev` and no local image built yet.** `setup` does not check whether the
`--dev` image tag actually exists locally (e.g. via `docker image inspect`)
at file-generation time — only the eventual `docker compose up -d`/
`container run` step would fail, surfacing the engine's own "no such image"
error verbatim. Run `make image`/`make image-max` first if you pass `--dev`.

#### Exit codes

| Exit | When |
|---|---|
| `0` | Wizard completed (files written, launched or not); or the user declined an overwrite/fresh-setup confirmation (no files changed). |
| `1` | `--dir` doesn't exist, isn't a directory, or isn't writable (checked before any prompt); an existing `bae-config.toml`/`.env`/launcher fails to parse on the Edit/summary path; the engine binary is missing from `PATH`; the engine exits non-zero while launching; the server never becomes healthy within the timeout; the in-container `baectl create profile`/`create key` call fails; `--yes` found no provider key in the environment; `--yes` hit partial/corrupted existing state (declined overwrite). |
| `2` | An unknown flag or invalid flag value (clap-level usage error, e.g. a non-existent flag). |

**Errors:** every failure prints `baectl: <message>` to stderr, matching
every other `baectl` command's convention.

---

### `baectl build`

```
baectl build <harness> [--sdk rust|typescript|python] [--harness-dir <path>]
             [--launcher local|schedule|api|webapp] [--id <id>]
             [--dev] [--dir <path>]
```

Packages a harness — a bundled example or your own project — into a disposable
local build artifact that `ready`/`run` act on. **Runs on the host**, exactly
like `setup`: it never opens a host-side admin client, and a container-mode
build shells out to your local `docker`/`container` binary directly. The
global `--admin-addr`/`--admin-token`/`--admin-key-file` flags are accepted
(they're `global = true`) but unused here.

**Flags:**

| Flag | Description |
|---|---|
| `<harness>` (positional, required) | A bundled example name (`issue-triage` / `reference-assistant`), resolved under `--sdk`. Still required, but **not used for resolution**, when `--harness-dir` is given — the manifest's own `name` field is authoritative there. |
| `--sdk <SDK>` | Which SDK directory (`client-<sdk>/`) a bundled example is resolved under: `rust` (default), `typescript`, or `python`. Has no effect with `--harness-dir` — the manifest's own `sdk` field is what actually gets recorded in the build id and `manifest.json`. |
| `--harness-dir <path>` | Build an arbitrary directory containing its own `bae-harness.toml`, instead of a bundled example. No repo checkout required. |
| `--launcher <LAUNCHER>` | How to package the harness: `local` (default, runs on the host), `schedule`, `api`, or `webapp` (the latter three build a container image and require the harness's `bae-harness.toml` to have a `[harness.launcher]` section). |
| `--id <id>` | Explicit build id. Overrides the derived default verbatim and is never collision-suffixed. Must be a single non-empty path component. |
| `--dev` | For container launchers, use the local `make image-launcher-<type>` tag (`better-agent-engine:launcher-<type>`) instead of the published `ghcr.io/prettysmartdev/better-agent-engine:launcher-<type>` tag. No effect on `--launcher local`. Recorded in the build's `manifest.json` for the [`ready`/`run` consistency-guard warning](#dev-on-ready-and-run). |
| `--dir <path>` | Workspace directory holding `.baectl/` and the files `setup` generated. Default `.`. |

**Harness resolution.** With no `--harness-dir`, `<harness>` resolves to
`<dir>/client-<sdk>/examples/<harness>/` — a usage error if `client-<sdk>/`
isn't found there:

```
could not resolve bundled harness '<harness>': client-<sdk>/ was not found under <dir>; use --harness-dir <path> for an external harness
```

With `--harness-dir <path>`, that directory is used directly — it must
contain its own `bae-harness.toml` (schema:
[Harness manifest reference](07-harness-manifest.md)), and no repo checkout is
required at all.

**Id.** Default `<name>-<sdk>-<launcher>`, taken from the harness's own
manifest (not from `--sdk`), collision-suffixed `-2`, `-3`, … only when
`--id` is omitted and a *different* harness/sdk/launcher combination already
used that bare id under `--dir`. Re-running `build` for the **same**
combination overwrites its `.baectl/builds/<id>/` files in place and prints
`` rebuilt `<id>` `` instead of `` built `<id>` ``; a rebuild always deletes that
build's `resolved.json` and `harness.env` first, and (for a local rebuild)
any stale container-generation files left behind under the same explicit id —
a changed artifact can never inherit a former readiness result or secret
file.

**`--launcher local` (default)** — no container engine involved. Writes
`<dir>/.baectl/builds/<id>/manifest.json` only (`kind: "local"`), recording
the harness's `run` command, its absolute `harness_dir`, `working_dir`, and
`requires` verbatim from `bae-harness.toml`. The command itself is **not**
run at build time — `cargo run`/`npm run`/`uv run` (etc.) build lazily on the
first `baectl run`, which is also the point `BAE_SERVER_URL`/`BAE_CLIENT_KEY`
become available; running it during `build` would launch the agent before
readiness has supplied credentials.

**`--launcher schedule|api|webapp`** — the harness is always compiled
**inside Docker**, never on the host, so `build` never needs `cargo`/`npm`/`uv`
on `PATH` for a container-mode build — only `docker`/`container`:

1. **Harness build stage.** Runs
   `docker build -f <dockerfile> [--target <target>] -t <id>-harness-build:latest <context>`.
   `<dockerfile>` is `[harness.launcher].dockerfile` if the manifest sets one
   (build context: the harness directory); otherwise `baectl` synthesizes a
   per-SDK default and writes it to
   `<dir>/.baectl/builds/<id>/Dockerfile.build.generated` (build context: the
   harness's `working_dir` — the bundled examples' manifests live two levels
   below their SDK project root and set `working_dir = "../.."` for exactly
   this reason):

   | `sdk` | Base image | Build command | Default artifact path |
   |---|---|---|---|
   | `rust` | `rust:1-bookworm` | `cargo build --release --example <name>` | `/build/target/release/<name>` |
   | `typescript` | `node:22-bookworm` | `npm ci && npm run build` | `/opt/bae-harness/bae-harness-entrypoint` |
   | `python` | `debian:bookworm-slim` + `python3`/`python3-venv` | `python3 -m venv /opt/bae-harness/venv && … pip install .` | `/opt/bae-harness/bae-harness-entrypoint` |

   Rust compiles to a self-contained ELF binary, so its artifact path is just
   the Cargo output. TypeScript and Python have no such artifact — their
   harness is source plus an interpreter plus installed dependencies — so their
   generated stages additionally **stage a self-contained tree under
   `/opt/bae-harness`** (project sources, build output, and `node_modules` or a
   virtualenv) and write an executable `sh` shim at
   `/opt/bae-harness/bae-harness-entrypoint`. That shim *is* the default
   `binary_path`, so step 2's single `COPY --from … /usr/local/bin/<name>`
   still lands a real executable rather than a bare `.ts`/`.py` source file.
   Python's stage deliberately uses Debian's own `python3` rather than the
   `python:3.12` image, so the staged virtualenv's interpreter symlink still
   resolves against the `python3` step 2 installs into the launcher image.

   `[harness.launcher].binary_path` overrides the derived default path when
   set; it is **required** when the harness supplies its own `dockerfile`
   (`[harness.launcher].binary_path is required when dockerfile is set`) —
   `baectl` has no way to know a custom Dockerfile's output path without
   invoking Docker itself.
   **`.dockerignore` in the build context.** Docker only honours
   `<context>/.dockerignore`, and the generated stage does `COPY . .`, so
   when `baectl` synthesizes the build Dockerfile it also makes sure the
   build context has one. If `<context>/.dockerignore` is absent it writes
   one with exactly these six lines and prints
   `wrote <context>/.dockerignore (build-context excludes)`:

   ```
   target/
   node_modules/
   .venv/
   __pycache__/
   .baectl/
   .git/
   ```

   An existing `.dockerignore` is **never** overwritten or edited. If it does
   not exclude the directory that matters for the harness's SDK (`target/`
   for Rust, `node_modules/` for TypeScript, `.venv/` for Python), `build`
   warns on stderr — `baectl: warning: <context>/.dockerignore does not
   exclude <dir>/ — host build output will be sent to the image build` — and
   carries on. The bundled SDK directories commit this same file. If your
   harness directory is a git repository, the written file shows up as a new
   untracked file; commit it or add it to your ignore list. Nothing is written
   when the manifest sets its own `[harness.launcher].dockerfile`, or for
   `--launcher local`.

2. **Launcher packaging.** Writes `<dir>/.baectl/builds/<id>/Dockerfile`:
   ```dockerfile
   FROM <base image tag>
   COPY --from=<id>-harness-build:latest <binary_path> /usr/local/bin/<name>
   COPY bae-{schedules,api,app}.toml /etc/bae/bae-{schedules,api,app}.toml
   ```
   When `baectl` also generated the build stage (i.e. the manifest sets no
   `[harness.launcher].dockerfile`), a short SDK-specific preamble is inserted
   between the `FROM` and the first `COPY`, and a closing `USER bae` restores
   the launcher base's unprivileged user:

   | `sdk` | Preamble |
   |---|---|
   | `rust` | `mkdir -p /build/examples/<name>` owned by `bae` — `cargo build --example` bakes `CARGO_MANIFEST_DIR` (`/build`) into the binary, and a harness that writes beside its own sources (every bundled example creates a `workspace/`) needs that prefix to exist and be writable. |
   | `typescript` | Install Node 22 via the same NodeSource pattern `Dockerfile.max` uses, then `COPY --from=<harness-build> --chown=bae:bae /opt/bae-harness /opt/bae-harness`. |
   | `python` | Install Debian's `python3`, then the same `--chown`ed `/opt/bae-harness` copy. |

   The preamble exists because the `bae-launcher-*` base images are
   `debian:bookworm-slim` with only `ca-certificates` installed: without it a
   TypeScript or Python harness would package successfully and then fail to
   spawn on every trigger. A harness that supplies its own
   `[harness.launcher].dockerfile` owns its artifact's runtime requirements, so
   **no** preamble is injected in that case — the generated file is exactly the
   three-line shape above.

   `build` also writes the matching config file — `bae-schedules.toml` (`schedule`) or
   `bae-api.toml`/`bae-app.toml` (`api`/`webapp`) — in the same shape
   [`examples/launchers/{schedule,api,webapp}/`](06-launchers.md) uses: one
   `[[agents]]` entry named after the harness with
   `command = "/usr/local/bin/<name>"`, and for `api`/`webapp` a one-field
   `request_schema`/`env_template` keyed on `[harness.launcher].prompt_env`,
   listening on `0.0.0.0:9090`. `--launcher schedule` additionally requires
   `[harness.launcher].default_schedule`
   (`[harness.launcher].default_schedule is required for --launcher schedule`
   if absent — checked at this point, after the harness build stage has
   already run). The base image tag follows `--dev` exactly as above.
3. **Final build.** Runs `docker build -t <id>:latest <dir>/.baectl/builds/<id>/`,
   then writes `manifest.json` (`kind: "container"`, `image_tag`,
   `harness_build_image`, `launcher_type`, `port` — `9090` for `api`/`webapp`,
   absent for `schedule` — and `requires`).

   Engine choice (Docker vs. Apple `container`) follows whichever `setup`
   already produced in `--dir`; with no prior `setup` at all, `build` defaults
   to Docker. Unlike `ready`/`run`, **`build` does not require a prior
   `setup`** — a container-mode build only needs `docker`/`container` and, if
   `--dev` is passed, the locally built launcher image (`make
   image-launcher-<type>`).

**Output:** `` built `<id>` `` (or `` rebuilt `<id>` `` on a same-combo
re-run), then `next: baectl ready <id>`.

**Exit codes:**

| Exit | When |
|---|---|
| `0` | Build completed; `manifest.json` written. |
| `1` | A build-artifact directory or file couldn't be created/read/written; the `docker`/`container` build subprocess failed to start or exited non-zero (its own output streams above the error); the build timestamp couldn't be generated. |
| `2` | `--dir`/`--harness-dir` doesn't exist or isn't a directory; a bundled `<harness>` couldn't be resolved (`client-<sdk>/` missing); `bae-harness.toml` is missing or malformed (message names the field); `--launcher schedule/api/webapp` was requested for a harness with no `[harness.launcher]` section; `--id` isn't a single non-empty path component; a harness-supplied `dockerfile` is missing, or its `binary_path`/`default_schedule` requirement wasn't met. |

---

### `baectl ready`

```
baectl ready <id> [--fix] [--dir <path>] [--dev]
```

Checks whether a build's requirements — client-side tools, MCP servers, env
vars, a usable client key — are satisfied by the server `setup` scaffolded in
`--dir`, and, with `--fix`, applies the safe fixes. **Runs on the host**;
reaches the admin API the same way `setup`'s own launch step does, by
exec'ing the in-container `baectl` (`docker compose exec -T <service> baectl
…` / `container exec <name> baectl …`) — never a direct host connection to
the loopback-only admin port. The global `--admin-addr`/`--admin-token`/
`--admin-key-file` flags are accepted but unused here.

**Flags:**

| Flag | Description |
|---|---|
| `<id>` (positional, required) | The build id to check (`<dir>/.baectl/builds/<id>/manifest.json`, written by `baectl build`). |
| `--fix` | After printing the report, if any fix is pending, ask **`Apply the N safe fix(es) above? [y/N]`** once and apply on `y`/`yes`. Asked even without a TTY (EOF or anything else → skip) — a state-mutating fix is never silently applied. |
| `--dir <path>` | Workspace directory holding `.baectl/` and the files `setup` generated. Default `.`. |
| `--dev` | Consistency guard only — see [`--dev` on `ready`/`run`](#dev-on-ready-and-run). |

**The six checks**, each printed as one line, `` `✓ <label>` ``,
`` `⚠ <label> — will be fixed by run` ``, or `` `✗ <label>` ``:

- `✓` — passed.
- `⚠` — failing, but `baectl run` (or `ready --fix`) resolves it
  automatically, with no prompt. Only checks **#2** and **#4** can print
  `⚠`, and only when their fix is actually applicable (a profile that can be
  additively widened, or a key that can be freshly created) — an
  irreconcilable #2 (e.g. no provider registered at all) is `✗` instead, not
  `⚠`.
- `✗` — failing and **blocking**: neither `run` nor `--fix` can resolve it
  unattended. Checks **#1**, **#3**, and **#5** are always `✗` on failure,
  never `⚠`.
- `ℹ` — informational only (check #6); never `✓`/`⚠`/`✗`.

1. **Server reachable** — the exec'd `baectl list profiles --json` succeeds.
   No launcher found at all in `--dir` (no `baectl setup` was ever run there),
   or the exec itself failing, prints guidance pointing at `baectl setup`.
   Failing this check aborts the whole pass — nothing else runs.
2. **Compatible profile** — a profile whose `allowed_tools`, `mcp_servers`,
   and `available_sandboxes` (see [Sandboxes](../guides/03-sandboxes.md) and
   [Harness manifest — `[harness.requires]`](07-harness-manifest.md#harnessrequires))
   are all supersets of the build's `requires`. Prefers the profile from a
   prior `resolved.json` if it's still compatible, else the first compatible
   one found. On `⚠`/`✗`, prints the exact `baectl update profile …` (widen
   an existing profile additively — the union of its current tools/servers/
   sandboxes with the harness's `requires`, never a drop) or
   `baectl create profile …` command that would fix it, in the exec form
   matching how `setup` launched — `docker compose exec -T <service>
   baectl …` or `container exec <bae|bae-max> baectl …` — so the hint is
   copy-pasteable as printed even though the admin port is loopback-only
   inside the container. `⚠` when the fix is `Update`/`Create` (applicable);
   `✗` when it's impossible (e.g. no provider is registered at all — see the
   exit-code table below). When the fix is actually applied, the union is
   recomputed against the profile's body re-read immediately before the
   write, not the snapshot taken at the top of the check pass — every list
   field is re-sent in full, since the underlying `update profile` call is a
   full replacement (see [`baectl update profile`](#baectl-update-profile)).
   The admin API offers no compare-and-swap, so this is a single-writer
   guarantee: a genuinely concurrent writer to the same profile (two `baectl
   run` invocations racing, or an out-of-band `update profile`) can still lose
   its change. Serialize `ready --fix`/`run` against a shared profile.
3. **Required MCP servers registered** — every `requires.mcp_servers` name is
   present among `<dir>/bae-config.toml`'s `[[mcp.servers]]` entries. Distinct
   from #2: a profile can allow a server name the registry doesn't actually
   define. **Always print-only, always `✗` on failure** — fixing it needs a
   config edit *and* a server restart, which `ready`/`--fix` will not do
   unattended — so `✗` prints the TOML snippet to add plus the restart
   command (`docker compose restart` / `./bae-setup.sh`), and always gates
   the pass.
4. **Client key with a stored secret** — a plaintext-bearing key `baectl` can
   hand to `run`: either one reused from a prior `resolved.json` (re-validated
   against the live key list, still bound to the resolved profile), or a
   freshly created one. This is narrower than "any key bound to the profile" —
   `baectl` can never recover an existing key's plaintext (the admin API
   shows it once), so an unrecoverable pre-existing key does not satisfy this
   check. `⚠` on a fresh build (no key needed yet, `run` mints one silently);
   the printed `baectl create key …` hint ends with
   `` (--fix also records the key for `run`) `` in report mode (no `--fix`),
   since applying the fix does more than the bare command shown.
5. **Required env vars resolvable** — `requires.env` plus the resolved
   profile's provider auth-token var (looked up via `primary_provider` in
   `bae-config.toml`, the same variable `run` exports as
   `BAE_PROVIDER_KEY_ENV` — see [`baectl run`](#baectl-run)). `kind: "local"`
   builds check the host process environment (what `run` inherits);
   `kind: "container"` builds check `<dir>/.env`, a prior run's
   `<dir>/.baectl/builds/<id>/harness.env`, or the host environment (what
   `run` passes through). **Always print-only and always `✗` on failure** —
   except that an interactive `run` (a TTY, container build, #5 the only
   blocking failure) reaches an env-var prompt instead of aborting; see
   [`baectl run`](#baectl-run).
6. **MAX reachability** — informational `ℹ` line with the dashboard URL, only
   when `setup`'s image variant was `max`. Never `✓`/`⚠`/`✗`.

The pass succeeds only when #1 ∧ #2 (resolved) ∧ #3 ∧ #4 (resolved) ∧ #5 all
hold. `--fix` only ever mutates #2/#4, and only after the single confirmation
above; #3 and #5 are never auto-applied, with or without `--fix`. Every check
runs and prints before any mutation happens, in every mode — a blocking `✗`
on #1/#3/#5, or an impossible #2, aborts before `ready --fix`/`run` ever
touches the admin API, so a run that can't succeed never creates an orphaned
client key.

**Output:** on full success, writes
`<dir>/.baectl/builds/<id>/resolved.json` (mode `0600`):

```json
{
  "profile_id": "pro_…",
  "profile_name": "default",
  "key_id": "key_…",
  "client_key_plaintext": "bae_…",
  "server_url": "http://localhost:8080",
  "max_url": "http://localhost:3000",
  "provider_env": "ANTHROPIC_API_KEY"
}
```

`client_key_plaintext` is present only when `baectl` legitimately holds the
secret (just created, or carried forward from a prior `resolved.json`) — it
is never fabricated for a key `baectl` didn't create, and is **omitted**
(not `null`) when absent. `max_url` is likewise omitted unless the variant is
`max`. `provider_env` records the **name** (never the value) of the resolved
profile's provider auth-token var that check #5 accepted, so a container `run`
can forward a value that lives only in the host environment; it is omitted when
no provider var could be resolved. Prints `` ready: '<id>' is good to run — `baectl run <id>` ``. On any
unresolved check, `resolved.json` is **not** written (a stale one from a
prior run is left untouched) and `ready` exits `3` (only auto-fixable `⚠`
checks remain) or `1` (something is blocking) — see the table below.

**Exit codes:**

| Exit | When |
|---|---|
| `0` | All six checks resolved (every `✓`, or `⚠` already fixed by `--fix`); `resolved.json` written. |
| `3` | **Every** failing check is auto-fixable (`⚠` only — #2/#4, no `✗`): `` baectl: all remaining issues are auto-fixable — run `baectl run <id>` (fixes them without prompting) or `baectl ready <id> --fix` ``. A fresh `setup --yes` followed immediately by `ready` on the bundled `reference-assistant` hits exactly this: two `⚠` (profile needs widening, key needs creating), no `✗`, exit `3` — `run` then succeeds with no further prompting. With `--fix`, declining the confirmation when only `⚠` checks remain also exits `3` with this same message (nothing was mutated). |
| `1` | At least one check is blocking (`✗`, or #2 with no fixable resolution) — without `--fix`: `` baectl: some checks are blocking — follow the guidance above, then re-run `baectl ready <id>` ``; with `--fix` (after applying #2/#4, `✗` checks remain): `baectl: some checks are still unresolved (see above)`. Also: `bae-config.toml` exists but fails to parse; an admin-API exec call fails or returns unparseable output; a profile fix is needed but no provider is registered in `bae-config.toml` at all (check #2 prints `✗ compatible profile` with the hint line ``    no providers are registered in bae-config.toml; run `baectl setup` to configure one first``, and the command exits with the blocking message above). |
| `2` | No build `<id>` found under `--dir`: `` no build '<id>' found under <dir> — run `baectl build …` first (or check --dir) ``. |

All checks are evaluated, and every line printed, **before** any admin-API
mutation — in every mode, including `--fix`'s Prompt mode and `run`'s Auto
mode. A blocking failure never leaves a half-created key or widened profile
behind: `ready`/`run` either fix #2 then #4 together, or fix neither.

<a id="dev-on-ready-and-run"></a>
**`--dev` on `ready` and `run`.** The artifact a `ready`/`run` invocation acts
on was already fixed at `build` time — there is nothing left for `--dev` to
switch on either command. Both still accept it, purely as a **consistency
guard**: if the target's `manifest.json` records a different `--dev` setting
than this invocation used, `ready`/`run` print a warning to stderr (never a
hard failure) naming the mismatch:

```
baectl: warning — build 'reference-assistant-rust-api' was built --dev but you invoked this without --dev; the artifact is already fixed, so --dev has no functional effect here
```

---

### `baectl run`

```
baectl run <id> [--dir <path>] [--no-ready] [--server-url <url>] [--dev]
```

Launches a built harness. By default `run` first performs the **same six
checks as `ready`** (re-validating, not just trusting, any existing
`resolved.json`) and **auto-applies the #2/#4 safe fixes with no prompt** —
printing `fixed: …` for each — which is what keeps `setup` → `build` → `run`
a genuinely non-interactive path. An unresolved #3/#5 aborts with the same
guidance `ready` prints, plus a pointer to `baectl ready <id>` for the full
report. **Runs on the host**, same admin-API access path as `ready`.

**Flags:**

| Flag | Description |
|---|---|
| `<id>` (positional, required) | The build id to launch. |
| `--dir <path>` | Workspace directory holding `.baectl/` and the files `setup` generated. Default `.`. |
| `--no-ready` | Skip the check pass entirely and launch straight from the existing `resolved.json`. Fails loudly if it's absent, or present but missing a stored plaintext key (a hand-edited or stale file). |
| `--server-url <url>` | Override the server URL the harness is given, in place of `resolved.json`'s `server_url` (the auto-derived container address). Does not rewrite `resolved.json`. |
| `--dev` | Consistency guard only — see [above](#dev-on-ready-and-run). |

**`kind: "local"`** — runs in the **foreground** with inherited stdio (the
child's output streams live; Ctrl-C reaches it directly). Exports
`BAE_SERVER_URL`/`BAE_CLIENT_KEY` from the resolved values, plus
`BAE_PROVIDER_KEY_ENV=<name of the resolved provider's auth-token env var>`
(e.g. `BAE_PROVIDER_KEY_ENV=OPENAI_API_KEY` for an OpenAI-backed profile) —
the same variable name readiness check #5 validated — whenever it's known;
every other host env var is left exactly as-is. This is what lets a harness
built against a non-Anthropic provider find its key under the right name
instead of only ever looking for `ANTHROPIC_API_KEY`.

**`prepare`** (`[harness] prepare` in `bae-harness.toml`, local launcher
only — see [Harness manifest reference](07-harness-manifest.md#harness)) runs
once before `<run_command>`, only when needed, printing
`` prepare: <cmd>   (in <workdir>) `` first. For an `npm `/`npx ` command,
"needed" means `node_modules/` is missing under the harness's working
directory, or `package-lock.json` is newer than it; any other command is
needed when `.baectl/builds/<id>/prepared` is missing or older than
`bae-harness.toml` (and is touched after success either way, so a `run`
right after `build` never installs twice). A non-zero exit aborts `run`
before the harness ever starts, with the child's own exit code and
`` baectl: prepare command failed (exit N): <cmd> ``. Container launchers
never run `prepare` — the generated (or harness-supplied) Dockerfile already
installs whatever the image needs at build time.

`cd`s to `harness_dir/working_dir` and runs `sh -c "<run_command>"`. Prints a
header first:

```
── running reference-assistant (local) ──────────────
profile:  default (pro_…)
key:      key_…
server:   http://localhost:8080
(Ctrl-C to stop)
```

`run`'s own process exit code is the **harness's own exit code** — not
always `0` — since it's propagated directly from the child process.

**`kind: "container"`** — launches **detached** (schedule/api/webapp are
servers, not one-shot commands; `run` doesn't block on them). Resolves
`requires.env` **plus `resolved.json`'s `provider_env`** (readiness check #5
accepts that provider auth-token var from the host environment, so a launch
that only forwarded `<dir>/.env` would silently drop it) in this order:
already declared (non-empty) in `<dir>/.env` → left alone, passed through by
`--env-file`; else present (non-empty) in the host environment → captured;
else a value saved in a prior `<dir>/.baectl/builds/<id>/harness.env` from an
earlier run → reused; else prompts (TTY) or fails loudly naming the variable
(no TTY, e.g. CI) — then (re)writes `harness.env` (mode `0600`, preserving
previously prompted secrets across runs). Removes any prior container of the
same name first (`docker rm -f <id>` / Apple `container stop`+`rm`, so a
re-run never collides on `--name`), then runs, conceptually:

```sh
docker run -d --name <id> \
  [--add-host host.docker.internal:host-gateway]   # Docker only
  [--env-file <dir>/.env]                          # if it exists
  --env-file <dir>/.baectl/builds/<id>/harness.env \
  [--publish <port>:<port>]                        # api/webapp only
  <image_tag>
```

**No secret ever appears in the launch command line.**
`BAE_SERVER_URL`/`BAE_CLIENT_KEY`, plus `BAE_PROVIDER_KEY_ENV` (see above)
when known, are written into `harness.env` — `BAE_PROVIDER_KEY_ENV` first,
then `BAE_SERVER_URL`/`BAE_CLIENT_KEY` last, so the latter two win over any
same-named key in `<dir>/.env` (the engine applies `--env-file`s in order) —
rather than passed as `--env NAME=value` arguments: an argv element is
readable by any local user through `ps` or `/proc/<pid>/cmdline` for the
lifetime of the engine client process. The whole of `<dir>/.env` is still
forwarded as-is, so unrelated variables an operator put there do reach the
harness container; keep workspace-wide secrets that no harness needs out of
that file.

Readiness check #5 also counts a non-empty value already saved in
`<dir>/.baectl/builds/<id>/harness.env` as satisfying a required env var (not
just `<dir>/.env` or the host environment) — a value captured on an earlier
`run` keeps satisfying `ready`/`run` on later ones without re-prompting. On
an interactive TTY, if check #5 is the **only** blocking failure, `run`
prompts `Value for required env var <VAR>?` for each missing one, appends the
answer to `harness.env` (mode `0600`), and re-evaluates #5 before continuing
— an empty answer aborts with
`` baectl: no value provided for required env var <VAR> ``. Without a TTY
(e.g. CI), a missing #5 var still aborts immediately as `✗`, with no prompt
attempted.

Then prints the "where/how" summary:

| `launcher_type` | Printed |
|---|---|
| `webapp` | `open the chat UI:  http://localhost:<port>/` |
| `api` | A ready-to-copy `curl --no-buffer -X POST http://localhost:<port>/agents/<name>/trigger …`, using the actual required prompt field read back from the generated `bae-api.toml` (falls back to the literal field name `prompt` if that file can't be read). |
| `schedule` | The cron expression read back from `bae-schedules.toml` (falls back to `(see bae-schedules.toml)` if it can't be read). |

— followed in every case by `` logs:  <docker|container> logs -f <id> ``.

**Container→server reachability is a best-effort default, not a guarantee.**
A `build`-produced image is a standalone container, not joined to `setup`'s
compose network, so by default it reaches `baesrv` at
`http://host.docker.internal:<port>` (the host's published client port);
Docker launches add `--add-host` so this alias also resolves on Linux, not
just Docker Desktop. If detection guesses wrong for your engine/OS, override
with `--server-url`.

**Exit codes:**

| Exit | When |
|---|---|
| *(the child's own)* | `kind: "local"` only — `run`'s exit code is the harness process's own exit code, propagated directly. |
| `0` | `kind: "container"` — the container started successfully. |
| `1` | Readiness checks didn't pass and couldn't be auto-fixed (`readiness checks did not pass (see above) — run \`baectl ready <id>\` for the full report and fix guidance`); `--no-ready` was given with no `resolved.json` for `<id>`, or one missing a stored plaintext key; a required container env var is unresolved with no TTY to prompt; the `docker`/`container run` subprocess failed to start or exited non-zero. |
| `2` | No build `<id>` found under `--dir`. |

---

## `--fallback`

`--fallback <NAME>` (on `create profile` / `update profile`) takes a plain
`[providers]` registry name — the same kind of bare name `primary_provider`
and `--mcp-server` already take. Repeat it for multiple fallbacks; they are
tried in order after the primary fails. There is no compound spec, no
`provider:model` syntax, and no client-side validation against the live
registry (see [`create profile`](#baectl-create-profile)).

Example:

```sh
baectl create profile main anthropic-sonnet \
  --fallback anthropic-haiku --fallback openai-gpt
```

---

## Exit codes

Per `aspec/uxui/cli.md`'s convention (shared with `baesrv`):

| Code | Meaning |
|---|---|
| `0` | Success. |
| `1` | Runtime error — connection failure, or any admin API error response (all RFC 7807 bodies), an unexpected/unparseable response body, an invalid `--dir` on `build`/`ready`/`run` (`` --dir <path> does not exist or is not a directory ``, checked once by canonicalizing it), or (`ready` only) one or more **blocking** (`✗`) checks. |
| `2` | Usage error — a missing required positional or unknown flag (clap reports these itself). |
| `3` | **`baectl ready` only** — every failing check is auto-fixable (`⚠` only, no `✗`): `baectl run <id>` or `ready <id> --fix` resolves them without prompting. Nothing but `ready` ever produces this code. |

All errors print `baectl: <message>` to **stderr**; stdout carries only
command results, so it stays scriptable.

---

## Errors

Every non-2xx admin API response is an RFC 7807 problem document (see
[Admin API → Errors](02-admin-api.md#errors)). `baectl` matches on the `type`
slug and maps it to a clean, actionable message (always exit `1`):

| `type` | `baectl` message |
|---|---|
| `unauthorized` | The three-option auth guidance block (see below). |
| `profile_in_use` | The API's `detail`, plus: run `baectl list keys` to find the profile's active keys, then `baectl delete key <id>` for each, then retry. |
| `profile_unavailable` | The API's `detail`, plus `(the referenced profile does not exist or was deleted)`. |
| `bad_request`, `not_found`, `duplicate_name`, any other/unknown slug | The API's `detail` verbatim — already specific about the offending field/id/name. |

**No token resolved, and the server enforces admin auth** — `baectl` prints:

```
baectl: admin API rejected the request: no valid admin token was supplied (401 unauthorized).
Provide an admin token in one of these ways (highest precedence first):
  1. --admin-token <token>   (or the BAE_ADMIN_TOKEN env var)
  2. --admin-key-file <path> (or the BAE_ADMIN_KEY_FILE env var)
  3. the default key file at /var/lib/bae/admin-key.pem, which baesrv writes on
     first boot — reachable automatically when baectl runs inside the same
     container as baesrv (e.g. `docker exec bae baectl …`).
```

**Server unreachable** (wrong `--admin-addr`, server not running, admin port
not yet bound):

```
baectl: could not connect to admin API at 127.0.0.1:8081 — is baesrv running and is --admin-addr correct?
```

**Version skew** (a 2xx response body that doesn't parse as expected JSON —
`baectl` and `baesrv` built from different versions):

```
baectl: unexpected response from admin API — check that baectl and the server are the same version
```

---

## See also

- [Admin API reference](02-admin-api.md) — the underlying REST surface `baectl` wraps.
- [Admin authentication guide](../guides/09-admin-authentication.md) — how the
  bootstrap key is created, rotated, disabled, and pre-provisioned.
- [Configuration reference](05-configuration.md) — every `BAE_*` env var,
  including the ones `baectl` reads.
- [Harness manifest reference](07-harness-manifest.md) — the `bae-harness.toml`
  schema `build`/`ready`/`run` act on.
- [Harness launchers reference](06-launchers.md) — the config files `build`
  generates for `--launcher schedule/api/webapp`.
- [Quickstart](../guides/00-quickstart.md#fastest-path-three-commands) —
  `setup` → `build` → `run` end to end.
- [`aspec/uxui/cli.md`](../../aspec/uxui/cli.md) — CLI design conventions
  shared by `baesrv` and `baectl`.
