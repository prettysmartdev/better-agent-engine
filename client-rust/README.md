# base-client (Rust)

Rust client library and customizable agent harness for the
[Better Agent Server Engine](../README.md). Thin and stateless by design:
durable agent state lives on the [server](../server/), and this crate gives
Rust programs an idiomatic way to drive it. Feature-parity is maintained with
the [TypeScript](../client-typescript/) and [Python](../client-python/)
clients.

## Develop

From the repo root (in Docker): `make test-client-rust`.

Directly in this directory:

```sh
make build   # cargo build
make test    # cargo test
make lint    # clippy -D warnings + fmt --check
```

## Publish

Released independently to crates.io as `base-client` (see
[`aspec/devops/cicd.md`](../aspec/devops/cicd.md)). `Cargo.toml` has
`publish = false` until the first release is cut.
