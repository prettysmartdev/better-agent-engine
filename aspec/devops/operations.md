# Operations

## Installing and running
Installation:
- Pull the published Docker image (or build locally with `make image`). No other artifacts are needed to run a server.
- Client libraries install from their registries: `cargo add base-client`, `npm install @base-engine/client`, `uv add base-client`.

Setup and run:
- `docker run -p 8080:8080 -v base-data:/var/lib/base better-agent-server-engine`
- First run creates the SQLite database, applies migrations, and prints a bootstrap admin API key (see uxui/setup.md). Health is at `GET /healthz`.

Environment variables:
- `BASE_ADDR` — listen address (default `0.0.0.0:8080`)
- `BASE_DB_PATH` — SQLite database path (default `/var/lib/base/base.db` in the image)
- `BASE_LOG` — tracing filter (default `info`)
- Provider credentials (e.g. `ANTHROPIC_API_KEY`) are passed through the environment of whichever process calls the provider.

Secrets:
- API keys are stored only as salted hashes in SQLite; plaintext is shown once at creation.
- Provider keys and the bootstrap admin key come from the environment / operator, are never written to the database or logs, and should be rotated on any suspicion of exposure.

## Ongoing operations

Version upgrades/downgrades:
- Upgrade by stopping the container and starting the new image tag against the same volume; migrations apply automatically on startup.
- Migrations are forward-only, so downgrade = restore: snapshot the database file before every upgrade (stop the container or use `sqlite3 .backup`), and roll back by restoring the snapshot with the old image.

Database migrations:
- Migrations are embedded in the server binary, numbered sequentially, and applied transactionally at startup; the server refuses to start against a database newer than itself.
- Never edit a shipped migration — append a new one. Schema changes within an API version must be backward compatible with that version's wire contract.
