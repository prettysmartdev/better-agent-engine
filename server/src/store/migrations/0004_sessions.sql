-- Migration 0004 — sessions table.
--
-- One row per session (a conversation/task context bound to a client key and a
-- profile). `client_tools` records the tool list the client declared at open
-- time, as a JSON blob.
CREATE TABLE sessions (
    id             TEXT PRIMARY KEY,
    client_key_id  TEXT REFERENCES keys(id),
    profile_id     TEXT REFERENCES profiles(id),
    state          TEXT CHECK(state IN ('open','closed','error')),
    client_version TEXT,
    client_tools   TEXT,
    created_at     TEXT,
    closed_at      TEXT
);
