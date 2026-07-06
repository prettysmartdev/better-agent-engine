-- Migration 0003 — profiles table.
--
-- A profile describes the LLM provider (plus ordered fallbacks), the MCP servers
-- exposed to clients, and the allowlist of client-side tools. The complex fields
-- are stored as JSON text blobs. Profiles are managed only via the admin API.
CREATE TABLE profiles (
    id               TEXT PRIMARY KEY,
    name             TEXT UNIQUE,
    provider_config  TEXT,
    fallback_configs TEXT,
    mcp_servers      TEXT,
    allowed_tools    TEXT,
    created_at       TEXT,
    updated_at       TEXT,
    deleted_at       TEXT
);
