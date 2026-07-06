# base-server

The Better Agent Server Engine server: a stateful HTTP service (Rust) that
owns all durable agent state — agents, sessions, events, runs — in SQLite.
Clients ([`client-rust`](../client-rust/),
[`client-typescript`](../client-typescript/),
[`client-python`](../client-python/)) talk to it over a versioned REST API
(`/api/v1`); see [`aspec/architecture/apis.md`](../aspec/architecture/apis.md).

## Develop

From the repo root (in Docker): `make test-server`, `make run`.

Directly in this directory (dev container or a host with Rust installed):

```sh
make build   # cargo build
make test    # cargo test
make lint    # clippy -D warnings + fmt --check
make run     # run the server
```

## Deploy

Built and shipped as a Docker image via the root [`Dockerfile`](../Dockerfile):
`make image` from the repo root. Configuration is via environment variables
(`BASE_ADDR`, `BASE_DB_PATH`, `BASE_LOG`); see
[`aspec/devops/operations.md`](../aspec/devops/operations.md).
