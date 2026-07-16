# Infrastructure

Deployment platform: other (single Docker container; anything that runs OCI images — a VM with docker, compose, or kubernetes)
Cloud platform: other (cloud-agnostic by design; no managed-cloud dependencies)
Automation: other (none yet — the deliverable is a Docker image; terraform modules may come later if a hosted offering emerges)

## Architecture:

Best practices:
- One server container + one volume: SQLite lives at /var/lib/bae inside the container — always mount a named volume or host path there, and back it up (the database file is the entire state of the system).
- Scale vertically, not horizontally: SQLite means exactly one server instance per database. Run one container per deployment; do not load-balance multiple instances over the same volume.
- Put a TLS-terminating reverse proxy in front (see architecture/security.md); keep the container on an internal network.
- Pin image tags to a semver (never `latest`) in production; upgrade deliberately (see devops/operations.md).

Security and RBAC:
- Run the container as the non-root `bae` user (the image default); no extra capabilities or host mounts beyond the data volume.
- Inject secrets (provider API keys) as environment variables from the platform's secret store; never bake them into images or compose files committed to git.
- Restrict volume/file access to the container: the SQLite file contains all agent history and hashed keys.

## The `bae-max` variant: a deliberate exception to one-process-per-container

`bae-max` (built from `Dockerfile.max`, see
[docs/guides/10-max-webapp.md](../../docs/guides/10-max-webapp.md)) is a second
image variant that additionally bundles MAX (`max/`), a browser dashboard.
Every other container this project ships runs exactly one supervised
process; `bae-max` is the one deliberate exception, because MAX has to run
alongside `baesrv` in the same container to reach the admin port over
loopback the same zero-configuration way `baectl` already does — putting it
in a second container would mean exposing the admin port across a network
boundary, which is exactly the thing this project's admin-port design avoids.

- `bae-max`'s entrypoint (`docker/bae-max-entrypoint.sh`) starts **both**
  `baesrv` and `max/server` as supervised child processes and forwards
  `SIGTERM`/`SIGINT` to both.
- **Either process dying takes the whole container down.** The entrypoint
  waits on whichever child exits first, then kills the other and exits with
  the first child's exit code. A `bae-max` container is never left running
  with only one of the two processes alive — that would look "healthy" from
  the outside (one of the two health checks would still pass) while the
  other half of the dashboard was silently dead. This failure-propagation
  behavior, not just "both processes start," is what the image's smoke test
  must verify.
- New port: MAX's own web+WS port (`BAE_MAX_ADDR`, default `0.0.0.0:3000`),
  exposed alongside the existing client port (`8080`). **The admin port
  (`8081`) is not newly exposed** — `max/server` reaches it over
  `127.0.0.1:8081` from inside the same container, exactly like `baectl`;
  `bae-max` adds no new network exposure to the admin port itself, only to
  MAX's own port. Because that port *is* reachable off-loopback, MAX gates
  every route behind its own login (see
  [docs/guides/10-max-webapp.md](../../docs/guides/10-max-webapp.md)).
- Same data volume, no new volume: MAX's self-generated password file and
  its per-profile observer-key file both live on the same `/var/lib/bae`
  volume operators already mount, alongside the admin key file.
- Choosing `bae-max` over the default image is opt-in per deployment — an
  operator who wants only the API server keeps running the default image
  unchanged; nothing about the default image's one-process posture changes.

## The three launcher base images: extended, not run standalone

Work item 0014 adds three new image variants — `bae-launcher-schedule`,
`bae-launcher-api`, and `bae-launcher-webapp` — published to the same GHCR
repository as suffixed tags (see devops/cicd.md's Publishing section). They
are categorically different from every other image this project ships:
**they are base images meant to be `FROM`-extended by an agent developer's own
Dockerfile, not run standalone in production.** A bare `docker run` on one of
these images starts a launcher with zero configured agents — useful only to
confirm the base image itself works, never a real deployment.

- **All three, including `bae-launcher-webapp`, are one-process-per-container
  — the opposite conclusion from `bae-max` above, not an extension of its
  exception.** `bae-launcher-webapp` reuses the exact same `baeapi` binary as
  `bae-launcher-api` unmodified; there is no second webapp backend process to
  supervise, no signal-forwarding between two processes, and no
  dual-process entrypoint script like `docker/bae-max-entrypoint.sh`. The
  webapp's frontend is served as static assets by `baeapi` itself
  (`BAE_LAUNCHER_WEBAPP_STATIC_DIR`), not by a separate process reaching the
  admin port the way `max/server` does — so the reasoning that forced
  `bae-max` into its one deliberate exception (MAX needing to reach the admin
  port over loopback inside the same container) simply does not apply here.
- **A hung or crashed child agent invocation never takes the launcher process
  itself down — an explicit, deliberate divergence from `bae-max`'s "either
  process dying takes the whole container down" model documented above.** A
  launcher typically hosts many independent agents/schedules in one
  container (`[[agents]]` is always an array, even for a single agent); one
  misbehaving agent's hung or crashed invocation must not stop the scheduler's
  other timers, or the API/webapp server's other routes, from continuing to
  work. Concretely: `launcher-core::spawn_and_stream` reports a spawn failure
  as an in-stream terminal value, never as a process-level `Err` that could
  propagate into a crash; a wedged child is force-reaped via
  `kill_on_drop(true)` when its consuming task is dropped (at shutdown, or —
  for the API/webapp launcher — when a client disconnects), never by taking
  the launcher process down to clean it up. The same guarantee bounds
  shutdown: both binaries cap their graceful drain
  (`BAE_SCHEDULES_SHUTDOWN_TIMEOUT` / `BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT`,
  default 30s each), so a hung child holding an in-flight invocation or
  trigger request open delays `SIGTERM` by at most the bound — it can never
  keep the container alive indefinitely.
- **No new volume.** Launchers have no local persistence in V1 — no database,
  no run history, no file-based output retention. Every agent's captured
  stdout/stderr is forwarded, `[name]`-prefixed, to the launcher's own
  stdout/stderr, so `docker logs` is the shared attributed log surface for
  all three images; the API/webapp launcher *additionally* streams the same
  lines back in each trigger's own response body (or the webapp UI rendering
  it). Nothing is mounted; there is no `/var/lib/bae` equivalent for these
  images.
- **New ports, distinct from every existing image's ports.** `bae-launcher-api`
  and `bae-launcher-webapp` listen on `BAE_LAUNCHER_API_ADDR`, default
  `0.0.0.0:9090` — chosen specifically to differ from `baesrv`'s `8080`/`8081`
  so a launcher and a `baesrv`/`bae-max` container can coexist on the same
  host/network without a collision. `bae-launcher-schedule` opens **no port at
  all** — no HTTP surface exists to expose, by design; its liveness is
  process-level only (`docker ps`, the container's own exit status).
- **Security posture, same reverse-proxy guidance as every other port.** The
  API/webapp launcher's trigger routes are open by default
  (`BAE_LAUNCHER_API_TOKEN` unset) — see
  [architecture/security.md](../architecture/security.md) and
  [docs/guides/11-harness-launchers.md](../../docs/guides/11-harness-launchers.md).
  A bearer token is optional hardening on top of, never a substitute for, the
  same TLS-terminating-reverse-proxy/internal-network requirement every other
  bae port already has.
- Choosing to build from one of these base images is entirely opt-in per
  agent developer — nothing about the default `bae`/`bae-max` images changes,
  and an operator who never touches `launchers/` sees zero behavior
  difference in anything else this project ships.
