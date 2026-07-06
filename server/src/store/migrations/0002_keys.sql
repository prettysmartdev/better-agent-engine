-- Migration 0002 — keys table.
--
-- Holds both client keys (role='client') and session keys (role='session');
-- always filter by `role` in lookups so a session key can never be accepted as
-- a client key or vice versa. `key_prefix` is the first 8 chars of the plaintext
-- key, stored for display only. `key_hash` is an Argon2id PHC string and is
-- NEVER returned in any API response. A key with `deleted_at` set is treated as
-- non-existent.
CREATE TABLE keys (
    id           TEXT PRIMARY KEY,
    name         TEXT,
    key_hash     TEXT,
    key_prefix   TEXT,
    role         TEXT CHECK(role IN ('client','session')),
    profile_id   TEXT REFERENCES profiles(id),
    client_id    TEXT REFERENCES keys(id),
    created_at   TEXT,
    last_used_at TEXT,
    deleted_at   TEXT
);
