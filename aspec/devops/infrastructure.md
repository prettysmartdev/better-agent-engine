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
[docs/guides/max-webapp.md](../../docs/guides/max-webapp.md)) is a second
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
  [docs/guides/max-webapp.md](../../docs/guides/max-webapp.md)).
- Same data volume, no new volume: MAX's self-generated password file and
  its per-profile observer-key file both live on the same `/var/lib/bae`
  volume operators already mount, alongside the admin key file.
- Choosing `bae-max` over the default image is opt-in per deployment — an
  operator who wants only the API server keeps running the default image
  unchanged; nothing about the default image's one-process posture changes.
