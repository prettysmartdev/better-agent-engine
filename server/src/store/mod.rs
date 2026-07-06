//! SQLite persistence.
//!
//! [`Store`] owns the single SQLite connection, runs embedded migrations on
//! open, and hands out guarded access to later layers. It is cheap to clone
//! (`Arc<Mutex<Connection>>` internally); dropping the last clone closes the
//! database.
//!
//! Key generation, hashing, and constant-time verification live in [`keys`].

pub mod keys;
mod migrations;
pub mod profiles;
pub mod sessions;

pub use migrations::{MigrationError, LATEST_VERSION};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::Connection;

/// SQL expression that renders "now" as a millisecond-precision UTC ISO-8601
/// string (`2026-07-06T18:26:01.123Z`). Used for every `created_at` /
/// `updated_at` / `last_used_at` / `deleted_at` timestamp so the whole database
/// speaks one timestamp format without pulling in a date-time crate.
pub const NOW_SQL: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// Generate an opaque, type-prefixed resource id, e.g. `ses_1a2b…`.
///
/// 16 random bytes (128 bits) from the OS CSPRNG, hex-encoded. IDs are opaque
/// and never parsed back apart — only their prefix communicates the type (per
/// `aspec/architecture/apis.md`).
pub fn generate_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(prefix.len() + 32);
    out.push_str(prefix);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in &bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Handle to the server's SQLite database.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// Failure opening or migrating the database.
#[derive(Debug)]
pub enum StoreError {
    /// The database file could not be opened or created at the given path.
    /// Typically a missing parent directory or insufficient permissions.
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    /// A migration failed, or the database schema is newer than this binary.
    Migrate(MigrationError),
    /// Any other SQLite error during setup (e.g. setting pragmas).
    Sqlite(rusqlite::Error),
}

impl StoreError {
    /// Process exit code — always 1 (runtime error) per `aspec/uxui/cli.md`.
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Open { path, source } => write!(
                f,
                "cannot open database at {}: {source} (is the directory present and writable?)",
                path.display()
            ),
            StoreError::Migrate(e) => write!(f, "migration failed: {e}"),
            StoreError::Sqlite(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl Store {
    /// Open (creating if absent) the database at `path`, apply pending
    /// migrations transactionally, and return a ready handle.
    ///
    /// Refuses to proceed if the database's `schema_version` is ahead of the
    /// highest migration this binary knows about.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let mut conn = Connection::open(path).map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Store::prepare(&mut conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory database with all migrations applied. Test-only.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Store, StoreError> {
        let mut conn = Connection::open_in_memory().map_err(StoreError::Sqlite)?;
        Store::prepare(&mut conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn prepare(conn: &mut Connection) -> Result<(), StoreError> {
        // Wait rather than error if another starting process holds the write
        // lock while it migrates.
        conn.busy_timeout(Duration::from_secs(10))
            .map_err(StoreError::Sqlite)?;
        // WAL improves concurrent read/write behaviour; best-effort (an in-memory
        // database, for instance, will not switch).
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        migrations::run(conn).map_err(StoreError::Migrate)?;
        Ok(())
    }

    /// Run a closure with locked access to the connection. This is the seam the
    /// API/engine layers use; keeping the `Mutex` internal means callers never
    /// hold a raw lock guard across an await point.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let guard = self.conn.lock().expect("database mutex poisoned");
        f(&guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_applies_all_migrations() {
        let store = Store::open_in_memory().unwrap();
        let version: i64 = store.with_conn(|c| {
            c.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(version, LATEST_VERSION);
    }

    #[test]
    fn expected_tables_exist() {
        let store = Store::open_in_memory().unwrap();
        for table in [
            "schema_version",
            "keys",
            "profiles",
            "sessions",
            "session_events",
        ] {
            let found: i64 = store.with_conn(|c| {
                c.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap()
            });
            assert_eq!(found, 1, "missing table {table}");
        }
    }
}
