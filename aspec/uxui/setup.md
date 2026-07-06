# Setup

## User installation

Download:
- Operators: pull the published Docker image (`docker pull ghcr.io/<org>/better-agent-server-engine`) or build it from source with `make image`.
- Agent developers: install a client library from its registry — `cargo add base-client`, `npm install @base-engine/client`, or `uv add base-client` — and point it at a running server.

Initial configuration:
- Start the server with a persistent volume: `docker run -p 8080:8080 -v base-data:/var/lib/base <image>`. First run creates the database, applies migrations, and prints a one-time bootstrap admin API key to stdout.
- Set `BASE_ADDR`/`BASE_DB_PATH`/`BASE_LOG` via environment only if the defaults don't fit; verify liveness with `GET /healthz`.
- With the bootstrap admin key, create per-developer `agent` keys via the API; developers configure their client with the server URL and their key (typically `BASE_URL`/`BASE_API_KEY` in their own environment).

Superuser access:
- The bootstrap admin key is the superuser credential: shown once, never stored in plaintext, intended to be rotated after creating a named admin key.
- If all admin keys are lost, recovery is via the CLI on the host with direct database access (`base-server key create --role admin`), which requires filesystem access to the volume — API access alone can never mint admin keys without an admin key.
