//! Previewed reorganization of already-managed files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::db::{
    file_identity, hash_path, remove_file_synced, JournalFile, Library, OperationKind,
};
use crate::import::{stage_relocation, PlannedTrack, SourceFingerprint};
use crate::pathformat::format_relative_path;
use crate::query::Query;
use crate::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct MovePlan {
    pub tracks: Vec<PlannedTrack>,
}

impl MovePlan {
    pub fn build(
        library: &Library,
        query: &Query,
        library_dir: &Path,
        path_format: &str,
    ) -> Result<Self> {
        let mut destinations = HashMap::<PathBuf, PathBuf>::new();
        let mut tracks = Vec::new();
        for mut item in library.query_items(query)? {
            let source = item.path.clone();
            let metadata = std::fs::symlink_metadata(&source)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(Error::Import(format!(
                    "move requires a regular managed file: {}",
                    source.display()
                )));
            }
            let extension = source
                .extension()
                .ok_or_else(|| Error::Import(format!("missing extension: {}", source.display())))?;
            let relative = format_relative_path(path_format, &item)?;
            let destination = append_extension(&library_dir.join(relative), extension);
            if source == destination {
                continue;
            }
            if destination.exists() || destination.is_symlink() {
                return Err(Error::Import(format!(
                    "move destination already exists: {}",
                    destination.display()
                )));
            }
            if let Some(other) = destinations.insert(destination.clone(), source.clone()) {
                return Err(Error::Import(format!(
                    "move collision: {} and {} both map to {}",
                    other.display(),
                    source.display(),
                    destination.display()
                )));
            }
            let modified = metadata.modified()?;
            let content_hash = hash_path(&source)?;
            item.path.clone_from(&destination);
            tracks.push(PlannedTrack {
                source,
                destination,
                fingerprint: SourceFingerprint {
                    size: metadata.len(),
                    modified,
                    content_hash,
                    identity: file_identity(&metadata),
                },
                item,
                already_managed: false,
            });
        }
        Ok(Self { tracks })
    }
}

#[derive(Debug, Clone)]
pub struct MoveFailure {
    pub source: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct MoveReport {
    pub moved: usize,
    pub failures: Vec<MoveFailure>,
}

pub struct MoveExecutor<'a> {
    library: &'a mut Library,
    library_dir: PathBuf,
}

impl<'a> MoveExecutor<'a> {
    pub const fn new(library: &'a mut Library, library_dir: PathBuf) -> Self {
        Self {
            library,
            library_dir,
        }
    }

    pub fn execute(&mut self, plan: MovePlan) -> MoveReport {
        let mut report = MoveReport::default();
        for track in plan.tracks {
            match self.move_one(&track) {
                Ok(()) => report.moved += 1,
                Err(error) => report.failures.push(MoveFailure {
                    source: track.source,
                    error: error.to_string(),
                }),
            }
        }
        report
    }

    fn move_one(&mut self, track: &PlannedTrack) -> Result<()> {
        let item_id = track
            .item
            .id
            .ok_or_else(|| Error::Import("move item has no database ID".into()))?;
        let transfer_id = uuid::Uuid::new_v4();
        let staged = staging_path(&track.destination, transfer_id)?;
        let journal = JournalFile {
            source: track.source.clone(),
            staged,
            destination: track.destination.clone(),
            content_hash: Some(track.fingerprint.content_hash.clone()),
            source_identity: Some(track.fingerprint.identity.clone()),
            owned_identity: None,
            role: "track".into(),
            state: "prepared".into(),
        };
        let operation_id = self
            .library
            .create_operation(OperationKind::ImportMove, std::slice::from_ref(&journal))?;
        let mut committed = false;
        let result = (|| {
            stage_relocation(
                self.library,
                &operation_id,
                &self.library_dir,
                track,
                &journal,
            )?;
            self.library.commit_path_move(
                &operation_id,
                item_id,
                &track.source,
                &track.destination,
            )?;
            committed = true;
            self.library
                .set_operation_state(&operation_id, "cleanup-pending", None)?;
            let metadata = std::fs::metadata(&track.source)?;
            if file_identity(&metadata) != track.fingerprint.identity
                || hash_path(&track.source)? != track.fingerprint.content_hash
            {
                return Err(Error::Recovery(format!(
                    "move source changed before cleanup: {}",
                    track.source.display()
                )));
            }
            remove_file_synced(&track.source)?;
            self.library.complete_operation(&operation_id)
        })();
        if let Err(error) = result {
            let state = if committed {
                "cleanup-pending"
            } else {
                "failed"
            };
            let _ =
                self.library
                    .set_operation_state(&operation_id, state, Some(&error.to_string()));
            let recovery = self.library.recover_pending()?;
            if recovery.unresolved.is_empty() {
                Err(error)
            } else {
                Err(Error::Recovery(recovery.unresolved.join("; ")))
            }
        } else {
            Ok(())
        }
    }
}

fn append_extension(path: &Path, extension: &std::ffi::OsStr) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".");
    value.push(extension);
    PathBuf::from(value)
}

fn staging_path(destination: &Path, id: uuid::Uuid) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Import("move destination filename is not valid UTF-8".into()))?;
    Ok(destination.with_file_name(format!(".{name}.rsbts-{id}.move")))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::db::OperationKind;
    use crate::{AudioFormat, ExtendedMetadata, Item};

    #[test]
    fn moves_managed_files_and_updates_the_catalog_path() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let temporary_path = temporary.path().canonicalize()?;
        let source = temporary_path.join("incoming.flac");
        let library_directory = temporary_path.join("organized");
        std::fs::write(&source, b"audio")?;
        let metadata = std::fs::metadata(&source)?;
        let item = Item {
            id: None,
            album_id: None,
            path: source.clone(),
            title: "Track".into(),
            artist: "Artist".into(),
            album: "Singles".into(),
            albumartist: None,
            genre: None,
            year: None,
            track: None,
            disc: None,
            format: AudioFormat::Flac,
            bitrate: 0,
            length: 1.0,
            file_size: Some(metadata.len()),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: metadata.modified()?.into(),
            singleton: true,
            extended: ExtendedMetadata::default(),
        };
        let mut library = Library::open_in_memory()?;
        let import = library.create_operation(OperationKind::ImportInPlace, &[])?;
        library.commit_import(&import, None, &[item])?;
        library.complete_operation(&import)?;

        let plan = MovePlan::build(
            &library,
            &Query::all(),
            &library_directory,
            "$artist/$title",
        )?;
        let destination = plan.tracks[0].destination.clone();
        let report = MoveExecutor::new(&mut library, library_directory).execute(plan);

        assert_eq!(report.moved, 1, "failures: {:?}", report.failures);
        assert!(report.failures.is_empty());
        assert!(!source.exists());
        assert_eq!(std::fs::read(&destination)?, b"audio");
        assert_eq!(library.query_items(&Query::all())?[0].path, destination);
        Ok(())
    }
}
