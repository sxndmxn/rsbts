//! SQLite-backed library, audit, journal, and recovery APIs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};

use crate::migrations::{self, MigrationReport};
use crate::query::Query;
use crate::{Album, AudioFormat, Error, ExternalId, Item, Result};

pub struct Library {
    conn: Connection,
    path: Option<PathBuf>,
    migration_report: MigrationReport,
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
    MissingFile { item_id: i64, path: PathBuf },
    UnknownFileSize { item_id: i64, path: PathBuf },
    OrphanedItem { item_id: i64, album_id: i64 },
    MissingFtsRow { item_id: i64 },
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
    RemoveDelete,
}

impl OperationKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ImportCopy => "import-copy",
            Self::ImportMove => "import-move",
            Self::ImportLink => "import-link",
            Self::RemoveDelete => "remove-delete",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "import-copy" => Ok(Self::ImportCopy),
            "import-move" => Ok(Self::ImportMove),
            "import-link" => Ok(Self::ImportLink),
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
        let mut library = Self {
            conn,
            path: Some(path.to_path_buf()),
            migration_report,
        };
        library.backfill_file_sizes()?;
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
            .prepare("SELECT id, path, file_size FROM items ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
                row.get::<_, Option<u64>>(2)?,
            ))
        })?;
        for row in rows {
            let (item_id, path, file_size) = row?;
            if !path.exists() {
                issues.push(AuditIssue::MissingFile { item_id, path });
            } else if file_size.is_none() {
                issues.push(AuditIssue::UnknownFileSize { item_id, path });
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

        let mut stmt = self.conn.prepare(
            "SELECT i.id FROM items i
             LEFT JOIN items_fts f ON f.rowid = i.id
             WHERE f.rowid IS NULL",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        for row in rows {
            issues.push(AuditIssue::MissingFtsRow { item_id: row? });
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
                    self.set_operation_state(&operation.id, "recovery-required", Some(&message))?;
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
                let pattern = format!("%{search}%");
                let mut stmt = self.conn.prepare(
                    "SELECT * FROM albums
                     WHERE album LIKE ?1 OR albumartist LIKE ?1
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
        self.conn.execute(
            "UPDATE items SET title=?1, artist=?2, album=?3, albumartist=?4, genre=?5,
             year=?6, track=?7, disc=?8, format=?9, bitrate=?10, length=?11,
             file_size=?12, mtime=?13 WHERE id=?14",
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
        )?;
        Ok(())
    }

    pub fn modify_item(&self, id: i64, fields: &[String]) -> Result<()> {
        for field in fields {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| Error::Query(format!("expected field=value: {field}")))?;
            let sql = match key {
                "title" => "UPDATE items SET title = ?1 WHERE id = ?2",
                "artist" => "UPDATE items SET artist = ?1 WHERE id = ?2",
                "album" => "UPDATE items SET album = ?1 WHERE id = ?2",
                "albumartist" => "UPDATE items SET albumartist = ?1 WHERE id = ?2",
                "genre" => "UPDATE items SET genre = ?1 WHERE id = ?2",
                "year" => "UPDATE items SET year = ?1 WHERE id = ?2",
                "track" => "UPDATE items SET track = ?1 WHERE id = ?2",
                "disc" => "UPDATE items SET disc = ?1 WHERE id = ?2",
                _ => return Err(Error::Query(format!("field cannot be modified: {key}"))),
            };
            self.conn.execute(sql, params![value, id])?;
        }
        Ok(())
    }

    pub(crate) fn item_exists(&self, path: &Path) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE path = ?1)",
                [path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .map_err(Into::into)
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
            transaction.execute(
                "INSERT INTO operation_files
                 (operation_id, ordinal, source_path, staged_path, destination_path,
                  content_hash, role, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'prepared')",
                params![
                    id,
                    ordinal,
                    file.source.to_string_lossy(),
                    file.staged.to_string_lossy(),
                    file.destination.to_string_lossy(),
                    file.content_hash,
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
        self.conn.execute(
            "UPDATE operation_journal
             SET state = ?1, updated_at = ?2, error = ?3 WHERE id = ?4",
            params![state, Utc::now().to_rfc3339(), error, id],
        )?;
        Ok(())
    }

    pub(crate) fn set_file_state(&self, id: &str, ordinal: usize, state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE operation_files SET state = ?1
             WHERE operation_id = ?2 AND ordinal = ?3",
            params![state, id, ordinal],
        )?;
        Ok(())
    }

    pub(crate) fn commit_import(
        &mut self,
        operation_id: &str,
        album: &Album,
        items: &[Item],
    ) -> Result<i64> {
        let transaction = self.conn.transaction()?;
        let album_id = find_or_insert_album(&transaction, album)?;
        for item in items {
            insert_item(&transaction, item, album_id)?;
        }
        transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), operation_id],
        )?;
        transaction.commit()?;
        Ok(album_id)
    }

    pub(crate) fn commit_removal(&mut self, operation_id: Option<&str>, ids: &[i64]) -> Result<()> {
        let transaction = self.conn.transaction()?;
        for id in ids {
            transaction.execute("DELETE FROM items WHERE id = ?1", [id])?;
        }
        transaction.execute(
            "DELETE FROM albums WHERE NOT EXISTS(
                SELECT 1 FROM items WHERE items.album_id = albums.id
            )",
            [],
        )?;
        if let Some(operation_id) = operation_id {
            transaction.execute(
                "UPDATE operation_journal SET state = 'db-committed', updated_at = ?1
                 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), operation_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn complete_operation(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM operation_journal WHERE id = ?1", [id])?;
        Ok(())
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
                "SELECT source_path, staged_path, destination_path, content_hash, role, state
                 FROM operation_files WHERE operation_id = ?1 ORDER BY ordinal",
            )?;
            let files = file_stmt
                .query_map([&id], |row| {
                    Ok(JournalFile {
                        source: PathBuf::from(row.get::<_, String>(0)?),
                        staged: PathBuf::from(row.get::<_, String>(1)?),
                        destination: PathBuf::from(row.get::<_, String>(2)?),
                        content_hash: row.get(3)?,
                        role: row.get(4)?,
                        state: row.get(5)?,
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

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(())
}

fn find_or_insert_album(transaction: &Transaction<'_>, album: &Album) -> Result<i64> {
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
            params![
                album.album,
                album.albumartist,
                album.year,
                album.artpath.as_ref().map(|path| path.to_string_lossy()),
                id,
            ],
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
            album.artpath.as_ref().map(|path| path.to_string_lossy()),
            musicbrainz_id(album.external_id.as_ref()),
            album.added.to_rfc3339(),
            provider,
            external_id,
        ],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn insert_item(transaction: &Transaction<'_>, item: &Item, album_id: i64) -> Result<i64> {
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
            item.path.to_string_lossy(),
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
        added: parse_datetime(&row.get::<_, String>("added")?),
        mtime: parse_datetime(&row.get::<_, String>("mtime")?),
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
        added: parse_datetime(&row.get::<_, String>("added")?),
    })
}

fn external_id(provider: Option<&str>, value: Option<String>) -> Option<ExternalId> {
    provider.zip(value).map(|(provider, value)| ExternalId {
        provider: provider.to_string(),
        value,
    })
}

fn parse_datetime(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .unwrap_or(DateTime::UNIX_EPOCH)
}

fn recover_operation(operation: &PendingOperation) -> Result<()> {
    let committed = operation.state == "db-committed" || operation.state == "cleanup-pending";
    for file in &operation.files {
        match (operation.kind, committed) {
            (OperationKind::RemoveDelete, false) => restore_quarantined(file)?,
            (OperationKind::RemoveDelete, true) => remove_if_owned(&file.staged, file)?,
            (OperationKind::ImportMove, true) if file.role == "track" => {
                verify_owned(&file.destination, file)?;
                if file.source.exists() {
                    std::fs::remove_file(&file.source)?;
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
        }
    }
    Ok(())
}

fn restore_quarantined(file: &JournalFile) -> Result<()> {
    if !file.staged.exists() {
        return Ok(());
    }
    if file.source.exists() || file.source.is_symlink() {
        if same_entry(&file.source, &file.staged)? {
            std::fs::remove_file(&file.staged)?;
            return Ok(());
        }
        return Err(Error::Recovery(format!(
            "cannot restore {}; destination already exists",
            file.source.display()
        )));
    }
    verify_owned(&file.staged, file)?;
    std::fs::rename(&file.staged, &file.source)?;
    Ok(())
}

fn remove_if_owned(path: &Path, file: &JournalFile) -> Result<()> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    verify_owned(path, file)?;
    std::fs::remove_file(path)?;
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
    std::fs::remove_file(path)?;
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
        let stats = library.stats()?;
        assert_eq!(stats.tracks, 0);
        assert_eq!(stats.unknown_sizes, 0);
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
}
