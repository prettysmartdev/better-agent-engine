# Project Foundation

Name: better-agent-server-engine (BASE)
Type: agent
Purpose: Provide a stateful server and customizable client libraries/harnesses for building useful AI agents. The server owns all durable agent state (agents, sessions, events, runs) in SQLite; thin, stateless client libraries in Rust, TypeScript, and Python let developers build and drive agents idiomatically in their language of choice.

# Technical Foundation

## Languages and Frameworks

### Backend (server/)
Language: Rust (stable, edition 2021)
Frameworks: axum + tokio (HTTP/async), rusqlite (SQLite storage), serde (serialization), tracing (logging)
Guidance:
- Ship as a single binary (`base-server`) and Docker image; SQLite is the only datastore.
- Keep all server logic in the library crate (`src/lib.rs` modules: api, store, engine); `main.rs` is a thin entrypoint.
- All configuration comes from environment variables (`BASE_*`); no config files required to run.

### Clients (client-rust/, client-typescript/, client-python/)
Language: Rust (edition 2021), TypeScript (Node ≥ 20), Python (≥ 3.10)
Frameworks: Rust — reqwest, serde, thiserror; TypeScript — zero runtime deps (built-in fetch), vitest for tests; Python — httpx, pydantic, pytest, managed with uv
Guidance:
- Clients are libraries, not frameworks: thin, stateless, and interchangeable; all durable state lives on the server.
- Maintain feature parity across the three clients — every API capability and harness concept exists in all three, named idiomatically per language.
- Each client is versioned, built, and published independently (crates.io, npm, PyPI).

# Best Practices
- Organize code in small, simple, modular components
- Each component should contain unit tests that validate its behaviour in terms of inputs and outputs
- The overall codebase should contain integration tests that validate the interation between components that are used together
- Everything for local development runs in Docker via Make (see devops/localdev.md); component Makefiles expose the same verbs everywhere (build, test, lint, fmt, clean).
- New feature work starts as a work item under aspec/work-items/ using 0000-template.md.
- Keep the API surface small and versioned (/api/v1); clients never bypass it.

# Personas

### Persona 1:
Name: Agent Developer
Purpose: Builds AI agents using one of the client libraries/harnesses against a BASE server.
Use-cases:
- Define an agent, open sessions, exchange messages/events, and run custom harness loops from Rust, TypeScript, or Python.
- Run a local BASE server in Docker to develop and test agents end to end.
RBAC:
- allowed: full CRUD on agents, sessions, and runs owned by their API key
- disallowed: server administration, other principals' data, direct database access

### Persona 2:
Name: Platform Operator
Purpose: Deploys, configures, and operates a BASE server instance for a team.
Use-cases:
- Run the server Docker image with a persistent volume; configure it via environment variables.
- Issue and revoke API keys, take backups of the SQLite database, and upgrade versions.
RBAC:
- allowed: server configuration, key management, migrations, backups, all administrative endpoints
- disallowed: nothing at the instance level (superuser), but admin credentials must never be embedded in agents or clients

### Persona 3:
Name: End User
Purpose: Interacts with agents that developers have built on BASE.
Use-cases:
- Converse with or delegate tasks to a deployed agent through whatever surface the agent developer ships.
RBAC:
- allowed: only the interactions the hosting agent exposes
- disallowed: any direct access to the BASE API or server
