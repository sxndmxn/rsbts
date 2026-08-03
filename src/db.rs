//! SQLite-backed library, audit, journal, and recovery APIs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::types::{Type, Value};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};

use crate::migrations::{self, MigrationReport};
use crate::query::Query;
use crate::{
    validate_item_metadata, Album, AudioFormat, Error, ExtendedMetadata, ExternalId, FlexibleValue,
    Item, Result,
};

pub struct Library {
    conn: Connection,
    path: Option<PathBuf>,
    migration_report: MigrationReport,
}

/// Validate `field=value` modifications without opening a library or changing any rows.
pub fn validate_modification_fields(fields: &[String]) -> Result<()> {
    parse_modifications(fields).map(|_modifications| ())
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub tracks: u64,
    pub albums: u64,
    pub artists: u64,
    pub total_length: f64,
    pub total_size: u64,
    pub unknown_sizes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Default)]
pub struct AuditReport {
    pub issues: Vec<AuditIssue>,
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    pub recovered_operations: Vec<String>,
    pub unresolved: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationKind {
    ImportCopy,
    ImportMove,
    ImportLink,
    ImportInPlace,
    TagWrite,
    RemoveDelete,
}

impl OperationKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ImportCopy => "import-copy",
            Self::ImportMove => "import-move",
            Self::ImportLink => "import-link",
            Self::ImportInPlace => "import-in-place",
            Self::TagWrite => "tag-write",
            Self::RemoveDelete => "remove-delete",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "import-copy" => Ok(Self::ImportCopy),
            "import-move" => Ok(Self::ImportMove),
            "import-link" => Ok(Self::ImportLink),
            "import-in-place" => Ok(Self::ImportInPlace),
            "tag-write" => Ok(Self::TagWrite),
            "remove-delete" => Ok(Self::RemoveDelete),
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
    pub source_identity: Option<String>,
    pub owned_identity: Option<String>,
    pub role: String,
    pub state: String,
}

#[derive(Debug)]
struct PendingOperation {
    id: String,
    kind: OperationKind,
    state: String,
    files: Vec<JournalFile>,
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        configure_connection(&conn)?;
        let migration_report = migrations::run_migrations(&mut conn, Some(path))?;
        let needs_size_backfill = migration_report.from_version < 2;
        let mut library = Self {
            conn,
            path: Some(path.to_path_buf()),
            migration_report,
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
    pub fn open_snapshot(path: &Path) -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        if path.exists() || path.is_symlink() {
            let source =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            configure_connection(&source)?;
            {
                let backup = rusqlite::backup::Backup::new(&source, &mut conn)?;
                backup.run_to_completion(100, Duration::from_millis(5), None)?;
            }
        }
        configure_connection(&conn)?;
        let migration_report = migrations::run_migrations(&mut conn, None)?;
        let needs_size_backfill = migration_report.from_version < 2;
        let mut library = Self {
            conn,
            path: None,
            migration_report,
        };
        if needs_size_backfill {
            library.backfill_file_sizes()?;
        }
        Ok(library)
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        configure_connection(&conn)?;
        let migration_report = migrations::run_migrations(&mut conn, None)?;
        Ok(Self {
            conn,
            path: None,
            migration_report,
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
        let mut issues = Vec::new();
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
        Ok(AuditReport { issues })
    }

    /// Reconcile journaled filesystem operations. This method is explicit for library callers.
    pub fn recover_pending(&mut self) -> Result<RecoveryReport> {
        let operations = self.pending_operations()?;
        let mut report = RecoveryReport::default();
        for operation in operations {
            match recover_operation(&operation) {
                Ok(()) => {
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
        let mut items = stmt
            .query_map(params_from_iter(compiled.parameters.iter()), row_to_item)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for item in &mut items {
            if let Some(id) = item.id {
                item.extended = load_extended_metadata(&self.conn, "item", id)?;
            }
        }
        Ok(items)
    }

    pub fn query_albums(&self, search: Option<&str>) -> Result<Vec<Album>> {
        match search {
            None => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT * FROM albums ORDER BY albumartist, year, album")?;
                let mut albums = stmt
                    .query_map([], row_to_album)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                drop(stmt);
                hydrate_albums(&self.conn, &mut albums)?;
                Ok(albums)
            }
            Some(search) => {
                let pattern = format!("%{}%", escape_like(search));
                let mut stmt = self.conn.prepare(
                    "SELECT * FROM albums
                     WHERE album LIKE ?1 ESCAPE '!' OR albumartist LIKE ?1 ESCAPE '!'
                     ORDER BY albumartist, year, album",
                )?;
                let mut albums = stmt
                    .query_map([pattern], row_to_album)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                drop(stmt);
                hydrate_albums(&self.conn, &mut albums)?;
                Ok(albums)
            }
        }
    }

    pub fn stats(&self) -> Result<Stats> {
        self.conn
            .query_row(
                "SELECT
                    COUNT(*),
                    COUNT(DISTINCT album_id),
                    COUNT(DISTINCT artist),
                    COALESCE(SUM(length), 0),
                    COALESCE(SUM(file_size), 0),
                    COALESCE(SUM(CASE WHEN file_size IS NULL THEN 1 ELSE 0 END), 0)
                 FROM items",
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
            let (track_identity_changed, release_identity_changed): (bool, bool) = transaction
                .query_row(
                    "SELECT
                        NOT (title IS ?1 AND artist IS ?2 AND track IS ?7 AND disc IS ?8),
                        NOT (album IS ?3
                             AND COALESCE(albumartist, artist) IS COALESCE(?4, ?2)
                             AND year IS ?6)
                     FROM items WHERE id = ?14",
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
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
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
            invalidate_external_ids(
                &transaction,
                *id,
                track_identity_changed,
                release_identity_changed,
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
            let invalidate_release_ids = invalidates_release_identity
                || (modifies_artist
                    && transaction.query_row(
                        "SELECT albumartist IS NULL FROM items WHERE id = ?1",
                        [id],
                        |row| row.get(0),
                    )?);
            invalidate_external_ids(
                &transaction,
                *id,
                invalidates_track_identity,
                invalidate_release_ids,
            )?;
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

    /// Import a complete, already validated external-library snapshot atomically.
    pub fn import_migrated_groups(&mut self, groups: &[(Option<Album>, Vec<Item>)]) -> Result<()> {
        let existing: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))?;
        if existing != 0 {
            return Err(Error::Import(
                "Beets migration requires an empty destination library".into(),
            ));
        }
        for (album, items) in groups {
            if let Some(album) = album {
                crate::validate_album_metadata(album)?;
            }
            for item in items {
                validate_item_metadata(item)?;
            }
        }
        let transaction = self.conn.transaction()?;
        for (album, items) in groups {
            let album_id = album
                .as_ref()
                .map(|album| find_or_insert_album(&transaction, album))
                .transpose()?;
            for item in items {
                insert_item(&transaction, item, album_id)?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn create_operation(
        &self,
        kind: OperationKind,
        files: &[JournalFile],
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO operation_journal (id, kind, state, created_at, updated_at)
             VALUES (?1, ?2, 'prepared', ?3, ?3)",
            params![id, kind.as_str(), now],
        )?;
        for (ordinal, file) in files.iter().enumerate() {
            let source = path_to_storage(&file.source)?;
            let staged = path_to_storage(&file.staged)?;
            let destination = path_to_storage(&file.destination)?;
            transaction.execute(
                "INSERT INTO operation_files
                 (operation_id, ordinal, source_path, staged_path, destination_path,
                  content_hash, source_identity, owned_identity, role, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'prepared')",
                params![
                    id,
                    ordinal,
                    source,
                    staged,
                    destination,
                    file.content_hash,
                    file.source_identity,
                    file.owned_identity,
                    file.role,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(id)
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
        require_journal_row(changed, "operation state")
    }

    pub(crate) fn set_file_state(&self, id: &str, ordinal: usize, state: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operation_files SET state = ?1
             WHERE operation_id = ?2 AND ordinal = ?3",
            params![state, id, ordinal],
        )?;
        require_journal_row(changed, "operation file state")
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
        require_journal_row(changed, "staged file identity")
    }

    pub(crate) fn commit_import(
        &mut self,
        operation_id: &str,
        album: Option<&Album>,
        items: &[Item],
    ) -> Result<i64> {
        let transaction = self.conn.transaction()?;
        let album_id = album
            .map(|album| find_or_insert_album(&transaction, album))
            .transpose()?;
        for item in items {
            insert_item(&transaction, item, album_id)?;
        }
        let changed = transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), operation_id],
        )?;
        require_journal_row(changed, "import commit")?;
        transaction.commit()?;
        Ok(album_id.unwrap_or(0))
    }

    pub(crate) fn commit_removal(
        &mut self,
        operation_id: Option<&str>,
        items: &[(i64, &Path)],
    ) -> Result<()> {
        let transaction = self.conn.transaction()?;
        for (id, path) in items {
            let stored_path = path_to_storage(path)?;
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
            "DELETE FROM albums WHERE NOT EXISTS(
                SELECT 1 FROM items WHERE items.album_id = albums.id
            )",
            [],
        )?;
        cleanup_orphan_metadata(&transaction)?;
        if let Some(operation_id) = operation_id {
            let changed = transaction.execute(
                "UPDATE operation_journal SET state = 'db-committed', updated_at = ?1
                 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), operation_id],
            )?;
            require_journal_row(changed, "removal commit")?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn commit_tag_write(
        &mut self,
        operation_id: &str,
        item_id: i64,
        path: &Path,
        file_size: u64,
        modified: DateTime<Utc>,
    ) -> Result<()> {
        let stored_path = path_to_storage(path)?;
        let transaction = self.conn.transaction()?;
        if transaction.execute(
            "UPDATE items SET file_size = ?1, mtime = ?2
             WHERE id = ?3 AND path = ?4",
            params![file_size, modified.to_rfc3339(), item_id, stored_path],
        )? != 1
        {
            return Err(Error::Import(format!(
                "tag-write plan is stale for {}; no row was updated",
                path.display()
            )));
        }
        let changed = transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), operation_id],
        )?;
        require_journal_row(changed, "tag-write commit")?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn commit_path_move(
        &mut self,
        operation_id: &str,
        item_id: i64,
        source: &Path,
        destination: &Path,
    ) -> Result<()> {
        let source = path_to_storage(source)?;
        let destination = path_to_storage(destination)?;
        let transaction = self.conn.transaction()?;
        if transaction.execute(
            "UPDATE items SET path = ?1 WHERE id = ?2 AND path = ?3",
            params![destination, item_id, source],
        )? != 1
        {
            return Err(Error::Import(
                "move plan is stale; no database row was updated".into(),
            ));
        }
        let changed = transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), operation_id],
        )?;
        require_journal_row(changed, "move commit")?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn complete_operation(&self, id: &str) -> Result<()> {
        let changed = self
            .conn
            .execute("DELETE FROM operation_journal WHERE id = ?1", [id])?;
        require_journal_row(changed, "operation completion")
    }

    fn pending_operations(&self) -> Result<Vec<PendingOperation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, state FROM operation_journal
             WHERE state != 'complete' ORDER BY created_at, id",
        )?;
        let headers = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut operations = Vec::with_capacity(headers.len());
        for (id, kind, state) in headers {
            let mut file_stmt = self.conn.prepare(
                "SELECT source_path, staged_path, destination_path, content_hash,
                        source_identity, owned_identity, role, state
                 FROM operation_files WHERE operation_id = ?1 ORDER BY ordinal",
            )?;
            let files = file_stmt
                .query_map([&id], |row| {
                    Ok(JournalFile {
                        source: PathBuf::from(row.get::<_, String>(0)?),
                        staged: PathBuf::from(row.get::<_, String>(1)?),
                        destination: PathBuf::from(row.get::<_, String>(2)?),
                        content_hash: row.get(3)?,
                        source_identity: row.get(4)?,
                        owned_identity: row.get(5)?,
                        role: row.get(6)?,
                        state: row.get(7)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            operations.push(PendingOperation {
                id,
                kind: OperationKind::parse(&kind)?,
                state,
                files,
            });
        }
        Ok(operations)
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
        let (current_album_id, album_name, albumartist, year, added, singleton): (
            Option<i64>,
            String,
            String,
            Option<i32>,
            String,
            bool,
        ) = transaction.query_row(
            "SELECT album_id, album, COALESCE(albumartist, artist), year, added, singleton
             FROM items WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        if singleton {
            if current_album_id.is_some() {
                transaction.execute("UPDATE items SET album_id = NULL WHERE id = ?1", [id])?;
            }
            continue;
        }
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

        let album_id = transaction
            .query_row(
                "SELECT id FROM albums
                 WHERE album = ?1 AND albumartist = ?2 AND year IS ?3 LIMIT 1",
                params![album_name, albumartist, year],
                |row| row.get(0),
            )
            .optional()?
            .map_or_else(
                || {
                    let album = Album {
                        id: None,
                        album: album_name,
                        albumartist,
                        year,
                        artpath: None,
                        external_id: None,
                        added: parse_datetime(&added)?,
                        extended: crate::ExtendedMetadata::default(),
                    };
                    find_or_insert_album(transaction, &album)
                },
                Ok,
            )?;
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
    cleanup_orphan_metadata(transaction)?;
    Ok(())
}

fn invalidate_external_ids(
    transaction: &Transaction<'_>,
    item_id: i64,
    track_identity: bool,
    release_identity: bool,
) -> Result<()> {
    if track_identity {
        transaction.execute(
            "DELETE FROM external_ids
             WHERE entity_type = 'item' AND entity_id = ?1
               AND kind IN ('recording', 'release_track')",
            [item_id],
        )?;
    }
    if release_identity {
        transaction.execute(
            "DELETE FROM external_ids
             WHERE entity_type = 'item' AND entity_id = ?1 AND kind = 'release'",
            [item_id],
        )?;
    }
    Ok(())
}

fn cleanup_orphan_metadata(transaction: &Transaction<'_>) -> Result<()> {
    for (table, entity_type, entity_table) in [
        ("entity_metadata", "item", "items"),
        ("entity_metadata", "album", "albums"),
        ("external_ids", "item", "items"),
        ("external_ids", "album", "albums"),
    ] {
        let sql = format!(
            "DELETE FROM {table}
             WHERE entity_type = ?1
               AND NOT EXISTS (SELECT 1 FROM {entity_table}
                               WHERE {entity_table}.id = {table}.entity_id)"
        );
        transaction.execute(&sql, [entity_type])?;
    }
    Ok(())
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.create_scalar_function(
        "regexp",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let pattern = context.get::<String>(0)?;
            let value = context.get::<Option<String>>(1)?;
            regex::Regex::new(&pattern)
                .map(|expression| value.is_some_and(|value| expression.is_match(&value)))
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
        },
    )?;
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
        save_extended_metadata(transaction, "album", id, &album.extended)?;
        save_external_ids(
            transaction,
            "album",
            id,
            album
                .extended
                .external_ids
                .iter()
                .chain(album.external_id.iter()),
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
    let id = transaction.last_insert_rowid();
    save_extended_metadata(transaction, "album", id, &album.extended)?;
    save_external_ids(
        transaction,
        "album",
        id,
        album
            .extended
            .external_ids
            .iter()
            .chain(album.external_id.iter()),
    )?;
    Ok(id)
}

fn insert_item(transaction: &Transaction<'_>, item: &Item, album_id: Option<i64>) -> Result<i64> {
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
          metadata_provider, external_track_id, external_release_id, singleton)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
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
            item.singleton,
        ],
    )?;
    let id = transaction.last_insert_rowid();
    save_extended_metadata(transaction, "item", id, &item.extended)?;
    save_external_ids(
        transaction,
        "item",
        id,
        item.extended.external_ids.iter().chain(
            item.track_external_id
                .iter()
                .chain(item.release_external_id.iter()),
        ),
    )?;
    Ok(id)
}

fn hydrate_albums(conn: &Connection, albums: &mut [Album]) -> Result<()> {
    for album in albums {
        if let Some(id) = album.id {
            album.extended = load_extended_metadata(conn, "album", id)?;
        }
    }
    Ok(())
}

fn save_extended_metadata(
    transaction: &Transaction<'_>,
    entity_type: &str,
    entity_id: i64,
    metadata: &ExtendedMetadata,
) -> Result<()> {
    transaction.execute(
        "DELETE FROM entity_metadata WHERE entity_type = ?1 AND entity_id = ?2",
        params![entity_type, entity_id],
    )?;
    let core = serde_json::to_string(metadata).map_err(|error| {
        Error::Database(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
    })?;
    transaction.execute(
        "INSERT INTO entity_metadata
         (entity_type, entity_id, field, ordinal, value_type, value_json)
         VALUES (?1, ?2, '__core', 0, 'string', ?3)",
        params![entity_type, entity_id, core],
    )?;
    for (field, value) in &metadata.flexible_fields {
        validate_flexible_field_name(field)?;
        let json = serde_json::to_string(value).map_err(|error| {
            Error::Database(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        transaction.execute(
            "INSERT INTO entity_metadata
             (entity_type, entity_id, field, ordinal, value_type, value_json)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![
                entity_type,
                entity_id,
                field,
                flexible_value_type(value),
                json
            ],
        )?;
    }
    Ok(())
}

fn save_external_ids<'a>(
    transaction: &Transaction<'_>,
    entity_type: &str,
    entity_id: i64,
    ids: impl Iterator<Item = &'a ExternalId>,
) -> Result<()> {
    transaction.execute(
        "DELETE FROM external_ids WHERE entity_type = ?1 AND entity_id = ?2",
        params![entity_type, entity_id],
    )?;
    let mut seen = HashSet::new();
    for id in ids {
        if id.provider.trim().is_empty() || id.value.trim().is_empty() {
            return Err(Error::Import(
                "external ID provider and value cannot be empty".into(),
            ));
        }
        let kind = if id.kind.is_empty() {
            "unknown"
        } else {
            &id.kind
        };
        if seen.insert((id.provider.as_str(), kind, id.value.as_str())) {
            transaction.execute(
                "INSERT INTO external_ids
                 (entity_type, entity_id, provider, kind, value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![entity_type, entity_id, id.provider, kind, id.value],
            )?;
        }
    }
    Ok(())
}

fn load_extended_metadata(
    conn: &Connection,
    entity_type: &str,
    entity_id: i64,
) -> Result<ExtendedMetadata> {
    let core = conn
        .query_row(
            "SELECT value_json FROM entity_metadata
             WHERE entity_type = ?1 AND entity_id = ?2 AND field = '__core'",
            params![entity_type, entity_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let mut metadata = core.map_or_else(
        || Ok(ExtendedMetadata::default()),
        |json| {
            serde_json::from_str(&json)
                .map_err(|error| Error::Recovery(format!("invalid stored metadata: {error}")))
        },
    )?;
    let mut statement = conn.prepare(
        "SELECT field, value_json FROM entity_metadata
         WHERE entity_type = ?1 AND entity_id = ?2 AND field != '__core'
         ORDER BY field, ordinal",
    )?;
    let rows = statement.query_map(params![entity_type, entity_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (field, json) = row?;
        let value = serde_json::from_str::<FlexibleValue>(&json)
            .map_err(|error| Error::Recovery(format!("invalid flexible field {field}: {error}")))?;
        metadata.flexible_fields.insert(field, value);
    }
    let mut statement = conn.prepare(
        "SELECT provider, kind, value FROM external_ids
         WHERE entity_type = ?1 AND entity_id = ?2 ORDER BY provider, kind, value",
    )?;
    metadata.external_ids = statement
        .query_map(params![entity_type, entity_id], |row| {
            Ok(ExternalId {
                provider: row.get(0)?,
                kind: row.get(1)?,
                value: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(metadata)
}

const fn flexible_value_type(value: &FlexibleValue) -> &'static str {
    match value {
        FlexibleValue::String(_) => "string",
        FlexibleValue::Integer(_) => "integer",
        FlexibleValue::Float(_) => "float",
        FlexibleValue::Boolean(_) => "boolean",
        FlexibleValue::Date(_) => "date",
        FlexibleValue::StringList(_) => "string_list",
    }
}

fn validate_flexible_field_name(field: &str) -> Result<()> {
    if field.is_empty()
        || field == "__core"
        || !field
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        Err(Error::Query(format!(
            "invalid flexible field name: {field}"
        )))
    } else {
        Ok(())
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
        singleton: row.get::<_, bool>("singleton")?,
        extended: crate::ExtendedMetadata::default(),
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
        extended: crate::ExtendedMetadata::default(),
    })
}

fn external_id(provider: Option<&str>, value: Option<String>) -> Option<ExternalId> {
    provider.zip(value).map(|(provider, value)| ExternalId {
        provider: provider.to_string(),
        kind: String::new(),
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

fn recover_operation(operation: &PendingOperation) -> Result<()> {
    if operation.state == "recovery-required" {
        return Err(Error::Recovery(
            "legacy journal entry has an ambiguous commit phase; refusing automatic recovery"
                .into(),
        ));
    }
    let committed = operation.state == "db-committed" || operation.state == "cleanup-pending";
    if operation.kind == OperationKind::TagWrite {
        for file in &operation.files {
            recover_tag_write(file, committed)?;
        }
        return Ok(());
    }
    for file in &operation.files {
        match (operation.kind, committed) {
            (OperationKind::RemoveDelete, false) => restore_quarantined(file)?,
            (OperationKind::RemoveDelete, true) => {
                if file.staged.exists() || file.staged.is_symlink() {
                    verify_source_identity(&file.staged, file)?;
                }
                remove_if_owned(&file.staged, file)?;
            }
            (OperationKind::ImportMove, true) if file.role == "track" => {
                verify_owned(&file.destination, file)?;
                if file.source.exists() || file.source.is_symlink() {
                    verify_source_identity(&file.source, file)?;
                    verify_content_hash(&file.source, file)?;
                    remove_file_synced(&file.source)?;
                }
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
            (OperationKind::ImportInPlace | OperationKind::TagWrite, _) => {}
        }
    }
    Ok(())
}

fn recover_tag_write(file: &JournalFile, committed: bool) -> Result<()> {
    let original_exists = file.source.exists() || file.source.is_symlink();
    let backup_exists = file.staged.exists() || file.staged.is_symlink();
    let rewritten_exists = file.destination.exists() || file.destination.is_symlink();
    if committed {
        if !original_exists && rewritten_exists {
            verify_owned(&file.destination, file)?;
            std::fs::rename(&file.destination, &file.source)?;
            if let Some(parent) = file.source.parent() {
                sync_directory(parent)?;
            }
        } else if rewritten_exists {
            remove_if_owned(&file.destination, file)?;
        }
        if backup_exists {
            verify_source_identity(&file.staged, file)?;
            remove_file_synced(&file.staged)?;
        }
        return Ok(());
    }

    if original_exists && backup_exists && same_entry(&file.source, &file.staged)? {
        verify_source_identity(&file.source, file)?;
        remove_file_synced(&file.staged)?;
        if rewritten_exists {
            remove_if_owned(&file.destination, file)?;
        }
        return Ok(());
    }
    if original_exists && backup_exists {
        verify_owned(&file.source, file)?;
        remove_file_synced(&file.source)?;
    }
    if backup_exists {
        verify_source_identity(&file.staged, file)?;
        std::fs::rename(&file.staged, &file.source)?;
        if let Some(parent) = file.source.parent() {
            sync_directory(parent)?;
        }
    }
    if rewritten_exists {
        remove_if_owned(&file.destination, file)?;
    }
    Ok(())
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
    std::fs::rename(&file.staged, &file.source)?;
    if let Some(parent) = file.source.parent() {
        sync_directory(parent)?;
    }
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

fn rollback_import_paths(
    file: &JournalFile,
    remove_owned: fn(&Path, &JournalFile) -> Result<()>,
) -> Result<()> {
    let staged_exists = file.staged.exists() || file.staged.is_symlink();
    let destination_exists = file.destination.exists() || file.destination.is_symlink();
    if file.state == "prepared" {
        return remove_owned(&file.staged, file);
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
    if actual == expected {
        Ok(())
    } else {
        Err(Error::Recovery(format!(
            "refusing to touch changed journal path {}",
            path.display()
        )))
    }
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

#[cfg(not(unix))]
pub(crate) fn file_identity(metadata: &std::fs::Metadata) -> String {
    format!("{}:{:?}", metadata.len(), metadata.modified().ok())
}

pub(crate) fn remove_file_synced(path: &Path) -> Result<()> {
    std::fs::remove_file(path)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
#[doc(hidden)]
pub fn sync_directory(path: &Path) -> Result<()> {
    match std::fs::File::open(path).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
#[doc(hidden)]
pub const fn sync_directory(_path: &Path) -> Result<()> {
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
            extended: crate::ExtendedMetadata::default(),
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
            singleton: false,
            extended: crate::ExtendedMetadata::default(),
        };
        library.commit_import(&operation, Some(&album), &[item])?;
        library.complete_operation(&operation)?;
        let audit = library.audit()?;
        assert!(matches!(
            audit.issues.first(),
            Some(AuditIssue::MissingFile { .. })
        ));
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
            extended: crate::ExtendedMetadata::default(),
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
            singleton: false,
            extended: crate::ExtendedMetadata::default(),
        };
        library.commit_import(&operation, Some(&album), &[item])?;
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
    fn committed_removal_recovery_deletes_the_quarantine() -> Result<()> {
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
        assert!(!staged.exists());
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
                kind: "release".into(),
                value: "release".into(),
            }),
            added: Utc::now(),
            extended: crate::ExtendedMetadata::default(),
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
                kind: "recording".into(),
                value: "track".into(),
            }),
            release_external_id: Some(ExternalId {
                provider: "musicbrainz".into(),
                kind: "release".into(),
                value: "release".into(),
            }),
            added: Utc::now(),
            mtime: Utc::now(),
            singleton: false,
            extended: crate::ExtendedMetadata::default(),
        };
        library.commit_import(&operation, Some(&album), &[item])?;
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
}
