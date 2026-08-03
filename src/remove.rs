//! Plan-first, invocation-atomic library removal.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::db::{
    file_identity, journal_hash_path, remove_file_synced, sync_directory, JournalFile, Library,
    OperationKind,
};
use crate::query::Query;
use crate::{Error, Item, Result};

#[derive(Debug, Clone)]
pub struct RemovalPlan {
    pub items: Vec<Item>,
    pub delete_files: bool,
    pub missing_files: Vec<PathBuf>,
    pub files: Vec<PlannedRemovalFile>,
}

#[derive(Debug, Clone)]
pub struct PlannedRemovalFile {
    pub path: PathBuf,
    pub content_hash: String,
    pub source_identity: String,
    pub role: String,
}

impl RemovalPlan {
    pub fn build(library: &Library, query: &Query, delete_files: bool) -> Result<Self> {
        let items = library.query_items(query)?;
        let mut missing_files = Vec::new();
        let mut files = Vec::new();
        if delete_files {
            for item in &items {
                if !item.path.exists() && !item.path.is_symlink() {
                    missing_files.push(item.path.clone());
                } else {
                    files.push(fingerprint_path(&item.path)?);
                }
            }
        } else {
            missing_files.extend(
                items
                    .iter()
                    .filter(|item| !item.path.exists() && !item.path.is_symlink())
                    .map(|item| item.path.clone()),
            );
        }
        Ok(Self {
            items,
            delete_files,
            missing_files,
            files,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct RemovalReport {
    pub removed_rows: usize,
    pub deleted_files: usize,
    pub missing_files: Vec<PathBuf>,
    pub cleanup_recovered: bool,
}

pub struct RemovalExecutor<'a> {
    library: &'a mut Library,
}

impl<'a> RemovalExecutor<'a> {
    pub const fn new(library: &'a mut Library) -> Self {
        Self { library }
    }

    pub fn execute(&mut self, plan: RemovalPlan) -> Result<RemovalReport> {
        validate_plan_shape(&plan)?;
        let database_items = plan
            .items
            .iter()
            .map(|item| {
                item.id.map(|id| (id, item.path.as_path())).ok_or_else(|| {
                    Error::Import("cannot remove an item without a database ID".into())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if database_items
            .iter()
            .map(|(id, _path)| id)
            .collect::<HashSet<_>>()
            .len()
            != database_items.len()
        {
            return Err(Error::Import(
                "removal plan contains duplicate database IDs".into(),
            ));
        }
        if database_items.is_empty() {
            return Ok(RemovalReport::default());
        }
        if plan.delete_files {
            for path in &plan.missing_files {
                if path.exists() || path.is_symlink() {
                    return Err(Error::Import(format!(
                        "file appeared after removal planning; preserving its row: {}",
                        path.display()
                    )));
                }
            }
        }
        if !plan.delete_files {
            self.library.commit_removal(None, &database_items)?;
            return Ok(RemovalReport {
                removed_rows: database_items.len(),
                missing_files: plan.missing_files,
                ..RemovalReport::default()
            });
        }

        let operation_uuid = uuid::Uuid::new_v4();
        for file in &plan.files {
            validate_planned_file(file)?;
        }
        let existing = plan
            .files
            .iter()
            .map(|file| {
                Ok(JournalFile {
                    source: file.path.clone(),
                    staged: quarantine_path(&file.path, operation_uuid)?,
                    destination: file.path.clone(),
                    content_hash: Some(file.content_hash.clone()),
                    source_identity: Some(file.source_identity.clone()),
                    owned_identity: Some(file.source_identity.clone()),
                    role: file.role.clone(),
                    state: "prepared".into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let operation_id = self
            .library
            .create_operation(OperationKind::RemoveDelete, &existing)?;

        if let Err(error) = self.quarantine(&operation_id, &existing) {
            return Err(self.rollback_failed(&operation_id, error));
        }
        if let Err(error) = self
            .library
            .commit_removal(Some(&operation_id), &database_items)
        {
            return Err(self.rollback_failed(&operation_id, error));
        }

        let cleanup_recovered = if let Err(error) = delete_quarantined(&existing) {
            self.library.set_operation_state(
                &operation_id,
                "cleanup-pending",
                Some(&error.to_string()),
            )?;
            let recovery = self.library.recover_pending()?;
            if !recovery.unresolved.is_empty() {
                return Err(Error::Recovery(recovery.unresolved.join("; ")));
            }
            true
        } else {
            self.library.complete_operation(&operation_id)?;
            false
        };

        Ok(RemovalReport {
            removed_rows: database_items.len(),
            deleted_files: existing.len(),
            missing_files: plan.missing_files,
            cleanup_recovered,
        })
    }

    fn quarantine(&self, operation_id: &str, files: &[JournalFile]) -> Result<()> {
        self.library
            .set_operation_state(operation_id, "staging", None)?;
        for (ordinal, file) in files.iter().enumerate() {
            if file.staged.exists() || file.staged.is_symlink() {
                return Err(Error::Import(format!(
                    "quarantine path already exists: {}",
                    file.staged.display()
                )));
            }
            validate_journal_path(&file.source, file)?;
            std::fs::hard_link(&file.source, &file.staged)?;
            validate_journal_path(&file.staged, file)?;
            validate_journal_path(&file.source, file)?;
            if let Some(parent) = file.staged.parent() {
                sync_directory(parent)?;
            }
            remove_file_synced(&file.source)?;
            self.library
                .set_file_state(operation_id, ordinal, "quarantined")?;
        }
        Ok(())
    }

    fn rollback_failed(&mut self, operation_id: &str, error: Error) -> Error {
        let _ = self
            .library
            .set_operation_state(operation_id, "failed", Some(&error.to_string()));
        match self.library.recover_pending() {
            Ok(report) if report.unresolved.is_empty() => error,
            Ok(report) => Error::Recovery(format!(
                "{error}; automatic rollback needs attention: {}",
                report.unresolved.join("; ")
            )),
            Err(recovery_error) => Error::Recovery(format!(
                "{error}; automatic rollback failed: {recovery_error}"
            )),
        }
    }
}

fn validate_plan_shape(plan: &RemovalPlan) -> Result<()> {
    let item_paths = plan
        .items
        .iter()
        .map(|item| item.path.as_path())
        .collect::<HashSet<_>>();
    if item_paths.len() != plan.items.len() {
        return Err(Error::Import(
            "removal plan contains duplicate item paths".into(),
        ));
    }
    let file_paths = plan
        .files
        .iter()
        .map(|file| file.path.as_path())
        .collect::<HashSet<_>>();
    let missing_paths = plan
        .missing_files
        .iter()
        .map(PathBuf::as_path)
        .collect::<HashSet<_>>();
    if file_paths.len() != plan.files.len() || missing_paths.len() != plan.missing_files.len() {
        return Err(Error::Import(
            "removal plan contains duplicate file paths".into(),
        ));
    }
    if !file_paths.is_disjoint(&missing_paths)
        || !file_paths.is_subset(&item_paths)
        || !missing_paths.is_subset(&item_paths)
    {
        return Err(Error::Import(
            "removal plan files do not match its database items".into(),
        ));
    }
    if plan.delete_files && file_paths.len() + missing_paths.len() != item_paths.len() {
        return Err(Error::Import(
            "file-deleting removal plan does not classify every item".into(),
        ));
    }
    if !plan.delete_files && !file_paths.is_empty() {
        return Err(Error::Import(
            "database-only removal plan unexpectedly contains files".into(),
        ));
    }
    Ok(())
}

fn fingerprint_path(path: &Path) -> Result<PlannedRemovalFile> {
    let role = if path.is_symlink() {
        "symlink"
    } else {
        "track"
    };
    let metadata = std::fs::symlink_metadata(path)?;
    let source_identity = file_identity(&metadata);
    let content_hash = journal_hash_path(path, role)?;
    let after = std::fs::symlink_metadata(path)?;
    if file_identity(&after) != source_identity || path.is_symlink() != (role == "symlink") {
        return Err(Error::Import(format!(
            "file changed while planning removal: {}",
            path.display()
        )));
    }
    Ok(PlannedRemovalFile {
        path: path.to_path_buf(),
        content_hash,
        source_identity,
        role: role.into(),
    })
}

fn validate_planned_file(file: &PlannedRemovalFile) -> Result<()> {
    let journal = JournalFile {
        source: file.path.clone(),
        staged: PathBuf::new(),
        destination: file.path.clone(),
        content_hash: Some(file.content_hash.clone()),
        source_identity: Some(file.source_identity.clone()),
        owned_identity: Some(file.source_identity.clone()),
        role: file.role.clone(),
        state: "prepared".into(),
    };
    validate_journal_path(&file.path, &journal)
}

fn validate_journal_path(path: &Path, file: &JournalFile) -> Result<()> {
    let expected_identity = file.source_identity.as_deref().ok_or_else(|| {
        Error::Recovery(format!(
            "journal has no source identity for {}",
            path.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(path)?;
    let actual_hash = journal_hash_path(path, &file.role)?;
    if file_identity(&metadata) != expected_identity
        || file.content_hash.as_deref() != Some(actual_hash.as_str())
        || path.is_symlink() != (file.role == "symlink")
    {
        return Err(Error::Import(format!(
            "file changed after removal planning: {}",
            path.display()
        )));
    }
    Ok(())
}

fn quarantine_path(path: &Path, operation_id: uuid::Uuid) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::Import(format!("invalid file path: {}", path.display())))?
        .to_str()
        .ok_or_else(|| {
            Error::Import(format!(
                "filename is not valid UTF-8 and cannot be quarantined safely: {}",
                path.display()
            ))
        })?;
    Ok(path.with_file_name(format!(".{name}.rsbts-{operation_id}.delete")))
}

fn delete_quarantined(files: &[JournalFile]) -> Result<()> {
    for file in files {
        validate_journal_path(&file.staged, file)?;
        remove_file_synced(&file.staged)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::{Album, AudioFormat};

    fn library_with_item(path: &Path) -> Result<Library> {
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
            extended: crate::ExtendedMetadata::default(),
        };
        let item = Item {
            id: None,
            album_id: None,
            path: path.to_path_buf(),
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
            file_size: Some(5),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
            singleton: false,
            extended: crate::ExtendedMetadata::default(),
        };
        library.commit_import(&operation, Some(&album), &[item])?;
        library.complete_operation(&operation)?;
        Ok(library)
    }

    #[test]
    fn deletion_is_journaled_and_removes_rows() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let plan = RemovalPlan::build(&library, &Query::all(), true)?;
        let report = RemovalExecutor::new(&mut library).execute(plan)?;
        assert_eq!(report.removed_rows, 1);
        assert_eq!(report.deleted_files, 1);
        assert!(!path.exists());
        assert!(library.query_items(&Query::all())?.is_empty());
        Ok(())
    }

    #[test]
    fn deletion_preserves_an_identical_replacement_made_after_planning() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        let preserved_original = temporary.path().join("original.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let plan = RemovalPlan::build(&library, &Query::all(), true)?;
        std::fs::rename(&path, preserved_original)?;
        std::fs::write(&path, b"audio")?;

        assert!(RemovalExecutor::new(&mut library).execute(plan).is_err());

        assert_eq!(std::fs::read(path)?, b"audio");
        assert_eq!(library.query_items(&Query::all())?.len(), 1);
        Ok(())
    }

    #[test]
    fn deletion_preserves_a_file_that_appears_after_missing_file_planning() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        let mut library = library_with_item(&path)?;
        let plan = RemovalPlan::build(&library, &Query::all(), true)?;
        assert_eq!(plan.missing_files.as_slice(), std::slice::from_ref(&path));
        std::fs::write(&path, b"new audio")?;

        assert!(RemovalExecutor::new(&mut library).execute(plan).is_err());

        assert_eq!(std::fs::read(path)?, b"new audio");
        assert_eq!(library.query_items(&Query::all())?.len(), 1);
        Ok(())
    }
}
