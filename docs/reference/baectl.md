# baectl Reference

`baectl` is a command-line HTTP client for the [admin API](admin-api.md)
(`/admin/v1/*`). It ships as a static binary at `/usr/local/bin/baectl` inside
both the dev and production images, alongside `baesrv`. Run it with
`docker exec`/`container exec` against a running container — it needs no Rust
toolchain and no network access to build or install.

```sh
docker exec bae baectl create profile main anthropic-sonnet \
  --allowed-tool get_current_time
```

`baectl` covers **profile and key management only**. It does not open
sessions or send messages — those hit the client port (8080) with a
client/session key and are documented in the [Client API](client-api.md) and
the [guides](../guides/quickstart.md).

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

See [Admin authentication](../guides/admin-authentication.md) for how the
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
| `primary_provider` | The **name** of a `[providers]` entry declared in `bae-config.toml` (e.g. `anthropic-sonnet`) — not a provider id or model. See [Configuration — `[providers]`](configuration.md#providers). |

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
[MCP Servers](../guides/mcp-servers.md#non-fatal-skips)); an unresolvable
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
[Admin API → Client Keys](admin-api.md#client-keys)). No flags.

**Output:** `revoked key <id>` on stdout.

**Errors:** `404 not_found`.

### `baectl auth create key`

```
baectl auth create key [--name <NAME>] [--out-dir <DIR>]
```

**This command never calls the admin API.** It is a local key-generation
utility for pre-provisioning one shared admin credential across multiple
independent server replicas. See
[Admin authentication → multi-replica walkthrough](../guides/admin-authentication.md#multi-replica-pre-provisioning)
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
[Key security](admin-api.md#key-security). Because Argon2id's PHC string
embeds its own salt and cost parameters, the hash `baectl` produces is
independently verifiable by the server with no shared code between the two
binaries.

**Output:** stdout prints the two file paths (scriptable); stderr prints
handling guidance for each file.

**Errors:** a runtime error (exit `1`) if either file cannot be written
(e.g. `--out-dir` doesn't exist or isn't writable).

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
[Admin API → Errors](admin-api.md#errors)). `baectl` matches on the `type`
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

- [Admin API reference](admin-api.md) — the underlying REST surface `baectl` wraps.
- [Admin authentication guide](../guides/admin-authentication.md) — how the
  bootstrap key is created, rotated, disabled, and pre-provisioned.
- [Configuration reference](configuration.md) — every `BAE_*` env var,
  including the ones `baectl` reads.
- [`aspec/uxui/cli.md`](../../aspec/uxui/cli.md) — CLI design conventions
  shared by `baesrv` and `baectl`.
