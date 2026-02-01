//! Database schema migrations
//!
//! This module handles versioned database migrations. Each migration is
//! tracked in a `_migrations` table to ensure migrations run exactly once.

use rusqlite::Connection;

use crate::Result;

/// A database migration with a version number and SQL to execute.
pub struct Migration {
    /// The version number (must be unique and increasing).
    pub version: u32,
    /// The SQL to execute for this migration.
    pub sql: &'static str,
}

/// All available migrations, in version order.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("migrations/001_initial.sql"),
}];

/// Run all pending migrations on the database connection.
///
/// # Errors
/// Returns an error if creating the migrations table or running a migration fails.
pub fn run_migrations(conn: &Connection) -> Result<()> {
    // Create migrations tracking table if it doesn't exist
    conn.execute(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
        [],
    )?;

    // Get the current migration version
    let current: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Run any pending migrations
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        conn.execute_batch(migration.sql)?;
        conn.execute(
            "INSERT INTO _migrations (version) VALUES (?1)",
            [migration.version],
        )?;
    }

    Ok(())
}

/// Get the current migration version.
///
/// # Errors
/// Returns an error if the query fails.
pub fn current_version(conn: &Connection) -> Result<u32> {
    // Check if migrations table exists
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_migrations'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !table_exists {
        return Ok(0);
    }

    let version: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_run_migrations() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Check that tables were created
        let albums_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='albums'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(albums_exists);

        // Check migration was recorded
        let version = current_version(&conn).unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn test_migrations_idempotent() {
        let conn = Connection::open_in_memory().unwrap();

        // Run migrations twice
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        // Should still be at version 1
        let version = current_version(&conn).unwrap();
        assert_eq!(version, 1);
    }
}
