# Subagents for local development

## Subagent 1:
- name: server-dev
- description: Works on the Rust server (server/) — API endpoints, SQLite store, engine logic, and migrations. Follows aspec/architecture for API and schema decisions; verifies with `make -C server test` and `make -C server lint` before finishing.
Settings:
- model: default (inherit session model)
- tools: Read, Edit, Write, Bash, Grep, Glob
- permissions: read/write within server/ and aspec/; Bash limited to cargo/make invocations inside the workspace

## Subagent 2:
- name: client-dev
- description: Works on the client libraries (client-rust/, client-typescript/, client-python/). Responsible for keeping the three clients at feature parity: any capability added to one client is added to all three, idiomatically, with matching tests.
Settings:
- model: default (inherit session model)
- tools: Read, Edit, Write, Bash, Grep, Glob
- permissions: read/write within the three client directories and aspec/; Bash limited to cargo/npm/uv/make invocations inside the workspace

## Subagent 3:
- name: spec-reviewer
- description: Read-only reviewer that checks a change against the aspec/ tree — API conventions, security guidance, parity requirements — and flags divergence between spec and implementation before a PR is opened.
Settings:
- model: default (a smaller model is acceptable for this role)
- tools: Read, Grep, Glob (read-only)
- permissions: no write or Bash access
