//! Embedded, forward-only migration runner.
//!
//! Every migration's SQL is embedded in the binary via [`include_str!`] so the
//! server carries its own schema — no migration files ship alongside it. On
//! startup the runner:
//!
//! 1. Opens a single `IMMEDIATE` transaction, which acquires the database write
//!    lock up front. If two processes start against the same database at once,
//!    one wins the lock and the other blocks (up to the busy timeout) and then
//!    sees the migrations already applied — so migrations are never
//!    double-applied.
//! 2. Reads the current schema version (0 if the `schema_version` table does not
//!    yet exist).
//! 3. Refuses to continue if that version is *ahead* of the highest migration
//!    this binary knows about (a database written by a newer server).
//! 4. Applies every migration with a higher version in order, recording each in
//!    `schema_version`, and commits the whole batch atomically.
//!
//! Migrations are forward-only: never edit a shipped migration, only append a
//! new one (see `aspec/devops/operations.md`).

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// One embedded migration.
struct Migration {
    version: i64,
    /// Short identifier, for logging.
    name: &'static str,
    sql: &'static str,
}

/// All known migrations, in ascending version order.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "schema_version",
        sql: include_str!("migrations/0001_schema_version.sql"),
    },
    Migration {
        version: 2,
        name: "keys",
        sql: include_str!("migrations/0002_keys.sql"),
    },
    Migration {
        version: 3,
        name: "profiles",
        sql: include_str!("migrations/0003_profiles.sql"),
    },
    Migration {
        version: 4,
        name: "sessions",
        sql: include_str!("migrations/0004_sessions.sql"),
    },
    Migration {
        version: 5,
        name: "session_events",
        sql: include_str!("migrations/0005_session_events.sql"),
    },
    Migration {
        version: 6,
        name: "profiles_sandboxes",
        sql: include_str!("migrations/0006_profiles_sandboxes.sql"),
    },
];

/// The highest migration version this binary knows how to apply.
pub const LATEST_VERSION: i64 = 6;

/// A migration-runner failure.
#[derive(Debug)]
pub enum MigrationError {
    /// The database's schema version is newer than this binary understands.
    /// Refuse to start rather than run against an unknown schema.
    Ahead { db_version: i64, known: i64 },
    /// An underlying SQLite error while reading or applying migrations.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrationError::Ahead { db_version, known } => write!(
                f,
                "database schema version {db_version} is newer than this binary supports \
                 (max {known}); upgrade the server or restore a compatible snapshot"
            ),
            MigrationError::Sqlite(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<rusqlite::Error> for MigrationError {
    fn from(e: rusqlite::Error) -> Self {
        MigrationError::Sqlite(e)
    }
}

/// Apply all pending migrations. See the module docs for the concurrency and
/// safety guarantees.
pub fn run(conn: &mut Connection) -> Result<(), MigrationError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let current = current_version(&tx)?;
    if current > LATEST_VERSION {
        return Err(MigrationError::Ahead {
            db_version: current,
            known: LATEST_VERSION,
        });
    }

    for m in MIGRATIONS {
        if m.version > current {
            tx.execute_batch(m.sql)?;
            tx.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (?1, datetime('now'))",
                params![m.version],
            )?;
            tracing::info!(version = m.version, name = m.name, "applied migration");
        }
    }

    tx.commit()?;
    Ok(())
}

/// The highest applied migration version, or 0 if the schema-version table does
/// not yet exist (a brand-new database).
fn current_version(conn: &Connection) -> Result<i64, MigrationError> {
    let table_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);

    if !table_exists {
        return Ok(0);
    }

    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_applies_everything() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), LATEST_VERSION);
    }

    #[test]
    fn rerun_is_a_noop() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        // A second run applies nothing and leaves exactly one row per migration.
        run(&mut conn).unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, LATEST_VERSION);
    }

    #[test]
    fn database_ahead_of_binary_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        // Simulate a database written by a future server.
        conn.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (?1, datetime('now'))",
            params![LATEST_VERSION + 1],
        )
        .unwrap();

        let err = run(&mut conn).unwrap_err();
        assert!(matches!(err, MigrationError::Ahead { .. }));
    }
}
