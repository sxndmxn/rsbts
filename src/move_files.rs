//! Previewed reorganization of already-managed files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::asset::digest_file;
use crate::db::{file_identity, JournalFile, Library, OperationKind, StoredAssetIdentity};
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
        for mut item in library.query_items_bounded(query, 9_999)? {
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
            let item_id = item
                .id
                .ok_or_else(|| Error::Import("move item has no database ID".into()))?;
            let asset = library.verified_asset_for_item(item_id, &source)?;
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
            let digests = digest_file(&source)?;
            let content_hash = digests.blake3().to_owned();
            let fingerprint = SourceFingerprint {
                size: metadata.len(),
                modified,
                content_hash,
                sha256: digests.sha256().to_owned(),
                identity: file_identity(&metadata),
            };
            require_stored_fingerprint(&asset, &fingerprint)?;
            item.path.clone_from(&destination);
            tracks.push(PlannedTrack {
                source,
                destination,
                fingerprint,
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
        let asset = self
            .library
            .verified_asset_for_item(item_id, &track.source)?;
        require_stored_fingerprint(&asset, &track.fingerprint)?;
        self.library.require_asset_root_mutation(&asset.asset_id)?;
        self.library.require_path_root_mutation(&self.library_dir)?;
        self.library
            .require_path_root_mutation(&track.destination)?;
        let transfer_id = uuid::Uuid::new_v4();
        let staged = staging_path(&track.destination, transfer_id)?;
        let journal = JournalFile {
            source: track.source.clone(),
            staged,
            destination: track.destination.clone(),
            content_hash: Some(track.fingerprint.content_hash.clone()),
            sha256: Some(track.fingerprint.sha256.clone()),
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
                &self.library_dir,
            )?;
            committed = true;
            self.library
                .set_operation_state(&operation_id, "cleanup-pending", None)?;
            let recovery = self.library.recover_pending()?;
            if recovery.unresolved.is_empty() {
                Ok(())
            } else {
                Err(Error::Recovery(recovery.unresolved.join("; ")))
            }
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

fn require_stored_fingerprint(
    asset: &StoredAssetIdentity,
    fingerprint: &SourceFingerprint,
) -> Result<()> {
    if asset.byte_size != fingerprint.size
        || asset.entry_identity != fingerprint.identity
        || asset.blake3 != fingerprint.content_hash
        || asset.sha256 != fingerprint.sha256
    {
        return Err(Error::Import(
            "managed source differs from its verified asset. Run verify before moving".into(),
        ));
    }
    Ok(())
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
    Ok(crate::fsops::bounded_sibling_path(
        destination,
        format!(".{name}.rsbts-{id}.move"),
    ))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::db::OperationKind;
    use crate::{AudioFormat, ExtendedMetadata, Item};

    fn library_with_item(source: &Path, root: Option<&Path>) -> Result<Library> {
        let metadata = std::fs::metadata(source)?;
        let item = Item {
            id: None,
            album_id: None,
            path: source.to_path_buf(),
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
        library.commit_import_at_root_with_artwork(&import, None, &[item], root, None)?;
        library.complete_operation(&import)?;
        Ok(library)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn move_rejects_disabled_source_roots() -> Result<()> {
        use crate::roots::RootState;

        for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
            let temporary = tempfile::tempdir()?;
            let source = temporary.path().join("incoming.flac");
            let destination_root = temporary.path().join("organized");
            std::fs::write(&source, b"generated audio")?;
            let mut library = library_with_item(&source, Some(temporary.path()))?;
            let root = library.register_root(temporary.path())?;
            let plan =
                MovePlan::build(&library, &Query::all(), &destination_root, "$artist/$title")?;
            library.set_root_state(root.id(), state)?;

            let report =
                MoveExecutor::new(&mut library, destination_root.clone()).execute(plan.clone());
            assert_eq!(report.moved, 0, "move ignored {state:?} source policy");
            assert_eq!(report.failures.len(), 1);
            assert_eq!(std::fs::read(&source)?, b"generated audio");
            assert!(!destination_root.exists());
            library.set_root_state(root.id(), RootState::Online)?;
            assert_eq!(
                MoveExecutor::new(&mut library, destination_root)
                    .execute(plan)
                    .moved,
                1
            );
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn move_rejects_disabled_destination_roots() -> Result<()> {
        use crate::roots::RootState;

        for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
            let temporary = tempfile::tempdir()?;
            let source = temporary.path().join("incoming.flac");
            let destination_root = temporary.path().join("organized");
            std::fs::write(&source, b"generated audio")?;
            let mut library = library_with_item(&source, None)?;
            let root = library.register_root(&destination_root)?;
            let plan =
                MovePlan::build(&library, &Query::all(), &destination_root, "$artist/$title")?;
            library.set_root_state(root.id(), state)?;

            let report =
                MoveExecutor::new(&mut library, destination_root.clone()).execute(plan.clone());
            assert_eq!(report.moved, 0, "move ignored {state:?} destination policy");
            assert_eq!(report.failures.len(), 1);
            assert_eq!(std::fs::read(&source)?, b"generated audio");
            assert!(!destination_root.exists());
            library.set_root_state(root.id(), RootState::Online)?;
            assert_eq!(
                MoveExecutor::new(&mut library, destination_root)
                    .execute(plan)
                    .moved,
                1
            );
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn move_rejects_disabled_child_roots() -> Result<()> {
        use crate::roots::RootState;

        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("incoming.flac");
        let destination_root = temporary.path().join("organized");
        std::fs::write(&source, b"generated audio")?;
        let mut library = library_with_item(&source, None)?;
        library.register_root(&destination_root)?;
        let plan = MovePlan::build(&library, &Query::all(), &destination_root, "$artist/$title")?;
        let nested = library.register_root(
            plan.tracks[0]
                .destination
                .parent()
                .ok_or_else(|| Error::Import("test destination has no parent".into()))?,
        )?;
        library.set_root_state(nested.id(), RootState::Offline)?;

        let report =
            MoveExecutor::new(&mut library, destination_root.clone()).execute(plan.clone());
        assert_eq!(report.moved, 0, "move wrote into a disabled child root");
        assert_eq!(report.failures.len(), 1);
        assert_eq!(std::fs::read(&source)?, b"generated audio");
        assert!(!destination_root.exists());
        library.set_root_state(nested.id(), RootState::Online)?;
        assert_eq!(
            MoveExecutor::new(&mut library, destination_root)
                .execute(plan)
                .moved,
            1
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn move_planning_requires_explicit_verification_of_legacy_assets() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("incoming.flac");
        let destination_root = temporary.path().join("organized");
        std::fs::write(&source, b"generated audio")?;
        let mut library = library_with_item(&source, None)?;
        library
            .conn
            .execute("UPDATE assets SET verification_state = 'unverified'", [])?;

        assert!(
            MovePlan::build(&library, &Query::all(), &destination_root, "$artist/$title").is_err(),
            "unverified legacy assets entered a destructive move plan"
        );
        assert_eq!(std::fs::read(&source)?, b"generated audio");
        assert_eq!(
            library
                .verify_items(&Query::all(), temporary.path())?
                .verified,
            1
        );
        let plan = MovePlan::build(&library, &Query::all(), &destination_root, "$artist/$title")?;
        assert_eq!(
            MoveExecutor::new(&mut library, destination_root)
                .execute(plan)
                .moved,
            1
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn move_planning_preserves_a_replaced_managed_file() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("incoming.flac");
        let destination_root = temporary.path().join("organized");
        std::fs::write(&source, b"generated audio")?;
        let library = library_with_item(&source, None)?;
        std::fs::rename(&source, temporary.path().join("retained-original.flac"))?;
        std::fs::write(&source, b"generated audio")?;

        assert!(
            MovePlan::build(&library, &Query::all(), &destination_root, "$artist/$title").is_err(),
            "a replacement file entered a destructive move plan"
        );
        assert_eq!(std::fs::read(&source)?, b"generated audio");
        assert!(!destination_root.exists());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn move_execution_rechecks_stored_verification() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("incoming.flac");
        let destination_root = temporary.path().join("organized");
        std::fs::write(&source, b"generated audio")?;
        let mut library = library_with_item(&source, None)?;
        let plan = MovePlan::build(&library, &Query::all(), &destination_root, "$artist/$title")?;
        library
            .conn
            .execute("UPDATE assets SET verification_state = 'unverified'", [])?;

        let report = MoveExecutor::new(&mut library, destination_root.clone()).execute(plan);
        assert_eq!(
            report.moved, 0,
            "execution moved an asset after verification was revoked"
        );
        assert_eq!(report.failures.len(), 1);
        assert_eq!(std::fs::read(&source)?, b"generated audio");
        assert!(!destination_root.exists());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
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
        assert!(library.audit()?.issues().is_empty());
        Ok(())
    }
}
