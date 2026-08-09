//! SQLite-backed library, audit, journal, and recovery APIs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::types::{Type, Value};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};

use crate::artwork::ArtworkAssetMetadata;
use crate::asset::digest_file;
use crate::failpoints;
use crate::fsops::AnchoredRoot;
use crate::lease::CollectionLease;
use crate::media::{decoded_audio_essence_hash, probe_media};
use crate::migrations::{self, MigrationReport};
use crate::operations::{append_plan_event, PlanId};
use crate::query::Query;
use crate::roots::RootCapabilities;
use crate::{validate_item_metadata, Album, AudioFormat, Error, ExternalId, Item, Result};

pub struct Library {
    pub(crate) conn: Connection,
    path: Option<PathBuf>,
    migration_report: MigrationReport,
    _lease: Option<CollectionLease>,
}

/// Validate `field=value` modifications without opening a library or changing any rows.
pub fn validate_modification_fields(fields: &[String]) -> Result<()> {
    parse_modifications(fields).map(|_modifications| ())
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Stats {
    pub tracks: u64,
    pub albums: u64,
    pub artists: u64,
    pub total_length: f64,
    pub total_size: u64,
    pub unknown_sizes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "issue", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AuditIssue {
    MissingFile {
        item_id: i64,
        path: PathBuf,
    },
    UnknownFileSize {
        item_id: i64,
        path: PathBuf,
    },
    OrphanedItem {
        item_id: i64,
        album_id: i64,
    },
    SearchIndexInconsistent {
        detail: String,
    },
    InvalidTimestamp {
        table: &'static str,
        row_id: i64,
        field: &'static str,
        value: String,
    },
    MissingManagedAsset {
        asset_id: String,
        path: PathBuf,
        role: String,
    },
    MissingAssetRecord {
        item_id: i64,
        path: PathBuf,
    },
    UnverifiedAsset {
        asset_id: String,
        path: PathBuf,
    },
    AssetSizeMismatch {
        asset_id: String,
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    AssetMtimeMismatch {
        asset_id: String,
        path: PathBuf,
        expected: String,
        actual: String,
    },
    AssetEntryIdentityMismatch {
        asset_id: String,
        path: PathBuf,
    },
    AssetDigestMismatch {
        asset_id: String,
        path: PathBuf,
        algorithm: &'static str,
    },
    AssetUnreadable {
        asset_id: String,
        path: PathBuf,
        detail: String,
    },
    OrphanedManagedAsset {
        asset_id: String,
        path: PathBuf,
    },
    ProjectionDiverged {
        asset_id: String,
        path: PathBuf,
        state: String,
    },
    MediaPropertiesMismatch {
        asset_id: String,
        path: PathBuf,
    },
    AudioEssenceMismatch {
        asset_id: String,
        path: PathBuf,
    },
}

/// Cost and depth of a library audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AuditMode {
    /// Compare catalog state, paths, sizes, mtimes, ownership, and projections.
    #[default]
    Quick,
    /// Also read every managed asset and compare BLAKE3 and SHA-256 fixity.
    Deep,
}

#[derive(Debug, Clone, Default)]
pub struct AuditReport {
    issues: Vec<AuditIssue>,
    omitted: u64,
}

impl AuditReport {
    /// Issues retained in this bounded report.
    #[must_use]
    pub fn issues(&self) -> &[AuditIssue] {
        &self.issues
    }

    /// Number of additional issues omitted after the report limit was reached.
    #[must_use]
    pub const fn omitted(&self) -> u64 {
        self.omitted
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.issues.is_empty() && self.omitted == 0
    }
}

const AUDIT_ISSUE_LIMIT: usize = 4_096;

#[derive(Default)]
struct AuditIssues {
    values: Vec<AuditIssue>,
    omitted: u64,
}

impl AuditIssues {
    fn push(&mut self, issue: AuditIssue) {
        if self.values.len() < AUDIT_ISSUE_LIMIT {
            self.values.push(issue);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

/// Result of an explicit full `SQLite` integrity and foreign-key check.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IntegrityReport {
    messages: Vec<String>,
    truncated: bool,
}

impl IntegrityReport {
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.messages.is_empty() && !self.truncated
    }

    #[must_use]
    pub fn messages(&self) -> &[String] {
        &self.messages
    }

    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    pub recovered_operations: Vec<String>,
    pub unresolved: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct VerificationReport {
    pub verified: usize,
    pub skipped: Vec<(i64, PathBuf, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationKind {
    ImportCopy,
    ImportMove,
    ImportLink,
    RemoveDelete,
    TagWrite,
    ManifestWrite,
    RestoreCopy,
    PurgeDelete,
    PathWrite,
    AncillaryCopy,
    ArtworkWrite,
}

impl OperationKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ImportCopy => "import-copy",
            Self::ImportMove => "import-move",
            Self::ImportLink => "import-link",
            Self::RemoveDelete => "remove-delete",
            Self::TagWrite => "tag-write",
            Self::ManifestWrite => "manifest-write",
            Self::RestoreCopy => "restore-copy",
            Self::PurgeDelete => "purge-delete",
            Self::PathWrite => "path-write",
            Self::AncillaryCopy => "ancillary-copy",
            Self::ArtworkWrite => "artwork-write",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "import-copy" => Ok(Self::ImportCopy),
            "import-move" => Ok(Self::ImportMove),
            "import-link" => Ok(Self::ImportLink),
            "remove-delete" => Ok(Self::RemoveDelete),
            "tag-write" => Ok(Self::TagWrite),
            "manifest-write" => Ok(Self::ManifestWrite),
            "restore-copy" => Ok(Self::RestoreCopy),
            "purge-delete" => Ok(Self::PurgeDelete),
            "path-write" => Ok(Self::PathWrite),
            "ancillary-copy" => Ok(Self::AncillaryCopy),
            "artwork-write" => Ok(Self::ArtworkWrite),
            _ => Err(Error::Recovery(format!(
                "unknown journal operation kind: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct JournalFile {
    pub source: PathBuf,
    pub staged: PathBuf,
    pub destination: PathBuf,
    pub content_hash: Option<String>,
    pub sha256: Option<String>,
    pub source_identity: Option<String>,
    pub owned_identity: Option<String>,
    pub role: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub(crate) struct StoredAssetIdentity {
    pub asset_id: String,
    pub path: PathBuf,
    pub role: String,
    pub byte_size: u64,
    pub blake3: String,
    pub sha256: String,
    pub entry_identity: String,
}

#[derive(Debug)]
struct PendingOperation {
    id: String,
    kind: OperationKind,
    state: String,
    plan_id: Option<String>,
    files: Vec<JournalFile>,
}

#[derive(Debug, Clone)]
pub(crate) struct RetainedJournalFile {
    pub operation_id: String,
    pub ordinal: usize,
    pub file: JournalFile,
}

#[derive(Debug)]
struct PreparedAsset {
    id: String,
    path: PathBuf,
    role: &'static str,
    verification_state: &'static str,
    byte_size: Option<u64>,
    blake3: Option<String>,
    sha256: Option<String>,
    mtime: Option<DateTime<Utc>>,
    entry_identity: Option<String>,
    media_json: Option<String>,
    audio_essence_hash: Option<String>,
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lease = CollectionLease::acquire_exclusive(path)?;
        let mut conn = Connection::open(path)?;
        configure_connection(&conn)?;
        let migration_report = migrations::run_migrations(&mut conn, Some(path))?;
        let needs_size_backfill = migration_report.from_version < 2;
        let mut library = Self {
            conn,
            path: Some(path.to_path_buf()),
            migration_report,
            _lease: Some(lease),
        };
        if needs_size_backfill {
            library.backfill_file_sizes()?;
        }
        Ok(library)
    }

    /// Open an in-memory snapshot without changing the source database.
    ///
    /// A missing source is represented by an empty, migrated in-memory database. Existing legacy
    /// schemas are migrated only inside the snapshot, which makes this suitable for dry runs.
    /// A current schema is opened read-only without copying the database.
    pub fn open_snapshot(path: &Path) -> Result<Self> {
        if path.exists() || path.is_symlink() {
            let source =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            configure_connection(&source)?;
            if migrations::current_version(&source)? == migrations::LATEST_VERSION {
                let migration_report = migrations::validate_current_schema(&source)?;
                return Ok(Self {
                    conn: source,
                    path: Some(path.to_path_buf()),
                    migration_report,
                    _lease: None,
                });
            }
            let mut conn = Connection::open_in_memory()?;
            {
                let backup = rusqlite::backup::Backup::new(&source, &mut conn)?;
                backup.run_to_completion(100, Duration::from_millis(5), None)?;
            }
            configure_connection(&conn)?;
            let migration_report = migrations::run_migrations(&mut conn, None)?;
            let needs_size_backfill = migration_report.from_version < 2;
            let mut library = Self {
                conn,
                path: None,
                migration_report,
                _lease: None,
            };
            if needs_size_backfill {
                library.backfill_file_sizes()?;
            }
            return Ok(library);
        }

        let mut conn = Connection::open_in_memory()?;
        configure_connection(&conn)?;
        let migration_report = migrations::run_migrations(&mut conn, None)?;
        Ok(Self {
            conn,
            path: None,
            migration_report,
            _lease: None,
        })
    }

    /// Open a current-schema catalog for ordinary reads without taking the writer lease.
    /// This never migrates, recovers, or copies the database.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        configure_connection(&conn)?;
        let migration_report = migrations::validate_current_schema(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            migration_report,
            _lease: None,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        configure_connection(&conn)?;
        let migration_report = migrations::run_migrations(&mut conn, None)?;
        Ok(Self {
            conn,
            path: None,
            migration_report,
            _lease: None,
        })
    }

    #[must_use]
    pub const fn migration_report(&self) -> &MigrationReport {
        &self.migration_report
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn audit(&self) -> Result<AuditReport> {
        self.audit_with_mode(AuditMode::Quick)
    }

    pub fn audit_with_mode(&self, mode: AuditMode) -> Result<AuditReport> {
        if mode == AuditMode::Deep {
            return Err(Error::Operation(
                "deep audit must use the durable, paged fixity workflow".into(),
            ));
        }
        let mut issues = AuditIssues::default();
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, file_size, added, mtime FROM items ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
                row.get::<_, Option<u64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (item_id, path, file_size, added, mtime) = row?;
            if !path.exists() {
                issues.push(AuditIssue::MissingFile { item_id, path });
            } else if file_size.is_none() {
                issues.push(AuditIssue::UnknownFileSize { item_id, path });
            }
            for (field, value) in [("added", added), ("mtime", mtime)] {
                if !valid_datetime(&value) {
                    issues.push(AuditIssue::InvalidTimestamp {
                        table: "items",
                        row_id: item_id,
                        field,
                        value,
                    });
                }
            }
        }

        let mut statement = self.conn.prepare(
            "SELECT i.id, i.path FROM items i
             WHERE NOT EXISTS(
                 SELECT 1 FROM item_assets ia
                 WHERE ia.item_id = i.id AND ia.relationship = 'audio'
             ) ORDER BY i.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
            ))
        })?;
        for row in rows {
            let (item_id, path) = row?;
            issues.push(AuditIssue::MissingAssetRecord { item_id, path });
        }

        let mut stmt = self
            .conn
            .prepare("SELECT id, added FROM albums ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (row_id, value) = row?;
            if !valid_datetime(&value) {
                issues.push(AuditIssue::InvalidTimestamp {
                    table: "albums",
                    row_id,
                    field: "added",
                    value,
                });
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT i.id, i.album_id
             FROM items i LEFT JOIN albums a ON a.id = i.album_id
             WHERE i.album_id IS NOT NULL AND a.id IS NULL",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        for row in rows {
            let (item_id, album_id) = row?;
            issues.push(AuditIssue::OrphanedItem { item_id, album_id });
        }

        // A normal SELECT from an external-content FTS5 table reads rows from `items`, so a
        // join cannot reveal drift in the actual index. Rank 1 asks FTS5 to compare its index
        // against that content table without changing either one.
        if let Err(error) = self.conn.execute(
            "INSERT INTO items_fts(items_fts, rank) VALUES('integrity-check', 1)",
            [],
        ) {
            issues.push(AuditIssue::SearchIndexInconsistent {
                detail: error.to_string(),
            });
        }
        self.audit_assets(&mut issues)?;
        Ok(AuditReport {
            issues: issues.values,
            omitted: issues.omitted,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn audit_assets(&self, issues: &mut AuditIssues) -> Result<()> {
        let mut statement = self.conn.prepare(
            "SELECT a.id, a.absolute_path, a.role, a.verification_state,
                    a.byte_size, a.mtime, a.entry_identity,
                    a.projection_state, a.managed, a.media_json,
                    EXISTS(SELECT 1 FROM item_assets ia WHERE ia.asset_id = a.id)
                    OR EXISTS(SELECT 1 FROM album_assets aa WHERE aa.asset_id = a.id)
                    OR EXISTS(SELECT 1 FROM asset_relationships ar
                              WHERE ar.parent_asset_id = a.id OR ar.child_asset_id = a.id)
             FROM assets a ORDER BY a.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<u64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, bool>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, bool>(10)?,
            ))
        })?;
        for row in rows {
            let (
                asset_id,
                path,
                role,
                verification_state,
                expected_size,
                expected_mtime,
                expected_entry_identity,
                projection_state,
                managed,
                expected_media,
                has_owner,
            ) = row?;
            if !managed {
                continue;
            }
            if !has_owner {
                issues.push(AuditIssue::OrphanedManagedAsset {
                    asset_id: asset_id.clone(),
                    path: path.clone(),
                });
            }
            if projection_state != "current" {
                issues.push(AuditIssue::ProjectionDiverged {
                    asset_id: asset_id.clone(),
                    path: path.clone(),
                    state: projection_state,
                });
            }
            if !path.exists() {
                issues.push(AuditIssue::MissingManagedAsset {
                    asset_id,
                    path,
                    role,
                });
                continue;
            }
            if verification_state != "verified" {
                issues.push(AuditIssue::UnverifiedAsset {
                    asset_id: asset_id.clone(),
                    path: path.clone(),
                });
            }
            let entry_metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    issues.push(AuditIssue::AssetUnreadable {
                        asset_id,
                        path,
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            let content_metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    issues.push(AuditIssue::AssetUnreadable {
                        asset_id,
                        path,
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            if expected_entry_identity
                .as_deref()
                .is_some_and(|expected| expected != file_identity(&entry_metadata))
            {
                issues.push(AuditIssue::AssetEntryIdentityMismatch {
                    asset_id: asset_id.clone(),
                    path: path.clone(),
                });
            }
            if let Some(expected) =
                expected_size.filter(|expected| *expected != content_metadata.len())
            {
                issues.push(AuditIssue::AssetSizeMismatch {
                    asset_id: asset_id.clone(),
                    path: path.clone(),
                    expected,
                    actual: content_metadata.len(),
                });
            }
            if let Some(expected) = expected_mtime {
                let actual = DateTime::<Utc>::from(entry_metadata.modified()?).to_rfc3339();
                if actual != expected {
                    issues.push(AuditIssue::AssetMtimeMismatch {
                        asset_id: asset_id.clone(),
                        path: path.clone(),
                        expected,
                        actual,
                    });
                }
            }
            if let Some(expected) = expected_media {
                match probe_media(&path)
                    .and_then(|media| serde_json::to_string(&media).map_err(Into::into))
                {
                    Ok(actual) if actual != expected => {
                        issues.push(AuditIssue::MediaPropertiesMismatch {
                            asset_id: asset_id.clone(),
                            path: path.clone(),
                        });
                    }
                    Err(error) => issues.push(AuditIssue::AssetUnreadable {
                        asset_id: asset_id.clone(),
                        path: path.clone(),
                        detail: error.to_string(),
                    }),
                    Ok(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Run `SQLite`'s full integrity check and foreign-key validation explicitly.
    /// Ordinary opens deliberately do not pay this cost.
    pub fn integrity_check(&self) -> Result<IntegrityReport> {
        const LIMIT: usize = 100;
        let mut messages = Vec::new();
        let mut truncated = false;
        let mut statement = self.conn.prepare("PRAGMA integrity_check(100)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            let message = row?;
            if message != "ok" {
                if messages.len() < LIMIT {
                    messages.push(message);
                } else {
                    truncated = true;
                }
            }
        }

        let mut statement = self.conn.prepare("PRAGMA foreign_key_check")?;
        let rows = statement.query_map([], |row| {
            Ok(format!(
                "foreign key violation: table={}, rowid={}, parent={}, constraint={}",
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?
                    .map_or_else(|| "unknown".into(), |value| value.to_string()),
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?
            ))
        })?;
        for row in rows {
            if messages.len() < LIMIT {
                messages.push(row?);
            } else {
                let _discarded = row?;
                truncated = true;
            }
        }
        Ok(IntegrityReport {
            messages,
            truncated,
        })
    }

    /// Reconcile journaled filesystem operations. This method is explicit for library callers.
    pub fn recover_pending(&mut self) -> Result<RecoveryReport> {
        self.recover_interrupted_provider_jobs()?;
        let operations = self.pending_operations()?;
        let mut report = RecoveryReport::default();
        for operation in operations {
            match recover_operation(&operation) {
                Ok(()) => {
                    if operation.kind == OperationKind::PurgeDelete {
                        if matches!(operation.state.as_str(), "db-committed" | "cleanup-pending") {
                            self.finalize_purge_history(&operation)?;
                        } else if let Some(plan_id) = &operation.plan_id {
                            let plan_id = PlanId::parse(plan_id.clone())?;
                            let transaction = self.conn.unchecked_transaction()?;
                            let changed = transaction.execute(
                                "UPDATE durable_plans
                                 SET state = 'failed', error = 'purge rolled back during recovery',
                                     updated_at = ?2, completed_at = ?2
                                 WHERE id = ?1 AND state IN ('approved', 'running', 'paused')",
                                params![plan_id.as_str(), Utc::now().to_rfc3339()],
                            )?;
                            if changed == 1 {
                                append_plan_event(
                                    &transaction,
                                    &plan_id,
                                    "failed",
                                    &serde_json::json!({
                                        "error": "purge rolled back during recovery",
                                        "recovered_operation": operation.id,
                                    }),
                                )?;
                            }
                            transaction.commit()?;
                        }
                    }
                    self.complete_operation(&operation.id)?;
                    report.recovered_operations.push(operation.id);
                }
                Err(error) => {
                    let message = format!("{}: {error}", operation.id);
                    self.set_operation_state(&operation.id, &operation.state, Some(&message))?;
                    report.unresolved.push(message);
                }
            }
        }
        Ok(report)
    }

    pub fn query_items(&self, query: &Query) -> Result<Vec<Item>> {
        let compiled = query.compile();
        let mut stmt = self.conn.prepare(&compiled.sql)?;
        let items = stmt
            .query_map(params_from_iter(compiled.parameters.iter()), row_to_item)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Collect a deliberately bounded selection for an atomic mutating command.
    ///
    /// The extra row distinguishes an exact-limit selection from a truncated
    /// one without materializing the remainder of the catalog.
    pub fn query_items_bounded(&self, query: &Query, maximum: u32) -> Result<Vec<Item>> {
        if maximum == 0 || maximum >= 10_000 {
            return Err(Error::Query(
                "bounded selection maximum must be between 1 and 9999".into(),
            ));
        }
        let items = self.query_items_page(query, None, maximum + 1)?;
        if items.len() > maximum as usize {
            return Err(Error::Query(format!(
                "selection exceeds the atomic command limit of {maximum} items; narrow the query"
            )));
        }
        Ok(items)
    }

    /// Read a stable keyset page ordered by item ID.
    pub fn query_items_page(
        &self,
        query: &Query,
        after_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Item>> {
        if limit == 0 || limit > 10_000 {
            return Err(Error::Query(
                "page limit must be between 1 and 10000".into(),
            ));
        }
        let mut compiled = query.compile();
        let (selection, _ordering) = compiled
            .sql
            .rsplit_once(" ORDER BY ")
            .ok_or_else(|| Error::Query("compiled query has no stable ordering".into()))?;
        let conjunction = if selection.contains(" WHERE ") {
            " AND"
        } else {
            " WHERE"
        };
        compiled.sql = format!("{selection}{conjunction} id > ? ORDER BY id ASC LIMIT ?");
        compiled
            .parameters
            .push(Value::Integer(after_id.unwrap_or(0)));
        compiled.parameters.push(Value::Integer(i64::from(limit)));
        let mut statement = self.conn.prepare(&compiled.sql)?;
        let items = statement
            .query_map(params_from_iter(compiled.parameters.iter()), row_to_item)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)?;
        Ok(items)
    }

    pub fn query_albums(&self, search: Option<&str>) -> Result<Vec<Album>> {
        match search {
            None => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT * FROM albums ORDER BY albumartist, year, album")?;
                let albums = stmt
                    .query_map([], row_to_album)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(Into::into);
                albums
            }
            Some(search) => {
                let pattern = format!("%{}%", escape_like(search));
                let mut stmt = self.conn.prepare(
                    "SELECT * FROM albums
                     WHERE album LIKE ?1 ESCAPE '!' OR albumartist LIKE ?1 ESCAPE '!'
                     ORDER BY albumartist, year, album",
                )?;
                let albums = stmt
                    .query_map([pattern], row_to_album)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(Into::into);
                albums
            }
        }
    }

    /// Read a stable keyset page of albums ordered by catalog identity.
    pub fn query_albums_page(
        &self,
        search: Option<&str>,
        after_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Album>> {
        if limit == 0 || limit > 10_000 {
            return Err(Error::Query(
                "page limit must be between 1 and 10000".into(),
            ));
        }
        let after_id = after_id.unwrap_or(0);
        let albums = if let Some(search) = search {
            let pattern = format!("%{}%", escape_like(search));
            let mut statement = self.conn.prepare(
                "SELECT * FROM albums
                 WHERE id > ?1
                   AND (album LIKE ?2 ESCAPE '!' OR albumartist LIKE ?2 ESCAPE '!')
                 ORDER BY id ASC LIMIT ?3",
            )?;
            let page = statement
                .query_map(params![after_id, pattern, limit], row_to_album)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            page
        } else {
            let mut statement = self
                .conn
                .prepare("SELECT * FROM albums WHERE id > ?1 ORDER BY id ASC LIMIT ?2")?;
            let page = statement
                .query_map(params![after_id, limit], row_to_album)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            page
        };
        Ok(albums)
    }

    /// Establish or refresh persistent ownership evidence for selected files.
    pub fn verify_items(
        &mut self,
        query: &Query,
        configured_root: &Path,
    ) -> Result<VerificationReport> {
        let items = self.query_items_bounded(query, 9_999)?;
        let mut prepared = Vec::new();
        let mut report = VerificationReport::default();
        for item in items {
            let Some(item_id) = item.id else {
                continue;
            };
            match prepare_asset(&item.path, "audio") {
                Ok(asset) if asset.verification_state == "verified" => {
                    let root = item
                        .path
                        .starts_with(configured_root)
                        .then(|| configured_root.to_path_buf());
                    prepared.push((item_id, asset, root));
                }
                Ok(_) => report.skipped.push((
                    item_id,
                    item.path,
                    "file is missing and cannot be verified".into(),
                )),
                Err(error) => report.skipped.push((item_id, item.path, error.to_string())),
            }
        }

        let transaction = self.conn.transaction()?;
        for (item_id, asset, root) in prepared {
            let root_id = find_or_insert_root(&transaction, root.as_deref())?;
            let existing = transaction
                .query_row(
                    "SELECT asset_id FROM item_assets
                     WHERE item_id = ?1 AND relationship = 'audio'",
                    [item_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(asset_id) = existing {
                update_asset(&transaction, &asset_id, &root_id, root.as_deref(), &asset)?;
            } else {
                let asset_id = insert_asset(&transaction, &root_id, root.as_deref(), &asset)?;
                transaction.execute(
                    "INSERT INTO item_assets (item_id, asset_id, relationship)
                     VALUES (?1, ?2, 'audio')",
                    params![item_id, asset_id],
                )?;
            }
            transaction.execute(
                "UPDATE items SET file_size = ?1, mtime = ?2 WHERE id = ?3",
                params![
                    asset.byte_size,
                    asset.mtime.map(|mtime| mtime.to_rfc3339()),
                    item_id,
                ],
            )?;
            report.verified += 1;
        }
        transaction.commit()?;
        Ok(report)
    }

    pub fn stats(&self) -> Result<Stats> {
        self.conn
            .query_row(
                "SELECT tracks, albums, artists, total_length, total_size, unknown_sizes
                 FROM library_statistics WHERE singleton = 1",
                [],
                |row| {
                    Ok(Stats {
                        tracks: row.get(0)?,
                        albums: row.get(1)?,
                        artists: row.get(2)?,
                        total_length: row.get(3)?,
                        total_size: row.get(4)?,
                        unknown_sizes: row.get(5)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn update_item(&self, id: i64, item: &Item) -> Result<()> {
        self.update_items(&[(id, item.clone())]).map(|_| ())
    }

    /// Update a set of tag snapshots in one transaction.
    pub fn update_items(&self, items: &[(i64, Item)]) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }
        for (_id, item) in items {
            validate_item_metadata(item)?;
        }
        let ids = items.iter().map(|(id, _item)| *id).collect::<Vec<_>>();
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(Error::Query("item IDs must not be repeated".into()));
        }
        let transaction = self.conn.unchecked_transaction()?;
        for (id, item) in items {
            if transaction.execute(
                "UPDATE items SET title=?1, artist=?2, album=?3, albumartist=?4, genre=?5,
                 year=?6, track=?7, disc=?8, format=?9, bitrate=?10, length=?11,
                 file_size=?12, mtime=?13,
                 mb_trackid=CASE
                     WHEN title IS ?1 AND artist IS ?2 AND track IS ?7 AND disc IS ?8
                     THEN mb_trackid ELSE NULL END,
                 external_track_id=CASE
                     WHEN title IS ?1 AND artist IS ?2 AND track IS ?7 AND disc IS ?8
                     THEN external_track_id ELSE NULL END,
                 mb_albumid=CASE
                     WHEN album IS ?3
                          AND COALESCE(albumartist, artist) IS COALESCE(?4, ?2)
                          AND year IS ?6
                     THEN mb_albumid ELSE NULL END,
                 external_release_id=CASE
                     WHEN album IS ?3
                          AND COALESCE(albumartist, artist) IS COALESCE(?4, ?2)
                          AND year IS ?6
                     THEN external_release_id ELSE NULL END
                 WHERE id=?14",
                params![
                    item.title,
                    item.artist,
                    item.album,
                    item.albumartist,
                    item.genre,
                    item.year,
                    item.track,
                    item.disc,
                    item.format.as_str(),
                    item.bitrate,
                    item.length,
                    item.file_size,
                    item.mtime.to_rfc3339(),
                    id,
                ],
            )? != 1
            {
                return Err(Error::Query(format!(
                    "item {id} no longer exists; no items were updated"
                )));
            }
            transaction.execute(
                "UPDATE items SET metadata_provider = NULL
                 WHERE id = ?1 AND external_track_id IS NULL AND external_release_id IS NULL",
                [id],
            )?;
        }
        reconcile_album_membership(&transaction, &ids)?;
        transaction.commit()?;
        Ok(items.len())
    }

    pub fn modify_item(&self, id: i64, fields: &[String]) -> Result<()> {
        self.modify_items(&[id], fields).map(|_| ())
    }

    /// Modify a complete set of items in one transaction.
    ///
    /// Every field and value is validated before any row is changed. Empty values clear the
    /// optional `albumartist`, `genre`, `year`, `track`, and `disc` fields.
    pub fn modify_items(&self, ids: &[i64], fields: &[String]) -> Result<usize> {
        let modifications = parse_modifications(fields)?;
        if ids.is_empty() {
            return Ok(0);
        }
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(Error::Query("item IDs must not be repeated".into()));
        }

        let assignments = modifications
            .iter()
            .map(|modification| format!("{} = ?", modification.column))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE items SET {assignments} WHERE id = ?");
        let invalidates_track_identity = modifications.iter().any(|modification| {
            matches!(modification.column, "title" | "artist" | "track" | "disc")
        });
        let invalidates_release_identity = modifications
            .iter()
            .any(|modification| matches!(modification.column, "album" | "albumartist" | "year"));
        let modifies_artist = modifications
            .iter()
            .any(|modification| modification.column == "artist");
        let transaction = self.conn.unchecked_transaction()?;
        for id in ids {
            let mut values = modifications
                .iter()
                .map(|modification| modification.value.clone())
                .collect::<Vec<_>>();
            values.push(Value::Integer(*id));
            if transaction.execute(&sql, params_from_iter(values.iter()))? != 1 {
                return Err(Error::Query(format!(
                    "item {id} no longer exists; no items were modified"
                )));
            }
            if invalidates_track_identity {
                transaction.execute(
                    "UPDATE items SET mb_trackid = NULL, external_track_id = NULL WHERE id = ?1",
                    [id],
                )?;
            }
            if invalidates_release_identity {
                transaction.execute(
                    "UPDATE items SET mb_albumid = NULL, external_release_id = NULL WHERE id = ?1",
                    [id],
                )?;
            } else if modifies_artist {
                transaction.execute(
                    "UPDATE items SET mb_albumid = NULL, external_release_id = NULL
                     WHERE id = ?1 AND albumartist IS NULL",
                    [id],
                )?;
            }
            if invalidates_track_identity || invalidates_release_identity || modifies_artist {
                transaction.execute(
                    "UPDATE items SET metadata_provider = NULL
                     WHERE id = ?1 AND external_track_id IS NULL AND external_release_id IS NULL",
                    [id],
                )?;
            }
        }
        if modifications.iter().any(|modification| {
            matches!(
                modification.column,
                "artist" | "album" | "albumartist" | "year"
            )
        }) {
            reconcile_album_membership(&transaction, ids)?;
        }
        transaction.commit()?;
        Ok(ids.len())
    }

    pub(crate) fn item_exists(&self, path: &Path) -> Result<bool> {
        let path = path_to_storage(path)?;
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE path = ?1)",
                [path],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn verified_asset_for_item(
        &self,
        item_id: i64,
        expected_path: &Path,
    ) -> Result<StoredAssetIdentity> {
        let asset = self
            .conn
            .query_row(
                "SELECT a.id, a.absolute_path, a.role, a.byte_size, a.blake3, a.sha256,
                        a.entry_identity, a.verification_state, a.managed
                 FROM assets a
                 JOIN item_assets ia ON ia.asset_id = a.id
                 WHERE ia.item_id = ?1 AND ia.relationship = 'audio'",
                [item_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        PathBuf::from(row.get::<_, String>(1)?),
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<u64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, bool>(8)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                Error::Import(format!(
                    "item {item_id} has no persistent audio asset; verify it before deletion"
                ))
            })?;
        let (asset_id, path, role, byte_size, blake3, sha256, entry_identity, state, managed) =
            asset;
        if path != expected_path {
            return Err(Error::Import(format!(
                "item {item_id} path does not match its persistent asset; preserving both"
            )));
        }
        if !managed || state != "verified" {
            return Err(Error::Import(format!(
                "asset {asset_id} is {state} and cannot be deleted until verified"
            )));
        }
        Ok(StoredAssetIdentity {
            asset_id,
            path,
            role,
            byte_size: byte_size
                .ok_or_else(|| Error::Import("verified asset has no stored byte size".into()))?,
            blake3: blake3
                .ok_or_else(|| Error::Import("verified asset has no BLAKE3 digest".into()))?,
            sha256: sha256
                .ok_or_else(|| Error::Import("verified asset has no SHA-256 digest".into()))?,
            entry_identity: entry_identity.ok_or_else(|| {
                Error::Import("verified asset has no stored filesystem identity".into())
            })?,
        })
    }

    pub(crate) fn album_item_count(&self, album_id: i64) -> Result<usize> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE album_id = ?1",
                [album_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn verified_assets_for_album(
        &self,
        album_id: i64,
    ) -> Result<Vec<StoredAssetIdentity>> {
        let mut statement = self.conn.prepare(
            "SELECT a.id, a.absolute_path, a.role, a.byte_size, a.blake3,
                    a.sha256, a.entry_identity, a.verification_state, a.managed
             FROM assets a
             JOIN album_assets aa ON aa.asset_id = a.id
             WHERE aa.album_id = ?1 ORDER BY a.id",
        )?;
        let rows = statement.query_map([album_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
                row.get::<_, String>(2)?,
                row.get::<_, Option<u64>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, bool>(8)?,
            ))
        })?;
        let mut assets = Vec::new();
        for row in rows {
            let (asset_id, path, role, byte_size, blake3, sha256, entry_identity, state, managed) =
                row?;
            if !managed || state != "verified" {
                return Err(Error::Import(format!(
                    "album asset {asset_id} is {state} and cannot be removed until verified"
                )));
            }
            assets.push(StoredAssetIdentity {
                asset_id,
                path,
                role,
                byte_size: byte_size.ok_or_else(|| {
                    Error::Import("verified album asset has no stored byte size".into())
                })?,
                blake3: blake3.ok_or_else(|| {
                    Error::Import("verified album asset has no BLAKE3 digest".into())
                })?,
                sha256: sha256.ok_or_else(|| {
                    Error::Import("verified album asset has no SHA-256 digest".into())
                })?,
                entry_identity: entry_identity.ok_or_else(|| {
                    Error::Import("verified album asset has no filesystem identity".into())
                })?,
            });
        }
        Ok(assets)
    }

    pub(crate) fn retained_removals_before(
        &self,
        completed_before: DateTime<Utc>,
    ) -> Result<Vec<RetainedJournalFile>> {
        let mut statement = self.conn.prepare(
            "SELECT oj.id, of.ordinal, of.source_path, of.staged_path,
                    of.destination_path, of.content_hash, of.sha256, of.source_identity,
                    of.owned_identity, of.role, of.state
             FROM operation_journal oj
             JOIN operation_files of ON of.operation_id = oj.id
             WHERE oj.state = 'complete' AND of.state = 'quarantined'
               AND (
                   oj.kind = 'remove-delete'
                   OR (oj.kind = 'tag-write' AND of.role = 'tag-original')
               )
               AND NOT EXISTS (
                   SELECT 1 FROM operation_journal purge
                   JOIN operation_files purge_file ON purge_file.operation_id = purge.id
                   WHERE purge.kind = 'purge-delete'
                     AND purge_file.source_path = of.staged_path
               )
               AND oj.completed_at <= ?1
             ORDER BY oj.completed_at, oj.id, of.ordinal",
        )?;
        let files = statement
            .query_map([completed_before.to_rfc3339()], |row| {
                Ok(RetainedJournalFile {
                    operation_id: row.get(0)?,
                    ordinal: row.get(1)?,
                    file: JournalFile {
                        source: PathBuf::from(row.get::<_, String>(2)?),
                        staged: PathBuf::from(row.get::<_, String>(3)?),
                        destination: PathBuf::from(row.get::<_, String>(4)?),
                        content_hash: row.get(5)?,
                        sha256: row.get(6)?,
                        source_identity: row.get(7)?,
                        owned_identity: row.get(8)?,
                        role: row.get(9)?,
                        state: row.get(10)?,
                    },
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(files)
    }

    pub(crate) fn create_operation(
        &self,
        kind: OperationKind,
        files: &[JournalFile],
    ) -> Result<String> {
        self.create_operation_for_plan(kind, files, None, None)
    }

    pub(crate) fn create_operation_for_plan(
        &self,
        kind: OperationKind,
        files: &[JournalFile],
        plan_id: Option<&str>,
        decision: Option<&serde_json::Value>,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO operation_journal
             (id, kind, state, plan_id, decision_json, created_at, updated_at)
             VALUES (?1, ?2, 'prepared', ?3, ?4, ?5, ?5)",
            params![
                id,
                kind.as_str(),
                plan_id,
                decision.map(serde_json::to_string).transpose()?,
                now
            ],
        )?;
        for (ordinal, file) in files.iter().enumerate() {
            let source = path_to_storage(&file.source)?;
            let staged = path_to_storage(&file.staged)?;
            let destination = path_to_storage(&file.destination)?;
            transaction.execute(
                "INSERT INTO operation_files
                 (operation_id, ordinal, source_path, staged_path, destination_path,
                  content_hash, sha256, source_identity, owned_identity, role, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'prepared')",
                params![
                    id,
                    ordinal,
                    source,
                    staged,
                    destination,
                    file.content_hash,
                    file.sha256,
                    file.source_identity,
                    file.owned_identity,
                    file.role,
                ],
            )?;
        }
        transaction.commit()?;
        failpoints::hit("db.create-operation-commit")?;
        Ok(id)
    }

    /// Add one bounded unit of work to an existing typed operation.
    pub(crate) fn append_operation_file(&self, id: &str, file: &JournalFile) -> Result<usize> {
        let transaction = self.conn.unchecked_transaction()?;
        let ordinal = transaction.query_row(
            "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM operation_files WHERE operation_id = ?1",
            [id],
            |row| row.get::<_, usize>(0),
        )?;
        transaction.execute(
            "INSERT INTO operation_files
             (operation_id, ordinal, source_path, staged_path, destination_path,
              content_hash, sha256, source_identity, owned_identity, role, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'prepared')",
            params![
                id,
                ordinal,
                path_to_storage(&file.source)?,
                path_to_storage(&file.staged)?,
                path_to_storage(&file.destination)?,
                file.content_hash,
                file.sha256,
                file.source_identity,
                file.owned_identity,
                file.role,
            ],
        )?;
        transaction.commit()?;
        failpoints::hit("db.append-operation-file")?;
        Ok(ordinal)
    }

    pub(crate) fn set_operation_state(
        &self,
        id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operation_journal
             SET state = ?1, updated_at = ?2, error = ?3 WHERE id = ?4",
            params![state, Utc::now().to_rfc3339(), error, id],
        )?;
        require_journal_row(changed, "operation state")?;
        failpoints::hit("db.operation-state")
    }

    pub(crate) fn record_operation_failure(&self, id: &str, error: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operation_journal
             SET state = CASE
                   WHEN state IN ('db-committed', 'cleanup-pending', 'complete') THEN state
                   ELSE 'failed'
                 END,
                 error = ?2, updated_at = ?3
             WHERE id = ?1",
            params![id, error, Utc::now().to_rfc3339()],
        )?;
        require_journal_row(changed, "operation failure")?;
        failpoints::hit("db.operation-failure")
    }

    pub(crate) fn set_file_state(&self, id: &str, ordinal: usize, state: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operation_files SET state = ?1
             WHERE operation_id = ?2 AND ordinal = ?3",
            params![state, id, ordinal],
        )?;
        require_journal_row(changed, "operation file state")?;
        failpoints::hit("db.operation-file-state")
    }

    pub(crate) fn set_staged_file_identity(
        &self,
        id: &str,
        ordinal: usize,
        identity: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operation_files SET state = 'staged', owned_identity = ?1
             WHERE operation_id = ?2 AND ordinal = ?3",
            params![identity, id, ordinal],
        )?;
        require_journal_row(changed, "staged file identity")?;
        failpoints::hit("db.staged-file-identity")
    }

    pub(crate) fn set_acquired_file_identity(
        &self,
        id: &str,
        ordinal: usize,
        identity: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operation_files SET state = 'acquired', owned_identity = ?1
             WHERE operation_id = ?2 AND ordinal = ?3 AND state = 'prepared'",
            params![identity, id, ordinal],
        )?;
        require_journal_row(changed, "acquired file identity")?;
        failpoints::hit("db.acquired-file-identity")
    }

    pub(crate) fn set_staged_file_full_evidence(
        &self,
        id: &str,
        ordinal: usize,
        identity: &str,
        blake3: &str,
        sha256: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operation_files
             SET state = 'staged', owned_identity = ?1, content_hash = ?2, sha256 = ?3
             WHERE operation_id = ?4 AND ordinal = ?5",
            params![identity, blake3, sha256, id, ordinal],
        )?;
        require_journal_row(changed, "staged file full evidence")?;
        failpoints::hit("db.staged-file-full-evidence")
    }

    #[cfg(test)]
    pub(crate) fn commit_import(
        &mut self,
        operation_id: &str,
        album: &Album,
        items: &[Item],
    ) -> Result<i64> {
        self.commit_import_at_root_with_artwork(operation_id, album, items, None, None)
    }

    pub(crate) fn commit_import_at_root_with_artwork(
        &mut self,
        operation_id: &str,
        album: &Album,
        items: &[Item],
        library_root: Option<&Path>,
        original_artwork: Option<&ArtworkAssetMetadata>,
    ) -> Result<i64> {
        let prepared_assets = prepare_import_assets(album, items)?;
        let prepared_original = original_artwork
            .map(|metadata| prepare_asset(metadata.path(), "artwork-original"))
            .transpose()?;
        let transaction = self.conn.transaction()?;
        let root_id = find_or_insert_root(&transaction, library_root)?;
        let album_id = find_or_insert_album(&transaction, album)?;
        let release_id = ensure_normalized_album(&transaction, album_id, album)?;
        for (item, asset) in items.iter().zip(&prepared_assets[..items.len()]) {
            let item_id = insert_item(&transaction, item, album_id)?;
            let (release_track_id, recording_id) =
                ensure_normalized_item(&transaction, item_id, &release_id, item)?;
            let asset_id = insert_asset(&transaction, &root_id, library_root, asset)?;
            transaction.execute(
                "INSERT INTO item_assets (item_id, asset_id, relationship)
                 VALUES (?1, ?2, 'audio')",
                params![item_id, asset_id],
            )?;
            transaction.execute(
                "INSERT INTO recording_assets
                 (recording_id, release_track_id, asset_id, relationship)
                 VALUES (?1, ?2, ?3, 'audio')",
                params![recording_id, release_track_id, asset_id],
            )?;
        }
        let projection_asset_id = if let Some(artwork) = prepared_assets.get(items.len()) {
            let asset_id = insert_asset(&transaction, &root_id, library_root, artwork)?;
            transaction.execute(
                "INSERT INTO album_assets (album_id, asset_id, relationship)
                 VALUES (?1, ?2, 'front')",
                params![album_id, asset_id],
            )?;
            Some(asset_id)
        } else {
            None
        };
        if let (Some(metadata), Some(original)) = (original_artwork, prepared_original.as_ref()) {
            let original_asset_id =
                find_or_reuse_asset(&transaction, &root_id, library_root, original)?;
            let relationship = format!("original-{}", metadata.provenance().role.as_str());
            transaction.execute(
                "INSERT OR IGNORE INTO album_assets (album_id, asset_id, relationship)
                 VALUES (?1, ?2, ?3)",
                params![album_id, original_asset_id, relationship],
            )?;
            store_artwork_metadata(
                &transaction,
                &original_asset_id,
                &original_asset_id,
                &release_id,
                metadata,
            )?;
            if let Some(projection_asset_id) = projection_asset_id.as_deref() {
                store_artwork_metadata(
                    &transaction,
                    projection_asset_id,
                    &original_asset_id,
                    &release_id,
                    metadata,
                )?;
            }
        }
        let changed = transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), operation_id],
        )?;
        require_journal_row(changed, "import commit")?;
        transaction.commit()?;
        failpoints::hit("db.import-commit")?;
        Ok(album_id)
    }

    pub(crate) fn commit_removal(
        &mut self,
        operation_id: Option<&str>,
        items: &[(i64, &Path)],
    ) -> Result<()> {
        let transaction = self.conn.transaction()?;
        for (id, path) in items {
            let stored_path = path_to_storage(path)?;
            transaction.execute(
                "UPDATE assets
                 SET managed = 0,
                     verification_state = CASE WHEN ?1 IS NULL THEN verification_state ELSE 'missing' END,
                     last_verified_at = ?2
                 WHERE id IN (SELECT asset_id FROM item_assets WHERE item_id = ?3)",
                params![operation_id, Utc::now().to_rfc3339(), id],
            )?;
            if transaction.execute(
                "DELETE FROM items WHERE id = ?1 AND path = ?2",
                params![id, stored_path],
            )? != 1
            {
                return Err(Error::Import(format!(
                    "removal plan is stale for {}; no rows were removed",
                    path.display()
                )));
            }
        }
        transaction.execute(
            "UPDATE assets
             SET managed = 0,
                 verification_state = CASE WHEN ?1 IS NULL THEN verification_state ELSE 'missing' END,
                 last_verified_at = ?2
             WHERE id IN (
                 SELECT aa.asset_id FROM album_assets aa
                 WHERE NOT EXISTS (
                     SELECT 1 FROM items i WHERE i.album_id = aa.album_id
                 )
             )",
            params![operation_id, Utc::now().to_rfc3339()],
        )?;
        transaction.execute(
            "DELETE FROM albums WHERE NOT EXISTS(
                SELECT 1 FROM items WHERE items.album_id = albums.id
            )",
            [],
        )?;
        if let Some(operation_id) = operation_id {
            let changed = transaction.execute(
                "UPDATE operation_journal SET state = 'db-committed', updated_at = ?1
                 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), operation_id],
            )?;
            require_journal_row(changed, "removal commit")?;
        }
        transaction.commit()?;
        failpoints::hit("db.removal-commit")?;
        Ok(())
    }

    pub(crate) fn complete_operation(&self, id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let changed = self.conn.execute(
            "UPDATE operation_journal
             SET state = 'complete', updated_at = ?1, completed_at = ?1, error = NULL
             WHERE id = ?2 AND state != 'complete'",
            params![now, id],
        )?;
        require_journal_row(changed, "operation completion")?;
        failpoints::hit("db.operation-complete")
    }

    fn pending_operations(&self) -> Result<Vec<PendingOperation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, state, plan_id FROM operation_journal
             WHERE state != 'complete' ORDER BY created_at, id",
        )?;
        let headers = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut operations = Vec::with_capacity(headers.len());
        for (id, kind, state, plan_id) in headers {
            let mut file_stmt = self.conn.prepare(
                "SELECT source_path, staged_path, destination_path, content_hash,
                        sha256, source_identity, owned_identity, role, state
                 FROM operation_files WHERE operation_id = ?1 ORDER BY ordinal",
            )?;
            let files = file_stmt
                .query_map([&id], |row| {
                    Ok(JournalFile {
                        source: PathBuf::from(row.get::<_, String>(0)?),
                        staged: PathBuf::from(row.get::<_, String>(1)?),
                        destination: PathBuf::from(row.get::<_, String>(2)?),
                        content_hash: row.get(3)?,
                        sha256: row.get(4)?,
                        source_identity: row.get(5)?,
                        owned_identity: row.get(6)?,
                        role: row.get(7)?,
                        state: row.get(8)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            operations.push(PendingOperation {
                id,
                kind: OperationKind::parse(&kind)?,
                state,
                plan_id,
                files,
            });
        }
        Ok(operations)
    }

    fn finalize_purge_history(&self, operation: &PendingOperation) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        for file in &operation.files {
            transaction.execute(
                "UPDATE operation_files SET state = 'purged'
                 WHERE staged_path = ?1 AND operation_id != ?2 AND state = 'quarantined'",
                params![path_to_storage(&file.source)?, operation.id],
            )?;
        }
        if let Some(plan_id) = &operation.plan_id {
            let plan_id = PlanId::parse(plan_id.clone())?;
            let changed = transaction.execute(
                "UPDATE durable_plans
                 SET state = 'complete', progress_current = progress_total,
                     updated_at = ?2, completed_at = ?2
                 WHERE id = ?1 AND state IN ('approved', 'running', 'paused')",
                params![plan_id.as_str(), Utc::now().to_rfc3339()],
            )?;
            if changed == 1 {
                append_plan_event(
                    &transaction,
                    &plan_id,
                    "complete",
                    &serde_json::json!({"recovered_operation": operation.id}),
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn backfill_file_sizes(&mut self) -> Result<()> {
        let rows = {
            let mut stmt = self
                .conn
                .prepare("SELECT id, path FROM items WHERE file_size IS NULL")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        PathBuf::from(row.get::<_, String>(1)?),
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let transaction = self.conn.transaction()?;
        for (id, path) in rows {
            if let Ok(metadata) = std::fs::metadata(path) {
                transaction.execute(
                    "UPDATE items SET file_size = ?1 WHERE id = ?2",
                    params![metadata.len(), id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

#[derive(Debug)]
struct Modification {
    column: &'static str,
    value: Value,
}

fn parse_modifications(fields: &[String]) -> Result<Vec<Modification>> {
    if fields.is_empty() {
        return Err(Error::Query("at least one field=value is required".into()));
    }

    let mut modifications = Vec::with_capacity(fields.len());
    for field in fields {
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| Error::Query(format!("expected field=value: {field}")))?;
        let (column, value) = match key {
            "title" | "artist" | "album" => {
                if value.trim().is_empty() {
                    return Err(Error::Query(format!("{key} cannot be empty")));
                }
                let column = match key {
                    "title" => "title",
                    "artist" => "artist",
                    _ => "album",
                };
                (column, Value::Text(value.to_string()))
            }
            "albumartist" | "genre" => {
                let column = if key == "albumartist" {
                    "albumartist"
                } else {
                    "genre"
                };
                (
                    column,
                    if value.is_empty() {
                        Value::Null
                    } else {
                        Value::Text(value.to_string())
                    },
                )
            }
            "year" => ("year", parse_optional_number::<i32>(key, value)?),
            "track" => ("track", parse_optional_number::<u32>(key, value)?),
            "disc" => ("disc", parse_optional_number::<u32>(key, value)?),
            _ => return Err(Error::Query(format!("field cannot be modified: {key}"))),
        };
        if modifications
            .iter()
            .any(|modification: &Modification| modification.column == column)
        {
            return Err(Error::Query(format!(
                "field is specified more than once: {key}"
            )));
        }
        modifications.push(Modification { column, value });
    }
    Ok(modifications)
}

fn parse_optional_number<T>(field: &str, value: &str) -> Result<Value>
where
    T: std::str::FromStr + Into<i64>,
{
    if value.is_empty() {
        return Ok(Value::Null);
    }
    value
        .parse::<T>()
        .map(|number| Value::Integer(number.into()))
        .map_err(|_error| Error::Query(format!("{field} must be a whole number or empty")))
}

fn reconcile_album_membership(transaction: &Transaction<'_>, ids: &[i64]) -> Result<()> {
    for id in ids {
        let (current_album_id, album_name, albumartist, year, added): (
            Option<i64>,
            String,
            String,
            Option<i32>,
            String,
        ) = transaction.query_row(
            "SELECT album_id, album, COALESCE(albumartist, artist), year, added
             FROM items WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let current_matches = if let Some(album_id) = current_album_id {
            transaction.query_row(
                "SELECT EXISTS(
                        SELECT 1 FROM albums
                        WHERE id = ?1 AND album = ?2 AND albumartist = ?3 AND year IS ?4
                    )",
                params![album_id, album_name, albumartist, year],
                |row| row.get::<_, bool>(0),
            )?
        } else {
            false
        };
        if current_matches {
            continue;
        }

        let album = Album {
            id: None,
            album: album_name,
            albumartist,
            year,
            artpath: None,
            external_id: None,
            added: parse_datetime(&added)?,
        };
        let album_id = find_or_insert_album(transaction, &album)?;
        transaction.execute(
            "UPDATE items SET album_id = ?1 WHERE id = ?2",
            params![album_id, id],
        )?;
    }
    transaction.execute(
        "DELETE FROM albums WHERE NOT EXISTS(
            SELECT 1 FROM items WHERE items.album_id = albums.id
        )",
        [],
    )?;
    Ok(())
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(())
}

fn find_or_insert_album(transaction: &Transaction<'_>, album: &Album) -> Result<i64> {
    let artpath = album.artpath.as_deref().map(path_to_storage).transpose()?;
    let existing = if let Some(external_id) = &album.external_id {
        transaction
            .query_row(
                "SELECT id FROM albums
                 WHERE metadata_provider = ?1 AND external_release_id = ?2 LIMIT 1",
                params![external_id.provider, external_id.value],
                |row| row.get(0),
            )
            .optional()?
    } else {
        transaction
            .query_row(
                "SELECT id FROM albums
                 WHERE album = ?1 AND albumartist = ?2 AND year IS ?3 LIMIT 1",
                params![album.album, album.albumartist, album.year],
                |row| row.get(0),
            )
            .optional()?
    };
    if let Some(id) = existing {
        transaction.execute(
            "UPDATE albums SET album = ?1, albumartist = ?2, year = ?3,
             artpath = COALESCE(?4, artpath) WHERE id = ?5",
            params![album.album, album.albumartist, album.year, artpath, id,],
        )?;
        return Ok(id);
    }

    let (provider, external_id) = split_external_id(album.external_id.as_ref());
    transaction.execute(
        "INSERT INTO albums
         (album, albumartist, year, artpath, mb_albumid, added,
          metadata_provider, external_release_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            album.album,
            album.albumartist,
            album.year,
            artpath,
            musicbrainz_id(album.external_id.as_ref()),
            album.added.to_rfc3339(),
            provider,
            external_id,
        ],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn insert_item(transaction: &Transaction<'_>, item: &Item, album_id: i64) -> Result<i64> {
    let path = path_to_storage(&item.path)?;
    let provider = item
        .release_external_id
        .as_ref()
        .or(item.track_external_id.as_ref())
        .map(|id| id.provider.as_str());
    transaction.execute(
        "INSERT INTO items
         (album_id, path, title, artist, album, albumartist, genre, year, track, disc,
          format, bitrate, length, mb_trackid, mb_albumid, added, mtime, file_size,
          metadata_provider, external_track_id, external_release_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            album_id,
            path,
            item.title,
            item.artist,
            item.album,
            item.albumartist,
            item.genre,
            item.year,
            item.track,
            item.disc,
            item.format.as_str(),
            item.bitrate,
            item.length,
            musicbrainz_id(item.track_external_id.as_ref()),
            musicbrainz_id(item.release_external_id.as_ref()),
            item.added.to_rfc3339(),
            item.mtime.to_rfc3339(),
            item.file_size,
            provider,
            item.track_external_id.as_ref().map(|id| id.value.as_str()),
            item.release_external_id
                .as_ref()
                .map(|id| id.value.as_str()),
        ],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn ensure_normalized_album(
    transaction: &Transaction<'_>,
    album_id: i64,
    album: &Album,
) -> Result<String> {
    if let Some(existing) = transaction.query_row(
        "SELECT canonical_release_id FROM albums WHERE id = ?1",
        [album_id],
        |row| row.get::<_, Option<String>>(0),
    )? {
        return Ok(existing);
    }

    let release_id = if let Some(external) = &album.external_id {
        transaction
            .query_row(
                "SELECT entity_id FROM external_ids
                 WHERE provider = ?1 AND entity_type = 'release' AND external_id = ?2",
                params![external.provider, external.value],
                |row| row.get::<_, String>(0),
            )
            .optional()?
    } else {
        None
    };
    if let Some(release_id) = release_id {
        transaction.execute(
            "UPDATE albums SET canonical_release_id = ?1 WHERE id = ?2",
            params![release_id, album_id],
        )?;
        return Ok(release_id);
    }

    let now = Utc::now().to_rfc3339();
    let release_group_id = uuid::Uuid::new_v4().to_string();
    let release_id = uuid::Uuid::new_v4().to_string();
    let release_date = album.year.map(|year| format!("{year:04}"));
    transaction.execute(
        "INSERT INTO release_groups (id, title, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?3)",
        params![release_group_id, album.album, now],
    )?;
    transaction.execute(
        "INSERT INTO releases
         (id, release_group_id, title, release_date, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![release_id, release_group_id, album.album, release_date, now],
    )?;
    attach_single_artist_credit(
        transaction,
        "release-group",
        &release_group_id,
        "primary",
        &album.albumartist,
    )?;
    attach_single_artist_credit(
        transaction,
        "release",
        &release_id,
        "primary",
        &album.albumartist,
    )?;
    if let Some(external) = &album.external_id {
        transaction.execute(
            "INSERT INTO external_ids
             (entity_type, entity_id, provider, external_id, data_license)
             VALUES ('release', ?1, ?2, ?3, ?4)",
            params![
                release_id,
                external.provider,
                external.value,
                provider_license(&external.provider)
            ],
        )?;
    }
    insert_local_claim(
        transaction,
        "release-group",
        &release_group_id,
        "title",
        &album.album,
    )?;
    insert_local_claim(transaction, "release", &release_id, "title", &album.album)?;
    if let Some(date) = &release_date {
        insert_local_claim(transaction, "release", &release_id, "release-date", date)?;
    }
    transaction.execute(
        "UPDATE albums SET canonical_release_id = ?1 WHERE id = ?2",
        params![release_id, album_id],
    )?;
    Ok(release_id)
}

#[allow(clippy::too_many_lines)]
fn ensure_normalized_item(
    transaction: &Transaction<'_>,
    item_id: i64,
    release_id: &str,
    item: &Item,
) -> Result<(String, String)> {
    let disc = item.disc.unwrap_or(1).max(1);
    let medium_id = transaction
        .query_row(
            "SELECT id FROM media WHERE release_id = ?1 AND position = ?2",
            params![release_id, disc],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    transaction.execute(
        "INSERT OR IGNORE INTO media (id, release_id, position, track_count)
         VALUES (?1, ?2, ?3, 0)",
        params![medium_id, release_id, disc],
    )?;

    let recording_id = if let Some(external) = &item.track_external_id {
        transaction
            .query_row(
                "SELECT entity_id FROM external_ids
                 WHERE provider = ?1 AND entity_type = 'recording' AND external_id = ?2",
                params![external.provider, external.value],
                |row| row.get::<_, String>(0),
            )
            .optional()?
    } else {
        None
    }
    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let now = Utc::now().to_rfc3339();
    transaction.execute(
        "INSERT OR IGNORE INTO recordings
         (id, title, duration_ms, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)",
        params![
            recording_id,
            item.title,
            (item.length * 1000.0).round() as u64,
            now
        ],
    )?;
    attach_single_artist_credit(
        transaction,
        "recording",
        &recording_id,
        "performance",
        &item.artist,
    )?;
    if let Some(external) = &item.track_external_id {
        transaction.execute(
            "INSERT OR IGNORE INTO external_ids
             (entity_type, entity_id, provider, external_id, data_license)
             VALUES ('recording', ?1, ?2, ?3, ?4)",
            params![
                recording_id,
                external.provider,
                external.value,
                provider_license(&external.provider)
            ],
        )?;
    }

    let next_position: u32 = transaction.query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM release_tracks WHERE medium_id = ?1",
        [&medium_id],
        |row| row.get(0),
    )?;
    let preferred = item.track.unwrap_or(next_position).max(1);
    let occupied: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM release_tracks WHERE medium_id = ?1 AND position = ?2)",
        params![medium_id, preferred],
        |row| row.get(0),
    )?;
    let position = if occupied { next_position } else { preferred };
    let release_track_id = uuid::Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO release_tracks
         (id, medium_id, recording_id, position, printed_position, title, length_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            release_track_id,
            medium_id,
            recording_id,
            position,
            item.track.map(|track| track.to_string()),
            item.title,
            (item.length * 1000.0).round() as u64
        ],
    )?;
    attach_single_artist_credit(
        transaction,
        "release-track",
        &release_track_id,
        "credited",
        &item.artist,
    )?;
    transaction.execute(
        "UPDATE media SET track_count = (
             SELECT COUNT(*) FROM release_tracks WHERE medium_id = ?1
         ) WHERE id = ?1",
        [&medium_id],
    )?;
    transaction.execute(
        "UPDATE items SET release_track_id = ?1, recording_id = ?2 WHERE id = ?3",
        params![release_track_id, recording_id, item_id],
    )?;
    insert_local_claim(
        transaction,
        "recording",
        &recording_id,
        "title",
        &item.title,
    )?;
    insert_local_claim(
        transaction,
        "release-track",
        &release_track_id,
        "title",
        &item.title,
    )?;
    Ok((release_track_id, recording_id))
}

fn attach_single_artist_credit(
    transaction: &Transaction<'_>,
    entity_type: &str,
    entity_id: &str,
    relationship: &str,
    display_name: &str,
) -> Result<()> {
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM entity_artist_credits
         WHERE entity_type = ?1 AND entity_id = ?2 AND relationship = ?3)",
        params![entity_type, entity_id, relationship],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(());
    }
    let artist_id = transaction
        .query_row(
            "SELECT id FROM artists WHERE name = ?1 ORDER BY id LIMIT 1",
            [display_name],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let now = Utc::now().to_rfc3339();
    transaction.execute(
        "INSERT OR IGNORE INTO artists (id, name, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?3)",
        params![artist_id, display_name, now],
    )?;
    let credit_id = uuid::Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO artist_credits (id, display_name) VALUES (?1, ?2)",
        params![credit_id, display_name],
    )?;
    transaction.execute(
        "INSERT INTO artist_credit_names
         (credit_id, position, artist_id, credited_name, join_phrase)
         VALUES (?1, 0, ?2, ?3, '')",
        params![credit_id, artist_id, display_name],
    )?;
    transaction.execute(
        "INSERT INTO entity_artist_credits
         (entity_type, entity_id, credit_id, relationship)
         VALUES (?1, ?2, ?3, ?4)",
        params![entity_type, entity_id, credit_id, relationship],
    )?;
    Ok(())
}

fn insert_local_claim(
    transaction: &Transaction<'_>,
    entity_type: &str,
    entity_id: &str,
    field: &str,
    value: &str,
) -> Result<()> {
    let claim_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let value_json = serde_json::to_string(value)?;
    transaction.execute(
        "INSERT INTO metadata_claims
         (id, entity_type, entity_id, field, value_state, value_json,
          source_kind, retrieved_at, confidence, data_license, locked)
         VALUES (?1, ?2, ?3, ?4, 'known', ?5, 'local-tags', ?6, 0.5, 'user-owned', 0)",
        params![claim_id, entity_type, entity_id, field, value_json, now],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO canonical_values
         (entity_type, entity_id, field, value_state, value_json,
          winning_claim_id, policy_version, resolved_at)
         VALUES (?1, ?2, ?3, 'known', ?4, ?5, 1, ?6)",
        params![entity_type, entity_id, field, value_json, claim_id, now],
    )?;
    Ok(())
}

fn provider_license(provider: &str) -> &'static str {
    match provider {
        "musicbrainz" | "discogs-dump" => "CC0-1.0",
        _ => "source-specific",
    }
}

fn prepare_import_assets(album: &Album, items: &[Item]) -> Result<Vec<PreparedAsset>> {
    let mut assets = Vec::with_capacity(items.len() + usize::from(album.artpath.is_some()));
    for item in items {
        assets.push(prepare_asset(&item.path, "audio")?);
    }
    if let Some(artwork) = &album.artpath {
        assets.push(prepare_asset(artwork, "artwork")?);
    }
    Ok(assets)
}

fn prepare_asset(path: &Path, role: &'static str) -> Result<PreparedAsset> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedAsset {
                id: uuid::Uuid::new_v4().to_string(),
                path: path.to_path_buf(),
                role,
                verification_state: "unverified",
                byte_size: None,
                blake3: None,
                sha256: None,
                mtime: None,
                entry_identity: None,
                media_json: None,
                audio_essence_hash: None,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let before_content = std::fs::metadata(path)?;
    let identity = file_identity(&before);
    let digests = digest_file(path)?;
    let after = std::fs::symlink_metadata(path)?;
    let after_content = std::fs::metadata(path)?;
    if file_identity(&after) != identity
        || before_content.len() != digests.byte_size()
        || after_content.len() != digests.byte_size()
    {
        return Err(Error::Import(format!(
            "asset changed while calculating persistent identity: {}",
            path.display()
        )));
    }
    let media_json = (role == "audio")
        .then(|| {
            probe_media(path)
                .ok()
                .and_then(|media| serde_json::to_string(&media).ok())
        })
        .flatten();
    let audio_essence_hash = if role == "audio" && media_json.is_some() {
        decoded_audio_essence_hash(path).ok()
    } else {
        None
    };
    Ok(PreparedAsset {
        id: uuid::Uuid::new_v4().to_string(),
        path: path.to_path_buf(),
        role,
        verification_state: "verified",
        byte_size: Some(digests.byte_size()),
        blake3: Some(digests.blake3().to_string()),
        sha256: Some(digests.sha256().to_string()),
        mtime: Some(after.modified()?.into()),
        entry_identity: Some(file_identity(&after)),
        media_json,
        audio_essence_hash,
    })
}

fn find_or_insert_root(transaction: &Transaction<'_>, root: Option<&Path>) -> Result<String> {
    const LEGACY_ROOT: &str = "00000000-0000-0000-0000-000000000000";
    let Some(root) = root else {
        return Ok(LEGACY_ROOT.into());
    };
    let stored = path_to_storage(root)?;
    if let Some(id) = transaction
        .query_row(
            "SELECT id FROM library_roots WHERE path = ?1",
            [stored],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let capabilities = RootCapabilities::detect(root)?;
    capabilities.require_safe_mutation()?;
    let capabilities_json = serde_json::to_string(&capabilities)?;
    transaction.execute(
        "INSERT INTO library_roots
         (id, path, state, capabilities_json, created_at, updated_at)
         VALUES (?1, ?2, 'online', ?3, ?4, ?4)",
        params![id, stored, capabilities_json, now],
    )?;
    Ok(id)
}

fn insert_asset(
    transaction: &Transaction<'_>,
    root_id: &str,
    root: Option<&Path>,
    asset: &PreparedAsset,
) -> Result<String> {
    let absolute_path = path_to_storage(&asset.path)?;
    if let Some(existing) = transaction
        .query_row(
            "SELECT id FROM assets WHERE absolute_path = ?1",
            [absolute_path],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Err(Error::Import(format!(
            "asset path is already registered as {existing}: {}",
            asset.path.display()
        )));
    }
    let relative = match root {
        Some(root) => asset.path.strip_prefix(root).map_err(|_error| {
            Error::Import(format!(
                "asset is outside its configured library root: {}",
                asset.path.display()
            ))
        })?,
        None => asset.path.as_path(),
    };
    let relative = path_to_storage(relative)?;
    let now = Utc::now().to_rfc3339();
    transaction.execute(
        "INSERT INTO assets
         (id, root_id, relative_path, absolute_path, role, managed,
          verification_state, byte_size, blake3, sha256, audio_essence_hash,
          mtime, entry_identity, media_json,
          first_seen_at, last_verified_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
        params![
            asset.id,
            root_id,
            relative,
            absolute_path,
            asset.role,
            asset.verification_state,
            asset.byte_size,
            asset.blake3,
            asset.sha256,
            asset.audio_essence_hash,
            asset.mtime.map(|mtime| mtime.to_rfc3339()),
            asset.entry_identity,
            asset.media_json,
            now,
        ],
    )?;
    Ok(asset.id.clone())
}

fn find_or_reuse_asset(
    transaction: &Transaction<'_>,
    root_id: &str,
    root: Option<&Path>,
    asset: &PreparedAsset,
) -> Result<String> {
    let absolute_path = path_to_storage(&asset.path)?;
    let existing = transaction
        .query_row(
            "SELECT id, managed, verification_state, byte_size, blake3, sha256
             FROM assets WHERE absolute_path = ?1",
            [&absolute_path],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<u64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;
    if let Some((id, managed, state, size, blake3, sha256)) = existing {
        if managed
            && state == "verified"
            && size == asset.byte_size
            && blake3 == asset.blake3
            && sha256 == asset.sha256
        {
            return Ok(id);
        }
        return Err(Error::Import(format!(
            "content-addressed asset path is not a verified identical managed asset: {}",
            asset.path.display()
        )));
    }
    insert_asset(transaction, root_id, root, asset)
}

fn store_artwork_metadata(
    transaction: &Transaction<'_>,
    asset_id: &str,
    original_asset_id: &str,
    release_id: &str,
    metadata: &ArtworkAssetMetadata,
) -> Result<()> {
    let release_group_id = transaction.query_row(
        "SELECT release_group_id FROM releases WHERE id = ?1",
        [release_id],
        |row| row.get::<_, Option<String>>(0),
    )?;
    let provenance = metadata.provenance();
    let (width, height) = metadata.dimensions();
    let exact_release_id = provenance.exact_release.then_some(release_id);
    let fallback_group = (!provenance.exact_release)
        .then_some(release_group_id)
        .flatten();
    let transform = (asset_id != original_asset_id).then(|| {
        serde_json::json!({
            "kind": "external-cover-projection",
            "source_asset_id": original_asset_id,
            "cropped": false,
            "upscaled": false,
            "generative": false
        })
        .to_string()
    });
    transaction.execute(
        "INSERT INTO artwork_metadata
         (asset_id, exact_release_id, release_group_id, potentially_inexact,
          role, source_provider, source_reference, provider_release_id,
          mime, width, height, approval_state, rights, original_asset_id, transform_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 'approved', ?12, ?13, ?14)
         ON CONFLICT(asset_id) DO NOTHING",
        params![
            asset_id,
            exact_release_id,
            fallback_group,
            !provenance.exact_release,
            provenance.role.as_str(),
            provenance.source_provider,
            provenance.source_reference,
            provenance.provider_release_id,
            metadata.mime(),
            width,
            height,
            provenance.rights,
            original_asset_id,
            transform,
        ],
    )?;
    Ok(())
}

fn update_asset(
    transaction: &Transaction<'_>,
    asset_id: &str,
    root_id: &str,
    root: Option<&Path>,
    asset: &PreparedAsset,
) -> Result<()> {
    let absolute_path = path_to_storage(&asset.path)?;
    let relative = match root {
        Some(root) => asset.path.strip_prefix(root).map_err(|_error| {
            Error::Import(format!(
                "asset is outside its configured library root: {}",
                asset.path.display()
            ))
        })?,
        None => asset.path.as_path(),
    };
    let relative = path_to_storage(relative)?;
    let now = Utc::now().to_rfc3339();
    let changed = transaction.execute(
        "UPDATE assets
         SET root_id = ?1, relative_path = ?2, absolute_path = ?3, role = ?4,
             managed = 1, verification_state = ?5, byte_size = ?6,
             blake3 = ?7, sha256 = ?8, audio_essence_hash = ?9,
             mtime = ?10, entry_identity = ?11, media_json = ?12,
             projection_state = 'current', last_verified_at = ?13
         WHERE id = ?14",
        params![
            root_id,
            relative,
            absolute_path,
            asset.role,
            asset.verification_state,
            asset.byte_size,
            asset.blake3,
            asset.sha256,
            asset.audio_essence_hash,
            asset.mtime.map(|mtime| mtime.to_rfc3339()),
            asset.entry_identity,
            asset.media_json,
            now,
            asset_id,
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(Error::Import(format!(
            "cannot verify missing asset record {asset_id}"
        )))
    }
}

fn split_external_id(external_id: Option<&ExternalId>) -> (Option<&str>, Option<&str>) {
    external_id.map_or((None, None), |id| {
        (Some(id.provider.as_str()), Some(id.value.as_str()))
    })
}

fn musicbrainz_id(external_id: Option<&ExternalId>) -> Option<&str> {
    external_id
        .filter(|id| id.provider == "musicbrainz")
        .map(|id| id.value.as_str())
}

fn path_to_storage(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        Error::Import(format!(
            "path is not valid UTF-8 and cannot be stored safely: {}",
            path.display()
        ))
    })
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let provider: Option<String> = row.get("metadata_provider")?;
    Ok(Item {
        id: row.get("id")?,
        album_id: row.get("album_id")?,
        path: PathBuf::from(row.get::<_, String>("path")?),
        title: row.get("title")?,
        artist: row.get("artist")?,
        album: row.get("album")?,
        albumartist: row.get("albumartist")?,
        genre: row.get("genre")?,
        year: row.get("year")?,
        track: row.get("track")?,
        disc: row.get("disc")?,
        format: AudioFormat::from_storage(&row.get::<_, String>("format")?),
        bitrate: row.get("bitrate")?,
        length: row.get("length")?,
        file_size: row.get("file_size")?,
        track_external_id: external_id(
            provider.as_deref(),
            row.get::<_, Option<String>>("external_track_id")?,
        ),
        release_external_id: external_id(
            provider.as_deref(),
            row.get::<_, Option<String>>("external_release_id")?,
        ),
        added: parse_datetime(&row.get::<_, String>("added")?)?,
        mtime: parse_datetime(&row.get::<_, String>("mtime")?)?,
    })
}

fn row_to_album(row: &rusqlite::Row<'_>) -> rusqlite::Result<Album> {
    Ok(Album {
        id: row.get("id")?,
        album: row.get("album")?,
        albumartist: row.get("albumartist")?,
        year: row.get("year")?,
        artpath: row.get::<_, Option<String>>("artpath")?.map(PathBuf::from),
        external_id: external_id(
            row.get::<_, Option<String>>("metadata_provider")?
                .as_deref(),
            row.get::<_, Option<String>>("external_release_id")?,
        ),
        added: parse_datetime(&row.get::<_, String>("added")?)?,
    })
}

fn external_id(provider: Option<&str>, value: Option<String>) -> Option<ExternalId> {
    provider.zip(value).map(|(provider, value)| ExternalId {
        provider: provider.to_string(),
        value,
    })
}

fn parse_datetime(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error)))
}

fn valid_datetime(value: &str) -> bool {
    DateTime::parse_from_rfc3339(value).is_ok()
}

fn require_journal_row(changed: usize, action: &str) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(Error::Recovery(format!(
            "{action} expected one journal row, changed {changed}"
        )))
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('!', "!!")
        .replace('%', "!%")
        .replace('_', "!_")
}

#[expect(
    clippy::too_many_lines,
    reason = "typed recovery keeps every operation/state/file-role branch visible in one exhaustive protocol"
)]
fn recover_operation(operation: &PendingOperation) -> Result<()> {
    if operation.state == "recovery-required" {
        return Err(Error::Recovery(
            "legacy journal entry has an ambiguous commit phase; refusing automatic recovery"
                .into(),
        ));
    }
    let committed = operation.state == "db-committed" || operation.state == "cleanup-pending";
    for (ordinal, file) in operation.files.iter().enumerate() {
        match (operation.kind, committed) {
            (OperationKind::TagWrite, false) if file.role == "tag-rewrite" => {
                rollback_regular_import(file)?;
            }
            (OperationKind::TagWrite, false) if file.role == "tag-original" => {
                restore_quarantined(file)?;
            }
            (OperationKind::TagWrite, true) if file.role == "tag-rewrite" => {
                remove_if_owned(&file.staged, file)?;
                verify_owned(&file.destination, file)?;
            }
            (OperationKind::TagWrite, true) if file.role == "tag-original" => {
                if file.staged.exists() || file.staged.is_symlink() {
                    verify_source_identity(&file.staged, file)?;
                    verify_owned(&file.staged, file)?;
                }
            }
            (OperationKind::ArtworkWrite, false) if file.role == "artwork-rewrite" => {
                rollback_regular_import(file)?;
            }
            (OperationKind::ArtworkWrite, false) if file.role == "artwork-original" => {
                restore_quarantined(file)?;
            }
            (OperationKind::ArtworkWrite, true) if file.role == "artwork-rewrite" => {
                remove_if_owned(&file.staged, file)?;
                verify_owned(&file.destination, file)?;
            }
            (OperationKind::ArtworkWrite, true) if file.role == "artwork-original" => {
                if file.staged.exists() || file.staged.is_symlink() {
                    verify_source_identity(&file.staged, file)?;
                    verify_owned(&file.staged, file)?;
                }
            }
            (OperationKind::RemoveDelete | OperationKind::PurgeDelete, false) => {
                restore_quarantined(file)?;
            }
            (OperationKind::RemoveDelete, true) => {
                if file.staged.exists() || file.staged.is_symlink() {
                    verify_source_identity(&file.staged, file)?;
                    verify_owned(&file.staged, file)?;
                }
            }
            (OperationKind::PurgeDelete, true) => remove_if_owned(&file.staged, file)?,
            (
                OperationKind::ManifestWrite
                | OperationKind::RestoreCopy
                | OperationKind::AncillaryCopy,
                true,
            ) => {
                remove_if_owned(&file.staged, file)?;
                verify_owned(&file.destination, file)?;
            }
            (
                OperationKind::ManifestWrite
                | OperationKind::RestoreCopy
                | OperationKind::AncillaryCopy,
                false,
            ) => rollback_regular_import(file)?,
            (OperationKind::PathWrite, committed) => {
                recover_path_projection(file, committed)?;
            }
            (OperationKind::ImportMove, true) if file.role == "track" => {
                verify_owned(&file.destination, file)?;
                cleanup_move_source(&operation.id, ordinal, file)?;
                remove_if_owned(&file.staged, file)?;
            }
            (OperationKind::ImportCopy | OperationKind::ImportMove, true) => {
                remove_if_owned(&file.staged, file)?;
            }
            (OperationKind::ImportLink, true) if file.role == "artwork" => {
                remove_if_owned(&file.staged, file)?;
            }
            (OperationKind::ImportLink, true) => {
                remove_link_if_owned(&file.staged, file)?;
            }
            (OperationKind::ImportCopy | OperationKind::ImportMove, false) => {
                rollback_regular_import(file)?;
            }
            (OperationKind::ImportLink, false) if file.role == "artwork" => {
                rollback_regular_import(file)?;
            }
            (OperationKind::ImportLink, false) => {
                rollback_link_import(file)?;
            }
            (OperationKind::TagWrite, _) => {
                return Err(Error::Recovery(format!(
                    "tag journal has unknown file role {:?}",
                    file.role
                )));
            }
            (OperationKind::ArtworkWrite, _) => {
                return Err(Error::Recovery(format!(
                    "artwork journal has unknown file role {:?}",
                    file.role
                )));
            }
        }
    }
    Ok(())
}

fn cleanup_move_source(operation_id: &str, ordinal: usize, file: &JournalFile) -> Result<()> {
    let quarantine = move_cleanup_path(&file.source, operation_id, ordinal)?;
    if !quarantine.exists() && !quarantine.is_symlink() {
        if !file.source.exists() && !file.source.is_symlink() {
            return Ok(());
        }
        rename_sibling_anchored(&file.source, &quarantine)?;
        if let Err(error) = verify_source_identity(&quarantine, file)
            .and_then(|()| verify_content_hash(&quarantine, file))
        {
            if let Err(restore_error) = rename_sibling_anchored(&quarantine, &file.source) {
                return Err(Error::Recovery(format!(
                    "{error}; acquired move source was preserved at {}; safe restore failed: {restore_error}",
                    quarantine.display()
                )));
            }
            return Err(error);
        }
    }
    verify_source_identity(&quarantine, file)?;
    verify_content_hash(&quarantine, file)?;
    remove_file_synced(&quarantine)
}

fn move_cleanup_path(source: &Path, operation_id: &str, ordinal: usize) -> Result<PathBuf> {
    let name = source.file_name().ok_or_else(|| {
        Error::Recovery(format!("move source has no filename: {}", source.display()))
    })?;
    let mut quarantine_name = std::ffi::OsString::from(".");
    quarantine_name.push(name);
    quarantine_name.push(format!(".rsbts-{operation_id}-{ordinal}.move"));
    Ok(source.with_file_name(quarantine_name))
}

fn restore_quarantined(file: &JournalFile) -> Result<()> {
    if !file.staged.exists() && !file.staged.is_symlink() {
        return Ok(());
    }
    if file.source.exists() || file.source.is_symlink() {
        if same_entry(&file.source, &file.staged)? {
            remove_file_synced(&file.staged)?;
            return Ok(());
        }
        return Err(Error::Recovery(format!(
            "cannot restore {}; destination already exists",
            file.source.display()
        )));
    }
    verify_source_identity(&file.staged, file)?;
    verify_owned(&file.staged, file)?;
    rename_sibling_anchored(&file.staged, &file.source)?;
    Ok(())
}

fn remove_if_owned(path: &Path, file: &JournalFile) -> Result<()> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    verify_owned(path, file)?;
    remove_file_synced(path)?;
    Ok(())
}

fn remove_link_if_owned(path: &Path, file: &JournalFile) -> Result<()> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    if !path.is_symlink() || std::fs::read_link(path)? != file.source {
        return Err(Error::Recovery(format!(
            "refusing to remove changed journal link {}",
            path.display()
        )));
    }
    verify_owned(path, file)?;
    remove_file_synced(path)?;
    Ok(())
}

fn rollback_regular_import(file: &JournalFile) -> Result<()> {
    rollback_import_paths(file, remove_if_owned)
}

fn rollback_link_import(file: &JournalFile) -> Result<()> {
    rollback_import_paths(file, remove_link_if_owned)
}

fn recover_path_projection(file: &JournalFile, committed: bool) -> Result<()> {
    if committed {
        remove_if_owned(&file.staged, file)?;
        return verify_owned(&file.destination, file);
    }
    let source_exists = file.source.exists() || file.source.is_symlink();
    let staged_exists = file.staged.exists() || file.staged.is_symlink();
    let destination_exists = file.destination.exists() || file.destination.is_symlink();
    if source_exists {
        if staged_exists || destination_exists {
            return Err(Error::Recovery(format!(
                "path-projection source is occupied; preserving every candidate for review: {}",
                file.source.display()
            )));
        }
        return Ok(());
    }
    let retained = if staged_exists {
        &file.staged
    } else if destination_exists {
        &file.destination
    } else {
        return Err(Error::Recovery(format!(
            "path projection lost every expected candidate for {}",
            file.source.display()
        )));
    };
    verify_owned(retained, file)?;
    let anchor = common_path_ancestor(retained, &file.source)?;
    AnchoredRoot::open(&anchor)?.rename_noreplace(retained, &file.source)
}

fn common_path_ancestor(left: &Path, right: &Path) -> Result<PathBuf> {
    let mut ancestor = left
        .parent()
        .ok_or_else(|| Error::Recovery(format!("path has no parent: {}", left.display())))?;
    while !right.starts_with(ancestor) {
        ancestor = ancestor.parent().ok_or_else(|| {
            Error::Recovery(format!(
                "paths do not share an absolute recovery root: {} and {}",
                left.display(),
                right.display()
            ))
        })?;
    }
    Ok(ancestor.to_path_buf())
}

fn rollback_import_paths(
    file: &JournalFile,
    remove_owned: fn(&Path, &JournalFile) -> Result<()>,
) -> Result<()> {
    let staged_exists = file.staged.exists() || file.staged.is_symlink();
    let destination_exists = file.destination.exists() || file.destination.is_symlink();
    if file.state == "prepared" {
        return remove_owned(&file.staged, file);
    }
    if file.state == "acquired" {
        return remove_if_entry_owned(&file.staged, file);
    }
    if staged_exists && destination_exists && !same_entry(&file.staged, &file.destination)? {
        // A no-clobber finalization failed because another file won the destination.
        return remove_owned(&file.staged, file);
    }
    if destination_exists {
        remove_owned(&file.destination, file)?;
    }
    if staged_exists {
        remove_owned(&file.staged, file)?;
    }
    Ok(())
}

fn remove_if_entry_owned(path: &Path, file: &JournalFile) -> Result<()> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    let expected = file.owned_identity.as_deref().ok_or_else(|| {
        Error::Recovery(format!(
            "journal has no acquired identity for {}; preserving it",
            path.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(path)?;
    if file_object_identity(&metadata) != expected {
        return Err(Error::Recovery(format!(
            "acquired journal path was replaced; preserving {}",
            path.display()
        )));
    }
    remove_file_synced(path)
}

#[cfg(unix)]
fn same_entry(left: &Path, right: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let left = std::fs::symlink_metadata(left)?;
    let right = std::fs::symlink_metadata(right)?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(not(unix))]
fn same_entry(left: &Path, right: &Path) -> Result<bool> {
    let left = std::fs::canonicalize(left)?;
    let right = std::fs::canonicalize(right)?;
    Ok(left == right)
}

fn verify_owned(path: &Path, file: &JournalFile) -> Result<()> {
    let expected_identity = file.owned_identity.as_deref().ok_or_else(|| {
        Error::Recovery(format!(
            "journal has no owned-file identity for {}; preserving it",
            path.display()
        ))
    })?;
    // Link imports use the track role because the journal role describes the payload, not its
    // transfer mechanism. Inspect the actual directory entry so ownership of an imported
    // symlink is compared with the symlink inode recorded during staging, never its target.
    let before = if path.is_symlink() {
        std::fs::symlink_metadata(path)?
    } else {
        std::fs::metadata(path)?
    };
    if file_identity(&before) != expected_identity {
        return Err(Error::Recovery(format!(
            "refusing to touch replaced journal path {}",
            path.display()
        )));
    }
    verify_content_hash(path, file)?;
    let after = if path.is_symlink() {
        std::fs::symlink_metadata(path)?
    } else {
        std::fs::metadata(path)?
    };
    if file_identity(&after) == expected_identity {
        Ok(())
    } else {
        Err(Error::Recovery(format!(
            "journal path changed while it was being verified: {}",
            path.display()
        )))
    }
}

fn verify_content_hash(path: &Path, file: &JournalFile) -> Result<()> {
    let expected = file.content_hash.as_deref().ok_or_else(|| {
        Error::Recovery(format!(
            "journal has no ownership hash for {}",
            path.display()
        ))
    })?;
    let actual = journal_hash_path(path, &file.role)?;
    if actual != expected {
        return Err(Error::Recovery(format!(
            "refusing to touch changed journal path {}",
            path.display()
        )));
    }
    if let Some(expected_sha256) = &file.sha256 {
        if path.is_symlink() || digest_file(path)?.sha256() != expected_sha256 {
            return Err(Error::Recovery(format!(
                "refusing to touch journal path with changed SHA-256: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn verify_source_identity(path: &Path, file: &JournalFile) -> Result<()> {
    let expected = file.source_identity.as_deref().ok_or_else(|| {
        Error::Recovery(format!(
            "journal has no source identity for {}; preserving it",
            path.display()
        ))
    })?;
    let metadata = if file.role == "symlink" {
        std::fs::symlink_metadata(path)?
    } else {
        std::fs::metadata(path)?
    };
    if file_identity(&metadata) == expected {
        Ok(())
    } else {
        Err(Error::Recovery(format!(
            "refusing to remove replaced move source {}",
            path.display()
        )))
    }
}

pub(crate) fn hash_path(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

pub(crate) fn journal_hash_path(path: &Path, role: &str) -> Result<String> {
    if role == "symlink" {
        let target = std::fs::read_link(path)?;
        Ok(hash_os_string(&target))
    } else {
        hash_path(path)
    }
}

#[cfg(unix)]
pub(crate) fn file_identity(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    format!(
        "{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime(),
        metadata.mtime_nsec()
    )
}

#[cfg(unix)]
pub(crate) fn file_object_identity(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    format!("{}:{}", metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
pub(crate) fn file_identity(metadata: &std::fs::Metadata) -> String {
    format!("{}:{:?}", metadata.len(), metadata.modified().ok())
}

#[cfg(not(unix))]
pub(crate) fn file_object_identity(metadata: &std::fs::Metadata) -> String {
    format!("{:?}", metadata.created().ok())
}

pub(crate) fn remove_file_synced(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::Recovery(format!(
            "filesystem entry has no anchored parent: {}",
            path.display()
        ))
    })?;
    AnchoredRoot::open(parent)?.remove_file(path)
}

fn rename_sibling_anchored(source: &Path, destination: &Path) -> Result<()> {
    let parent = source.parent().ok_or_else(|| {
        Error::Recovery(format!(
            "filesystem entry has no anchored parent: {}",
            source.display()
        ))
    })?;
    if destination.parent() != Some(parent) {
        return Err(Error::Recovery(format!(
            "safe recovery rename must remain in one directory: {} -> {}",
            source.display(),
            destination.display()
        )));
    }
    AnchoredRoot::open(parent)?.rename_noreplace(source, destination)
}

#[cfg(all(test, unix))]
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    match std::fs::File::open(path).and_then(|directory| directory.sync_all()) {
        Ok(()) => failpoints::hit("fs.sync-directory"),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(all(test, not(unix)))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) const fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn hash_os_string(value: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    blake3::hash(value.as_os_str().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(not(unix))]
fn hash_os_string(value: &Path) -> String {
    blake3::hash(value.to_string_lossy().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_schema_dry_run_uses_a_read_only_connection_without_copying() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let database = temporary.path().join("library.db");
        drop(Library::open(&database)?);

        let snapshot = Library::open_snapshot(&database)?;
        assert_eq!(snapshot.path(), Some(database.as_path()));
        assert!(snapshot
            .create_operation(OperationKind::ImportCopy, &[])
            .is_err());
        Ok(())
    }

    #[test]
    fn a_second_library_writer_is_rejected_before_database_work() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let database = temporary.path().join("library.db");
        let first = Library::open(&database)?;
        assert!(Library::open(&database).is_err());
        assert!(first.query_items(&Query::all())?.is_empty());
        Ok(())
    }

    #[test]
    fn audit_preserves_and_reports_missing_files() -> Result<()> {
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: "Missing".into(),
            albumartist: "Nobody".into(),
            year: None,
            artpath: None,
            external_id: None,
            added: Utc::now(),
        };
        let item = Item {
            id: None,
            album_id: None,
            path: PathBuf::from("/definitely/missing.flac"),
            title: "Missing".into(),
            artist: "Nobody".into(),
            album: "Missing".into(),
            albumartist: None,
            genre: None,
            year: None,
            track: Some(1),
            disc: Some(1),
            format: AudioFormat::Flac,
            bitrate: 0,
            length: 1.0,
            file_size: None,
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
        };
        library.commit_import(&operation, &album, &[item])?;
        library.complete_operation(&operation)?;
        let audit = library.audit()?;
        assert!(matches!(
            audit.issues.first(),
            Some(AuditIssue::MissingFile { .. })
        ));
        Ok(())
    }

    #[test]
    fn quick_and_deep_audit_detect_external_asset_changes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"original audio")?;
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: "Album".into(),
            albumartist: "Artist".into(),
            year: None,
            artpath: None,
            external_id: None,
            added: Utc::now(),
        };
        let item = Item {
            id: None,
            album_id: None,
            path: path.clone(),
            title: "Track".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            albumartist: None,
            genre: None,
            year: None,
            track: Some(1),
            disc: Some(1),
            format: AudioFormat::Flac,
            bitrate: 1,
            length: 1.0,
            file_size: Some(14),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
        };
        library.commit_import(&operation, &album, &[item])?;
        library.complete_operation(&operation)?;

        std::fs::write(&path, b"changed")?;
        let quick = library.audit_with_mode(AuditMode::Quick)?;
        assert!(quick.issues.iter().any(|issue| matches!(
            issue,
            AuditIssue::AssetSizeMismatch { path: changed, .. } if changed == &path
        )));

        assert!(matches!(
            library.audit_with_mode(AuditMode::Deep),
            Err(Error::Operation(detail))
                if detail.contains("durable, paged fixity workflow")
        ));
        Ok(())
    }

    #[test]
    fn completed_operations_remain_as_durable_history() -> Result<()> {
        let library = Library::open_in_memory()?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        library.complete_operation(&operation)?;
        let (state, completed_at): (String, Option<String>) = library.conn.query_row(
            "SELECT state, completed_at FROM operation_journal WHERE id = ?1",
            [&operation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(state, "complete");
        assert!(completed_at.is_some());
        assert!(library.pending_operations()?.is_empty());
        Ok(())
    }

    #[test]
    fn current_databases_preserve_unknown_sizes_for_explicit_audit() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let database_path = temporary.path().join("library.db");
        let track_path = temporary.path().join("track.flac");
        std::fs::write(&track_path, b"audio")?;
        {
            let library = Library::open(&database_path)?;
            library.conn.execute(
                "INSERT INTO albums (album, albumartist, added)
                 VALUES ('Album', 'Artist', '2024-01-01T00:00:00Z')",
                [],
            )?;
            let album_id = library.conn.last_insert_rowid();
            library.conn.execute(
                "INSERT INTO items
                 (album_id, path, title, artist, album, format, bitrate, length, added, mtime)
                 VALUES (?1, ?2, 'Track', 'Artist', 'Album', 'FLAC', 1, 1,
                         '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
                params![album_id, path_to_storage(&track_path)?],
            )?;
        }

        let library = Library::open(&database_path)?;
        assert!(library.audit()?.issues.iter().any(|issue| matches!(
            issue,
            AuditIssue::UnknownFileSize { path, .. } if path == &track_path
        )));
        Ok(())
    }

    #[test]
    fn audit_detects_external_content_search_index_drift() -> Result<()> {
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: "Indexed".into(),
            albumartist: "Artist".into(),
            year: None,
            artpath: None,
            external_id: None,
            added: Utc::now(),
        };
        let item = Item {
            id: None,
            album_id: None,
            path: PathBuf::from("/definitely/missing-indexed.flac"),
            title: "Indexed".into(),
            artist: "Artist".into(),
            album: "Indexed".into(),
            albumartist: None,
            genre: None,
            year: None,
            track: Some(1),
            disc: Some(1),
            format: AudioFormat::Flac,
            bitrate: 0,
            length: 1.0,
            file_size: Some(1),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
        };
        library.commit_import(&operation, &album, &[item])?;
        library.complete_operation(&operation)?;
        library.conn.execute(
            "INSERT INTO items_fts(items_fts, rowid, title, artist, album, albumartist, genre)
             SELECT 'delete', id, title, artist, album, albumartist, genre FROM items",
            [],
        )?;

        let audit = library.audit()?;
        assert!(audit
            .issues
            .iter()
            .any(|issue| matches!(issue, AuditIssue::SearchIndexInconsistent { .. })));
        Ok(())
    }

    #[test]
    fn parameterized_query_handles_quotes() -> Result<()> {
        let library = Library::open_in_memory()?;
        let query = Query::parse("artist:o'brien")?;
        assert!(library.query_items(&query)?.is_empty());
        Ok(())
    }

    #[test]
    fn full_text_quotes_are_literal_and_empty_stats_are_valid() -> Result<()> {
        let library = Library::open_in_memory()?;
        assert!(library.query_items(&Query::parse("o'brien")?)?.is_empty());
        assert!(library.query_items(&Query::parse("C++")?)?.is_empty());
        let stats = library.stats()?;
        assert_eq!(stats.tracks, 0);
        assert_eq!(stats.unknown_sizes, 0);
        Ok(())
    }

    #[test]
    fn cached_statistics_follow_insert_update_and_delete() -> Result<()> {
        let library = Library::open_in_memory()?;
        library.conn.execute(
            "INSERT INTO albums (album, albumartist, added)
             VALUES ('First', 'Artist A', '2026-01-01T00:00:00Z'),
                    ('Second', 'Artist B', '2026-01-01T00:00:00Z')",
            [],
        )?;
        let first_album: i64 =
            library
                .conn
                .query_row("SELECT id FROM albums WHERE album = 'First'", [], |row| {
                    row.get(0)
                })?;
        let second_album: i64 =
            library
                .conn
                .query_row("SELECT id FROM albums WHERE album = 'Second'", [], |row| {
                    row.get(0)
                })?;
        library.conn.execute(
            "INSERT INTO items
             (album_id, path, title, artist, album, format, bitrate, length,
              added, mtime, file_size)
             VALUES (?1, '/one.wav', 'One', 'Artist A', 'First', 'WAV', 0, 2.0,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL),
                    (?1, '/two.wav', 'Two', 'Artist B', 'First', 'WAV', 0, 3.0,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 4)",
            [first_album],
        )?;
        let stats = library.stats()?;
        assert_eq!(
            (
                stats.tracks,
                stats.albums,
                stats.artists,
                stats.total_length,
                stats.total_size,
                stats.unknown_sizes
            ),
            (2, 1, 2, 5.0, 4, 1)
        );

        library.conn.execute(
            "UPDATE items SET album_id = ?1, artist = 'Artist B',
                    length = 7.0, file_size = 8
             WHERE title = 'One'",
            [second_album],
        )?;
        let stats = library.stats()?;
        assert_eq!(
            (
                stats.tracks,
                stats.albums,
                stats.artists,
                stats.total_length,
                stats.total_size,
                stats.unknown_sizes
            ),
            (2, 2, 1, 10.0, 12, 0)
        );

        library
            .conn
            .execute("DELETE FROM items WHERE title = 'Two'", [])?;
        let stats = library.stats()?;
        assert_eq!(
            (
                stats.tracks,
                stats.albums,
                stats.artists,
                stats.total_length,
                stats.total_size,
                stats.unknown_sizes
            ),
            (1, 1, 1, 7.0, 8, 0)
        );
        Ok(())
    }

    #[test]
    fn album_substrings_treat_like_metacharacters_literally() -> Result<()> {
        let library = Library::open_in_memory()?;
        library.conn.execute(
            "INSERT INTO albums (album, albumartist, added) VALUES
             ('100% Real', 'Artist', '2024-01-01T00:00:00Z'),
             ('100X Real', 'Artist', '2024-01-01T00:00:00Z')",
            [],
        )?;

        let albums = library.query_albums(Some("100%"))?;
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].album, "100% Real");
        Ok(())
    }

    #[test]
    fn invalid_timestamps_are_audited_and_never_coerced_to_epoch() -> Result<()> {
        let library = Library::open_in_memory()?;
        library.conn.execute(
            "INSERT INTO albums (album, albumartist, added)
             VALUES ('Broken', 'Artist', 'not-a-timestamp')",
            [],
        )?;

        let audit = library.audit()?;
        assert!(audit.issues.iter().any(|issue| matches!(
            issue,
            AuditIssue::InvalidTimestamp {
                table: "albums",
                field: "added",
                ..
            }
        )));
        assert!(library.query_albums(None).is_err());
        Ok(())
    }

    #[test]
    fn rollback_preserves_a_destination_that_lost_the_race() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let staged = temporary.path().join("staged");
        let destination = temporary.path().join("destination");
        std::fs::write(&source, b"same bytes")?;
        std::fs::write(&staged, b"same bytes")?;
        std::fs::write(&destination, b"same bytes")?;
        let hash = hash_path(&source)?;
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::ImportCopy,
            &[JournalFile {
                source,
                staged: staged.clone(),
                destination: destination.clone(),
                content_hash: Some(hash),
                sha256: None,
                source_identity: None,
                owned_identity: Some(file_identity(&std::fs::metadata(&staged)?)),
                role: "track".into(),
                state: "prepared".into(),
            }],
        )?;
        library.set_file_state(&operation, 0, "staged")?;
        library.set_operation_state(&operation, "failed", None)?;
        let report = library.recover_pending()?;
        assert!(report.unresolved.is_empty());
        assert!(!staged.exists());
        assert!(destination.exists());
        Ok(())
    }

    #[test]
    fn rollback_removes_a_hard_link_finalized_by_rsbts() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let staged = temporary.path().join("staged");
        let destination = temporary.path().join("destination");
        std::fs::write(&source, b"audio")?;
        std::fs::write(&staged, b"audio")?;
        std::fs::hard_link(&staged, &destination)?;
        let hash = hash_path(&source)?;
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::ImportCopy,
            &[JournalFile {
                source,
                staged: staged.clone(),
                destination: destination.clone(),
                content_hash: Some(hash),
                sha256: None,
                source_identity: None,
                owned_identity: Some(file_identity(&std::fs::metadata(&staged)?)),
                role: "track".into(),
                state: "prepared".into(),
            }],
        )?;
        library.set_file_state(&operation, 0, "staged")?;
        library.set_operation_state(&operation, "failed", None)?;
        let report = library.recover_pending()?;
        assert!(report.unresolved.is_empty());
        assert!(!staged.exists());
        assert!(!destination.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rollback_removes_a_finalized_import_link_owned_by_rsbts() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let staged = temporary.path().join("staged");
        let destination = temporary.path().join("destination");
        std::fs::write(&source, b"audio")?;
        std::os::unix::fs::symlink(&source, &staged)?;
        std::fs::hard_link(&staged, &destination)?;
        let owned_identity = file_identity(&std::fs::symlink_metadata(&staged)?);
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::ImportLink,
            &[JournalFile {
                source,
                staged: staged.clone(),
                destination: destination.clone(),
                content_hash: Some(hash_path(&staged)?),
                sha256: None,
                source_identity: None,
                owned_identity: Some(owned_identity),
                role: "track".into(),
                state: "prepared".into(),
            }],
        )?;
        library.set_file_state(&operation, 0, "finalized")?;
        library.set_operation_state(&operation, "failed", None)?;

        let report = library.recover_pending()?;

        assert_eq!(report.recovered_operations, [operation]);
        assert!(!staged.is_symlink());
        assert!(!destination.is_symlink());
        Ok(())
    }

    #[test]
    fn rollback_preserves_an_identical_replacement_after_finalization() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let staged = temporary.path().join("staged");
        let destination = temporary.path().join("destination");
        let replaced = temporary.path().join("rsbts-finalized");
        std::fs::write(&source, b"audio")?;
        std::fs::write(&staged, b"audio")?;
        std::fs::hard_link(&staged, &destination)?;
        let hash = hash_path(&source)?;
        let owned_identity = file_identity(&std::fs::metadata(&destination)?);
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::ImportCopy,
            &[JournalFile {
                source,
                staged: staged.clone(),
                destination: destination.clone(),
                content_hash: Some(hash),
                sha256: None,
                source_identity: None,
                owned_identity: Some(owned_identity),
                role: "track".into(),
                state: "prepared".into(),
            }],
        )?;
        library.set_file_state(&operation, 0, "finalized")?;
        std::fs::remove_file(&staged)?;
        std::fs::rename(&destination, replaced)?;
        std::fs::write(&destination, b"audio")?;
        library.set_operation_state(&operation, "failed", None)?;

        let report = library.recover_pending()?;

        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(std::fs::read(destination)?, b"audio");
        Ok(())
    }

    #[test]
    fn committed_move_recovery_preserves_a_changed_source() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let staged = temporary.path().join("staged");
        let destination = temporary.path().join("destination");
        std::fs::write(&source, b"original audio")?;
        std::fs::write(&destination, b"original audio")?;
        let hash = hash_path(&source)?;
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::ImportMove,
            &[JournalFile {
                source: source.clone(),
                staged,
                destination: destination.clone(),
                content_hash: Some(hash),
                sha256: None,
                source_identity: Some(file_identity(&std::fs::metadata(&source)?)),
                owned_identity: Some(file_identity(&std::fs::metadata(&destination)?)),
                role: "track".into(),
                state: "prepared".into(),
            }],
        )?;
        library.set_operation_state(&operation, "db-committed", None)?;
        std::fs::write(&source, b"replacement audio")?;

        let report = library.recover_pending()?;

        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(std::fs::read(&source)?, b"replacement audio");
        let state: String = library.conn.query_row(
            "SELECT state FROM operation_journal WHERE id = ?1",
            [&operation],
            |row| row.get(0),
        )?;
        assert_eq!(state, "db-committed");

        let preserved = temporary.path().join("preserved-replacement");
        std::fs::rename(&source, preserved)?;
        std::fs::write(&source, b"original audio")?;
        let report = library.recover_pending()?;
        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(std::fs::read(source)?, b"original audio");
        Ok(())
    }

    #[test]
    fn committed_move_recovery_removes_the_original_source() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let staged = temporary.path().join("staged");
        let destination = temporary.path().join("destination");
        std::fs::write(&source, b"audio")?;
        std::fs::write(&destination, b"audio")?;
        let hash = hash_path(&source)?;
        let identity = file_identity(&std::fs::metadata(&source)?);
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::ImportMove,
            &[JournalFile {
                source: source.clone(),
                staged,
                destination: destination.clone(),
                content_hash: Some(hash),
                sha256: None,
                source_identity: Some(identity),
                owned_identity: Some(file_identity(&std::fs::metadata(&destination)?)),
                role: "track".into(),
                state: "prepared".into(),
            }],
        )?;
        library.set_operation_state(&operation, "db-committed", None)?;

        let report = library.recover_pending()?;

        assert_eq!(report.recovered_operations, [operation]);
        assert!(!source.exists());
        assert_eq!(std::fs::read(destination)?, b"audio");
        Ok(())
    }

    #[test]
    fn uncommitted_removal_recovery_restores_a_regular_file() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let staged = temporary.path().join("staged");
        std::fs::write(&staged, b"audio")?;
        let hash = hash_path(&staged)?;
        let identity = file_identity(&std::fs::metadata(&staged)?);
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::RemoveDelete,
            &[JournalFile {
                source: source.clone(),
                staged: staged.clone(),
                destination: source.clone(),
                content_hash: Some(hash),
                sha256: None,
                source_identity: Some(identity.clone()),
                owned_identity: Some(identity),
                role: "track".into(),
                state: "prepared".into(),
            }],
        )?;
        library.set_file_state(&operation, 0, "quarantined")?;
        library.set_operation_state(&operation, "staging", None)?;

        let report = library.recover_pending()?;

        assert_eq!(report.recovered_operations, [operation]);
        assert_eq!(std::fs::read(source)?, b"audio");
        assert!(!staged.exists());
        Ok(())
    }

    #[test]
    fn committed_removal_recovery_retains_the_quarantine() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let staged = temporary.path().join("staged");
        std::fs::write(&staged, b"audio")?;
        let hash = hash_path(&staged)?;
        let identity = file_identity(&std::fs::metadata(&staged)?);
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::RemoveDelete,
            &[JournalFile {
                source,
                staged: staged.clone(),
                destination: PathBuf::new(),
                content_hash: Some(hash),
                sha256: None,
                source_identity: Some(identity.clone()),
                owned_identity: Some(identity),
                role: "track".into(),
                state: "prepared".into(),
            }],
        )?;
        library.set_file_state(&operation, 0, "quarantined")?;
        library.set_operation_state(&operation, "db-committed", None)?;

        let report = library.recover_pending()?;

        assert_eq!(report.recovered_operations, [operation]);
        assert_eq!(std::fs::read(staged)?, b"audio");
        Ok(())
    }

    #[test]
    fn modification_validation_is_atomic_and_typed() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: "Album".into(),
            albumartist: "Artist".into(),
            year: Some(2000),
            artpath: None,
            external_id: Some(ExternalId {
                provider: "musicbrainz".into(),
                value: "release".into(),
            }),
            added: Utc::now(),
        };
        let item = Item {
            id: None,
            album_id: None,
            path,
            title: "Original".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            albumartist: None,
            genre: Some("Rock".into()),
            year: Some(2000),
            track: Some(1),
            disc: Some(1),
            format: AudioFormat::Flac,
            bitrate: 1,
            length: 1.0,
            file_size: Some(5),
            track_external_id: Some(ExternalId {
                provider: "musicbrainz".into(),
                value: "track".into(),
            }),
            release_external_id: Some(ExternalId {
                provider: "musicbrainz".into(),
                value: "release".into(),
            }),
            added: Utc::now(),
            mtime: Utc::now(),
        };
        library.commit_import(&operation, &album, &[item])?;
        library.complete_operation(&operation)?;
        let id = library.query_items(&Query::all())?[0]
            .id
            .ok_or_else(|| Error::Query("test item has no ID".into()))?;

        let mut refreshed = library.query_items(&Query::all())?[0].clone();
        refreshed.title = "Must Not Persist".into();
        assert!(library
            .update_items(&[(id, refreshed.clone()), (id + 1, refreshed)])
            .is_err());
        assert_eq!(library.query_items(&Query::all())?[0].title, "Original");

        let mut refreshed = library.query_items(&Query::all())?[0].clone();
        refreshed.title = "Tag Update".into();
        assert_eq!(library.update_items(&[(id, refreshed)])?, 1);
        let tag_updated = &library.query_items(&Query::all())?[0];
        assert_eq!(tag_updated.track_external_id, None);
        assert!(tag_updated.release_external_id.is_some());

        assert!(library
            .modify_items(&[id], &["title=Changed".into(), "year=invalid".into()])
            .is_err());
        let unchanged = &library.query_items(&Query::all())?[0];
        assert_eq!(unchanged.title, "Tag Update");
        assert_eq!(unchanged.year, Some(2000));

        assert_eq!(
            library.modify_items(
                &[id],
                &["title=Changed".into(), "year=2024".into(), "genre=".into()]
            )?,
            1
        );
        let changed = &library.query_items(&Query::all())?[0];
        assert_eq!(changed.title, "Changed");
        assert_eq!(changed.year, Some(2024));
        assert_eq!(changed.genre, None);
        assert_eq!(changed.track_external_id, None);
        assert_eq!(changed.release_external_id, None);
        let albums = library.query_albums(None)?;
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].year, Some(2024));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_fail_closed_before_storage() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(std::ffi::OsStr::from_bytes(b"track-\xff.flac"));
        assert!(path_to_storage(path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn removal_recovery_restores_a_dangling_symlink() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source-link");
        let staged = temporary.path().join("staged-link");
        let missing_target = temporary.path().join("missing-target");
        std::os::unix::fs::symlink(&missing_target, &source)?;
        let hash = journal_hash_path(&source, "symlink")?;
        let identity = file_identity(&std::fs::symlink_metadata(&source)?);
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::RemoveDelete,
            &[JournalFile {
                source: source.clone(),
                staged: staged.clone(),
                destination: source.clone(),
                content_hash: Some(hash),
                sha256: None,
                source_identity: Some(identity.clone()),
                owned_identity: Some(identity),
                role: "symlink".into(),
                state: "prepared".into(),
            }],
        )?;
        std::fs::hard_link(&source, &staged)?;
        std::fs::remove_file(&source)?;
        library.set_file_state(&operation, 0, "quarantined")?;
        library.set_operation_state(&operation, "failed", None)?;

        let report = library.recover_pending()?;

        assert!(report.unresolved.is_empty());
        assert!(source.is_symlink());
        assert_eq!(std::fs::read_link(&source)?, missing_target);
        assert!(!staged.is_symlink());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn move_cleanup_never_treats_a_dangling_quarantine_as_absent() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let operation = uuid::Uuid::new_v4().to_string();
        let quarantine = move_cleanup_path(&source, &operation, 0)?;
        symlink(temporary.path().join("missing-target"), &quarantine)?;
        let file = JournalFile {
            source,
            staged: PathBuf::new(),
            destination: temporary.path().join("destination.flac"),
            content_hash: Some("unreachable".into()),
            sha256: None,
            source_identity: Some("unreachable".into()),
            owned_identity: None,
            role: "track".into(),
            state: "finalized".into(),
        };

        assert!(cleanup_move_source(&operation, 0, &file).is_err());
        assert!(quarantine.is_symlink());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn move_cleanup_never_treats_a_dangling_source_as_missing() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        symlink(temporary.path().join("missing-target"), &source)?;
        let operation = uuid::Uuid::new_v4().to_string();
        let file = JournalFile {
            source: source.clone(),
            staged: PathBuf::new(),
            destination: temporary.path().join("destination.flac"),
            content_hash: Some("unreachable".into()),
            sha256: None,
            source_identity: Some("unreachable".into()),
            owned_identity: None,
            role: "track".into(),
            state: "finalized".into(),
        };

        assert!(cleanup_move_source(&operation, 0, &file).is_err());
        assert!(source.is_symlink());
        assert!(!move_cleanup_path(&source, &operation, 0)?.is_symlink());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn quarantine_restore_rejects_a_dangling_newcomer_before_rename() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let staged = temporary.path().join("quarantine.flac");
        std::fs::write(&staged, b"owned bytes")?;
        symlink(temporary.path().join("missing-target"), &source)?;
        let metadata = std::fs::metadata(&staged)?;
        let digests = digest_file(&staged)?;
        let file = JournalFile {
            source: source.clone(),
            staged: staged.clone(),
            destination: source.clone(),
            content_hash: Some(digests.blake3().into()),
            sha256: Some(digests.sha256().into()),
            source_identity: Some(file_identity(&metadata)),
            owned_identity: Some(file_identity(&metadata)),
            role: "track".into(),
            state: "quarantined".into(),
        };

        assert!(matches!(
            restore_quarantined(&file),
            Err(Error::Recovery(detail)) if detail.contains("destination already exists")
        ));
        assert!(source.is_symlink());
        assert_eq!(std::fs::read(staged)?, b"owned bytes");
        Ok(())
    }
}
