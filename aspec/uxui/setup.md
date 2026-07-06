# Setup

## User installation

Download:
- Operators: pull the published Docker image (`docker pull ghcr.io/prettysmartdev/better-agent-engine`) or build it from source with `make image`.
- Agent developers: install a client library from its registry — `cargo add bae-rs`, `npm install @prettysmartdev/bae-ts`, or `uv add bae-py` — and point it at a running server.

Initial configuration:
- Start the server with a persistent volume: `docker run -p 8080:8080 -v bae-data:/var/lib/bae <image>`. First run creates the database, applies migrations, and prints a one-time bootstrap admin API key to stdout.
- Set `BAE_ADDR`/`BAE_DB_PATH`/`BAE_LOG` via environment only if the defaults don't fit; verify liveness with `GET /healthz`.
- With the bootstrap admin key, create per-developer `agent` keys via the API; developers configure their client with the server URL and their key (typically `BAE_URL`/`BAE_API_KEY` in their own environment).

Superuser access:
- The bootstrap admin key is the superuser credential: shown once, never stored in plaintext, intended to be rotated after creating a named admin key.
- If all admin keys are lost, recovery is via the CLI on the host with direct database access (`baesrv key create --role admin`), which requires filesystem access to the volume — API access alone can never mint admin keys without an admin key.
