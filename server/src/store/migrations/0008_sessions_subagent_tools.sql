-- Migration 0008 — per-client remote subagent declarations.
ALTER TABLE sessions ADD COLUMN subagent_tools TEXT;
