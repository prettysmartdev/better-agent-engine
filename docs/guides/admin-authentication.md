# Admin Authentication

Every route under `/admin/v1/*` requires an `Authorization: Bearer <admin_key>`
header. This guide covers how that key comes to exist, how to find it, how to
rotate or disable auth, and how to pre-provision one shared key across
multiple server replicas.

If you only need the day-to-day command surface, see the
[baectl reference](../reference/baectl.md) — `baectl` auto-discovers and uses
the admin key with no configuration when run inside the container. This guide
is about the key's lifecycle, not `baectl`'s command syntax.

---

## The bootstrap key (first boot)

On every `serve` startup, after the database opens and before either listener
binds, `baesrv` checks for an active admin key. On a brand-new deployment
there isn't one yet, so the server:

1. Generates a `bae_admin_<48 hex chars>` token from the OS CSPRNG (192 bits
   of entropy).
2. Stores only its Argon2id hash in SQLite (`role='admin'`) — the same hash
   parameters used for client keys, see
   [Admin API → Key security](../reference/admin-api.md#key-security).
3. Writes the **plaintext** token to `BAE_ADMIN_KEY_FILE`
   (default `/var/lib/bae/admin-key.pem`) with `0600` permissions — readable
   only by the container's non-root `bae` user.
4. Logs `INFO "no admin key found; generated new admin key, written to file"`
   — the key value itself is never logged.

This happens once. On every subsequent restart, the server finds the
existing admin row and does nothing — the key file is left untouched.

### Finding the key manually

`baectl`, run inside the container, reads this file automatically. To read
it yourself (e.g. to hand it to a script, or to inspect it):

```sh
docker exec bae cat /var/lib/bae/admin-key.pem
```

The file is a single line with a trailing newline — trim whitespace before
using it. To call the admin API directly with it:

```sh
ADMIN_KEY=$(docker exec bae cat /var/lib/bae/admin-key.pem)
docker exec bae curl -s http://127.0.0.1:8081/admin/v1/profiles \
  -H "Authorization: Bearer $ADMIN_KEY"
```

### `baectl` auto-discovery

Because `baectl` ships in the same image and reads the same file, no
`docker exec bae cat ...` step is actually needed in normal operation:

```sh
docker exec bae baectl list profiles
```

`baectl` resolves the token in this order: `--admin-token`/`BAE_ADMIN_TOKEN`,
then `--admin-key-file`/`BAE_ADMIN_KEY_FILE`, then the default path above.
See [baectl reference → Auto-configuration](../reference/baectl.md#auto-configuration)
for the full precedence.

---

## Rotating the key

Rotate the admin key with `--rotate-admin-key` on the next `serve` startup:

```sh
docker run ... ghcr.io/prettysmartdev/better-agent-engine:latest baesrv --rotate-admin-key
# or, restarting an existing container's process with an extra flag,
# however your deployment passes CLI args through.
```

On a rotation, the server:

1. Soft-deletes every currently active `role='admin'` row — the old
   plaintext stops authenticating immediately.
2. Deletes the existing `BAE_ADMIN_KEY_FILE` if present.
3. Generates fresh key material and writes it to `BAE_ADMIN_KEY_FILE`, exactly
   as on first boot.
4. Logs `INFO "admin key rotated"` with the file's path (never the key
   value).

**Rotation always mints brand-new material, even if a pre-provisioned hash
file is also present at `BAE_ADMIN_KEY_HASH_FILE`.** A rotation that silently
re-ingested the same stale hash would defeat the point of rotating — so the
hash file is deliberately ignored on this path. If you rotate a replica that
was set up via the [multi-replica flow](#multi-replica-pre-provisioning)
below, that replica now has its own distinct admin key and is no longer in
sync with the others; re-provision it deliberately if you want it back in
the shared-key group.

> **`--rotate-admin-key` has no environment-variable equivalent, by
> design.** Every other admin-auth flag has one (see
> [`aspec/uxui/cli.md`](../../aspec/uxui/cli.md)), but an env var for this
> one would rotate the key on **every** restart of a long-lived deployment —
> env vars tend to be baked into compose files or Kubernetes manifests and
> persist across restarts, which is exactly the surprising, unwanted
> behavior a one-shot operator action must avoid. Pass it as an explicit
> flag on the one startup where you actually mean to rotate.

Rotating and disabling auth in the same startup is a contradiction — rotating
a key that nothing will check — so `--rotate-admin-key` combined with
`--dangerously-disable-admin-auth` (flag or env) is a usage error, exit `2`,
checked before the database even opens.

---

## Disabling admin auth

```sh
baesrv --dangerously-disable-admin-auth
# or: BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH=1
```

This restores the admin port's original (pre-this-feature) behavior: no key
is generated or checked, and every `/admin/v1/*` route is open to anyone who
can reach it. The server logs a `WARN` on **every** boot with this flag set:

```
WARN admin API authentication is DISABLED (--dangerously-disable-admin-auth) — anyone able to reach the admin port has full control
```

so it is never silently forgotten in a long-running deployment.

**Do not use this in production.** Anyone able to `docker exec`/
`container exec` into the container, or reach `BAE_ADMIN_ADDR` directly (e.g.
over a misconfigured network), can then create keys, read profile configs,
and fully administer the server with no credential at all. This flag exists
for local development and CI, where the admin port is not reachable by
anyone but the developer running the container. Unlike `--rotate-admin-key`,
this flag **does** have an environment-variable equivalent
(`BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH`), because leaving auth off is a
standing deployment choice (e.g. baked into a dev-only compose file), not a
one-shot action — the asymmetry with `--rotate-admin-key` is intentional.

---

## Stale key file edge case

If `BAE_ADMIN_KEY_FILE` already exists on disk but no active `role='admin'`
row exists in the database — for example, an operator manually deleted the
admin row, or restored a database backup taken before the file existed — the
server does **not** trust that pre-existing file as a source of truth. It
never re-hashes a file it finds; it only ever *writes* that path itself. On
the next boot with no active admin row, the server follows its ordinary
bootstrap decision (ingest a hash file if present, else self-generate), which
means:

- **This silently overwrites the stale file with a freshly generated,
  different key.** Any script or operator relying on the old plaintext in
  that file will start getting `401`s with no warning beyond the new
  `INFO "no admin key found; generated new admin key, written to file"` log
  line.

If you find yourself in this situation (e.g. after restoring a backup),
delete the stale `BAE_ADMIN_KEY_FILE` yourself before restarting, or restore
the matching `keys` row alongside the backup, so the file and the database
stay in sync.

---

## Multi-replica pre-provisioning

`aspec/devops/infrastructure.md` runs one server instance per SQLite
database — so a multi-replica deployment is multiple independent servers,
each generating its **own** admin key by default. That's fine for a single
instance, but inconvenient if you want one admin credential that works
against every replica without logging into each one individually to read its
self-generated key.

`baectl auth create key` solves this: it generates one admin credential
locally (no network call, no server needed) and produces the two artifacts
each replica needs.

### Step 1 — generate the pair

Run this anywhere `baectl` is available — it doesn't need to reach a server:

```sh
baectl auth create key --name shared-admin --out-dir ./admin-key
```

This writes two files into `./admin-key/`:

- **`admin-key.pem`** — the plaintext `bae_admin_<random>` token. This is the
  **live credential** — treat it like a password. `0600` permissions.
- **`admin-key-hash.pem`** — a JSON document holding the Argon2id hash of
  that same token, plus its display prefix and `--name`. `0600` permissions,
  lower sensitivity (a one-way hash can't be turned back into the
  plaintext), but still worth keeping off of world-readable storage — anyone
  who can plant this file on a replica's volume before its first boot can
  make that replica accept the paired plaintext as admin.

```json
{
  "key_hash": "$argon2id$v=19$m=65536,t=3,p=1$<b64salt>$<b64hash>",
  "prefix": "bae_admin_1a2b",
  "name": "shared-admin"
}
```

### Step 2 — distribute the hash file to every replica

Before (or at) each replica's first boot, copy `admin-key-hash.pem` onto
**that replica's own persistent data volume**, at the path
`BAE_ADMIN_KEY_HASH_FILE` resolves to (default
`/var/lib/bae/admin-key-hash.pem`):

```sh
# repeat for every replica, before its first boot
docker cp admin-key/admin-key-hash.pem replica-N:/var/lib/bae/admin-key-hash.pem
```

On first boot, each replica finds this file, parses it, and inserts the hash
directly into its own `keys` table as a `role='admin'` row — **the server
never learns the plaintext in this path.** It logs
`INFO "admin key hash loaded from pre-provisioned file"` and does **not**
write `BAE_ADMIN_KEY_FILE` (it has nothing to write; it never had the
plaintext).

A malformed hash file (bad JSON, missing `key_hash`/`prefix`, or a
`key_hash` that isn't a valid Argon2id PHC string) is a startup usage error
(exit `2`) — caught once at boot rather than silently ignored.

### Step 3 — keep the plaintext wherever you run `baectl`/operate

Copy `admin-key.pem` (or its contents) to wherever `baectl` or operators run
requests from, at the path `BAE_ADMIN_KEY_FILE` resolves to on that machine —
or just pass it explicitly:

```sh
baectl --admin-addr replica-3.internal:8081 \
       --admin-key-file ./admin-key/admin-key.pem \
       list profiles
```

Because every replica independently hashed and stored the *same* plaintext's
hash, **one** plaintext key now authenticates against **all** of them — no
need to log into any individual replica to read a self-generated key.

### Why this works with no shared code

The hash file's Argon2id PHC string embeds its own salt and cost parameters.
`baectl`'s local Argon2id implementation and the server's are independent —
they don't share code or coordinate parameters out of band — but because
both sides implement the standard PHC Argon2id format, the server can verify
a hash `baectl` produced with no special-casing. This is proven by the
integration test that generates a pair with `baectl auth create key`, feeds
the hash into a freshly booted test server, and confirms the paired
plaintext authenticates.

---

## See also

- [baectl reference](../reference/baectl.md) — full command syntax for
  `auth create key` and every other subcommand.
- [Admin API reference](../reference/admin-api.md) — the REST surface this
  key authenticates against.
- [Configuration reference](../reference/configuration.md) — every env var
  mentioned above.
- [`aspec/architecture/security.md`](../../aspec/architecture/security.md) —
  the broader authentication/RBAC model.
