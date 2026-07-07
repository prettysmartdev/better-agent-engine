# APIs

Convention: rest + JSON-RPC 2.0 (hybrid, see below)
Protocol: http

## Design:

Versioning:
- All routes live under a version prefix, starting at `/api/v1`. Within a version, changes are additive only (new endpoints, new optional fields); breaking changes require `/api/v2`.
- The server reports its version and supported API versions at `GET /api/v1/meta`; clients check compatibility at connect time.

Objects:
- Core resources: `agents` (definitions/config), `sessions` (a conversation/task context bound to an agent), `events` (append-only messages/tool-calls/results within a session), `runs` (one execution of an agent loop over a session), and `keys` (admin-managed API keys).
- Resource IDs are server-generated, opaque, and prefixed by type (e.g. `agt_…`, `ses_…`, `evt_…`, `run_…`).
- Events are append-only; history is never mutated, only added to.

Authentication:
- Bearer API keys: `Authorization: Bearer bae_…` on every request; no anonymous endpoints except health.
- Keys are created/revoked via admin endpoints and hashed at rest (see architecture/security.md).

Conventions:
- JSON request/response bodies with `snake_case` field names.
- List endpoints use cursor pagination (`?cursor=…&limit=…`) returning `{items, next_cursor}`.
- Errors return an RFC 7807-style body: `{type, title, status, detail}` with appropriate HTTP status codes.
- Health at `GET /healthz` (unauthenticated, plain HTTP, for probes — no JSON-RPC envelope).
- The **client port** (`BAE_ADDR`) is a hybrid: REST/HTTP for all management
  operations (session open/close, metadata, event history), plus one JSON-RPC
  2.0 endpoint — `POST /api/v1/sessions/{id}/rpc` — for the live session loop.
  That endpoint streams `application/x-ndjson`; all other client-port endpoints
  return single buffered JSON bodies.
- The **admin port** (`BAE_ADMIN_ADDR`) is REST/HTTP throughout. No JSON-RPC,
  no SSE anywhere on the admin port.
