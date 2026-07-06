# Work Item: Superseded

Title: Server skeleton — HTTP service, health endpoint, SQLite bootstrap
Issue: (none yet)

> **Merged into [0002-session-and-auth.md](0002-session-and-auth.md).** All content from this work item is fully incorporated there. Do not implement from this file.

## Summary:
- Turn the bootstrapped `server/` crate into a running service: an axum HTTP server with `GET /healthz` and `GET /api/v1/meta`, environment-driven configuration (`BASE_ADDR`, `BASE_DB_PATH`, `BASE_LOG`), and SQLite initialization with an embedded, forward-only migration runner (migration 0001 creating the schema-version table).

## User Stories

### User Story 1:
As a: Platform Operator

I want to:
run the server Docker image with a data volume and hit a health endpoint

So I can:
verify an instance is alive and correctly configured before pointing agent developers at it.

### User Story 2:
As a: Agent Developer

I want to:
query `GET /api/v1/meta` for the server version and supported API versions

So I can:
have my client library check compatibility at connect time.

## Implementation Details:
- Add `axum`, `tokio`, `rusqlite`, `serde`, `tracing`, `tracing-subscriber` to `server/Cargo.toml`.
- Structure per aspec/architecture/design.md: `api` (router, handlers), `store` (SQLite open + migration runner), `engine` (empty for now); `main.rs` stays a thin entrypoint that loads config from env and calls into the library.
- On startup: open/create the database at `BASE_DB_PATH`, apply pending migrations transactionally, refuse to start if the database is newer than the binary.
- `/healthz` returns 200 with no auth; `/api/v1/meta` returns `{version, api_versions}` (auth comes in a later work item).

## Edge Case Considerations:
- Missing or unwritable `BASE_DB_PATH` directory → clear startup error, non-zero exit.
- Invalid `BASE_ADDR` → usage error (exit code 2) per aspec/uxui/cli.md.
- Concurrent startup against the same database must not double-apply migrations (transactional migration runner).

## Test Considerations:
- Unit tests: config parsing from env, migration runner (fresh DB, up-to-date DB, future-versioned DB).
- Integration test: boot the server on an ephemeral port with a temp DB, assert `/healthz` and `/api/v1/meta` responses.
- All tests run offline via `make test-server`.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Verify the production image still builds (`make image`) since this introduces the first real binary behavior.
