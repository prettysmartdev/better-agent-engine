# base-client (Python)

Python client library and customizable agent harness for the
[Better Agent Server Engine](../README.md). Thin and stateless by design:
durable agent state lives on the [server](../server/), and this package gives
Python programs an idiomatic way to drive it. Feature-parity is maintained
with the [Rust](../client-rust/) and [TypeScript](../client-typescript/)
clients.

Requires Python ≥ 3.10. Managed with [uv](https://docs.astral.sh/uv/).

## Develop

From the repo root (in Docker): `make test-client-python`.

Directly in this directory:

```sh
make install   # uv sync
make test      # pytest
make lint      # ruff check + format check
make build     # sdist + wheel into dist/
```

## Publish

Released independently to PyPI as `base-client` (see
[`aspec/devops/cicd.md`](../aspec/devops/cicd.md)). The
`Private :: Do Not Upload` classifier stays in `pyproject.toml` until the
first release is cut.
