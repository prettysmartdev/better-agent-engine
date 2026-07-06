-- Migration 0005 — session_events table.
--
-- The append-only, timestamped log of everything that happens in a session:
-- client messages, provider requests/responses, tool calls/results, MCP
-- exchanges, and lifecycle events. `event_type` is one of the closed set of
-- message-type strings (see `events::EventType`); `payload` is freeform JSON.
-- Append-only: rows are never updated or deleted, so a full session can always
-- be replayed by reading its events in `created_at` order.
CREATE TABLE session_events (
    id            TEXT PRIMARY KEY,
    session_id    TEXT REFERENCES sessions(id),
    client_key_id TEXT,
    event_type    TEXT NOT NULL,
    payload       TEXT NOT NULL,
    created_at    TEXT NOT NULL
);
