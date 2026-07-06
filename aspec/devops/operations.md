# Operations

## Installing and running
Installation:
- Pull the published Docker image (or build locally with `make image`). No other artifacts are needed to run a server.
- Client libraries install from their registries: `cargo add bae-rs`, `npm install @prettysmartdev/bae-ts`, `uv add bae-py`.

Setup and run:
- `docker run -p 8080:8080 -v bae-data:/var/lib/bae better-agent-engine`
- First run creates the SQLite database, applies migrations, and prints a bootstrap admin API key (see uxui/setup.md). Health is at `GET /healthz`.

Environment variables:
- `BAE_ADDR` — client-facing listen address (default `0.0.0.0:8080`). Plain HTTP; TLS terminates at an upstream proxy.
- `BAE_ADMIN_ADDR` — admin-only listen address (default `127.0.0.1:8081`). Must be a loopback address — the server refuses to start otherwise, so the admin surface is never reachable off-host.
- `BAE_DB_PATH` — SQLite database path (default `/var/lib/bae/bae.db` in the image)
- `BAE_LOG` — tracing filter (default `info`)
- `BAE_TLS_ENABLED` — whether an upstream proxy terminates TLS (default `false`). Informational only: the container always speaks plain HTTP internally.
- `BAE_SHUTDOWN_TIMEOUT` — seconds to drain in-flight requests on SIGTERM before forcing shutdown (default `30`).
- Invalid values are usage errors (exit code 2); an unwritable `BAE_DB_PATH` or an in-use admin port is a runtime error (exit code 1) reported before the server begins serving.
- Provider credentials (e.g. `ANTHROPIC_API_KEY`) are passed through the environment of whichever process calls the provider.

Secrets:
- API keys are stored only as Argon2id salted hashes in SQLite; plaintext is shown once at creation and then discarded.
- Argon2id parameters: memory 64 MiB, iterations (time cost) 3, parallelism 1, output 32 bytes. These are embedded in the stored PHC string, so existing hashes remain verifiable after a parameter change. To tune for your hardware: raise memory cost first (more GPU-resistant), then iterations; parallelism can be increased on multi-core verifiers.
- Provider keys and the bootstrap admin key come from the environment / operator, are never written to the database or logs, and should be rotated on any suspicion of exposure.

## Ongoing operations

Version upgrades/downgrades:
- Upgrade by stopping the container and starting the new image tag against the same volume; migrations apply automatically on startup.
- Migrations are forward-only, so downgrade = restore: snapshot the database file before every upgrade (stop the container or use `sqlite3 .backup`), and roll back by restoring the snapshot with the old image.

Database migrations:
- Migrations are embedded in the server binary, numbered sequentially, and applied transactionally at startup; the server refuses to start against a database newer than itself.
- Never edit a shipped migration — append a new one. Schema changes within an API version must be backward compatible with that version's wire contract.
