//! API key generation, hashing, and verification.
//!
//! Three kinds of key live in the `keys` table and all flow through here:
//!
//! - **client keys** — `bae_<random>`, exchanged for a session.
//! - **session keys** — `bae_ses_<random>`, used to drive a single session.
//! - **admin keys** — `bae_admin_<random>`, authorize the loopback admin port.
//!
//! # Security properties
//!
//! - **Entropy.** Both key bodies draw [`KEY_ENTROPY_BYTES`] (24 bytes = 192
//!   bits, comfortably above the required 128) from the OS CSPRNG
//!   (`rand::rngs::OsRng`).
//! - **At rest.** Only an Argon2id hash is stored, never the plaintext. The
//!   plaintext is returned to the caller exactly once, at creation.
//! - **Argon2id parameters** (documented so operators can tune per deployment):
//!   memory = 64 MiB, iterations (time cost) = 3, parallelism = 1, 32-byte
//!   output. These meet the work item's floor (memory ≥ 64 MiB, iterations ≥ 3,
//!   parallelism = 1).
//! - **Verification** recomputes the hash with the parameters embedded in the
//!   stored PHC string and compares the raw digests with
//!   [`subtle::ConstantTimeEq`], so a partial-match timing oracle cannot leak
//!   information.

use argon2::password_hash::rand_core::OsRng as SaltRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use subtle::ConstantTimeEq;

use super::{generate_id, NOW_SQL};

/// Prefix on every key row id.
pub const KEY_ID_PREFIX: &str = "key_";
/// The `keys.role` value for client keys.
pub const ROLE_CLIENT: &str = "client";
/// The `keys.role` value for session keys.
pub const ROLE_SESSION: &str = "session";
/// The `keys.role` value for admin keys (loopback admin port).
pub const ROLE_ADMIN: &str = "admin";

/// Prefix on every client key's plaintext.
pub const CLIENT_KEY_PREFIX: &str = "bae_";
/// Prefix on every session key's plaintext.
pub const SESSION_KEY_PREFIX: &str = "bae_ses_";
/// Prefix on every admin key's plaintext. Distinguishes an admin key from a
/// `bae_` client key and a `bae_ses_` session key by sight.
pub const ADMIN_KEY_PREFIX: &str = "bae_admin_";
/// Random bytes drawn per key body: 24 bytes = 192 bits of entropy (≥ 128).
pub const KEY_ENTROPY_BYTES: usize = 24;
/// Number of leading characters of a key stored/displayed as its prefix.
pub const KEY_PREFIX_LEN: usize = 8;

// --- Argon2id parameters (see module docs) ---
/// Memory cost in KiB: 65536 KiB = 64 MiB.
const ARGON2_MEMORY_KIB: u32 = 64 * 1024;
/// Time cost (iterations).
const ARGON2_ITERATIONS: u32 = 3;
/// Parallelism (lanes).
const ARGON2_PARALLELISM: u32 = 1;
/// Output length in bytes.
const ARGON2_OUTPUT_LEN: usize = 32;

/// A freshly generated key: the one-time plaintext plus its display prefix.
///
/// `plaintext` must be shown to the operator/agent exactly once and never
/// stored; only the [hash](hash_key) of it is persisted.
#[derive(Debug, Clone)]
pub struct GeneratedKey {
    /// Full plaintext key, e.g. `bae_1a2b…`. Shown once, never stored.
    pub plaintext: String,
    /// First [`KEY_PREFIX_LEN`] characters, safe to store and display.
    pub prefix: String,
}

/// Errors from hashing or verifying a key.
#[derive(Debug)]
pub enum KeyError {
    /// The Argon2 parameters were rejected (should not happen with our constants).
    Params(argon2::Error),
    /// Hashing failed.
    Hash(argon2::password_hash::Error),
    /// The stored hash string was malformed / not a valid PHC string.
    MalformedHash(argon2::password_hash::Error),
    /// A SQLite error occurred while looking up a key for authentication.
    Db(rusqlite::Error),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Params(e) => write!(f, "invalid Argon2 parameters: {e}"),
            KeyError::Hash(e) => write!(f, "key hashing failed: {e}"),
            KeyError::MalformedHash(e) => write!(f, "stored key hash is malformed: {e}"),
            KeyError::Db(e) => write!(f, "database error during key lookup: {e}"),
        }
    }
}

impl std::error::Error for KeyError {}

/// Generate a new client key (`bae_<random>`).
pub fn generate_client_key() -> GeneratedKey {
    generate_key(CLIENT_KEY_PREFIX)
}

/// Generate a new session key (`bae_ses_<random>`).
pub fn generate_session_key() -> GeneratedKey {
    generate_key(SESSION_KEY_PREFIX)
}

/// Generate a new admin key (`bae_admin_<random>`). Same CSPRNG + entropy as a
/// client/session key, only the prefix differs.
pub fn generate_admin_key() -> GeneratedKey {
    generate_key(ADMIN_KEY_PREFIX)
}

fn generate_key(prefix: &str) -> GeneratedKey {
    let mut bytes = [0u8; KEY_ENTROPY_BYTES];
    // `OsRng` is a cryptographically secure, OS-backed RNG; `fill_bytes` cannot
    // partially fill or silently fall back.
    OsRng.fill_bytes(&mut bytes);
    let plaintext = format!("{prefix}{}", to_hex(&bytes));
    let prefix = key_prefix(&plaintext);
    GeneratedKey { plaintext, prefix }
}

/// The display prefix for a key: its first [`KEY_PREFIX_LEN`] characters.
///
/// Uses `char` boundaries (our keys are ASCII, but this stays correct if that
/// ever changes).
pub fn key_prefix(key: &str) -> String {
    key.chars().take(KEY_PREFIX_LEN).collect()
}

/// Hash a plaintext key with Argon2id, returning a self-describing PHC string
/// (algorithm, parameters, salt, and digest) suitable for storage.
pub fn hash_key(plaintext: &str) -> Result<String, KeyError> {
    let salt = SaltString::generate(&mut SaltRng);
    let hash = hasher()?
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(KeyError::Hash)?;
    Ok(hash.to_string())
}

/// Verify a plaintext key against a stored PHC hash in constant time.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch, and `Err` only if the
/// stored hash cannot be parsed. The comparison recomputes the digest using the
/// parameters recorded in `stored` (forward-compatible if we ever retune) and
/// compares raw bytes with [`ConstantTimeEq`].
pub fn verify_key(plaintext: &str, stored: &str) -> Result<bool, KeyError> {
    let parsed = PasswordHash::new(stored).map_err(KeyError::MalformedHash)?;
    let salt = parsed.salt.ok_or(KeyError::MalformedHash(
        argon2::password_hash::Error::SaltInvalid(
            argon2::password_hash::errors::InvalidValue::Malformed,
        ),
    ))?;
    let expected = parsed.hash.ok_or(KeyError::MalformedHash(
        argon2::password_hash::Error::Password,
    ))?;

    // Rebuild the hasher from the stored PHC parameters so old hashes still
    // verify after a parameter change.
    let algorithm = Algorithm::try_from(parsed.algorithm).map_err(KeyError::Hash)?;
    let params = Params::try_from(&parsed).map_err(KeyError::Hash)?;
    let argon2 = Argon2::new(algorithm, Version::V0x13, params);

    let computed = argon2
        .hash_password(plaintext.as_bytes(), salt)
        .map_err(KeyError::Hash)?;
    let computed = computed
        .hash
        .ok_or(KeyError::Hash(argon2::password_hash::Error::Password))?;

    // Constant-time compare of the raw digests. `ct_eq` returns 0 immediately
    // for differing lengths (lengths are not secret), and otherwise compares
    // every byte without early return.
    Ok(bool::from(computed.as_bytes().ct_eq(expected.as_bytes())))
}

/// Whether `stored` parses as a well-formed Argon2id PHC hash string.
///
/// Used to validate a pre-provisioned admin-key hash file at startup (so a
/// malformed hash is rejected once, at boot, rather than silently failing every
/// admin request later). Only checks structural validity — that the string is a
/// parseable PHC hash naming the `argon2id` algorithm with a salt and digest —
/// not that it hashes any particular plaintext.
pub fn is_valid_key_hash(stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    parsed.algorithm.as_str() == "argon2id" && parsed.salt.is_some() && parsed.hash.is_some()
}

/// Build an Argon2id hasher with our fixed parameters.
fn hasher() -> Result<Argon2<'static>, KeyError> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(ARGON2_OUTPUT_LEN),
    )
    .map_err(KeyError::Params)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Lowercase-hex encode, no external dependency.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// Key-row persistence and authentication.
//
// Both client and session keys live in the one `keys` table, distinguished by
// `role`; every query filters by `role` so a session key can never be accepted
// as a client key or vice versa. Session keys additionally store their owning
// session id in the `name` column, which is how a session key is looked up for
// authentication without an O(number-of-keys) Argon2 scan.
// ---------------------------------------------------------------------------

/// A key row as surfaced to the API. Deliberately omits `key_hash`, which must
/// never leave the store.
#[derive(Debug, Clone)]
pub struct KeyRecord {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub role: String,
    pub profile_id: Option<String>,
    pub client_id: Option<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

fn row_to_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<KeyRecord> {
    Ok(KeyRecord {
        id: row.get("id")?,
        name: row.get("name")?,
        prefix: row.get("key_prefix")?,
        role: row.get("role")?,
        profile_id: row.get("profile_id")?,
        client_id: row.get("client_id")?,
        created_at: row.get("created_at")?,
        last_used_at: row.get("last_used_at")?,
    })
}

const KEY_COLS: &str =
    "id, name, key_prefix, role, profile_id, client_id, created_at, last_used_at";

/// Persist a freshly generated client key. `plaintext` is hashed here and never
/// stored; the caller is responsible for returning it to the operator once.
pub fn insert_client_key(
    conn: &Connection,
    name: &str,
    profile_id: &str,
    generated: &GeneratedKey,
) -> Result<KeyRecord, InsertError> {
    let id = generate_id(KEY_ID_PREFIX);
    let hash = hash_key(&generated.plaintext).map_err(InsertError::Key)?;
    let sql = format!(
        "INSERT INTO keys \
           (id, name, key_hash, key_prefix, role, profile_id, client_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, '{ROLE_CLIENT}', ?5, NULL, {NOW_SQL}) \
         RETURNING {KEY_COLS}"
    );
    conn.query_row(
        &sql,
        params![id, name, hash, generated.prefix, profile_id],
        row_to_key,
    )
    .map_err(InsertError::Sqlite)
}

/// Persist a session key. Its owning `session_id` is stored in `name` (the
/// lookup selector for session auth) and `client_id` records the client key
/// that opened the session.
pub fn insert_session_key(
    conn: &Connection,
    session_id: &str,
    client_key_id: &str,
    profile_id: &str,
    generated: &GeneratedKey,
) -> Result<KeyRecord, InsertError> {
    let id = generate_id(KEY_ID_PREFIX);
    let hash = hash_key(&generated.plaintext).map_err(InsertError::Key)?;
    let sql = format!(
        "INSERT INTO keys \
           (id, name, key_hash, key_prefix, role, profile_id, client_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, '{ROLE_SESSION}', ?5, ?6, {NOW_SQL}) \
         RETURNING {KEY_COLS}"
    );
    conn.query_row(
        &sql,
        params![
            id,
            session_id,
            hash,
            generated.prefix,
            profile_id,
            client_key_id
        ],
        row_to_key,
    )
    .map_err(InsertError::Sqlite)
}

/// One page of active (non-deleted) client keys, ordered by insertion (rowid).
/// `key_hash` is never selected. The bool is true when more rows remain.
pub fn list_client_keys(
    conn: &Connection,
    after: i64,
    limit: i64,
) -> rusqlite::Result<(Vec<(i64, KeyRecord)>, bool)> {
    let sql = format!(
        "SELECT rowid, {KEY_COLS} FROM keys \
         WHERE role = '{ROLE_CLIENT}' AND deleted_at IS NULL AND rowid > ?1 \
         ORDER BY rowid LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![after, limit + 1], |row| {
        Ok((row.get::<_, i64>(0)?, row_to_key(row)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    let has_more = out.len() as i64 > limit;
    out.truncate(limit as usize);
    Ok((out, has_more))
}

/// Authenticate a bearer token as a **client** key.
///
/// Candidates are narrowed by `key_prefix` (a public, deterministic selector)
/// and `role = 'client'` with `deleted_at IS NULL` enforced *in the query* — a
/// deleted key is never even hashed against. Each candidate's stored Argon2
/// hash is verified in constant time. On success `last_used_at` is stamped and
/// the record returned.
pub fn authenticate_client(conn: &Connection, token: &str) -> Result<Option<KeyRecord>, KeyError> {
    let prefix = key_prefix(token);
    let sql = format!(
        "SELECT {KEY_COLS}, key_hash FROM keys \
         WHERE role = '{ROLE_CLIENT}' AND key_prefix = ?1 AND deleted_at IS NULL"
    );
    authenticate(conn, &sql, params![prefix], token)
}

/// Authenticate a bearer token as the **session** key for `session_id`.
///
/// The candidate is selected by `name = session_id` and `role = 'session'`
/// (with `deleted_at IS NULL`), so a valid session key presented on the wrong
/// session simply finds no matching row and is rejected.
pub fn authenticate_session(
    conn: &Connection,
    token: &str,
    session_id: &str,
) -> Result<Option<KeyRecord>, KeyError> {
    let sql = format!(
        "SELECT {KEY_COLS}, key_hash FROM keys \
         WHERE role = '{ROLE_SESSION}' AND name = ?1 AND deleted_at IS NULL"
    );
    authenticate(conn, &sql, params![session_id], token)
}

/// Shared verify-and-touch path for both key roles. Iterates the candidate rows
/// (usually one), constant-time verifies each, and on the first match updates
/// `last_used_at` and returns the record.
fn authenticate(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    token: &str,
) -> Result<Option<KeyRecord>, KeyError> {
    let mut stmt = conn.prepare(sql).map_err(sqlite_as_key_err)?;
    let mut rows = stmt.query(params).map_err(sqlite_as_key_err)?;
    while let Some(row) = rows.next().map_err(sqlite_as_key_err)? {
        let record = row_to_key(row).map_err(sqlite_as_key_err)?;
        let hash: String = row.get("key_hash").map_err(sqlite_as_key_err)?;
        // A stored hash that fails to parse is treated as a non-match, not a
        // hard error, so one corrupt row cannot lock out other keys.
        if verify_key(token, &hash).unwrap_or(false) {
            touch_last_used(conn, &record.id).map_err(sqlite_as_key_err)?;
            return Ok(Some(record));
        }
    }
    Ok(None)
}

/// Stamp `last_used_at = now` on successful authentication.
pub fn touch_last_used(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    let sql = format!("UPDATE keys SET last_used_at = {NOW_SQL} WHERE id = ?1");
    conn.execute(&sql, params![id])?;
    Ok(())
}

/// Revoke (soft-delete) a client key and invalidate everything it opened.
///
/// Sets `deleted_at` on the client key, soft-deletes its session keys (so they
/// can no longer authenticate), and moves its open sessions to `closed`.
/// Returns `None` if no active client key has `id`, otherwise the ids of the
/// sessions that were closed (so the caller can log `session.close` events).
pub fn revoke_client_key(conn: &Connection, id: &str) -> rusqlite::Result<Option<Vec<String>>> {
    let active: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT 1 FROM keys WHERE id = ?1 AND role = '{ROLE_CLIENT}' \
                 AND deleted_at IS NULL"
            ),
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if active.is_none() {
        return Ok(None);
    }

    // Collect the open sessions before we mutate them.
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM sessions WHERE client_key_id = ?1 AND state = '{STATE_OPEN}'"
    ))?;
    let closed: Vec<String> = stmt
        .query_map(params![id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let now_client = format!("UPDATE keys SET deleted_at = {NOW_SQL} WHERE id = ?1");
    conn.execute(&now_client, params![id])?;
    // Session keys reference their opening client key via client_id.
    let now_sessions_keys = format!(
        "UPDATE keys SET deleted_at = {NOW_SQL} \
         WHERE role = '{ROLE_SESSION}' AND client_id = ?1 AND deleted_at IS NULL"
    );
    conn.execute(&now_sessions_keys, params![id])?;
    let now_sessions = format!(
        "UPDATE sessions SET state = '{STATE_CLOSED}', closed_at = {NOW_SQL} \
         WHERE client_key_id = ?1 AND state = '{STATE_OPEN}'"
    );
    conn.execute(&now_sessions, params![id])?;
    Ok(Some(closed))
}

const STATE_OPEN: &str = "open";
const STATE_CLOSED: &str = "closed";

// ---------------------------------------------------------------------------
// Admin keys.
//
// Admin keys (`role='admin'`) authorize the loopback admin port. They are
// bootstrapped once at server startup (see `crate::admin_auth`): either
// self-generated (the server writes the plaintext to a file and stores only the
// hash) or ingested from a pre-provisioned Argon2id hash (the server never sees
// the plaintext). Unlike client/session keys they carry no `profile_id`/
// `client_id`. There is normally exactly one active admin row, but the code
// never assumes that — a pre-provisioned replica and a manually recovered key
// can briefly coexist, and any active admin row is a valid credential.
// ---------------------------------------------------------------------------

/// Return the first active (`deleted_at IS NULL`) `role='admin'` key, if any.
///
/// Used at startup to decide whether the bootstrap needs to mint or ingest a
/// key. Only its existence matters to the bootstrap; the `key_hash` is not
/// selected.
pub fn find_active_admin_key(conn: &Connection) -> rusqlite::Result<Option<KeyRecord>> {
    let sql = format!(
        "SELECT {KEY_COLS} FROM keys \
         WHERE role = '{ROLE_ADMIN}' AND deleted_at IS NULL \
         ORDER BY rowid LIMIT 1"
    );
    conn.query_row(&sql, [], row_to_key).optional()
}

/// Persist a freshly generated admin key. `generated.plaintext` is hashed here
/// and never stored; the caller (the startup bootstrap) is responsible for
/// writing the plaintext to `BAE_ADMIN_KEY_FILE` exactly once.
pub fn insert_generated_admin_key(
    conn: &Connection,
    name: &str,
    generated: &GeneratedKey,
) -> Result<KeyRecord, InsertError> {
    let id = generate_id(KEY_ID_PREFIX);
    let hash = hash_key(&generated.plaintext).map_err(InsertError::Key)?;
    let sql = format!(
        "INSERT INTO keys \
           (id, name, key_hash, key_prefix, role, profile_id, client_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, '{ROLE_ADMIN}', NULL, NULL, {NOW_SQL}) \
         RETURNING {KEY_COLS}"
    );
    conn.query_row(&sql, params![id, name, hash, generated.prefix], row_to_key)
        .map_err(InsertError::Sqlite)
}

/// Persist an admin key from a pre-provisioned Argon2id PHC hash. The server
/// never learns the plaintext in this path — this is the multi-replica
/// pre-provisioning flow, where every replica ingests the identical hash
/// (produced by `baectl auth create key`) so one plaintext authenticates
/// against all of them. `key_hash` must already be a valid PHC string; `prefix`
/// is stored for display only.
pub fn insert_admin_key_from_hash(
    conn: &Connection,
    name: &str,
    prefix: &str,
    key_hash: &str,
) -> rusqlite::Result<KeyRecord> {
    let id = generate_id(KEY_ID_PREFIX);
    let sql = format!(
        "INSERT INTO keys \
           (id, name, key_hash, key_prefix, role, profile_id, client_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, '{ROLE_ADMIN}', NULL, NULL, {NOW_SQL}) \
         RETURNING {KEY_COLS}"
    );
    conn.query_row(&sql, params![id, name, key_hash, prefix], row_to_key)
}

/// Soft-delete every active admin key, returning how many rows were revoked.
/// Used by `--rotate-admin-key` before minting fresh material.
pub fn revoke_active_admin_keys(conn: &Connection) -> rusqlite::Result<usize> {
    let sql = format!(
        "UPDATE keys SET deleted_at = {NOW_SQL} \
         WHERE role = '{ROLE_ADMIN}' AND deleted_at IS NULL"
    );
    conn.execute(&sql, [])
}

/// Authenticate a bearer token against **every** active admin key.
///
/// Unlike client-key auth this does not narrow candidates by `key_prefix`: it
/// checks the token, in constant time, against every active `role='admin'` row
/// (normally one, occasionally more — see the section comment). A client- or
/// session-role key can never match, since only admin rows are selected. On the
/// first match `last_used_at` is stamped and the record returned.
pub fn authenticate_admin(conn: &Connection, token: &str) -> Result<Option<KeyRecord>, KeyError> {
    let sql = format!(
        "SELECT {KEY_COLS}, key_hash FROM keys \
         WHERE role = '{ROLE_ADMIN}' AND deleted_at IS NULL"
    );
    authenticate(conn, &sql, params![], token)
}

/// Error inserting a key row.
#[derive(Debug)]
pub enum InsertError {
    /// Hashing the plaintext failed.
    Key(KeyError),
    /// An underlying SQLite error.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for InsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InsertError::Key(e) => write!(f, "{e}"),
            InsertError::Sqlite(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for InsertError {}

/// Map a SQLite error hit during authentication to a [`KeyError::Db`], logging
/// the underlying detail (which the API layer will render as a 500).
fn sqlite_as_key_err(e: rusqlite::Error) -> KeyError {
    tracing::error!("key store SQLite error during authentication: {e}");
    KeyError::Db(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_key_shape() {
        let k = generate_client_key();
        assert!(k.plaintext.starts_with(CLIENT_KEY_PREFIX));
        assert!(!k.plaintext.starts_with(SESSION_KEY_PREFIX));
        // prefix + 24 bytes as hex = 4 + 48 chars.
        assert_eq!(
            k.plaintext.len(),
            CLIENT_KEY_PREFIX.len() + KEY_ENTROPY_BYTES * 2
        );
        assert_eq!(k.prefix.len(), KEY_PREFIX_LEN);
        assert_eq!(k.prefix, &k.plaintext[..KEY_PREFIX_LEN]);
    }

    #[test]
    fn session_key_shape() {
        let k = generate_session_key();
        assert!(k.plaintext.starts_with(SESSION_KEY_PREFIX));
        assert_eq!(
            k.plaintext.len(),
            SESSION_KEY_PREFIX.len() + KEY_ENTROPY_BYTES * 2
        );
    }

    #[test]
    fn keys_are_unique() {
        let a = generate_client_key().plaintext;
        let b = generate_client_key().plaintext;
        assert_ne!(a, b);
    }

    #[test]
    fn hash_round_trips() {
        let k = generate_client_key();
        let hash = hash_key(&k.plaintext).unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_key(&k.plaintext, &hash).unwrap());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let hash = hash_key("bae_correct").unwrap();
        assert!(!verify_key("bae_wrong", &hash).unwrap());
    }

    #[test]
    fn distinct_salts_produce_distinct_hashes() {
        let h1 = hash_key("bae_same").unwrap();
        let h2 = hash_key("bae_same").unwrap();
        assert_ne!(h1, h2, "each hash must use a fresh random salt");
        // ...yet both verify.
        assert!(verify_key("bae_same", &h1).unwrap());
        assert!(verify_key("bae_same", &h2).unwrap());
    }

    #[test]
    fn malformed_stored_hash_errors() {
        assert!(verify_key("bae_x", "not-a-phc-string").is_err());
    }

    #[test]
    fn entropy_meets_floor() {
        // A generated key's random body (hex-encoded) must decode to at least the
        // required 128 bits of entropy. Measured off an actual key so the check
        // reflects what is really emitted, not just a constant.
        let k = generate_client_key();
        let body_hex_chars = k.plaintext.len() - CLIENT_KEY_PREFIX.len();
        let entropy_bits = (body_hex_chars / 2) * 8;
        assert!(entropy_bits >= 128, "key entropy {entropy_bits} bits < 128");
    }

    #[test]
    fn admin_key_shape_entropy_and_hash_round_trip() {
        // Mirrors `client_key_shape` + `entropy_meets_floor` + `hash_round_trips`
        // for the new admin key: prefix, ≥128 bits of entropy measured off a real
        // key, and an Argon2id hash that verifies its own plaintext and rejects a
        // wrong one (the property `baectl`'s independent hasher must match).
        let k = generate_admin_key();
        assert!(k.plaintext.starts_with(ADMIN_KEY_PREFIX));
        assert!(!k.plaintext.starts_with(SESSION_KEY_PREFIX));
        assert_eq!(
            k.plaintext.len(),
            ADMIN_KEY_PREFIX.len() + KEY_ENTROPY_BYTES * 2
        );
        // Entropy floor, measured off the actual emitted key body (hex chars).
        let body_hex_chars = k.plaintext.len() - ADMIN_KEY_PREFIX.len();
        let entropy_bits = (body_hex_chars / 2) * 8;
        assert!(
            entropy_bits >= 128,
            "admin key entropy {entropy_bits} bits < 128"
        );
        assert_eq!(k.prefix.len(), KEY_PREFIX_LEN);
        assert_eq!(k.prefix, &k.plaintext[..KEY_PREFIX_LEN]);

        let hash = hash_key(&k.plaintext).unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_key(&k.plaintext, &hash).unwrap());
        assert!(!verify_key("bae_admin_wrong", &hash).unwrap());
    }

    #[test]
    fn near_miss_key_is_rejected() {
        // A same-length key differing only in its final character must fail. The
        // constant-time compare has no early return, so a near-miss is rejected
        // just like a wholly different key — this exercises the full-length path.
        let correct = "bae_0000000000000000000000000000000000000000000000000";
        let near_miss = "bae_0000000000000000000000000000000000000000000000001";
        assert_eq!(correct.len(), near_miss.len());
        let hash = hash_key(correct).unwrap();
        assert!(verify_key(correct, &hash).unwrap());
        assert!(!verify_key(near_miss, &hash).unwrap());
    }
}
