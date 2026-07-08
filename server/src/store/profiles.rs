//! Profile persistence.
//!
//! A profile is the admin-managed configuration a client key is bound to: the
//! primary LLM provider name, ordered fallback provider names, the opt-in list
//! of MCP server names (each naming a `bae-config.toml` registry entry), and
//! the allowlist of client-side tools a session may declare. Complex fields
//! are stored as JSON blobs (see migration 0003) and surfaced here as parsed
//! [`serde_json::Value`]s. (`mcp_servers` changed from an opaque blob to an
//! array of name strings in work item 0003; `provider_config` /
//! `fallback_configs` changed from inline config objects to a JSON string /
//! string-array of `[providers]` registry names in work item 0005 — both are
//! application-layer contract changes, not schema changes: the columns stay
//! `TEXT`. In the admin API those two surface as `primary_provider` /
//! `fallback_providers`.)
//!
//! Soft-delete only: [`soft_delete`] stamps `deleted_at`; rows are never removed.
//! Every query here filters `deleted_at IS NULL` so a deleted profile is
//! invisible to the rest of the system.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::{generate_id, NOW_SQL};

/// Prefix on every profile id.
pub const PROFILE_ID_PREFIX: &str = "pro_";

/// A profile row, with its JSON blob columns parsed.
#[derive(Debug, Clone)]
pub struct ProfileRecord {
    pub id: String,
    pub name: String,
    pub provider_config: Value,
    pub fallback_configs: Value,
    pub mcp_servers: Value,
    pub allowed_tools: Value,
    pub created_at: String,
    pub updated_at: String,
}

/// Fields an admin supplies to create or replace a profile. JSON blobs are
/// stored verbatim (already validated by the handler).
#[derive(Debug, Clone)]
pub struct ProfileInput {
    pub name: String,
    pub provider_config: Value,
    pub fallback_configs: Value,
    pub mcp_servers: Value,
    pub allowed_tools: Value,
}

/// A create/replace failed because another active profile already owns `name`
/// (the `profiles.name` UNIQUE constraint).
#[derive(Debug)]
pub struct DuplicateName;

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProfileRecord> {
    let parse = |s: String| serde_json::from_str(&s).unwrap_or(Value::Null);
    Ok(ProfileRecord {
        id: row.get("id")?,
        name: row.get("name")?,
        provider_config: parse(row.get("provider_config")?),
        fallback_configs: parse(row.get("fallback_configs")?),
        mcp_servers: parse(row.get("mcp_servers")?),
        allowed_tools: parse(row.get("allowed_tools")?),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const SELECT_COLS: &str = "id, name, provider_config, fallback_configs, mcp_servers, \
     allowed_tools, created_at, updated_at";

/// Insert a new profile, returning the stored record. A UNIQUE-constraint
/// violation on `name` maps to [`DuplicateName`]; any other SQLite error
/// propagates.
pub fn create(conn: &Connection, input: &ProfileInput) -> Result<ProfileRecord, CreateError> {
    let id = generate_id(PROFILE_ID_PREFIX);
    let sql = format!(
        "INSERT INTO profiles \
         (id, name, provider_config, fallback_configs, mcp_servers, allowed_tools, \
          created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, {NOW_SQL}, {NOW_SQL}) \
         RETURNING {SELECT_COLS}"
    );
    conn.query_row(
        &sql,
        params![
            id,
            input.name,
            input.provider_config.to_string(),
            input.fallback_configs.to_string(),
            input.mcp_servers.to_string(),
            input.allowed_tools.to_string(),
        ],
        row_to_record,
    )
    .map_err(CreateError::from)
}

/// Full replacement of an existing (non-deleted) profile. Returns `Ok(None)` if
/// no active profile has `id`.
pub fn replace(
    conn: &Connection,
    id: &str,
    input: &ProfileInput,
) -> Result<Option<ProfileRecord>, CreateError> {
    let sql = format!(
        "UPDATE profiles SET \
           name = ?2, provider_config = ?3, fallback_configs = ?4, \
           mcp_servers = ?5, allowed_tools = ?6, updated_at = {NOW_SQL} \
         WHERE id = ?1 AND deleted_at IS NULL \
         RETURNING {SELECT_COLS}"
    );
    conn.query_row(
        &sql,
        params![
            id,
            input.name,
            input.provider_config.to_string(),
            input.fallback_configs.to_string(),
            input.mcp_servers.to_string(),
            input.allowed_tools.to_string(),
        ],
        row_to_record,
    )
    .optional()
    .map_err(CreateError::from)
}

/// Fetch a single active profile by id.
pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<ProfileRecord>> {
    let sql = format!("SELECT {SELECT_COLS} FROM profiles WHERE id = ?1 AND deleted_at IS NULL");
    conn.query_row(&sql, params![id], row_to_record).optional()
}

/// One page of active profiles ordered by insertion (rowid). `after` is the
/// exclusive rowid cursor; `limit` rows are returned. The returned bool is true
/// when more rows remain after this page.
pub fn list(
    conn: &Connection,
    after: i64,
    limit: i64,
) -> rusqlite::Result<(Vec<(i64, ProfileRecord)>, bool)> {
    let sql = format!(
        "SELECT rowid, {SELECT_COLS} FROM profiles \
         WHERE deleted_at IS NULL AND rowid > ?1 \
         ORDER BY rowid LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    // Fetch limit+1 to detect whether a further page exists.
    let rows = stmt.query_map(params![after, limit + 1], |row| {
        Ok((row.get::<_, i64>(0)?, row_to_record(row)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    let has_more = out.len() as i64 > limit;
    out.truncate(limit as usize);
    Ok((out, has_more))
}

/// Soft-delete a profile. Returns the outcome so the handler can distinguish
/// "not found" from "blocked by active keys".
pub fn soft_delete(conn: &Connection, id: &str) -> rusqlite::Result<DeleteOutcome> {
    // The profile must exist and be active.
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM profiles WHERE id = ?1 AND deleted_at IS NULL",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(DeleteOutcome::NotFound);
    }
    // Refuse while any active client key still references it.
    let referencing: i64 = conn.query_row(
        "SELECT count(*) FROM keys \
         WHERE profile_id = ?1 AND role = 'client' AND deleted_at IS NULL",
        params![id],
        |r| r.get(0),
    )?;
    if referencing > 0 {
        return Ok(DeleteOutcome::HasActiveKeys(referencing));
    }
    let sql = format!("UPDATE profiles SET deleted_at = {NOW_SQL} WHERE id = ?1");
    conn.execute(&sql, params![id])?;
    Ok(DeleteOutcome::Deleted)
}

/// Result of a [`soft_delete`] attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    /// Blocked: this many active client keys still reference the profile.
    HasActiveKeys(i64),
}

/// Error from a create/replace: a duplicate name, or an underlying SQLite error.
#[derive(Debug)]
pub enum CreateError {
    Duplicate,
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for CreateError {
    fn from(e: rusqlite::Error) -> Self {
        // A UNIQUE violation on profiles.name is the one we translate.
        if let rusqlite::Error::SqliteFailure(err, _) = &e {
            if err.code == rusqlite::ErrorCode::ConstraintViolation {
                return CreateError::Duplicate;
            }
        }
        CreateError::Sqlite(e)
    }
}
