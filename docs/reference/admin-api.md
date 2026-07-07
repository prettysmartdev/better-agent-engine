# Admin API Reference

The admin API is served on `BAE_ADMIN_ADDR` (default `127.0.0.1:8081`). It
binds to a loopback address only — the server refuses to start with a
non-loopback admin address. Reach it via `docker exec`, a local process, or
an SSH tunnel. **No authentication is required on the admin port.**

All requests and responses use `Content-Type: application/json`. Field names
are `snake_case`. The admin port is **REST/HTTP throughout** — no JSON-RPC.

---

## Errors

Every non-2xx response body follows RFC 7807:

```json
{
  "type": "not_found",
  "title": "Not Found",
  "status": 404,
  "detail": "no profile with id pro_abc"
}
```

Match on `type` (a short, stable slug) rather than `status` or `title`.

| `type` | HTTP status | When |
|---|---|---|
| `bad_request` | 400 | Missing or invalid fields. |
| `not_found` | 404 | Resource does not exist. |
| `duplicate_name` | 409 | Profile name already taken. |
| `profile_in_use` | 409 | Profile has active client keys; cannot delete. |
| `profile_unavailable` | 422 | Profile does not exist or has been deleted. |
| `internal` | 500 | Unexpected server error. |

---

## Pagination

List endpoints accept `?cursor=<opaque>&limit=<n>` and return:

```json
{
  "items": [ … ],
  "next_cursor": "42"
}
```

- `next_cursor` is `null` on the last page; otherwise pass it back verbatim as
  `?cursor=` on the next request.
- Default limit: **50**. Maximum limit: **200**.
- The cursor value is opaque — never parse or construct it.

---

## Profiles

### `POST /admin/v1/profiles` — create

**Request body:**

```json
{
  "name": "main",
  "provider_config": {
    "provider": "anthropic",
    "base_url": "https://api.anthropic.com",
    "model": "claude-sonnet-4-6",
    "auth_token": "${ANTHROPIC_API_KEY}",
    "max_tokens": 8096
  },
  "fallback_configs": [],
  "mcp_servers": ["filesystem"],
  "allowed_tools": ["get_current_time"]
}
```

- `name` — required, must be unique.
- `provider_config` — required. See [profiles.md](../profiles.md) for the schema.
- `fallback_configs` — optional, default `[]`. Array of provider configs tried
  in order if the primary fails.
- `mcp_servers` — optional, default `[]`. **Array of MCP server name strings**
  matching entries in `bae-config.toml`. Every element must be a string; a
  non-string element returns `400 bad_request`.
- `allowed_tools` — optional, default `[]`. Names of client-side tools agents
  may declare. **An empty list permits no client-side tools.**

**Response `201 Created`:**

```json
{
  "id": "pro_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "name": "main",
  "created_at": "2026-07-06T18:26:01.123Z"
}
```

**Errors:** `400 bad_request` (blank name, malformed `provider_config`,
non-array or non-string list fields), `409 duplicate_name`.

---

### `GET /admin/v1/profiles` — list

```
GET /admin/v1/profiles?limit=20&cursor=
```

**Response `200 OK`:**

```json
{
  "items": [
    {
      "id": "pro_…",
      "name": "main",
      "provider_config": {
        "provider": "anthropic",
        "base_url": "https://api.anthropic.com",
        "model": "claude-sonnet-4-6",
        "auth_token": "${ANTHROPIC_API_KEY}",
        "max_tokens": 8096
      },
      "fallback_configs": [],
      "mcp_servers": ["filesystem"],
      "allowed_tools": ["get_current_time"],
      "created_at": "2026-07-06T18:26:01.123Z",
      "updated_at": "2026-07-06T18:26:01.123Z"
    }
  ],
  "next_cursor": null
}
```

The admin surface returns the full `provider_config` including the literal
`auth_token` string (e.g. `"${ANTHROPIC_API_KEY}"`). This is a template
reference, not an actual secret. Deleted profiles are excluded from the list.

---

### `GET /admin/v1/profiles/{id}` — get one

**Response `200 OK`:** full Profile object (same shape as list items).

**Errors:** `404 not_found`.

---

### `PUT /admin/v1/profiles/{id}` — replace

Full replacement — all fields are overwritten. Body shape is identical to
`POST`. Bumps `updated_at`.

**Response `200 OK`:** full Profile object.

**Errors:** `400 bad_request`, `404 not_found`.

---

### `DELETE /admin/v1/profiles/{id}` — soft-delete

Marks the profile as deleted. The profile's rows are retained for audit;
deleted profiles are excluded from list and get responses.

**Response `204 No Content`**

**Errors:**
- `404 not_found` — no profile with this id.
- `409 profile_in_use` — the profile still has active (non-deleted) client
  keys. Revoke them first (`DELETE /admin/v1/keys/{id}`), then delete the
  profile.

---

## Client Keys

### `POST /admin/v1/keys` — create

**Request body:**

```json
{
  "name": "my-agent",
  "profile_id": "pro_…"
}
```

- `name` — required, human label.
- `profile_id` — required. Must refer to a non-deleted profile.

**Response `201 Created`:**

```json
{
  "id": "key_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "name": "my-agent",
  "key": "bae_1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f",
  "prefix": "bae_1a2b",
  "profile_id": "pro_…",
  "created_at": "2026-07-06T18:26:05.000Z"
}
```

> **`key` is shown exactly once.** Copy the plaintext now — only an Argon2id
> hash is stored and there is no way to retrieve the plaintext later. See
> [Key security](#key-security) below.

**Errors:**
- `400 bad_request` — blank name.
- `422 profile_unavailable` — the referenced profile does not exist or is
  deleted.

---

### `GET /admin/v1/keys` — list active keys

```
GET /admin/v1/keys?limit=50&cursor=
```

**Response `200 OK`:**

```json
{
  "items": [
    {
      "id": "key_…",
      "name": "my-agent",
      "prefix": "bae_1a2b",
      "profile_id": "pro_…",
      "created_at": "2026-07-06T18:26:05.000Z",
      "last_used_at": "2026-07-06T19:00:00.000Z"
    }
  ],
  "next_cursor": null
}
```

- `last_used_at` is `null` if the key has never authenticated.
- `key_hash` is **never** returned in any response.
- Revoked (soft-deleted) keys are excluded.

---

### `DELETE /admin/v1/keys/{id}` — revoke

Revokes the key and cascades:

1. Sets `deleted_at` on the client key.
2. Soft-deletes all session keys created by this client key.
3. Moves all open sessions for this client key to `closed`.
4. Appends a `session.close` event (`{"reason":"client_key_revoked"}`) to each
   closed session.

After revocation, session keys from this client key cannot authenticate and
existing open sessions return `401` on subsequent requests.

**Response `204 No Content`**

**Errors:** `404 not_found`.

---

## MCP Servers

### `GET /admin/v1/mcp-servers`

Returns the currently loaded MCP server registry — the set of servers parsed
from `bae-config.toml` at startup (or reloaded on restart). Useful to confirm
what a running server has available without reading the config file on disk.

```sh
curl http://127.0.0.1:8081/admin/v1/mcp-servers
```

**Response `200 OK`:**

```json
{
  "items": [
    {"name": "fetch",      "transport": "stdio"},
    {"name": "filesystem", "transport": "stdio"}
  ]
}
```

Items are sorted by name. Only `name` and `transport` are returned — no
`command`, `args`, `url`, or `headers` (secrets are never exposed).

The registry is rebuilt on restart; this endpoint reflects the current in-memory
state. An empty `items` array means the server started without a config file,
or the config file had no `[[mcp.servers]]` entries.

---

## Key security

Keys are generated with 192 bits of entropy from the OS CSPRNG (24 random
bytes, hex-encoded). Only an Argon2id hash is stored in SQLite — the
plaintext is discarded immediately after it is returned to the caller. Hash
parameters:

| Parameter | Value |
|---|---|
| Algorithm | Argon2id |
| Memory cost | 64 MiB (65536 KiB) |
| Time cost (iterations) | 3 |
| Parallelism | 1 |
| Output length | 32 bytes |
| Salt | Fresh OS CSPRNG random per hash |

Verification is constant-time (`subtle::ConstantTimeEq`) to prevent
timing-oracle attacks. Parameters are embedded in the stored PHC string, so
retuning them for a new deployment does not invalidate existing hashes.

To tune parameters for your hardware: increase memory cost first (more
resistant to GPU attacks), then iterations. Parallelism can be raised on
multi-core verifiers but 1 is the conservative default.
