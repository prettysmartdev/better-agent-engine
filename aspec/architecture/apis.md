# APIs

Convention: rest
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
- Bearer API keys: `Authorization: Bearer base_…` on every request; no anonymous endpoints except health.
- Keys are created/revoked via admin endpoints and hashed at rest (see architecture/security.md).

Conventions:
- JSON request/response bodies with `snake_case` field names.
- List endpoints use cursor pagination (`?cursor=…&limit=…`) returning `{items, next_cursor}`.
- Errors return an RFC 7807-style body: `{type, title, status, detail}` with appropriate HTTP status codes.
- Health at `GET /healthz` (unauthenticated, for probes); long-running run output streamed via SSE.
