# Better Agent Engine (BAE)

A stateful server and customizable client libraries/harnesses for building
useful AI agents. The server owns all durable state (agents, sessions,
events) in SQLite; the client libraries — Rust, TypeScript, and Python —
give agent developers an idiomatic harness in their language of choice while
staying thin, stateless, and interchangeable.

> Status: bootstrapped, pre-functional. The codebases, tooling, and project
> specification are in place; the first feature work items live in
> [`aspec/work-items/`](aspec/work-items/).

## Repository layout

| Path | Component | Language | Published as |
|------|-----------|----------|--------------|
| [`server/`](server/) | Stateful agent server (HTTP + SQLite) | Rust | Docker image / `baesrv` binary |
| [`client-rust/`](client-rust/) | Client library & harness | Rust | `bae-rs` (crates.io) |
| [`client-typescript/`](client-typescript/) | Client library & harness | TypeScript | `@prettysmartdev/bae-ts` (npm) |
| [`client-python/`](client-python/) | Client library & harness | Python | `bae-py` (PyPI) |
| [`aspec/`](aspec/) | Project specification: architecture, devops, UX, agents, work items | — | — |

Each component is independently buildable, testable, versioned, and
publishable: every one has its own manifest, `Makefile`, and README.

## Local development

Everything runs in Docker via Make. The only host requirements are `docker`
and `make`.

```sh
make dev-image     # build the dev toolchain image (Rust, Node 22, Python/uv)
make build         # build all four components inside the container
make test          # run all tests
make lint          # clippy / tsc / ruff across components
make fmt           # format all components
make shell         # interactive shell in the dev container
```

Work on a single component with `<verb>-<component>`:

```sh
make test-server
make build-client-typescript
make lint-client-python
```

Inside the dev container (or on a host with the toolchains installed) you
can also work directly in a component directory: `make -C server test`.

Run `make help` for the full target list.

## Server

Build the production image and run it with a persistent data volume:

```sh
make image
docker run -p 8080:8080 -v bae-data:/var/lib/bae better-agent-engine
```

Configuration is environment-driven (see
[`aspec/devops/operations.md`](aspec/devops/operations.md)): `BAE_ADDR`,
`BAE_DB_PATH`, `BAE_LOG`, and provider credentials such as
`ANTHROPIC_API_KEY` are passed through the environment, never stored in the
database.

## Project specification (`aspec/`)

The `aspec/` tree is the source of truth for how this project is designed
and operated: [foundation](aspec/foundation.md),
[architecture](aspec/architecture/), [devops](aspec/devops/),
[UX](aspec/uxui/), [agents](aspec/genai/agents.md), and
[work items](aspec/work-items/). New feature work starts as a work item
following [`aspec/work-items/0000-template.md`](aspec/work-items/0000-template.md).

## License

Apache-2.0 — see [LICENSE](LICENSE).
