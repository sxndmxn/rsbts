//! Transactional, backup-first database migrations.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{Connection, DatabaseName, TransactionBehavior};

use crate::{Error, Result};

pub const LATEST_VERSION: u32 = 9;

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
    Migration {
        version: 3,
        sql: include_str!("migrations/003_source_identity.sql"),
    },
    Migration {
        version: 4,
        sql: include_str!("migrations/004_asset_ownership.sql"),
    },
    Migration {
        version: 5,
        sql: include_str!("migrations/005_canonical_catalog.sql"),
    },
    Migration {
        version: 6,
        sql: include_str!("migrations/006_operations_and_preservation.sql"),
    },
    Migration {
        version: 7,
        sql: include_str!("migrations/007_fixity_scheduling.sql"),
    },
    Migration {
        version: 8,
        sql: include_str!("migrations/008_ancillary_assets.sql"),
    },
    Migration {
        version: 9,
        sql: include_str!("migrations/009_constant_time_statistics.sql"),
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
    verify_foreign_keys(conn)?;
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
        if has_non_tracking_user_tables(conn)? {
            return Err(Error::Recovery(
                "database contains non-rsbts tables or a partial untracked schema; refusing to initialize over it"
                    .into(),
            ));
        }
        0
    };
    if current > LATEST_VERSION {
        return Err(Error::Recovery(format!(
            "database schema {current} is newer than supported schema {LATEST_VERSION}"
        )));
    }
    if current > 0 {
        verify_schema_version(conn, current)?;
    }
    let from_version = current;
    let needs_migration = current < LATEST_VERSION || recorded_version == 0;

    if needs_migration && current > 0 {
        verify_integrity(conn, "before migration")?;
    }

    // A current, tracked schema needs no writes and no deep scan. Full integrity and
    // foreign-key checks are intentionally owned by explicit audit and real migrations.
    if had_tracking && current == LATEST_VERSION {
        return Ok(MigrationReport {
            from_version,
            to_version: current,
            backup_path: None,
        });
    }

    // Validate existing data before changing either its schema or migration bookkeeping.
    // A brand-new empty database has nothing meaningful to scan before schema creation.
    if had_schema {
        verify_integrity(conn, "before migration")?;
        verify_foreign_keys(conn)?;
    }

    // The backup precedes even migration bookkeeping, so it is an exact legacy snapshot.
    let backup_path = if current > 0 && needs_migration {
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
    if needs_migration {
        verify_integrity(conn, "after migration")?;
    }
    verify_foreign_keys(conn)?;
    verify_schema_version(conn, current)?;

    Ok(MigrationReport {
        from_version,
        to_version: current,
        backup_path,
    })
}

fn detect_untracked_version(conn: &Connection) -> Result<u32> {
    let has_journal = table_exists(conn, "operation_journal")?;
    let has_assets = table_exists(conn, "assets")?
        && table_exists(conn, "library_roots")?
        && table_exists(conn, "item_assets")?
        && table_exists(conn, "album_assets")?;
    let has_source_identity =
        has_journal && column_exists(conn, "operation_files", "source_identity")?;
    let has_owned_identity =
        has_journal && column_exists(conn, "operation_files", "owned_identity")?;
    let has_singleton = column_exists(conn, "items", "singleton")?;
    let has_entity_metadata = table_exists(conn, "entity_metadata")?;
    let has_external_ids = table_exists(conn, "external_ids")?;
    let v4_markers = [has_singleton, has_entity_metadata, has_external_ids];
    if v4_markers.into_iter().any(|present| present) {
        if v4_markers.into_iter().all(|present| present)
            && has_journal
            && has_source_identity
            && has_owned_identity
        {
            return Ok(4);
        }
        return Err(Error::Recovery(
            "database has a partial untracked core-metadata schema; refusing to guess a migration"
                .into(),
        ));
    }
    if has_journal {
        if has_source_identity
            && has_owned_identity
            && has_assets
            && table_exists(conn, "release_groups")?
            && table_exists(conn, "metadata_claims")?
            && table_exists(conn, "durable_plans")?
            && table_exists(conn, "fixity_schedules")?
            && table_exists(conn, "ancillary_metadata")?
        {
            Ok(8)
        } else if has_source_identity
            && has_owned_identity
            && has_assets
            && table_exists(conn, "release_groups")?
            && table_exists(conn, "metadata_claims")?
            && table_exists(conn, "durable_plans")?
            && table_exists(conn, "fixity_schedules")?
        {
            Ok(7)
        } else if has_source_identity
            && has_owned_identity
            && has_assets
            && table_exists(conn, "release_groups")?
            && table_exists(conn, "metadata_claims")?
            && table_exists(conn, "durable_plans")?
        {
            Ok(6)
        } else if has_source_identity
            && has_owned_identity
            && has_assets
            && table_exists(conn, "release_groups")?
            && table_exists(conn, "metadata_claims")?
        {
            Ok(5)
        } else if has_source_identity && has_owned_identity && has_assets {
            Ok(4)
        } else if has_source_identity && has_owned_identity && !has_assets {
            Ok(3)
        } else if !has_source_identity
            && !has_owned_identity
            && column_exists(conn, "items", "file_size")?
        {
            Ok(2)
        } else {
            Err(Error::Recovery(
                "database has a partial untracked journal schema; refusing to guess a migration"
                    .into(),
            ))
        }
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
    let mut statement = conn.prepare("SELECT version FROM _migrations ORDER BY version")?;
    let versions = statement
        .query_map([], |row| row.get::<_, u32>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (index, version) in versions.iter().enumerate() {
        let expected = u32::try_from(index + 1)
            .map_err(|error| Error::Recovery(format!("invalid migration history: {error}")))?;
        if *version != expected {
            return Err(Error::Recovery(format!(
                "migration history is not contiguous: expected version {expected}, found {version}"
            )));
        }
    }
    Ok(versions.last().copied().unwrap_or(0))
}

fn verify_schema_version(conn: &Connection, version: u32) -> Result<()> {
    if version == 0 {
        return Ok(());
    }
    let v1 = table_exists(conn, "albums")?
        && table_exists(conn, "items")?
        && table_exists(conn, "items_fts")?
        && schema_object_exists(conn, "trigger", "items_ai")?
        && schema_object_exists(conn, "trigger", "items_ad")?
        && schema_object_exists(conn, "trigger", "items_au")?
        && column_exists(conn, "albums", "mb_albumid")?
        && column_exists(conn, "items", "mb_trackid")?;
    let v2 = version < 2
        || (table_exists(conn, "operation_journal")?
            && table_exists(conn, "operation_files")?
            && column_exists(conn, "items", "file_size")?
            && column_exists(conn, "items", "metadata_provider")?
            && column_exists(conn, "items", "external_track_id")?
            && column_exists(conn, "items", "external_release_id")?
            && column_exists(conn, "albums", "metadata_provider")?
            && column_exists(conn, "albums", "external_release_id")?
            && column_exists(conn, "operation_files", "content_hash")?);
    let v3 = version < 3
        || (column_exists(conn, "operation_files", "source_identity")?
            && column_exists(conn, "operation_files", "owned_identity")?);
    let v4 = version < 4
        || (table_exists(conn, "library_roots")?
            && table_exists(conn, "assets")?
            && table_exists(conn, "item_assets")?
            && table_exists(conn, "album_assets")?
            && column_exists(conn, "assets", "entry_identity")?
            && column_exists(conn, "operation_journal", "completed_at")?
            && column_exists(conn, "operation_files", "sha256")?
            && column_exists(conn, "operation_files", "asset_id")?);
    let v5 = version < 5
        || (table_exists(conn, "release_groups")?
            && table_exists(conn, "releases")?
            && table_exists(conn, "media")?
            && table_exists(conn, "release_tracks")?
            && table_exists(conn, "recordings")?
            && table_exists(conn, "works")?
            && table_exists(conn, "artists")?
            && table_exists(conn, "metadata_claims")?
            && table_exists(conn, "canonical_values")?
            && table_exists(conn, "provider_snapshots")?
            && table_exists(conn, "provider_jobs")?
            && column_exists(conn, "albums", "canonical_release_id")?
            && column_exists(conn, "items", "release_track_id")?
            && column_exists(conn, "items", "recording_id")?);
    let v6 = version < 6
        || (table_exists(conn, "durable_plans")?
            && table_exists(conn, "plan_events")?
            && table_exists(conn, "fixity_runs")?
            && table_exists(conn, "fixity_results")?
            && table_exists(conn, "preservation_manifests")?
            && table_exists(conn, "backup_restore_runs")?
            && table_exists(conn, "artwork_metadata")?
            && table_exists(conn, "projection_plans")?
            && table_exists(conn, "asset_projection_steps")?
            && table_exists(conn, "dedup_decisions")?
            && table_exists(conn, "recording_assets")?
            && column_exists(conn, "operation_journal", "plan_id")?
            && column_exists(conn, "operation_files", "root_id")?);
    let v7 = version < 7
        || (table_exists(conn, "fixity_schedules")?
            && column_exists(conn, "fixity_runs", "schedule_id")?);
    let v8 = version < 8 || table_exists(conn, "ancillary_metadata")?;
    let v9 = version < 9
        || (table_exists(conn, "library_statistics")?
            && table_exists(conn, "statistics_album_members")?
            && table_exists(conn, "statistics_artist_members")?);
    if v1 && v2 && v3 && v4 && v5 && v6 && v7 && v8 && v9 {
        Ok(())
    } else {
        Err(Error::Recovery(format!(
            "database schema does not match recorded migration version {version}"
        )))
    }
}

/// Validate that a read-only connection already uses the current schema.
pub fn validate_current_schema(conn: &Connection) -> Result<MigrationReport> {
    verify_foreign_keys(conn)?;
    let version = current_version(conn)?;
    if version != LATEST_VERSION {
        return Err(Error::Recovery(format!(
            "database schema {version} requires migration to {LATEST_VERSION}"
        )));
    }
    verify_schema_version(conn, version)?;
    Ok(MigrationReport {
        from_version: version,
        to_version: version,
        backup_path: None,
    })
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

fn schema_object_exists(conn: &Connection, object_type: &str, name: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2
        )",
        [object_type, name],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn has_non_tracking_user_tables(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name != '_migrations'
        )",
        [],
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

pub(crate) fn integrity_issues(conn: &Connection) -> Result<Vec<String>> {
    let mut statement = conn.prepare("PRAGMA integrity_check")?;
    let results = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(results
        .into_iter()
        .filter(|result| result != "ok")
        .collect())
}

pub(crate) fn foreign_key_violation_count(conn: &Connection) -> Result<u64> {
    conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
        row.get(0)
    })
    .map_err(Into::into)
}

fn verify_integrity(conn: &Connection, context: &str) -> Result<()> {
    let issues = integrity_issues(conn)?;
    if issues.is_empty() {
        Ok(())
    } else {
        Err(Error::Recovery(format!(
            "database integrity check failed {context}: {}",
            issues.join("; ")
        )))
    }
}

fn verify_foreign_keys(conn: &Connection) -> Result<()> {
    let violations = foreign_key_violation_count(conn)?;
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
    use std::cell::RefCell;

    use super::*;

    thread_local! {
        static TRACED_SQL: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn trace_sql(sql: &str) {
        TRACED_SQL.with(|statements| statements.borrow_mut().push(sql.to_string()));
    }

    fn take_traced_sql() -> Vec<String> {
        TRACED_SQL.with(|statements| std::mem::take(&mut *statements.borrow_mut()))
    }

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
    fn current_schema_open_skips_deep_checks() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        run_migrations(&mut connection, None)?;
        connection.trace(Some(trace_sql));
        let report = run_migrations(&mut connection, None)?;
        connection.trace(None);

        assert_eq!(report.from_version, LATEST_VERSION);
        let traced = take_traced_sql().join("\n").to_ascii_lowercase();
        assert!(!traced.contains("integrity_check"));
        assert!(!traced.contains("foreign_key_check"));
        Ok(())
    }

    #[test]
    fn pending_migration_runs_deep_checks() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        connection.trace(Some(trace_sql));
        run_migrations(&mut connection, None)?;
        connection.trace(None);

        let traced = take_traced_sql().join("\n").to_ascii_lowercase();
        assert!(traced.contains("integrity_check"));
        assert!(traced.contains("foreign_key_check"));
        Ok(())
    }

    #[test]
    fn recognizes_untracked_v1_schema() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        let report = run_migrations(&mut connection, None)?;
        assert_eq!(report.from_version, 1);
        assert_eq!(report.to_version, LATEST_VERSION);
        Ok(())
    }

    #[test]
    fn recognizes_v1_schema_with_empty_tracking_table() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        ensure_tracking_table(&connection)?;
        let report = run_migrations(&mut connection, None)?;
        assert_eq!(report.from_version, 1);
        assert_eq!(report.to_version, LATEST_VERSION);
        Ok(())
    }

    #[test]
    fn recognizes_untracked_v2_schema() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        connection.execute_batch(include_str!("migrations/002_safety.sql"))?;
        let report = run_migrations(&mut connection, None)?;
        assert_eq!(report.from_version, 2);
        assert_eq!(report.to_version, LATEST_VERSION);
        assert!(column_exists(
            &connection,
            "operation_files",
            "source_identity"
        )?);
        assert!(column_exists(
            &connection,
            "operation_files",
            "owned_identity"
        )?);
        Ok(())
    }

    #[test]
    fn v4_backfills_legacy_assets_as_unverified_and_drops_the_duplicate_index() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        connection.execute_batch(include_str!("migrations/002_safety.sql"))?;
        connection.execute_batch(include_str!("migrations/003_source_identity.sql"))?;
        connection.execute(
            "INSERT INTO albums (album, albumartist, added)
             VALUES ('Album', 'Artist', '2024-01-01T00:00:00Z')",
            [],
        )?;
        let album_id = connection.last_insert_rowid();
        connection.execute(
            "INSERT INTO items
             (album_id, path, title, artist, album, format, bitrate, length, added, mtime)
             VALUES (?1, '/legacy.flac', 'Track', 'Artist', 'Album', 'FLAC', 1, 1,
                     '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            [album_id],
        )?;
        ensure_tracking_table(&connection)?;
        connection.execute("INSERT INTO _migrations (version) VALUES (1), (2), (3)", [])?;

        let report = run_migrations(&mut connection, None)?;
        assert_eq!(report.from_version, 3);
        let (state, digest): (String, Option<String>) =
            connection.query_row("SELECT verification_state, blake3 FROM assets", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        assert_eq!(state, "unverified");
        assert!(digest.is_none());
        assert!(!schema_object_exists(
            &connection,
            "index",
            "idx_items_path"
        )?);
        Ok(())
    }

    #[test]
    fn rejects_a_partial_untracked_identity_migration() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        connection.execute_batch(include_str!("migrations/002_safety.sql"))?;
        connection.execute(
            "ALTER TABLE operation_files ADD COLUMN source_identity TEXT",
            [],
        )?;

        assert!(run_migrations(&mut connection, None).is_err());
        assert!(!column_exists(
            &connection,
            "operation_files",
            "owned_identity"
        )?);
        assert!(!table_exists(&connection, "_migrations")?);
        Ok(())
    }

    #[test]
    fn rejects_gapped_migration_history() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        connection.execute_batch(include_str!("migrations/002_safety.sql"))?;
        connection.execute_batch(include_str!("migrations/003_source_identity.sql"))?;
        ensure_tracking_table(&connection)?;
        connection.execute("INSERT INTO _migrations (version) VALUES (1), (3)", [])?;
        assert!(run_migrations(&mut connection, None).is_err());
        Ok(())
    }

    #[test]
    fn rejects_a_recorded_version_that_the_schema_does_not_have() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        ensure_tracking_table(&connection)?;
        connection.execute("INSERT INTO _migrations (version) VALUES (1), (2), (3)", [])?;
        assert!(run_migrations(&mut connection, None).is_err());
        Ok(())
    }

    #[test]
    fn refuses_to_initialize_over_an_unrelated_database() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute("CREATE TABLE unrelated (value TEXT)", [])?;

        assert!(run_migrations(&mut connection, None).is_err());

        assert!(table_exists(&connection, "unrelated")?);
        assert!(!table_exists(&connection, "items")?);
        assert!(!table_exists(&connection, "_migrations")?);
        Ok(())
    }

    #[test]
    fn refuses_foreign_key_corruption_before_changing_the_schema() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
        connection.pragma_update(None, "foreign_keys", "OFF")?;
        connection.execute(
            "INSERT INTO items
             (album_id, path, title, artist, album, format, bitrate, length, added, mtime)
             VALUES (999, '/missing.flac', 'Track', 'Artist', 'Album', 'FLAC', 1, 1,
                     '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            [],
        )?;

        assert!(run_migrations(&mut connection, None).is_err());
        assert!(!table_exists(&connection, "_migrations")?);
        assert!(!table_exists(&connection, "operation_journal")?);
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

    #[test]
    fn v3_migration_backs_up_the_v2_journal() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let database_path = temporary.path().join("library.db");
        {
            let connection = Connection::open(&database_path)?;
            connection.execute_batch(include_str!("migrations/001_initial.sql"))?;
            connection.execute_batch(include_str!("migrations/002_safety.sql"))?;
            ensure_tracking_table(&connection)?;
            connection.execute("INSERT INTO _migrations (version) VALUES (1), (2)", [])?;
        }

        let mut connection = Connection::open(&database_path)?;
        let report = run_migrations(&mut connection, Some(&database_path))?;
        let backup_path = report
            .backup_path
            .ok_or_else(|| Error::Recovery("v2 migration did not create a backup".into()))?;
        assert!(column_exists(
            &connection,
            "operation_files",
            "source_identity"
        )?);
        assert!(column_exists(
            &connection,
            "operation_files",
            "owned_identity"
        )?);
        let backup =
            Connection::open_with_flags(backup_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        assert_eq!(current_version(&backup)?, 2);
        assert!(!column_exists(
            &backup,
            "operation_files",
            "source_identity"
        )?);
        assert!(!column_exists(
            &backup,
            "operation_files",
            "owned_identity"
        )?);
        Ok(())
    }
}
