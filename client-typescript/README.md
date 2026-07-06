# @base-engine/client (TypeScript)

TypeScript client library and customizable agent harness for the
[Better Agent Server Engine](../README.md). Thin and stateless by design:
durable agent state lives on the [server](../server/), and this package gives
Node.js/TypeScript programs an idiomatic way to drive it. Feature-parity is
maintained with the [Rust](../client-rust/) and [Python](../client-python/)
clients.

Requires Node.js ≥ 20.

## Develop

From the repo root (in Docker): `make test-client-typescript`.

Directly in this directory:

```sh
make build   # npm install + tsc
make test    # vitest
make lint    # tsc --noEmit + prettier --check
```

## Publish

Released independently to npm as `@base-engine/client` (see
[`aspec/devops/cicd.md`](../aspec/devops/cicd.md)). `package.json` is marked
`"private": true` until the first release is cut.
