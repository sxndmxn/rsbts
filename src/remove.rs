//! Plan-first, invocation-atomic library removal.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::asset::{digest_file, digest_reader};
use crate::db::{
    file_identity, journal_hash_path, JournalFile, Library, OperationKind, RetainedJournalFile,
    StoredAssetIdentity,
};
use crate::fsops::AnchoredRoot;
use crate::operations::{PlanId, PlanKind, PlanState};
use crate::query::Query;
use crate::{Error, Item, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovalPlan {
    pub items: Vec<Item>,
    pub delete_files: bool,
    pub missing_files: Vec<PathBuf>,
    pub files: Vec<PlannedRemovalFile>,
    plan_id: Option<PlanId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedRemovalFile {
    pub asset_id: String,
    pub path: PathBuf,
    pub content_hash: String,
    pub sha256: String,
    pub source_identity: String,
    pub role: String,
}

impl RemovalPlan {
    pub fn build(library: &Library, query: &Query, delete_files: bool) -> Result<Self> {
        let items = library.query_items_bounded(query, 9_999)?;
        let mut missing_files = Vec::new();
        let mut files = Vec::new();
        if delete_files {
            let mut selected_albums = HashMap::<i64, usize>::new();
            for item in &items {
                if let Some(album_id) = item.album_id {
                    *selected_albums.entry(album_id).or_default() += 1;
                }
                if !item.path.exists() && !item.path.is_symlink() {
                    missing_files.push(item.path.clone());
                } else {
                    let item_id = item.id.ok_or_else(|| {
                        Error::Import(
                            "cannot delete a file for an item without a database ID".into(),
                        )
                    })?;
                    let asset = library.verified_asset_for_item(item_id, &item.path)?;
                    files.push(fingerprint_path(&asset)?);
                }
            }
            for (album_id, selected_count) in selected_albums {
                if selected_count == library.album_item_count(album_id)? {
                    for asset in library.verified_assets_for_album(album_id)? {
                        files.push(fingerprint_path(&asset)?);
                    }
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
            plan_id: None,
        })
    }

    /// Persist and approve the exact selection. Dry runs deliberately stop before this call.
    pub fn approve(mut self, library: &Library) -> Result<Self> {
        if self.plan_id.is_some() {
            return Err(Error::Operation("removal plan is already approved".into()));
        }
        let request = serde_json::json!({
            "item_ids": self.items.iter().filter_map(|item| item.id).collect::<Vec<_>>(),
            "delete_files": self.delete_files,
        });
        let preview = serde_json::to_value(&self)?;
        let id = library.create_durable_plan(
            PlanKind::Removal,
            &request,
            &preview,
            Some(self.items.len() as u64),
        )?;
        library.approve_durable_plan(&id)?;
        self.plan_id = Some(id);
        Ok(self)
    }

    #[must_use]
    pub const fn id(&self) -> Option<&PlanId> {
        self.plan_id.as_ref()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemovalReport {
    pub removed_rows: usize,
    pub quarantined_files: usize,
    pub missing_files: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PurgePlan {
    files: Vec<RetainedJournalFile>,
    completed_before: chrono::DateTime<chrono::Utc>,
    plan_id: Option<PlanId>,
}

impl PurgePlan {
    pub fn build(library: &Library, older_than_days: u64) -> Result<Self> {
        let days = i64::try_from(older_than_days).map_err(|_error| {
            Error::Import("purge retention exceeds the supported duration".into())
        })?;
        let completed_before = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::days(days))
            .ok_or_else(|| Error::Import("purge retention is out of range".into()))?;
        Ok(Self {
            files: library.retained_removals_before(completed_before)?,
            completed_before,
            plan_id: None,
        })
    }

    /// Persist and approve the exact preview. Dry-run callers deliberately do not call this.
    pub fn approve(mut self, library: &Library) -> Result<Self> {
        if self.plan_id.is_some() {
            return Err(Error::Operation("purge plan is already approved".into()));
        }
        let request = serde_json::json!({
            "completed_before": self.completed_before,
        });
        let preview = serde_json::json!({
            "quarantined_files": self.files.len(),
            "permanent": true,
            "journaled": true,
        });
        let total = u64::try_from(self.files.len())
            .map_err(|error| Error::Operation(format!("purge plan is too large: {error}")))?;
        let id = library.create_durable_plan(PlanKind::Purge, &request, &preview, Some(total))?;
        library.approve_durable_plan(&id)?;
        self.plan_id = Some(id);
        Ok(self)
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.files.iter().map(|entry| entry.file.staged.as_path())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PurgeReport {
    pub purged_files: usize,
    pub already_missing: usize,
}

pub struct PurgeExecutor<'a> {
    library: &'a mut Library,
}

impl<'a> PurgeExecutor<'a> {
    pub const fn new(library: &'a mut Library) -> Self {
        Self { library }
    }

    pub fn execute(&mut self, plan: &PurgePlan) -> Result<PurgeReport> {
        let plan_id = plan.plan_id.as_ref().ok_or_else(|| {
            Error::Operation("purge plan requires explicit durable approval".into())
        })?;
        self.library.start_durable_plan(plan_id)?;
        for entry in &plan.files {
            if entry.file.staged.exists() || entry.file.staged.is_symlink() {
                validate_journal_path(&entry.file.staged, &entry.file)?;
            }
        }
        let missing = plan
            .files
            .iter()
            .filter(|entry| !entry.file.staged.exists() && !entry.file.staged.is_symlink())
            .count();
        let transfer = uuid::Uuid::new_v4();
        let mut sources = Vec::new();
        let mut journal = Vec::new();
        for entry in plan
            .files
            .iter()
            .filter(|entry| entry.file.staged.exists() || entry.file.staged.is_symlink())
        {
            sources.push((entry.operation_id.clone(), entry.ordinal));
            journal.push(JournalFile {
                source: entry.file.staged.clone(),
                staged: purge_staging_path(&entry.file.staged, transfer)?,
                destination: entry.file.staged.clone(),
                content_hash: entry.file.content_hash.clone(),
                sha256: entry.file.sha256.clone(),
                source_identity: entry.file.owned_identity.clone(),
                owned_identity: entry.file.owned_identity.clone(),
                role: "purge-retained-file".into(),
                state: "prepared".into(),
            });
        }
        if journal.is_empty() {
            self.library
                .finish_durable_plan(plan_id, PlanState::Complete, None)?;
            return Ok(PurgeReport {
                purged_files: 0,
                already_missing: missing,
            });
        }
        let operation = self.library.create_operation_for_plan(
            OperationKind::PurgeDelete,
            &journal,
            Some(plan_id.as_str()),
            Some(&serde_json::json!({
                "retention_cutoff": plan.completed_before,
                "files": journal.len(),
            })),
        )?;
        let result = self.execute_journaled_purge(plan_id, &operation, &journal, &sources);
        match result {
            Ok(()) => Ok(PurgeReport {
                purged_files: journal.len(),
                already_missing: missing,
            }),
            Err(error) => {
                let _ = self
                    .library
                    .record_operation_failure(&operation, &error.to_string());
                let recovery = self.library.recover_pending();
                match recovery {
                    Ok(report) if report.unresolved.is_empty() => Err(error),
                    Ok(report) => Err(Error::Recovery(format!(
                        "{error}; purge recovery requires review: {}",
                        report.unresolved.join("; ")
                    ))),
                    Err(recovery) => Err(Error::Recovery(format!(
                        "{error}; purge recovery could not run: {recovery}"
                    ))),
                }
            }
        }
    }

    fn execute_journaled_purge(
        &self,
        plan_id: &PlanId,
        operation: &str,
        files: &[JournalFile],
        sources: &[(String, usize)],
    ) -> Result<()> {
        self.library
            .set_operation_state(operation, "quarantining", None)?;
        for (ordinal, file) in files.iter().enumerate() {
            let parent = file
                .source
                .parent()
                .ok_or_else(|| Error::Recovery("purge source has no anchored parent".into()))?;
            let root = AnchoredRoot::open(parent)?;
            root.rename_noreplace(&file.source, &file.staged)?;
            self.library
                .set_file_state(operation, ordinal, "quarantined")?;
        }
        self.library
            .set_operation_state(operation, "db-committed", None)?;
        for (ordinal, file) in files.iter().enumerate() {
            let parent = file.staged.parent().ok_or_else(|| {
                Error::Recovery("purge staging path has no anchored parent".into())
            })?;
            AnchoredRoot::open(parent)?.remove_file(&file.staged)?;
            self.library.set_file_state(operation, ordinal, "purged")?;
        }
        let transaction = self.library.conn.unchecked_transaction()?;
        for (source_operation, source_ordinal) in sources {
            transaction.execute(
                "UPDATE operation_files SET state = 'purged'
                 WHERE operation_id = ?1 AND ordinal = ?2 AND state = 'quarantined'",
                rusqlite::params![source_operation, source_ordinal],
            )?;
        }
        transaction.commit()?;
        self.library
            .finish_durable_plan(plan_id, PlanState::Complete, None)?;
        self.library.complete_operation(operation)
    }
}

fn purge_staging_path(path: &Path, transfer: uuid::Uuid) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::Recovery("purge path has no filename".into()))?;
    let mut staged = std::ffi::OsString::from(".");
    staged.push(name);
    staged.push(format!(".rsbts-{transfer}.purge"));
    Ok(path.with_file_name(staged))
}

pub struct RemovalExecutor<'a> {
    library: &'a mut Library,
}

impl<'a> RemovalExecutor<'a> {
    pub const fn new(library: &'a mut Library) -> Self {
        Self { library }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "removal keeps its ordered approval, quarantine, database-commit, and recovery protocol visible"
    )]
    pub fn execute(&mut self, plan: RemovalPlan) -> Result<RemovalReport> {
        let plan_id = plan
            .plan_id
            .as_ref()
            .ok_or_else(|| Error::Operation("removal requires explicit durable approval".into()))?;
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
            self.library.start_durable_plan(plan_id)?;
            self.library
                .finish_durable_plan(plan_id, PlanState::Complete, None)?;
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
        self.library.start_durable_plan(plan_id)?;
        if !plan.delete_files {
            if let Err(error) = self.library.commit_removal(None, &database_items) {
                let _ = self.library.finish_durable_plan(
                    plan_id,
                    PlanState::Failed,
                    Some(&error.to_string()),
                );
                return Err(error);
            }
            self.library
                .finish_durable_plan(plan_id, PlanState::Complete, None)?;
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
                    sha256: Some(file.sha256.clone()),
                    source_identity: Some(file.source_identity.clone()),
                    owned_identity: Some(file.source_identity.clone()),
                    role: file.role.clone(),
                    state: "prepared".into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let operation_id = match self.library.create_operation_for_plan(
            OperationKind::RemoveDelete,
            &existing,
            Some(plan_id.as_str()),
            Some(&serde_json::json!({"delete_files": true})),
        ) {
            Ok(operation) => operation,
            Err(error) => {
                let _ = self.library.recover_pending();
                let _ = self.library.finish_durable_plan(
                    plan_id,
                    PlanState::Failed,
                    Some(&error.to_string()),
                );
                return Err(error);
            }
        };

        if let Err(error) = self.quarantine(&operation_id, &existing) {
            return Err(self.rollback_failed(plan_id, &operation_id, error));
        }
        if let Err(error) = self
            .library
            .commit_removal(Some(&operation_id), &database_items)
        {
            return Err(self.rollback_failed(plan_id, &operation_id, error));
        }

        self.library
            .finish_durable_plan(plan_id, PlanState::Complete, None)?;
        self.library.complete_operation(&operation_id)?;

        Ok(RemovalReport {
            removed_rows: database_items.len(),
            quarantined_files: existing.len(),
            missing_files: plan.missing_files,
        })
    }

    fn quarantine(&self, operation_id: &str, files: &[JournalFile]) -> Result<()> {
        self.library
            .set_operation_state(operation_id, "staging", None)?;
        for (ordinal, file) in files.iter().enumerate() {
            let parent = file.source.parent().ok_or_else(|| {
                Error::Root(format!(
                    "removal path has no anchored parent: {}",
                    file.source.display()
                ))
            })?;
            let root = AnchoredRoot::open(parent)?;
            if root.entry_metadata(&file.staged).is_ok() {
                return Err(Error::Import(format!(
                    "quarantine path already exists: {}",
                    file.staged.display()
                )));
            }
            root.rename_noreplace(&file.source, &file.staged)?;
            if let Err(error) = validate_journal_path_anchored(&root, &file.staged, file) {
                if let Err(restore_error) = root.rename_noreplace(&file.staged, &file.source) {
                    return Err(Error::Recovery(format!(
                        "{error}; acquired object was preserved at {}; safe restore failed: {restore_error}",
                        file.staged.display()
                    )));
                }
                return Err(error);
            }
            self.library
                .set_file_state(operation_id, ordinal, "quarantined")?;
        }
        Ok(())
    }

    fn rollback_failed(&mut self, plan_id: &PlanId, operation_id: &str, error: Error) -> Error {
        let _ = self
            .library
            .record_operation_failure(operation_id, &error.to_string());
        let recovery = self.library.recover_pending();
        if self
            .library
            .durable_plan(plan_id)
            .is_ok_and(|plan| plan.state() == PlanState::Running)
        {
            let _ = self.library.finish_durable_plan(
                plan_id,
                PlanState::Failed,
                Some(&error.to_string()),
            );
        }
        match recovery {
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
    if !file_paths.is_disjoint(&missing_paths) || !missing_paths.is_subset(&item_paths) {
        return Err(Error::Import(
            "removal plan files do not match its database items".into(),
        ));
    }
    let audio_paths = plan
        .files
        .iter()
        .filter(|file| file.role != "artwork")
        .map(|file| file.path.as_path())
        .collect::<HashSet<_>>();
    if !audio_paths.is_subset(&item_paths) {
        return Err(Error::Import(
            "removal plan contains an audio asset not selected by its item query".into(),
        ));
    }
    if plan.delete_files && audio_paths.len() + missing_paths.len() != item_paths.len() {
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

fn fingerprint_path(asset: &StoredAssetIdentity) -> Result<PlannedRemovalFile> {
    let path = &asset.path;
    let role = if path.is_symlink() {
        "symlink"
    } else if asset.role == "artwork" {
        "artwork"
    } else {
        "track"
    };
    let metadata = std::fs::symlink_metadata(path)?;
    let content_metadata = std::fs::metadata(path)?;
    let source_identity = file_identity(&metadata);
    let digests = digest_file(path)?;
    let after = std::fs::symlink_metadata(path)?;
    if file_identity(&after) != source_identity
        || path.is_symlink() != (role == "symlink")
        || source_identity != asset.entry_identity
        || content_metadata.len() != asset.byte_size
        || digests.blake3() != asset.blake3
        || digests.sha256() != asset.sha256
    {
        return Err(Error::Import(format!(
            "file no longer matches its persistent asset identity: {}",
            path.display()
        )));
    }
    Ok(PlannedRemovalFile {
        asset_id: asset.asset_id.clone(),
        path: path.clone(),
        content_hash: asset.blake3.clone(),
        sha256: asset.sha256.clone(),
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
        sha256: Some(file.sha256.clone()),
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

fn validate_journal_path_anchored(
    root: &AnchoredRoot,
    path: &Path,
    file: &JournalFile,
) -> Result<()> {
    let expected_identity = file.source_identity.as_deref().ok_or_else(|| {
        Error::Recovery(format!(
            "journal has no source identity for {}",
            path.display()
        ))
    })?;
    let metadata = root.entry_metadata(path)?;
    let actual_hash = if file.role == "symlink" {
        hash_link_target(&root.read_link(path)?)
    } else {
        digest_reader(root.open_file(path)?)?.blake3().to_owned()
    };
    if file_identity(&metadata) != expected_identity
        || file.content_hash.as_deref() != Some(actual_hash.as_str())
        || metadata.file_type().is_symlink() != (file.role == "symlink")
    {
        return Err(Error::Import(format!(
            "file changed after removal planning: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn hash_link_target(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    blake3::hash(path.as_os_str().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(not(unix))]
fn hash_link_target(path: &Path) -> String {
    blake3::hash(path.to_string_lossy().as_bytes())
        .to_hex()
        .to_string()
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
        };
        library.commit_import(&operation, &album, &[item])?;
        library.complete_operation(&operation)?;
        Ok(library)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn deletion_is_journaled_and_removes_rows() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let plan = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        let report = RemovalExecutor::new(&mut library).execute(plan)?;
        assert_eq!(report.removed_rows, 1);
        assert_eq!(report.quarantined_files, 1);
        assert!(!path.exists());
        assert!(library.query_items(&Query::all())?.is_empty());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn every_removal_boundary_retains_the_managed_bytes() -> Result<()> {
        fn one(fail_at: Option<usize>) -> Result<(Result<RemovalReport>, Vec<&'static str>)> {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join("track.flac");
            std::fs::write(&path, b"managed audio")?;
            let mut library = library_with_item(&path)?;
            let plan = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
            let (result, hits) = match fail_at {
                None => crate::failpoints::run_recording(|| {
                    RemovalExecutor::new(&mut library).execute(plan)
                }),
                Some(index) => crate::failpoints::run_failing(index, || {
                    RemovalExecutor::new(&mut library).execute(plan)
                }),
            };
            let recovery = library.recover_pending()?;
            assert!(recovery.unresolved.is_empty(), "{recovery:?}");
            assert!(library.recover_pending()?.unresolved.is_empty());

            let mut copies = 0;
            for entry in std::fs::read_dir(temporary.path())? {
                let entry = entry?;
                if entry.file_type()?.is_file() && std::fs::read(entry.path())? == b"managed audio"
                {
                    copies += 1;
                }
            }
            assert_eq!(copies, 1, "managed bytes were lost or duplicated");
            let items = library.query_items(&Query::all())?;
            if items.is_empty() {
                assert!(!path.exists());
            } else {
                assert_eq!(items.len(), 1);
                assert_eq!(std::fs::read(&path)?, b"managed audio");
            }
            Ok((result, hits))
        }

        let (success, boundaries) = one(None)?;
        assert!(success.is_ok());
        assert!(boundaries.len() >= 7, "insufficient boundary coverage");
        for index in 0..boundaries.len() {
            let (result, observed) = one(Some(index))?;
            assert!(
                result.is_err(),
                "boundary {index} did not fail: {observed:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn deletion_preserves_an_identical_replacement_made_after_planning() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        let preserved_original = temporary.path().join("original.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let plan = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        std::fs::rename(&path, preserved_original)?;
        std::fs::write(&path, b"audio")?;

        assert!(RemovalExecutor::new(&mut library).execute(plan).is_err());

        assert_eq!(std::fs::read(path)?, b"audio");
        assert_eq!(library.query_items(&Query::all())?.len(), 1);
        Ok(())
    }

    #[test]
    fn deletion_rejects_a_replacement_made_before_planning() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        let original = temporary.path().join("original.flac");
        std::fs::write(&path, b"audio")?;
        let library = library_with_item(&path)?;
        std::fs::rename(&path, &original)?;
        std::fs::write(&path, b"replacement")?;

        assert!(RemovalPlan::build(&library, &Query::all(), true).is_err());
        assert_eq!(std::fs::read(path)?, b"replacement");
        assert_eq!(std::fs::read(original)?, b"audio");
        Ok(())
    }

    #[test]
    fn deletion_rejects_an_identical_new_inode_before_planning() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        let original = temporary.path().join("original.flac");
        std::fs::write(&path, b"audio")?;
        let library = library_with_item(&path)?;
        std::fs::rename(&path, &original)?;
        std::fs::write(&path, b"audio")?;

        assert!(RemovalPlan::build(&library, &Query::all(), true).is_err());
        assert_eq!(std::fs::read(path)?, b"audio");
        assert_eq!(std::fs::read(original)?, b"audio");
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn permanent_purge_is_a_separate_explicit_operation() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        let report = RemovalExecutor::new(&mut library).execute(removal)?;
        assert_eq!(report.quarantined_files, 1);

        let purge = PurgePlan::build(&library, 0)?;
        assert_eq!(purge.len(), 1);
        let quarantined =
            purge.paths().next().map(Path::to_path_buf).ok_or_else(|| {
                Error::Recovery("purge plan did not expose its quarantine".into())
            })?;
        assert!(quarantined.exists());

        let purge = purge.approve(&library)?;
        let report = PurgeExecutor::new(&mut library).execute(&purge)?;
        assert_eq!(report.purged_files, 1);
        assert!(!quarantined.exists());
        assert!(PurgePlan::build(&library, 0)?.is_empty());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn removing_a_complete_album_quarantines_its_managed_artwork() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let track = temporary.path().join("track.flac");
        let artwork = temporary.path().join("cover.jpg");
        std::fs::write(&track, b"audio")?;
        std::fs::write(&artwork, b"artwork")?;
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: "Album".into(),
            albumartist: "Artist".into(),
            year: None,
            artpath: Some(artwork.clone()),
            external_id: None,
            added: Utc::now(),
        };
        let metadata = std::fs::metadata(&track)?;
        let item = Item {
            id: None,
            album_id: None,
            path: track.clone(),
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
            file_size: Some(metadata.len()),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: metadata.modified()?.into(),
        };
        library.commit_import(&operation, &album, &[item])?;
        library.complete_operation(&operation)?;

        let plan = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        assert_eq!(plan.files.len(), 2);
        let report = RemovalExecutor::new(&mut library).execute(plan)?;
        assert_eq!(report.quarantined_files, 2);
        assert!(!track.exists());
        assert!(!artwork.exists());
        assert_eq!(PurgePlan::build(&library, 0)?.len(), 2);
        assert!(library.audit()?.issues().is_empty());
        Ok(())
    }

    #[test]
    fn deletion_preserves_a_file_that_appears_after_missing_file_planning() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        let mut library = library_with_item(&path)?;
        let plan = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        assert_eq!(plan.missing_files.as_slice(), std::slice::from_ref(&path));
        std::fs::write(&path, b"new audio")?;

        assert!(RemovalExecutor::new(&mut library).execute(plan).is_err());

        assert_eq!(std::fs::read(path)?, b"new audio");
        assert_eq!(library.query_items(&Query::all())?.len(), 1);
        Ok(())
    }
}
