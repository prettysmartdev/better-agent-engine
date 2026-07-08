//! Admin-port authentication bootstrap and its runtime configuration.
//!
//! The admin listener (`BAE_ADMIN_ADDR`) used to be open — loopback-only, but
//! unauthenticated. This module implements the bootstrap-admin-key lifecycle
//! that closes it (see the "Admin authentication" design in work item 0004 and
//! `aspec/architecture/security.md`):
//!
//! On every `serve` startup — after the store opens, before either listener
//! binds — [`bootstrap`] ensures an active `role='admin'` key exists (unless
//! auth is explicitly disabled), by one of three paths:
//!
//! - **self-generate** (first boot, or after a rotation): mint a fresh
//!   `bae_admin_<random>` token, store only its Argon2id hash, and write the
//!   plaintext to [`AdminAuthConfig::key_file`] with `0600` permissions. This is
//!   the file `baectl` auto-reads.
//! - **ingest a pre-provisioned hash** (multi-replica flow): if a JSON hash file
//!   exists at [`AdminAuthConfig::hash_file`], insert its hash verbatim. The
//!   server never learns the plaintext in this path.
//! - **no-op**: an active admin key already exists and no rotation was asked
//!   for.
//!
//! Enforcement itself lives in [`crate::api::admin`]'s middleware; this module
//! only decides *whether* enforcement is on and prepares the key material.

use std::path::PathBuf;

use serde::Deserialize;

use crate::store::{keys, Store};

/// Default plaintext admin-key file (written by the server on self-generate).
pub const DEFAULT_ADMIN_KEY_FILE: &str = "/var/lib/bae/admin-key.pem";
/// Default pre-provisioned hash file (read-only input; never written).
pub const DEFAULT_ADMIN_KEY_HASH_FILE: &str = "/var/lib/bae/admin-key-hash.pem";

/// Env var overriding the plaintext key-file path.
pub const ENV_ADMIN_KEY_FILE: &str = "BAE_ADMIN_KEY_FILE";
/// Env var overriding the hash-file path.
pub const ENV_ADMIN_KEY_HASH_FILE: &str = "BAE_ADMIN_KEY_HASH_FILE";
/// Env var that, when truthy, disables admin auth (standing deployment choice).
pub const ENV_DISABLE_ADMIN_AUTH: &str = "BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH";

/// The `name` recorded for a self-generated admin key.
const SELF_GENERATED_NAME: &str = "bootstrap-admin";

/// Resolved admin-auth settings for one `serve` run.
///
/// Built from the `--admin-key-file` / `--admin-key-hash-file` flags (which win
/// over the matching env vars, which win over the defaults), plus the
/// `--rotate-admin-key` and `--dangerously-disable-admin-auth` /
/// `BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH` switches.
#[derive(Debug, Clone)]
pub struct AdminAuthConfig {
    /// Plaintext key file — written on self-generate, read by `baectl`.
    pub key_file: PathBuf,
    /// Pre-provisioned hash file — read-only input.
    pub hash_file: PathBuf,
    /// Mint fresh material this boot, discarding any existing admin key and
    /// ignoring the hash file. One-shot; deliberately has no env-var equivalent.
    pub rotate: bool,
    /// Disable enforcement entirely (today's zero-auth behavior), loudly logged.
    pub disabled: bool,
}

/// The on-disk pre-provisioned hash document (`BAE_ADMIN_KEY_HASH_FILE`).
///
/// Argon2id's PHC encoding embeds its own salt and cost parameters, so the
/// `key_hash` is independently verifiable by the server with no coordination
/// with whatever produced it (`baectl auth create key`).
#[derive(Debug, Deserialize)]
struct AdminKeyHashFile {
    /// Argon2id PHC string, e.g. `$argon2id$v=19$m=65536,t=3,p=1$...`.
    key_hash: String,
    /// Display prefix, e.g. `bae_admin_1a2b`.
    prefix: String,
    /// Human name recorded on the key row. Optional in the file (only `key_hash`
    /// and `prefix` are required); defaults to match `baectl auth create key`.
    #[serde(default = "default_hash_file_name")]
    name: String,
}

/// Default `name` when a hash file omits it — matches `baectl auth create key`'s
/// own `--name` default.
fn default_hash_file_name() -> String {
    "provisioned-admin".to_string()
}

/// A failure during the admin-auth bootstrap.
#[derive(Debug)]
pub enum AdminAuthError {
    /// The hash file exists but could not be parsed (bad JSON, missing field, or
    /// a `key_hash` that is not a valid Argon2id PHC string). An operator
    /// authoring/transfer mistake — a usage error (exit 2).
    MalformedHashFile { path: PathBuf, detail: String },
    /// A database error while reading, inserting, or revoking admin keys.
    Db(rusqlite::Error),
    /// Hashing a self-generated key failed (should not happen with valid params).
    Key(keys::KeyError),
    /// Writing the plaintext key file failed (e.g. directory absent, permissions).
    WriteKeyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Deleting the plaintext key file during rotation failed.
    RemoveKeyFile {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl AdminAuthError {
    /// Process exit code per `aspec/uxui/cli.md`: a malformed hash file is a
    /// usage error (2); every other variant is a runtime error (1).
    pub fn exit_code(&self) -> i32 {
        match self {
            AdminAuthError::MalformedHashFile { .. } => 2,
            _ => 1,
        }
    }
}

impl std::fmt::Display for AdminAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminAuthError::MalformedHashFile { path, detail } => write!(
                f,
                "admin key hash file {} is malformed: {detail}",
                path.display()
            ),
            AdminAuthError::Db(e) => write!(f, "database error during admin-auth bootstrap: {e}"),
            AdminAuthError::Key(e) => write!(f, "failed to hash generated admin key: {e}"),
            AdminAuthError::WriteKeyFile { path, source } => write!(
                f,
                "cannot write admin key file {}: {source}",
                path.display()
            ),
            AdminAuthError::RemoveKeyFile { path, source } => write!(
                f,
                "cannot delete admin key file {} during rotation: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for AdminAuthError {}

/// Run the startup admin-auth bootstrap.
///
/// Returns `Ok(true)` when enforcement should be layered on the admin router,
/// `Ok(false)` when auth is disabled. Implements the six-step lifecycle from the
/// work item; see the module docs. Must be called after the store opens and
/// before either listener binds.
///
/// The caller is responsible for having already rejected the contradictory
/// `--dangerously-disable-admin-auth` + `--rotate-admin-key` combination at
/// flag-parse time (exit 2) so nothing here touches the DB or filesystem in that
/// case.
pub fn bootstrap(store: &Store, cfg: &AdminAuthConfig) -> Result<bool, AdminAuthError> {
    // Step 5: auth disabled — never touch keys, and warn loudly on every boot so
    // it is never silently forgotten in a long-lived deployment.
    if cfg.disabled {
        tracing::warn!(
            "admin API authentication is DISABLED (--dangerously-disable-admin-auth) — \
             anyone able to reach the admin port has full control"
        );
        return Ok(false);
    }

    // Step 3: rotation always mints brand-new material and ignores any hash file.
    if cfg.rotate {
        store
            .with_conn(keys::revoke_active_admin_keys)
            .map_err(AdminAuthError::Db)?;
        remove_key_file_if_present(cfg)?;
        self_generate(store, cfg)?;
        tracing::info!(path = %cfg.key_file.display(), "admin key rotated");
        return Ok(true);
    }

    // Steps 1–2: an active admin key already exists and no rotation was asked
    // for — nothing to do.
    let existing = store
        .with_conn(keys::find_active_admin_key)
        .map_err(AdminAuthError::Db)?;
    if existing.is_some() {
        return Ok(true);
    }

    // Step 4: no active admin key — ingest a pre-provisioned hash if one is
    // present, else self-generate.
    if cfg.hash_file.exists() {
        let parsed = read_hash_file(cfg)?;
        store
            .with_conn(|c| {
                keys::insert_admin_key_from_hash(c, &parsed.name, &parsed.prefix, &parsed.key_hash)
            })
            .map_err(AdminAuthError::Db)?;
        tracing::info!(
            path = %cfg.hash_file.display(),
            "admin key hash loaded from pre-provisioned file"
        );
    } else {
        self_generate(store, cfg)?;
        tracing::info!(
            path = %cfg.key_file.display(),
            "no admin key found; generated new admin key, written to file"
        );
    }
    Ok(true)
}

/// Mint a fresh admin key, store its hash, and write the plaintext `0600`.
fn self_generate(store: &Store, cfg: &AdminAuthConfig) -> Result<(), AdminAuthError> {
    let generated = keys::generate_admin_key();
    store
        .with_conn(|c| keys::insert_generated_admin_key(c, SELF_GENERATED_NAME, &generated))
        .map_err(|e| match e {
            keys::InsertError::Key(e) => AdminAuthError::Key(e),
            keys::InsertError::Sqlite(e) => AdminAuthError::Db(e),
        })?;
    write_key_file(&cfg.key_file, &generated.plaintext)?;
    Ok(())
}

/// Read and validate the pre-provisioned hash file. A read error, invalid JSON,
/// missing field, or a `key_hash` that is not a valid Argon2id PHC string is a
/// usage error (the operator authored/transferred it wrong).
fn read_hash_file(cfg: &AdminAuthConfig) -> Result<AdminKeyHashFile, AdminAuthError> {
    let raw =
        std::fs::read_to_string(&cfg.hash_file).map_err(|e| AdminAuthError::MalformedHashFile {
            path: cfg.hash_file.clone(),
            detail: format!("cannot read file: {e}"),
        })?;
    let parsed: AdminKeyHashFile =
        serde_json::from_str(&raw).map_err(|e| AdminAuthError::MalformedHashFile {
            path: cfg.hash_file.clone(),
            detail: e.to_string(),
        })?;
    if parsed.key_hash.trim().is_empty() || parsed.prefix.trim().is_empty() {
        return Err(AdminAuthError::MalformedHashFile {
            path: cfg.hash_file.clone(),
            detail: "key_hash and prefix must be non-empty".to_string(),
        });
    }
    // Reject a hash that is not a parseable Argon2id PHC string now, at boot,
    // rather than letting every admin request silently fail to verify later.
    if !keys::is_valid_key_hash(&parsed.key_hash) {
        return Err(AdminAuthError::MalformedHashFile {
            path: cfg.hash_file.clone(),
            detail: "key_hash is not a valid Argon2id PHC string".to_string(),
        });
    }
    Ok(parsed)
}

/// Write `plaintext` (single line) to `path` with owner-only (`0600`)
/// permissions. The container runs as the non-root `bae` user, so `0600` keeps
/// the live credential readable only by that user.
fn write_key_file(path: &std::path::Path, plaintext: &str) -> Result<(), AdminAuthError> {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|source| AdminAuthError::WriteKeyFile {
            path: path.to_path_buf(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode(0o600)` above only applies when the file is *created*; a stale
        // pre-existing key file being overwritten would keep its old (possibly
        // looser) permissions, so clamp explicitly before the secret is written.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| AdminAuthError::WriteKeyFile {
                path: path.to_path_buf(),
                source,
            })?;
    }
    // A trailing newline keeps `cat`/editors friendly; readers (`baectl`) must
    // trim surrounding whitespace before using the token.
    writeln!(file, "{plaintext}").map_err(|source| AdminAuthError::WriteKeyFile {
        path: path.to_path_buf(),
        source,
    })
}

/// Delete the plaintext key file during rotation, if it exists. A missing file
/// is fine (nothing to remove).
fn remove_key_file_if_present(cfg: &AdminAuthConfig) -> Result<(), AdminAuthError> {
    match std::fs::remove_file(&cfg.key_file) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AdminAuthError::RemoveKeyFile {
            path: cfg.key_file.clone(),
            source,
        }),
    }
}
