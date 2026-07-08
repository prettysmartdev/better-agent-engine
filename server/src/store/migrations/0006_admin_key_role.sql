-- Migration 0006 — widen keys.role to include 'admin'.
--
-- The admin port (`BAE_ADMIN_ADDR`) gains real authentication: the server
-- self-provisions (or ingests) a `role='admin'` key at startup and enforces a
-- bearer token on every `/admin/v1/*` route. Admin keys live in the same `keys`
-- table as client and session keys, distinguished by `role` exactly like the
-- others, so the existing hashing/verification path is reused unchanged.
--
-- SQLite cannot `ALTER TABLE ... DROP/ADD CONSTRAINT`, so widening the `role`
-- CHECK from `IN ('client','session')` to `IN ('client','session','admin')`
-- requires the standard create-copy-drop-rename table rebuild. Every existing
-- column and row is preserved byte-for-byte across the rebuild. Foreign-key
-- enforcement IS on for these connections, and `DROP TABLE keys` would violate
-- the copied rows' self-referencing `client_id` — the migration runner
-- (`migrations.rs::run`) disables enforcement around the batch (PRAGMA
-- foreign_keys is a no-op inside a transaction, so it cannot be done here).
-- The new table's schema is otherwise identical to migration 0002.
CREATE TABLE keys_new (
    id           TEXT PRIMARY KEY,
    name         TEXT,
    key_hash     TEXT,
    key_prefix   TEXT,
    role         TEXT CHECK(role IN ('client','session','admin')),
    profile_id   TEXT REFERENCES profiles(id),
    client_id    TEXT REFERENCES keys(id),
    created_at   TEXT,
    last_used_at TEXT,
    deleted_at   TEXT
);

INSERT INTO keys_new
    (id, name, key_hash, key_prefix, role, profile_id, client_id, created_at, last_used_at, deleted_at)
SELECT
    id, name, key_hash, key_prefix, role, profile_id, client_id, created_at, last_used_at, deleted_at
FROM keys;

DROP TABLE keys;

ALTER TABLE keys_new RENAME TO keys;
