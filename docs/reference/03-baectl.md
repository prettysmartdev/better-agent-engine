# baectl Reference

`baectl` is a command-line HTTP client for the [admin API](02-admin-api.md)
(`/admin/v1/*`). It ships as a static binary at `/usr/local/bin/baectl` inside
both the dev and production images, alongside `baesrv`. Run it with
`docker exec`/`container exec` against a running container — it needs no Rust
toolchain and no network access to build or install.

```sh
docker exec bae baectl create profile main anthropic-sonnet \
  --allowed-tool get_current_time
```

`baectl` covers **profile and key management**, plus one local scaffolding
command, [`baectl setup`](#baectl-setup), that generates a runnable
deployment (compose file/script, `.env`, `bae-config.toml`) before a server
exists to talk to. It does not open sessions or send messages — those hit the
client port (8080) with a client/session key and are documented in the
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
`mcp_servers`, `allowed_tools`, `created_at`, `updated_at`. Empty list fields
print `(none)`.

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
| *(same config flags as `create profile`)* | `--fallback`, `--mcp-server`, `--allowed-tool`, `--json`. |

> **`--name` is optional, filling a gap in the admin API.** `PUT
> /admin/v1/profiles/{id}` always requires a `name` in its body, but
> `update profile`'s positional signature has none. When `--name` is
> omitted, `baectl` first `GET`s the current profile and reuses its existing
> name, so a plain `baectl update profile <id> <primary_provider>` changes
> the provider reference without renaming. Pass `--name` to rename during
> the same replace.

Any repeatable flag left unset (`--fallback`, `--mcp-server`,
`--allowed-tool`) serializes as an explicit empty array in the `PUT` body —
a full replacement clears fields that aren't re-specified, exactly like a
direct `PUT` call would.

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
    "key_hash": "$argon2id$v=19$m=65536,t=3,p=1$<b64salt>$<b64hash>",
    "prefix": "bae_admin_1a2b",
    "name": "provisioned-admin"
  }
  ```

  Drop this file onto **every replica's** data volume at the path
  `BAE_ADMIN_KEY_HASH_FILE` resolves to, before that replica's first boot.

The token is generated with 192 bits of CSPRNG entropy (24 random bytes,
hex-encoded) and hashed with Argon2id using the exact same parameters as the
server (memory 64 MiB, iterations 3, parallelism 1, output 32 bytes) — see
[Key security](02-admin-api.md#key-security). Because Argon2id's PHC string
embeds its own salt and cost parameters, the hash `baectl` produces is
independently verifiable by the server with no shared code between the two
binaries.

**Output:** stdout prints the two file paths (scriptable); stderr prints
handling guidance for each file.

**Errors:** a runtime error (exit `1`) if either file cannot be written
(e.g. `--out-dir` doesn't exist or isn't writable).

### `baectl setup`

```
baectl setup [--dev] [--apple] [--dir <DIR>]
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
`baesrv` and which carries no `docker`/`container` client. `baectl` ships as a
self-contained static binary inside the image; copy it out once and run it on
the host:

```sh
cid=$(docker create ghcr.io/prettysmartdev/better-agent-engine:latest)
docker cp "$cid":/usr/local/bin/baectl ./baectl
docker rm "$cid" >/dev/null
./baectl setup
```

(Or use the `baectl` a source build produced.)

**Flags:**

| Flag | Description |
|---|---|
| `--dev` | Use the image tags a local `make image`/`make image-max` produces (`better-agent-engine:latest` / `:max`) instead of the published GHCR tags (`ghcr.io/prettysmartdev/better-agent-engine:latest` / `:max`). For contributors iterating on a local build. |
| `--apple` | Emit `bae-setup.sh` (a shell script driving Apple's `container` CLI) instead of `docker-compose.yml`. Both output modes read the same `.env`. |
| `--dir <DIR>` | Directory to read/write the three generated files in. Default `.` (current directory), mirroring `auth create key`'s `--out-dir` convention. |

No flag is required — `baectl setup` with no arguments still produces a
complete, working setup, consistent with every other `baectl`/`baesrv`
command's "no required flags" convention.

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

None of these four files are excluded by `.gitignore` **except** `.env`
(matched by the existing `.env`/`.env.*` entries — it holds live secrets).
`docker-compose.yml`, `bae-setup.sh`, and `bae-config.toml` are ordinary,
trackable files: a team that wants to commit its generated deployment
alongside the repo (or a subdirectory of it) can do so; `setup` does not
force that choice either way.

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
| `1` | `--dir` doesn't exist, isn't a directory, or isn't writable (checked before any prompt); an existing `bae-config.toml`/`.env`/launcher fails to parse on the Edit/summary path; the engine binary is missing from `PATH`; the engine exits non-zero while launching; the server never becomes healthy within the timeout; the in-container `baectl create profile`/`create key` call fails. |
| `2` | An unknown flag or invalid flag value (clap-level usage error, e.g. a non-existent flag). |

**Errors:** every failure prints `baectl: <message>` to stderr, matching
every other `baectl` command's convention.

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
| `1` | Runtime error — connection failure, or any admin API error response (all RFC 7807 bodies), or an unexpected/unparseable response body. |
| `2` | Usage error — a missing required positional or unknown flag (clap reports these itself). |

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
- [`aspec/uxui/cli.md`](../../aspec/uxui/cli.md) — CLI design conventions
  shared by `baesrv` and `baectl`.
