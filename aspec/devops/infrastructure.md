# Infrastructure

Deployment platform: other (single Docker container; anything that runs OCI images — a VM with docker, compose, or kubernetes)
Cloud platform: other (cloud-agnostic by design; no managed-cloud dependencies)
Automation: other (none yet — the deliverable is a Docker image; terraform modules may come later if a hosted offering emerges)

## Architecture:

Best practices:
- One server container + one volume: SQLite lives at /var/lib/base inside the container — always mount a named volume or host path there, and back it up (the database file is the entire state of the system).
- Scale vertically, not horizontally: SQLite means exactly one server instance per database. Run one container per deployment; do not load-balance multiple instances over the same volume.
- Put a TLS-terminating reverse proxy in front (see architecture/security.md); keep the container on an internal network.
- Pin image tags to a semver (never `latest`) in production; upgrade deliberately (see devops/operations.md).

Security and RBAC:
- Run the container as the non-root `base` user (the image default); no extra capabilities or host mounts beyond the data volume.
- Inject secrets (provider API keys) as environment variables from the platform's secret store; never bake them into images or compose files committed to git.
- Restrict volume/file access to the container: the SQLite file contains all agent history and hashed keys.
