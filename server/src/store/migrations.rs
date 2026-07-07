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
        name: "admin_key_role",
        sql: include_str!("migrations/0006_admin_key_role.sql"),
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
///
/// Foreign-key enforcement is disabled for the duration of the migration batch
/// and restored to its prior state afterward. This is the SQLite-recommended
/// procedure for the create-copy-drop-rename table rebuilds some migrations use
/// (e.g. 0006 rebuilds the self-referential `keys` table): with foreign keys on,
/// `DROP TABLE keys` performs an implicit `DELETE` that a copied row's
/// `client_id` self-reference would violate. `PRAGMA foreign_keys` is a no-op
/// inside a transaction, so it must be toggled here, around the migration
/// transaction opened by [`run_batch`].
pub fn run(conn: &mut Connection) -> Result<(), MigrationError> {
    let fk_was_on: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))? == 1;
    if fk_was_on {
        conn.pragma_update(None, "foreign_keys", "OFF")?;
    }
    let result = run_batch(conn);
    if fk_was_on {
        // Restore the connection's prior enforcement state. Best-effort: a
        // successful migration must not be masked by a failure to turn FK back
        // on, and the batch itself already committed or rolled back.
        let _ = conn.pragma_update(None, "foreign_keys", "ON");
    }
    result
}

/// Apply all pending migrations within a single immediate transaction. Callers
/// go through [`run`], which manages foreign-key enforcement around this.
fn run_batch(conn: &mut Connection) -> Result<(), MigrationError> {
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

    /// Apply every migration with `version <= max_version` directly (outside the
    /// single-transaction `run`), so a test can observe the schema *between* two
    /// migrations — here, the state just before 0006 widens the `role` CHECK.
    fn apply_up_to(conn: &Connection, max_version: i64) {
        for m in MIGRATIONS.iter().filter(|m| m.version <= max_version) {
            conn.execute_batch(m.sql).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (?1, datetime('now'))",
                params![m.version],
            )
            .unwrap();
        }
    }

    #[test]
    fn migration_0006_preserves_rows_and_widens_role_check() {
        // A connection with foreign-key enforcement ON — the realistic upgrade
        // environment. `run` must disable FK for the rebuild; if it does not, the
        // `DROP TABLE keys` in 0006 fails on the seeded session key's self-
        // referential `client_id`, so this test guards that fix directly.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        assert_eq!(
            conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "test precondition: foreign keys must be enforced",
        );

        // Schema as it stood before 0006 (client/session only).
        apply_up_to(&conn, 5);

        // The `keys.profile_id` foreign key is enforced on this connection, so
        // the referenced profile must exist before a key can point at it.
        conn.execute("INSERT INTO profiles (id, name) VALUES ('pro_1','p1')", [])
            .unwrap();

        // Seed a client row and a session row with fully specified values.
        conn.execute(
            "INSERT INTO keys \
               (id, name, key_hash, key_prefix, role, profile_id, client_id, created_at, last_used_at) \
             VALUES ('key_c','client-1','hash_c','bae_1a2b','client','pro_1',NULL,\
                     '2026-01-01T00:00:00.000Z','2026-01-02T00:00:00.000Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO keys \
               (id, name, key_hash, key_prefix, role, profile_id, client_id, created_at, last_used_at) \
             VALUES ('key_s','ses_1','hash_s','bae_ses_','session',NULL,'key_c',\
                     '2026-01-03T00:00:00.000Z',NULL)",
            [],
        )
        .unwrap();

        // The OLD CHECK constraint must reject an admin row.
        assert!(
            conn.execute("INSERT INTO keys (id, role) VALUES ('key_a','admin')", [])
                .is_err(),
            "pre-0006 CHECK must reject role='admin'"
        );

        // Apply the keys-table-rebuild migration through the real runner (which
        // manages foreign-key enforcement around the rebuild).
        run(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), LATEST_VERSION);
        // FK enforcement is restored to its prior (on) state after the batch.
        assert_eq!(
            conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "foreign-key enforcement must be restored after migrating",
        );

        // Every existing row is preserved unchanged (id, name, hashes, role,
        // fk columns, timestamps). Columns in order:
        // id, name, key_hash, key_prefix, role, profile_id, created_at, last_used_at.
        #[allow(clippy::type_complexity)]
        let client: (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT id, name, key_hash, key_prefix, role, profile_id, created_at, last_used_at \
                 FROM keys WHERE id='key_c'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            client,
            (
                "key_c".into(),
                "client-1".into(),
                "hash_c".into(),
                "bae_1a2b".into(),
                "client".into(),
                Some("pro_1".to_string()),
                "2026-01-01T00:00:00.000Z".into(),
                Some("2026-01-02T00:00:00.000Z".to_string()),
            ),
        );

        let session: (String, String, String, Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT id, name, role, profile_id, client_id, last_used_at FROM keys WHERE id='key_s'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(
            session,
            (
                "key_s".into(),
                "ses_1".into(),
                "session".into(),
                None,
                Some("key_c".to_string()),
                None,
            ),
        );

        // No rows were dropped or duplicated by the rebuild.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // The widened CHECK now accepts an admin insert the old one rejected.
        conn.execute("INSERT INTO keys (id, role) VALUES ('key_a','admin')", [])
            .unwrap();
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
