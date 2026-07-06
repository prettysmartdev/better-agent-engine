-- Migration 0001 — schema_version table.
--
-- Tracks which migrations have been applied. The migration runner inserts one
-- row per migration it applies, all inside a single transaction, so a partial
-- or concurrent start can never leave the table half-populated.
CREATE TABLE schema_version (
    version    INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);
