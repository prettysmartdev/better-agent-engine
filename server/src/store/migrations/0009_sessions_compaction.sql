-- Migration 0009 — session-level compaction config and a covering index for
-- the per-turn "latest completed compaction" lookup.
--
-- `compaction` stores the serialized `CompactionConfig` JSON for the session
-- (or NULL on pre-migration rows, read back as "no config" == client mode).
ALTER TABLE sessions ADD COLUMN compaction TEXT;

-- Compaction's hot lookup is "the most recent session.compaction.completed
-- event for a session" (run once per turn in auto mode). This two-column index
-- covers that query; SQLite already appends the rowid to every ordinary index
-- entry as its tie-breaker, so ORDER BY rowid within a (session_id, event_type)
-- group is served directly — do not name the implicit rowid in the index, which
-- SQLite rejects.
CREATE INDEX idx_session_events_session_type
    ON session_events(session_id, event_type);
