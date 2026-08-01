//! Plan-first, invocation-atomic library removal.

use std::path::{Path, PathBuf};

use crate::db::{journal_hash_path, JournalFile, Library, OperationKind};
use crate::query::Query;
use crate::{Error, Item, Result};

#[derive(Debug, Clone)]
pub struct RemovalPlan {
    pub items: Vec<Item>,
    pub delete_files: bool,
    pub missing_files: Vec<PathBuf>,
}

impl RemovalPlan {
    pub fn build(library: &Library, query: &Query, delete_files: bool) -> Result<Self> {
        let items = library.query_items(query)?;
        let missing_files = items
            .iter()
            .filter(|item| !item.path.exists() && !item.path.is_symlink())
            .map(|item| item.path.clone())
            .collect();
        Ok(Self {
            items,
            delete_files,
            missing_files,
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
        let ids = plan
            .items
            .iter()
            .filter_map(|item| item.id)
            .collect::<Vec<_>>();
        if ids.len() != plan.items.len() {
            return Err(Error::Import(
                "cannot remove items that do not have database IDs".into(),
            ));
        }
        if !plan.delete_files {
            self.library.commit_removal(None, &ids)?;
            return Ok(RemovalReport {
                removed_rows: ids.len(),
                missing_files: plan.missing_files,
                ..RemovalReport::default()
            });
        }

        let operation_uuid = uuid::Uuid::new_v4();
        let existing = plan
            .items
            .iter()
            .filter(|item| item.path.exists() || item.path.is_symlink())
            .map(|item| {
                let role = if item.path.is_symlink() {
                    "symlink"
                } else {
                    "track"
                };
                let content_hash = journal_hash_path(&item.path, role)?;
                Ok(JournalFile {
                    source: item.path.clone(),
                    staged: quarantine_path(&item.path, operation_uuid)?,
                    destination: item.path.clone(),
                    content_hash: Some(content_hash),
                    role: role.into(),
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
        if let Err(error) = self.library.commit_removal(Some(&operation_id), &ids) {
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
            removed_rows: ids.len(),
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
            let actual = journal_hash_path(&file.source, &file.role)?;
            if file.content_hash.as_deref() != Some(actual.as_str()) {
                return Err(Error::Import(format!(
                    "file changed after removal planning: {}",
                    file.source.display()
                )));
            }
            std::fs::hard_link(&file.source, &file.staged)?;
            std::fs::remove_file(&file.source)?;
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

fn quarantine_path(path: &Path, operation_id: uuid::Uuid) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::Import(format!("invalid file path: {}", path.display())))?
        .to_string_lossy();
    Ok(path.with_file_name(format!(".{name}.rsbts-{operation_id}.delete")))
}

fn delete_quarantined(files: &[JournalFile]) -> Result<()> {
    for file in files {
        if journal_hash_path(&file.staged, &file.role)?
            != file.content_hash.clone().unwrap_or_default()
        {
            return Err(Error::Recovery(format!(
                "quarantined file changed: {}",
                file.staged.display()
            )));
        }
        std::fs::remove_file(&file.staged)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::{Album, AudioFormat};

    #[test]
    fn deletion_is_journaled_and_removes_rows() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
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
            file_size: Some(5),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
        };
        library.commit_import(&operation, &album, &[item])?;
        library.complete_operation(&operation)?;
        let plan = RemovalPlan::build(&library, &Query::all(), true)?;
        let report = RemovalExecutor::new(&mut library).execute(plan)?;
        assert_eq!(report.removed_rows, 1);
        assert_eq!(report.deleted_files, 1);
        assert!(!path.exists());
        assert!(library.query_items(&Query::all())?.is_empty());
        Ok(())
    }
}
