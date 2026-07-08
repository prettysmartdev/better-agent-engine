<p align="center">
  <strong>Local/remote hybrid agents with ultra customizable client harnesses.</strong> <br>
  Build multi-user agents with TypeScript, Python, or Rust to fit your exact needs.<br>
  <br>
  <img src="./docs/images/bae_logo.svg" width="620" alt="bae logo">
</p>

<p align="center">
  <img src="https://github.com/prettysmartdev/better-agent-engine/actions/workflows/test.yml/badge.svg">
</p>

### With Better Agent Engine (BAE), you will:
- Build hybrid agents that execute tools locally OR remotely with durable, replayable sessions.
- Attach mutiple clients to the same session simultaneously for multiplayer collaboration.
- Create ultra-customized agent harnesses to accomplish your specific goals with ease.
- Stay comfortable in your language of choice: TypeScript, Python or Rust.
- Use builtin primitives such as sandboxes, CLI subagents (Claude, Codex, etc.), and MCP servers.

### How does it work?
- `baesrv` is BAE's tiny Rust server that handles LLM provider connections, durable session logs, MCP servers, tool call loops, and auth.
- BAE client libraries (TS, Python, Rust) let you quickly and easily build a custom agent harness by defining tools, sandboxes, prompts, and full lifecycle control of the agent loop with hooks.
- Your harness (local) connects to `baesrv` (remote) so local tools can collaborate with remote MCP servers and LLM providers to accomplish tasks with the full power of both environments.
- Multiple harness instances can join the same session in parallel to drive prompts, provide tools running in different locations, with each client getting a live stream of the entire session for true multi-user support.
- `baectl` handles server configuration, operations, and client/profile/session CRUD.

# Local and Cloud working together to get complex work done for you and your team. 

> Status: alpha. The codebase, tooling, and project specification are in place; APIs and SDKs will likely change.

## Quickstart

```sh
docker run -p 8080:8080 -v bae-data:/var/lib/bae better-agent-engine
curl http://localhost:8080/healthz
```

Then create a profile and client key via the admin API, exchange the key
for a session, and send a message. Full walkthrough:
[`docs/quickstart.md`](docs/quickstart.md).

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
