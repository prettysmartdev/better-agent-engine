-- Migration 0006 — sandbox support (work item 0006).
--
-- `profiles.available_sandboxes` is the per-profile allowlist of sandbox
-- container image names, stored as a JSON string-array blob in a TEXT column —
-- the same convention as `mcp_servers`/`allowed_tools` (migration 0003).
--
-- `sessions.sandbox_tools` is the per-client object of Auto-mode sandbox tool
-- declarations (`{"<client_key_id>": [{name, description, input_schema}, …]}`),
-- a sibling of `client_tools` (migration 0004) kept in its own column so the
-- two tool kinds are never confused.
ALTER TABLE profiles ADD COLUMN available_sandboxes TEXT;
ALTER TABLE sessions ADD COLUMN sandbox_tools TEXT;
