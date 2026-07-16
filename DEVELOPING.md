# Developing BAE

This guide covers building, testing, and operating BAE from source. If you
just want to *use* BAE, start with the [README](README.md) and the
[documentation](docs/README.md).

BAE is a monorepo of independently buildable components: the Rust server
(`server/`), three client libraries (`client-rust/`, `client-typescript/`,
`client-python/`), the admin CLI (`baectl/`), and the MAX web dashboard
(`max/`). Every component exposes the same Make verbs — `build`, `test`,
`lint`, `fmt`, `clean` — so local development and CI are one loop over
components.

## Requirements

Everything for local development runs in Docker via Make. The only host
requirements are **`docker`** (or Apple `container`) and **`make`** — no Rust,
Node, or Python toolchain on your host.

## Common tasks

```sh
make dev-image     # build the dev toolchain image (Rust, Node 22, Python/uv)
make build         # build all components inside the container
make test          # run all tests
make lint          # clippy / tsc / ruff across components
make fmt           # format all components
make shell         # interactive shell in the dev container
make help          # the full target list
```

## Working on one component

Every component honors the same verbs via `<verb>-<component>`:

```sh
make test-server
make build-client-typescript
make lint-client-python
make test-max
```

The components are `server`, `baectl`, `client-rust`, `client-typescript`,
`client-python`, and `max`.

Inside the dev container (or on a host with the toolchains installed) you can
also work directly in a component directory: `make -C server test`.

## Building and running the server

Build the production image locally and run it with a persistent data volume.
`make image` tags the local image `better-agent-engine:latest`:

```sh
make image
docker run -p 8080:8080 -v bae-data:/var/lib/bae better-agent-engine:latest
```

To build the **`bae-max`** image variant — which bundles `baesrv`, `baectl`,
and the MAX web dashboard into one container — use `make image-max` (tagged
`better-agent-engine:max`):

```sh
make image-max
docker run -p 8080:8080 -p 3000:3000 -v bae-data:/var/lib/bae better-agent-engine:max
```

Or build-and-run in one step with the detected engine: `make run/baesrv` or
`make run/baemax`.

> These are **local** build tags. The published images live at
> `ghcr.io/prettysmartdev/better-agent-engine` — `:latest` / `:<semver>` for the
> server and `:max` / `:<semver>-max` for the bae-max variant. Cutting a release
> that publishes them is documented in [RELEASING.md](RELEASING.md).

You can also run the server directly in the dev container during development:

```sh
make run            # runs baesrv in the dev container (port $PORT)
```

## Configuration

The server is entirely environment-driven — no config files are required to
run, and credentials are never stored in the database. Key variables (see
[`aspec/devops/operations.md`](aspec/devops/operations.md) and
[`docs/reference/05-configuration.md`](docs/reference/05-configuration.md) for the
complete list):

- `BAE_ADDR` — client-port bind address.
- `BAE_DB_PATH` — SQLite database path.
- `BAE_LOG` — log level/filter.
- `ANTHROPIC_API_KEY` (and other provider credentials) — passed through the
  environment, never persisted.

MCP servers and LLM providers are declared in a `bae-config.toml` file and
referenced by name from profiles; see
[`examples/bae-config/`](examples/bae-config/) for ready-to-run examples.

## Project specification (`aspec/`)

The `aspec/` tree is the source of truth for how this project is designed and
operated:

- [foundation](aspec/foundation.md) — purpose, personas, technical foundation.
- [architecture](aspec/architecture/) — design principles, APIs, security.
- [devops](aspec/devops/) — local dev, CI/CD, infrastructure, operations.
- [UX/UI](aspec/uxui/) — CLI, setup, interface, and experience constraints.
- [agents](aspec/genai/agents.md) — the example agents this repo ships.
- [work items](aspec/work-items/) — feature specifications.

New feature work starts as a work item following
[`aspec/work-items/0000-template.md`](aspec/work-items/0000-template.md).

## License

Apache-2.0 — see [LICENSE](LICENSE).
