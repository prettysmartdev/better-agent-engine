# Setup

## User installation

Download:
- Operators: pull the published Docker image (`docker pull ghcr.io/prettysmartdev/better-agent-engine`) or build it from source with `make image`.
- Agent developers: install a client library from its registry — `cargo add bae-rs`, `npm install @prettysmartdev/bae-ts`, or `uv add bae-py` — and point it at a running server.

Initial configuration:
- Start the server with a persistent volume: `docker run -p 8080:8080 -v bae-data:/var/lib/bae <image>`. First run creates the database, applies migrations, and generates the bootstrap admin API key — written to `BAE_ADMIN_KEY_FILE` on the data volume (default `/var/lib/bae/admin-key.pem`, `0600` permissions), **not** printed to stdout/logs (a log line can end up shipped to a log aggregator and can't be read programmatically the moment the container starts; a file on the data volume can be).
- Set `BAE_ADDR`/`BAE_DB_PATH`/`BAE_LOG` via environment only if the defaults don't fit; verify liveness with `GET /healthz`.
- Read the bootstrap admin key with `docker exec bae cat /var/lib/bae/admin-key.pem`, or use [`baectl`](../../docs/reference/baectl.md), which reads that same file automatically with zero configuration when run via `docker exec`/`container exec`. With the bootstrap admin key, create per-developer `agent` keys via `baectl create key`/the admin API; developers configure their client with the server URL and their key (typically `BAE_URL`/`BAE_API_KEY` in their own environment).
- See [Admin authentication](../../docs/guides/admin-authentication.md) for the full bootstrap/rotation/multi-replica-provisioning walkthrough.

Superuser access:
- The bootstrap admin key is the superuser credential: its plaintext exists only in `BAE_ADMIN_KEY_FILE` on the data volume (only an Argon2id hash is stored in the database); rotate it after initial setup with `baesrv --rotate-admin-key`, which revokes the old key and writes a fresh one to the same file. Because this volume now holds live credential material, restrict its access accordingly (see devops/infrastructure.md).
- If the current admin key is lost (but the data volume and the ability to restart `baesrv` are not), recovery is `baesrv --rotate-admin-key`: it does not require presenting an existing admin key, only filesystem/process access sufficient to restart the server with that flag — API access alone can never mint or recover admin keys without an admin key. `baectl` then auto-discovers the freshly written key with no further configuration.
- **Remaining gap, explicitly out of scope for work item 0004:** a `baesrv key create --role admin` recovery subcommand for the case where *even the key file itself* is lost or corrupted (direct DB/filesystem recovery with no working key file to rotate) is not yet built. `--rotate-admin-key` covers the "key lost, file/volume intact" case; the "file/volume itself unusable" case remains unaddressed and should not be assumed to work.
