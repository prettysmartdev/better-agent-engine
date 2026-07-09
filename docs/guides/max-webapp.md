# MAX Webapp

MAX is a browser dashboard for administering and observing a running `bae`
server: create/list/revoke profiles and client keys, and watch any session's
events render live as an interactive graph — all from a desktop, tablet, or
phone browser, with no `docker exec`, no Rust toolchain, and no hand-assembled
admin-API `curl` calls.

MAX ships only in the **`bae-max`** container image variant, a second image
alongside the default one. It is not a separate deployment: `bae-max` runs
`baesrv` and MAX's own web server (`max/server`) as two processes in the same
container, and MAX talks to `baesrv`'s admin and client ports exactly the way
`baectl` does — over loopback, inside the container, using the same
auto-generated admin key.

> **Security posture, read this first.** MAX's web port is the **first**
> thing in this project that exposes admin-equivalent capability (create/
> revoke keys and profiles, observe every session) off of loopback. Every
> other admin surface (`baectl`, the raw admin API) is reachable only via
> `docker exec`/a loopback connection. MAX's own login gate (see
> [MAX's password](#max-password-not-the-admin-key) below) exists specifically
> to cover that gap, and — unlike `--dangerously-disable-admin-auth` on the
> admin port — **there is no flag to disable it.** The admin port's
> zero-auth-by-default posture is defensible because it's loopback-only; the
> web port you're about to expose to your network is not, so this gate has
> no escape hatch.

---

## Pulling and running `bae-max`

Pull the `-max`-suffixed tag from the same image repository as the default
image — it is not a separate repository:

```sh
docker pull ghcr.io/prettysmartdev/better-agent-engine:latest-max
# or pin a version:
docker pull ghcr.io/prettysmartdev/better-agent-engine:0.3.0-max
```

Or build it from source:

```sh
make image-max
```

Run it the same way as the default image, mounting the same data volume, but
also publish MAX's web port (`3000` by default) alongside the client port:

```sh
docker run \
  -p 8080:8080 -p 3000:3000 \
  -v bae-data:/var/lib/bae \
  ghcr.io/prettysmartdev/better-agent-engine:latest-max
```

Do **not** publish the admin port (`8081`) — `bae-max` does not expose it,
and it doesn't need to be: `max/server` reaches it over `127.0.0.1:8081`
from inside the same container, exactly like `baectl`. Publishing MAX's port
(`3000`) is the only new network exposure this image variant adds.

The two processes are supervised together: if `baesrv` or `max/server` dies,
the container exits — there's no "healthy-looking" container running only
one of the two. See
[devops/infrastructure.md](../../aspec/devops/infrastructure.md) for why.

Once it's up, open `http://<host>:3000` in a browser.

---

## First boot: finding the MAX password

On first boot, if no password is configured, `max/server` generates a random
one and writes it to `/var/lib/bae/max-password.pem` (`0600` permissions) —
the same self-generate-and-write-to-a-file pattern the admin key already
uses. Read it the same way you'd read the admin key:

```sh
docker exec bae cat /var/lib/bae/max-password.pem
```

Enter it on MAX's login page. MAX issues a signed session cookie on success;
every other MAX route (REST and the live-session WebSocket) requires that
cookie.

To set the password explicitly instead of letting MAX generate one, set
`BAE_MAX_PASSWORD` (highest precedence) or point `BAE_MAX_PASSWORD_FILE` at
your own file — the same explicit-value > file-path > default-file-path
precedence `baectl`'s admin-token resolution already uses.

### MAX password not the admin key

**The MAX password and the admin key are two different secrets.** The admin
key (`/var/lib/bae/admin-key.pem`) authenticates directly against
`baesrv`'s admin API; the MAX password only unlocks the MAX web UI, which
then uses its *own*, separately-held admin key on your behalf — the browser
never sees it. Don't assume rotating one rotates the other, and don't reuse
one file's contents as the other's value.

### Rotating the MAX password

Delete the password file and restart `max/server` (restarting the container
is the simplest way to do this):

```sh
docker exec bae rm /var/lib/bae/max-password.pem
docker restart bae
```

On the next boot, MAX finds no password file and self-generates a fresh one,
exactly as on first boot. Because the session cookie is a stateless HMAC
token signed with a secret derived from the password, rotating the password
changes the signing secret — **every previously issued cookie stops
authenticating immediately.** Every browser with an open MAX session is
forced back to the login page. This is the expected rotation behavior, the
same posture `--rotate-admin-key` already establishes for the admin key: the
old plaintext (and everything it signed) stops working the moment you
rotate.

---

## Auto-created observer keys and profile deletion

To watch a session's live events, MAX needs a client key scoped to that
session's profile — it never uses the admin key for this, since the admin
key can't authenticate on the client port. The first time you ask MAX to
observe a session under a given profile, MAX mints itself a client key named
`max-observer-<profile_id>` via the admin API and reuses it for every later
session under that same profile, so it does not mint a new key per session.

These keys show up in the **Keys** tab like any other key, badged
**"auto-created by MAX"** so you don't mistake them for a key you created
yourself.

**This has one sharp edge worth knowing up front: `DELETE
/admin/v1/profiles/{id}` returns `409 profile_in_use` while any active key
references that profile** — including MAX's own observer key. Because MAX
never automatically revokes an observer key once it creates one, a profile
MAX has ever been asked to observe can become permanently undeletable until
an operator notices and manually revokes MAX's key for it, from the Keys
tab or `DELETE /admin/v1/keys/{id}`. There's no reliable signal for "MAX no
longer needs to observe this profile," so this isn't cleaned up
automatically — look for the "auto-created by MAX" badge if a profile
delete unexpectedly fails with `409`.

---

## Walkthrough: Keys → Profiles → a live session's graph

1. **Log in** with the MAX password (see above). You land on the **Keys**
   tab.
2. **Keys tab**: list existing client keys, or create one — pick a name and
   a profile from the picker. The plaintext key is shown exactly once, with
   a "copy this now" warning; there's no way to retrieve it again
   afterward, the same one-time-display rule the admin API itself enforces.
   Revoke a key from the same list.
3. **Profiles tab**: create a profile by picking its primary provider,
   fallback providers, and MCP servers from pickers populated from the
   currently configured registries — you can't typo a provider or MCP
   server name here, since it's never free text. `allowed_tools` is still
   free text (client-declared tool names aren't something the server knows
   about in advance).
4. **Sessions tab**: lists sessions, filtered to `open` by default with a
   toggle to include `closed`/`error` sessions too. Click any row to open
   its detail view.
5. **The event graph**: nodes are events, laid out in chronological order
   and color/shape-coded by event type (client turns, provider requests/
   responses, tool calls/results, MCP exchanges, session lifecycle, join/
   driver events). For an open session, new nodes append live as they
   happen — MAX is watching the same event stream you'd get from
   `session.subscribe`, just rendered. Click (or, from the keyboard, focus
   and press Enter/Space on) any node to open a detail panel with its full
   JSON payload.

MAX only ever **observes** — it never registers as a driver and never sends
a message into a session. Watching a session in MAX never competes with,
interferes with, or is visible to the session's real driver as anything
other than another observer.

---

## See also

- [Admin API reference](../reference/admin-api.md) — the endpoints MAX's
  Keys/Profiles/Sessions tabs are thin proxies over, including the two
  session-visibility routes MAX depends on.
- [Admin authentication](admin-authentication.md) — the admin key lifecycle
  MAX reuses internally.
- [Event Streaming](event-streaming.md) — the underlying `session.subscribe`
  model MAX's live event graph is built on.
- [aspec/devops/infrastructure.md](../../aspec/devops/infrastructure.md) —
  the `bae-max` container's dual-process requirement.
