//! Transactional, backup-first database migrations.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{Connection, DatabaseName, TransactionBehavior};

use crate::{Error, Result};

const LATEST_VERSION: u32 = 2;

pub struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("migrations/001_initial.sql"),
    },
    Migration {
        version: 2,
        sql: include_str!("migrations/002_safety.sql"),
    },
];

#[derive(Debug, Clone, Default)]
pub struct MigrationReport {
    pub from_version: u32,
    pub to_version: u32,
    pub backup_path: Option<PathBuf>,
}

pub fn run_migrations(
    conn: &mut Connection,
    database_path: Option<&Path>,
) -> Result<MigrationReport> {
    verify_integrity(conn, "before migration")?;
    let had_schema = table_exists(conn, "items")?;
    let had_tracking = table_exists(conn, "_migrations")?;
    let recorded_version = if had_tracking {
        current_version(conn)?
    } else {
        0
    };
    let mut current = if recorded_version > 0 {
        recorded_version
    } else if had_schema {
        detect_untracked_version(conn)?
    } else {
        0
    };
    if current > LATEST_VERSION {
        return Err(Error::Recovery(format!(
            "database schema {current} is newer than supported schema {LATEST_VERSION}"
        )));
    }
    let from_version = current;

    // The backup precedes even migration bookkeeping, so it is an exact legacy snapshot.
    let backup_path = if current > 0 && current < LATEST_VERSION {
        database_path
            .map(|path| create_verified_backup(conn, path))
            .transpose()?
    } else {
        None
    };

    ensure_tracking_table(conn)?;
    if recorded_version == 0 && current > 0 {
        for version in 1..=current {
            conn.execute(
                "INSERT OR IGNORE INTO _migrations (version) VALUES (?1)",
                [version],
            )?;
        }
    }

    let starting_version = current;
    for migration in MIGRATIONS
        .iter()
        .skip_while(|migration| migration.version <= starting_version)
    {
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migration.sql)?;
        transaction.execute(
            "INSERT INTO _migrations (version) VALUES (?1)",
            [migration.version],
        )?;
        transaction.commit()?;
        current = migration.version;
    }
    verify_integrity(conn, "after migration")?;
    verify_foreign_keys(conn)?;

    Ok(MigrationReport {
        from_version,
        to_version: current,
        backup_path,
    })
}

fn detect_untracked_version(conn: &Connection) -> Result<u32> {
    if table_exists(conn, "operation_journal")? && column_exists(conn, "items", "file_size")? {
        Ok(2)
    } else if table_exists(conn, "albums")?
        && column_exists(conn, "items", "mb_trackid")?
        && column_exists(conn, "albums", "mb_albumid")?
    {
        Ok(1)
    } else {
        Err(Error::Recovery(
            "database has an unrecognized untracked schema; refusing to guess a migration".into(),
        ))
    }
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let sql = format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1");
    conn.query_row(&sql, [column], |row| row.get::<_, u64>(0))
        .map(|count| count > 0)
        .map_err(Into::into)
}

pub fn current_version(conn: &Connection) -> Result<u32> {
    if !table_exists(conn, "_migrations")? {
        return Ok(0);
    }
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _migrations",
        [],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn ensure_tracking_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
        [],
    )?;
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
        )",
        [name],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn create_verified_backup(conn: &Connection, database_path: &Path) -> Result<PathBuf> {
    let parent = database_path.parent().ok_or_else(|| {
        Error::Config(format!(
            "database path has no parent: {}",
            database_path.display()
        ))
    })?;
    let filename = database_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Config("database filename is not valid UTF-8".into()))?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.fZ");
    let backup_path = parent.join(format!("{filename}.backup-{stamp}"));
    conn.backup(DatabaseName::Main, &backup_path, None)?;
    let backup =
        Connection::open_with_flags(&backup_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    verify_integrity(&backup, "backup verification")?;
    Ok(backup_path)
}

fn verify_integrity(conn: &Connection, context: &str) -> Result<()> {
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(Error::Recovery(format!(
            "database integrity check failed {context}: {result}"
        )))
    }
}

fn verify_foreign_keys(conn: &Connection) -> Result<()> {
    let violations: u64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations == 0 {
        Ok(())
    } else {
        Err(Error::Recovery(format!(
            "database has {violations} foreign-key violation(s)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        let first = run_migrations(&mut connection, None)?;
        let second = run_migrations(&mut connection, None)?;
        assert_eq!(first.to_version, LATEST_VERSION);
        assert_eq!(second.from_version, LATEST_VERSION);
        assert_eq!(current_version(&connection)?, LATEST_VERSION);
        Ok(())
    }

    #[test]
    fn recognizes_untracked_v1_schema() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        let report = run_migrations(&mut connection, None)?;
        assert_eq!(report.from_version, 1);
        assert_eq!(report.to_version, 2);
        Ok(())
    }

    #[test]
    fn recognizes_v1_schema_with_empty_tracking_table() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        ensure_tracking_table(&connection)?;
        let report = run_migrations(&mut connection, None)?;
        assert_eq!(report.from_version, 1);
        assert_eq!(report.to_version, 2);
        Ok(())
    }

    #[test]
    fn file_migration_creates_a_verified_v1_backup() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let database_path = temporary.path().join("library.db");
        {
            let connection = Connection::open(&database_path)?;
            connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        }
        let mut connection = Connection::open(&database_path)?;
        let report = run_migrations(&mut connection, Some(&database_path))?;
        let backup_path = report
            .backup_path
            .ok_or_else(|| Error::Recovery("migration did not create a backup".into()))?;
        assert!(backup_path.exists());
        let backup =
            Connection::open_with_flags(backup_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        assert_eq!(current_version(&backup)?, 0);
        assert!(!table_exists(&backup, "_migrations")?);
        assert!(!table_exists(&backup, "operation_journal")?);
        Ok(())
    }
}
