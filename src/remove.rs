//! Plan-first, invocation-atomic library removal.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension as _;
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
        validate_plan_shape(&self)?;
        validate_plan_ownership(library, &self)?;
        let request = self.approval_request();
        let preview = self.approval_preview()?;
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

    fn approval_request(&self) -> serde_json::Value {
        serde_json::json!({
            "item_ids": self.items.iter().filter_map(|item| item.id).collect::<Vec<_>>(),
            "delete_files": self.delete_files,
        })
    }

    fn approval_preview(&self) -> Result<serde_json::Value> {
        let mut preview = serde_json::to_value(self)?;
        // Approval assigns its ID after it stores the preview.
        if let Some(fields) = preview.as_object_mut() {
            fields.insert("plan_id".into(), serde_json::Value::Null);
        }
        Ok(preview)
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
    /// Calculate the cutoff date for a valid retention period.
    /// This calculation does not open the library.
    pub fn retention_cutoff(older_than_days: u64) -> Result<chrono::DateTime<chrono::Utc>> {
        let days = i64::try_from(older_than_days).map_err(|_error| {
            Error::Import("purge retention exceeds the supported duration".into())
        })?;
        let duration = chrono::Duration::try_days(days)
            .ok_or_else(|| Error::Import("purge retention is out of range".into()))?;
        chrono::Utc::now()
            .checked_sub_signed(duration)
            .ok_or_else(|| Error::Import("purge retention is out of range".into()))
    }

    pub fn build(library: &Library, older_than_days: u64) -> Result<Self> {
        let completed_before = Self::retention_cutoff(older_than_days)?;
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
        for entry in &plan.files {
            self.library.require_retained_root_mutation(entry)?;
        }
        self.library.start_durable_plan(plan_id)?;
        let journal = match prepare_purge_files(plan) {
            Ok(journal) => journal,
            Err(error) => {
                return Err(self.finish_failed_plan(plan_id, error));
            }
        };
        let missing = journal
            .iter()
            .filter(|file| file.state == "already-missing")
            .count();
        let sources = plan
            .files
            .iter()
            .map(|entry| (entry.operation_id.clone(), entry.ordinal))
            .collect::<Vec<_>>();
        if journal.is_empty() {
            self.library
                .finish_durable_plan(plan_id, PlanState::Complete, None)?;
            return Ok(PurgeReport {
                purged_files: 0,
                already_missing: missing,
            });
        }
        let operation = match self.library.create_operation_for_plan(
            OperationKind::PurgeDelete,
            &journal,
            Some(plan_id.as_str()),
            Some(&serde_json::json!({
                "retention_cutoff": plan.completed_before,
                "files": journal.len(),
            })),
        ) {
            Ok(operation) => operation,
            Err(error) => return Err(self.recover_failed(plan_id, None, error)),
        };
        let result = self.execute_journaled_purge(plan_id, &operation, &journal, &sources);
        match result {
            Ok(()) => Ok(PurgeReport {
                purged_files: journal.len() - missing,
                already_missing: missing,
            }),
            Err(error) => Err(self.recover_failed(plan_id, Some(&operation), error)),
        }
    }

    fn finish_failed_plan(&self, plan_id: &PlanId, error: Error) -> Error {
        match self
            .library
            .finish_durable_plan(plan_id, PlanState::Failed, Some(&error.to_string()))
        {
            Ok(()) => error,
            Err(failure) => Error::Recovery(format!(
                "{error}; purge plan failure could not be recorded: {failure}"
            )),
        }
    }

    fn recover_failed(&mut self, plan_id: &PlanId, operation: Option<&str>, error: Error) -> Error {
        if let Some(operation) = operation {
            let _ = self
                .library
                .record_operation_failure(operation, &error.to_string());
        }
        match self.library.recover_pending() {
            Ok(report) if report.unresolved.is_empty() => {
                match self.library.durable_plan(plan_id) {
                    Ok(plan) if plan.state() == PlanState::Running => {
                        self.finish_failed_plan(plan_id, error)
                    }
                    Ok(_) => error,
                    Err(failure) => Error::Recovery(format!(
                        "{error}; purge plan state could not be read: {failure}"
                    )),
                }
            }
            Ok(report) => Error::Recovery(format!(
                "{error}; purge recovery requires review: {}",
                report.unresolved.join("; ")
            )),
            Err(recovery) => {
                Error::Recovery(format!("{error}; purge recovery could not run: {recovery}"))
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
            if file.state == "already-missing" {
                self.library
                    .set_file_state(operation, ordinal, "already-missing")?;
                continue;
            }
            let parent = file
                .source
                .parent()
                .ok_or_else(|| Error::Recovery("purge source has no anchored parent".into()))?;
            let root = AnchoredRoot::open(parent)?;
            root.rename_noreplace(&file.source, &file.staged)?;
            validate_journal_path_anchored(&root, &file.staged, file)?;
            self.library
                .set_file_state(operation, ordinal, "quarantined")?;
        }
        self.library
            .set_operation_state(operation, "db-committed", None)?;
        for (ordinal, file) in files.iter().enumerate() {
            if file.state == "already-missing" {
                continue;
            }
            let parent = file.staged.parent().ok_or_else(|| {
                Error::Recovery("purge staging path has no anchored parent".into())
            })?;
            let root = AnchoredRoot::open(parent)?;
            validate_journal_path_anchored(&root, &file.staged, file)?;
            root.remove_file(&file.staged)?;
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

fn prepare_purge_files(plan: &PurgePlan) -> Result<Vec<JournalFile>> {
    let transfer = uuid::Uuid::new_v4();
    plan.files
        .iter()
        .map(|entry| {
            let state = match std::fs::symlink_metadata(&entry.file.staged) {
                Ok(_) => {
                    validate_journal_path(&entry.file.staged, &entry.file)?;
                    "prepared"
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => "already-missing",
                Err(error) => return Err(error.into()),
            };
            Ok(JournalFile {
                source: entry.file.staged.clone(),
                staged: purge_staging_path(&entry.file.staged, transfer)?,
                destination: entry.file.staged.clone(),
                content_hash: entry.file.content_hash.clone(),
                sha256: entry.file.sha256.clone(),
                source_identity: entry.file.owned_identity.clone(),
                owned_identity: entry.file.owned_identity.clone(),
                role: if entry.file.role == "symlink" {
                    "symlink"
                } else {
                    "purge-retained-file"
                }
                .into(),
                state: state.into(),
            })
        })
        .collect()
}

fn purge_staging_path(path: &Path, transfer: uuid::Uuid) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::Recovery("purge path has no filename".into()))?;
    let mut staged = std::ffi::OsString::from(".");
    staged.push(name);
    staged.push(format!(".rsbts-{transfer}.purge"));
    Ok(crate::fsops::bounded_sibling_path(path, staged))
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
        self.library.require_reviewed_plan(
            plan_id,
            PlanKind::Removal,
            &plan.approval_request(),
            &plan.approval_preview()?,
        )?;
        validate_plan_shape(&plan)?;
        validate_plan_ownership(self.library, &plan)?;
        if plan.delete_files {
            for file in &plan.files {
                self.library.require_asset_root_mutation(&file.asset_id)?;
            }
        }
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
        let existing = if plan.delete_files {
            let operation_uuid = uuid::Uuid::new_v4();
            for file in &plan.files {
                validate_planned_file(file)?;
            }
            plan.files
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
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        if let Err(error) = self.library.start_durable_plan(plan_id) {
            if let Err(failure) = self.library.finish_durable_plan(
                plan_id,
                PlanState::Failed,
                Some(&error.to_string()),
            ) {
                return Err(Error::Recovery(format!(
                    "{error}; removal plan failure could not be recorded: {failure}"
                )));
            }
            return Err(error);
        }
        if database_items.is_empty() {
            self.library
                .finish_durable_plan(plan_id, PlanState::Complete, None)?;
            return Ok(RemovalReport::default());
        }
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

        for (ordinal, file) in plan.files.iter().enumerate() {
            if let Err(error) =
                self.library
                    .bind_operation_asset(&operation_id, ordinal, &file.asset_id)
            {
                return Err(self.rollback_failed(plan_id, &operation_id, error));
            }
        }
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

fn validate_plan_ownership(library: &Library, plan: &RemovalPlan) -> Result<()> {
    let mut selected_ids = HashSet::new();
    let mut selected_albums = HashMap::<i64, usize>::new();
    for item in &plan.items {
        let id = item
            .id
            .ok_or_else(|| Error::Import("cannot remove an item without a database ID".into()))?;
        if !selected_ids.insert(id) {
            return Err(Error::Import(
                "removal plan contains duplicate database IDs".into(),
            ));
        }
        let stored = library
            .conn
            .query_row(
                "SELECT path, album_id FROM items WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, Option<i64>>(1)?,
                    ))
                },
            )
            .optional()?;
        let Some((_path, album_id)) = stored.filter(|(path, _album)| path == &item.path) else {
            return Err(Error::Import(
                "removal item no longer matches its catalog row".into(),
            ));
        };
        if let Some(album_id) = album_id {
            *selected_albums.entry(album_id).or_default() += 1;
        }
    }
    let mut complete_albums = HashSet::new();
    for (album, count) in selected_albums {
        if count == library.album_item_count(album)? {
            complete_albums.insert(album);
        }
    }
    for file in &plan.files {
        validate_file_ownership(library, file, &selected_ids, &complete_albums)?;
    }
    Ok(())
}

fn validate_file_ownership(
    library: &Library,
    file: &PlannedRemovalFile,
    selected_ids: &HashSet<i64>,
    complete_albums: &HashSet<i64>,
) -> Result<()> {
    let stored = library
        .conn
        .query_row(
            "SELECT absolute_path, role, entry_identity, blake3, sha256 FROM assets
         WHERE id = ?1 AND managed = 1 AND verification_state = 'verified'",
            [&file.asset_id],
            |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((path, role, identity, blake3, sha256)) = stored else {
        return Err(Error::Import(
            "removal file has no verified managed asset".into(),
        ));
    };
    if path != file.path
        || identity.as_ref() != Some(&file.source_identity)
        || blake3.as_ref() != Some(&file.content_hash)
        || sha256.as_ref() != Some(&file.sha256)
    {
        return Err(Error::Import(
            "removal file differs from its catalog asset".into(),
        ));
    }
    let (sql, selected) = match (file.role.as_str(), role.as_str()) {
        ("artwork", "artwork") => (
            "SELECT album_id FROM album_assets WHERE asset_id = ?1",
            complete_albums,
        ),
        ("track" | "symlink", "audio") => (
            "SELECT item_id FROM item_assets WHERE asset_id = ?1 AND relationship = 'audio'",
            selected_ids,
        ),
        _ => {
            return Err(Error::Import(
                "removal file role differs from its catalog role".into(),
            ))
        }
    };
    let mut statement = library.conn.prepare(sql)?;
    let owners = statement.query_map([&file.asset_id], |row| row.get::<_, i64>(0))?;
    let mut has_owner = false;
    for owner in owners {
        has_owner = true;
        if !selected.contains(&owner?) {
            return Err(Error::Import(
                "removal file has an unselected catalog owner".into(),
            ));
        }
    }
    if !has_owner {
        return Err(Error::Import(
            "removal file has no selected catalog owner".into(),
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
    Ok(crate::fsops::bounded_sibling_path(
        path,
        format!(".{name}.rsbts-{operation_id}.delete"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::{Album, AudioFormat};

    fn library_with_item(path: &Path) -> Result<Library> {
        library_with_item_at_root(path, None)
    }

    fn library_with_item_at_root(path: &Path, root: Option<&Path>) -> Result<Library> {
        library_with_artwork_at_root(path, root, None, None)
    }

    fn library_with_artwork_at_root(
        path: &Path,
        root: Option<&Path>,
        artwork: Option<&Path>,
        original: Option<&crate::artwork::ArtworkAssetMetadata>,
    ) -> Result<Library> {
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: "Album".into(),
            albumartist: "Artist".into(),
            year: None,
            artpath: artwork.map(Path::to_path_buf),
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
        library.commit_import_at_root_with_artwork(
            &operation,
            Some(&album),
            &[item],
            root,
            original,
        )?;
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

    #[test]
    fn removal_rejects_a_changed_approved_selection() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("approved.flac");
        let other_path = temporary.path().join("unapproved.flac");
        std::fs::write(&path, b"approved audio")?;
        std::fs::write(&other_path, b"unapproved audio")?;
        let mut library = library_with_item(&path)?;
        let mut other_item = library.query_items(&Query::all())?[0].clone();
        other_item.id = None;
        other_item.path = other_path.clone();
        other_item.title = "Other".into();
        let import = library.create_operation(OperationKind::ImportCopy, &[])?;
        library.commit_import(&import, None, &[other_item])?;
        library.complete_operation(&import)?;
        let before = serde_json::to_value(library.query_items(&Query::all())?)?;
        let mut plan = RemovalPlan::build(&library, &Query::parse("title:=Track")?, false)?
            .approve(&library)?;
        plan.items = library.query_items(&Query::parse("title:=Other")?)?;

        let result = RemovalExecutor::new(&mut library).execute(plan);
        assert!(
            result.is_err(),
            "removal accepted another selection: {result:?}"
        );
        assert_eq!(
            serde_json::to_value(library.query_items(&Query::all())?)?,
            before
        );
        assert_eq!(std::fs::read(path)?, b"approved audio");
        assert_eq!(std::fs::read(other_path)?, b"unapproved audio");
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn removal_rejects_file_deletion_added_after_approval() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let before = serde_json::to_value(library.query_items(&Query::all())?)?;
        let mut plan = RemovalPlan::build(&library, &Query::all(), false)?.approve(&library)?;
        let deletion = RemovalPlan::build(&library, &Query::all(), true)?;
        plan.delete_files = true;
        plan.files = deletion.files;
        plan.missing_files = deletion.missing_files;

        let result = RemovalExecutor::new(&mut library).execute(plan);
        assert!(
            result.is_err(),
            "removal accepted unapproved deletion: {result:?}"
        );
        assert_eq!(
            serde_json::to_value(library.query_items(&Query::all())?)?,
            before
        );
        assert_eq!(std::fs::read(path)?, b"audio");
        Ok(())
    }

    #[test]
    fn removal_rejects_an_unrelated_approval() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let before = serde_json::to_value(library.query_items(&Query::all())?)?;
        let mut plan = RemovalPlan::build(&library, &Query::all(), false)?;
        let request = serde_json::json!({
            "item_ids": plan.items.iter().filter_map(|item| item.id).collect::<Vec<_>>(),
            "delete_files": plan.delete_files,
        });
        let preview = serde_json::to_value(&plan)?;
        let id = library.create_durable_plan(PlanKind::Purge, &request, &preview, Some(1))?;
        library.approve_durable_plan(&id)?;
        plan.plan_id = Some(id);

        let result = RemovalExecutor::new(&mut library).execute(plan);
        assert!(
            result.is_err(),
            "removal accepted purge approval: {result:?}"
        );
        assert_eq!(
            serde_json::to_value(library.query_items(&Query::all())?)?,
            before
        );
        assert_eq!(std::fs::read(path)?, b"audio");
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn removal_rejects_a_root_policy_change_after_approval() -> Result<()> {
        use crate::roots::RootState;

        for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join("track.flac");
            std::fs::write(&path, b"audio")?;
            let mut library = library_with_item_at_root(&path, Some(temporary.path()))?;
            let root = library.register_root(temporary.path())?;
            let plan = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
            library.set_root_state(root.id(), state)?;

            let result = RemovalExecutor::new(&mut library).execute(plan);
            assert!(result.is_err(), "removal ignored root state {state:?}");
            assert_eq!(std::fs::read(path)?, b"audio");
            assert_eq!(library.query_items(&Query::all())?.len(), 1);
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn catalog_only_removal_allows_an_unavailable_root() -> Result<()> {
        use crate::roots::RootState;

        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item_at_root(&path, Some(temporary.path()))?;
        let root = library.register_root(temporary.path())?;
        let plan = RemovalPlan::build(&library, &Query::all(), false)?.approve(&library)?;
        library.set_root_state(root.id(), RootState::Offline)?;

        let result = RemovalExecutor::new(&mut library).execute(plan);
        assert!(result.is_ok(), "catalog-only removal rejected root policy");
        assert_eq!(std::fs::read(path)?, b"audio");
        assert!(library.query_items(&Query::all())?.is_empty());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn purge_rejects_a_root_policy_change_after_approval() -> Result<()> {
        use crate::roots::RootState;

        for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join("track.flac");
            std::fs::write(&path, b"audio")?;
            let mut library = library_with_item_at_root(&path, Some(temporary.path()))?;
            let root = library.register_root(temporary.path())?;
            let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
            RemovalExecutor::new(&mut library).execute(removal)?;
            let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
            library.set_root_state(root.id(), state)?;

            let result = PurgeExecutor::new(&mut library).execute(&purge);
            assert!(result.is_err(), "purge ignored root state {state:?}");
            assert_eq!(std::fs::read(&purge.files[0].file.staged)?, b"audio");
            assert_eq!(PurgePlan::build(&library, 0)?.len(), 1);
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn purge_honors_its_origin_root_after_asset_reclassification() -> Result<()> {
        use crate::roots::RootState;

        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item_at_root(&path, Some(temporary.path()))?;
        let root = library.register_root(temporary.path())?;
        let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        RemovalExecutor::new(&mut library).execute(removal)?;
        let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
        library.conn.execute(
            "UPDATE assets SET root_id = '00000000-0000-0000-0000-000000000000'",
            [],
        )?;
        library.set_root_state(root.id(), RootState::Offline)?;

        let result = PurgeExecutor::new(&mut library).execute(&purge);
        assert!(result.is_err(), "purge discarded its origin root policy");
        assert_eq!(std::fs::read(&purge.files[0].file.staged)?, b"audio");
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
            assert_eq!(
                library.recover_pending()?.unresolved,
                [] as [std::string::String; 0]
            );

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
        let plan_id = plan
            .id()
            .ok_or_else(|| Error::Operation("removal fixture has no plan ID".into()))?
            .clone();
        std::fs::rename(&path, preserved_original)?;
        std::fs::write(&path, b"audio")?;

        assert!(RemovalExecutor::new(&mut library).execute(plan).is_err());

        assert_eq!(
            library.durable_plan(&plan_id)?.state(),
            PlanState::Approved,
            "source validation started a removal without a journal"
        );
        let journals: u64 = library.conn.query_row(
            "SELECT COUNT(*) FROM operation_journal WHERE kind = 'remove-delete'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(journals, 0);
        assert_eq!(std::fs::read(path)?, b"audio");
        assert_eq!(library.query_items(&Query::all())?.len(), 1);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn removal_start_errors_do_not_leave_running_plans() -> Result<()> {
        let mut wrong_states = Vec::new();
        for delete_files in [false, true] {
            for empty in [false, true] {
                let temporary = tempfile::tempdir()?;
                let path = temporary.path().join("track.flac");
                std::fs::write(&path, b"audio")?;
                let mut library = library_with_item(&path)?;
                let query = if empty {
                    Query::parse("title:=Absent")?
                } else {
                    Query::all()
                };
                let plan = RemovalPlan::build(&library, &query, delete_files)?.approve(&library)?;
                let plan_id = plan
                    .id()
                    .ok_or_else(|| Error::Operation("removal fixture has no plan ID".into()))?
                    .clone();
                let (result, hits) = crate::failpoints::run_failing(0, || {
                    RemovalExecutor::new(&mut library).execute(plan)
                });

                assert!(result.is_err());
                assert_eq!(hits.first(), Some(&"db.plan-transition"));
                let state = library.durable_plan(&plan_id)?.state();
                if state != PlanState::Failed {
                    wrong_states.push((delete_files, empty, state));
                }
                let journals: u64 = library.conn.query_row(
                    "SELECT COUNT(*) FROM operation_journal WHERE kind = 'remove-delete'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(journals, 0);
                assert_eq!(std::fs::read(&path)?, b"audio");
                assert_eq!(library.query_items(&Query::all())?.len(), 1);
            }
        }
        assert!(
            wrong_states.is_empty(),
            "committed start errors left removal plans unfinished: {wrong_states:?}"
        );
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
    fn purge_rollback_keeps_the_restored_quarantine_selectable() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        RemovalExecutor::new(&mut library).execute(removal)?;
        let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
        let source = &purge.files[0].file;
        let journal = JournalFile {
            source: source.staged.clone(),
            staged: purge_staging_path(&source.staged, uuid::Uuid::new_v4())?,
            destination: source.staged.clone(),
            content_hash: source.content_hash.clone(),
            sha256: source.sha256.clone(),
            source_identity: source.owned_identity.clone(),
            owned_identity: source.owned_identity.clone(),
            role: "purge-retained-file".into(),
            state: "prepared".into(),
        };
        let plan_id = purge
            .plan_id
            .as_ref()
            .ok_or_else(|| Error::Recovery("approved purge has no durable plan ID".into()))?;
        library.start_durable_plan(plan_id)?;
        let operation = library.create_operation_for_plan(
            OperationKind::PurgeDelete,
            std::slice::from_ref(&journal),
            Some(plan_id.as_str()),
            None,
        )?;
        library.set_operation_state(&operation, "quarantining", None)?;
        std::fs::rename(&journal.source, &journal.staged)?;
        library.set_file_state(&operation, 0, "quarantined")?;

        assert_eq!(
            library.recover_pending()?.unresolved,
            [] as [std::string::String; 0]
        );
        assert_eq!(std::fs::read(&journal.source)?, b"audio");
        assert_eq!(library.durable_plan(plan_id)?.state(), PlanState::Failed);
        assert_eq!(PurgePlan::build(&library, 0)?.len(), 1);
        let retry = PurgePlan::build(&library, 0)?.approve(&library)?;
        assert_eq!(
            PurgeExecutor::new(&mut library)
                .execute(&retry)?
                .purged_files,
            1
        );
        assert!(PurgePlan::build(&library, 0)?.is_empty());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn purge_consumes_already_missing_quarantines() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        RemovalExecutor::new(&mut library).execute(removal)?;
        let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
        std::fs::remove_file(&purge.files[0].file.staged)?;

        let report = PurgeExecutor::new(&mut library).execute(&purge)?;
        assert_eq!(report.already_missing, 1);
        assert_eq!(report.purged_files, 0);
        assert!(PurgePlan::build(&library, 0)?.is_empty());
        assert_eq!(
            library.recover_pending()?.unresolved,
            [] as [std::string::String; 0]
        );
        assert!(PurgePlan::build(&library, 0)?.is_empty());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn committed_purge_recovery_consumes_missing_quarantines() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        let second_path = temporary.path().join("second.flac");
        std::fs::write(&path, b"audio")?;
        std::fs::write(&second_path, b"other audio")?;
        let mut library = library_with_item(&path)?;
        let mut second_item = library.query_items(&Query::all())?[0].clone();
        second_item.id = None;
        second_item.path = second_path;
        let import = library.create_operation(OperationKind::ImportCopy, &[])?;
        library.commit_import(&import, None, &[second_item])?;
        library.complete_operation(&import)?;
        let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        RemovalExecutor::new(&mut library).execute(removal)?;
        let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
        assert_eq!(purge.len(), 2);
        std::fs::remove_file(&purge.files[0].file.staged)?;
        library.conn.execute_batch(
            "CREATE TEMP TRIGGER interrupt_committed_purge
             AFTER UPDATE OF state ON operation_journal
             WHEN NEW.kind = 'purge-delete' AND NEW.state = 'db-committed'
               AND OLD.state != NEW.state
             BEGIN SELECT RAISE(FAIL, 'injected failure after purge commit'); END;",
        )?;

        assert!(PurgeExecutor::new(&mut library).execute(&purge).is_err());
        let plan_id = purge
            .plan_id
            .as_ref()
            .ok_or_else(|| Error::Recovery("approved purge has no durable plan ID".into()))?;
        assert_eq!(library.durable_plan(plan_id)?.state(), PlanState::Complete);
        assert!(PurgePlan::build(&library, 0)?.is_empty());
        assert!(purge.paths().all(|path| !path.exists()));
        assert_eq!(
            library.recover_pending()?.unresolved,
            [] as [std::string::String; 0]
        );
        assert!(PurgePlan::build(&library, 0)?.is_empty());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn purge_journal_creation_failure_finishes_the_durable_plan() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        RemovalExecutor::new(&mut library).execute(removal)?;
        let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
        library.conn.execute_batch(
            "CREATE TEMP TRIGGER reject_purge_journal
             BEFORE INSERT ON operation_journal WHEN NEW.kind = 'purge-delete'
             BEGIN SELECT RAISE(ABORT, 'injected purge journal failure'); END;",
        )?;

        assert!(PurgeExecutor::new(&mut library).execute(&purge).is_err());
        let plan_id = purge
            .plan_id
            .as_ref()
            .ok_or_else(|| Error::Recovery("approved purge has no durable plan ID".into()))?;
        assert_eq!(library.durable_plan(plan_id)?.state(), PlanState::Failed);
        assert!(library.durable_plan(plan_id)?.error().is_some());
        assert_eq!(std::fs::read(&purge.files[0].file.staged)?, b"audio");
        assert_eq!(
            library.recover_pending()?.unresolved,
            [] as [std::string::String; 0]
        );
        assert_eq!(PurgePlan::build(&library, 0)?.len(), 1);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn purge_validation_failure_finishes_the_durable_plan() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.flac");
        let preserved = temporary.path().join("original.flac");
        std::fs::write(&path, b"audio")?;
        let mut library = library_with_item(&path)?;
        let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        RemovalExecutor::new(&mut library).execute(removal)?;
        let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
        let quarantined = &purge.files[0].file.staged;
        std::fs::rename(quarantined, &preserved)?;
        std::fs::write(quarantined, b"replacement")?;

        let result = PurgeExecutor::new(&mut library).execute(&purge);
        assert!(result.is_err(), "purge accepted a changed quarantine");
        let plan_id = purge
            .plan_id
            .as_ref()
            .ok_or_else(|| Error::Recovery("approved purge has no durable plan ID".into()))?;
        assert_eq!(library.durable_plan(plan_id)?.state(), PlanState::Failed);
        assert!(library.durable_plan(plan_id)?.error().is_some());
        assert_eq!(std::fs::read(quarantined)?, b"replacement");
        assert_eq!(std::fs::read(preserved)?, b"audio");
        assert_eq!(
            library.recover_pending()?.unresolved,
            [] as [std::string::String; 0]
        );
        assert_eq!(library.durable_plan(plan_id)?.state(), PlanState::Failed);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn assert_purge_preserves_replacement(state: &str, path_column: &str) -> Result<()> {
        for replacement in [b"replacement".as_slice(), b"audio".as_slice()] {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join("track.flac");
            let original = temporary.path().join("preserved-original.flac");
            std::fs::write(&path, b"audio")?;
            let mut library = library_with_item(&path)?;
            let removal = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
            RemovalExecutor::new(&mut library).execute(removal)?;
            let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
            let preserved = original.clone();
            library.conn.create_scalar_function(
                "replace_purge_file",
                1,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                move |context| {
                    let current: String = context.get(0)?;
                    std::fs::rename(&current, &preserved)
                        .and_then(|()| std::fs::write(&current, replacement))
                        .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
                    Ok(0_i64)
                },
            )?;
            library.conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER replace_during_purge
                 AFTER UPDATE OF state ON operation_journal
                 WHEN NEW.kind = 'purge-delete' AND NEW.state = '{state}' AND OLD.state != NEW.state
                 BEGIN
                   SELECT replace_purge_file({path_column}) FROM operation_files
                   WHERE operation_id = NEW.id AND ordinal = 0;
                 END;"
            ))?;

            let result = PurgeExecutor::new(&mut library).execute(&purge);
            let survivors = std::fs::read_dir(temporary.path())?
                .map(|entry| Ok(entry?.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            assert!(
                survivors.iter().any(|path| path != &original
                    && std::fs::read(path).is_ok_and(|bytes| bytes == replacement)),
                "purge deleted the replacement at state {state}: {result:?}"
            );
            assert!(result.is_err(), "purge accepted a changed file: {result:?}");
            assert_eq!(std::fs::read(&original)?, b"audio");
            for _ in 0..2 {
                library.recover_pending()?;
                assert_eq!(std::fs::read(&original)?, b"audio");
                assert!(survivors.iter().any(|path| path != &original
                    && std::fs::read(path).is_ok_and(|bytes| bytes == replacement)));
            }
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn purge_preserves_replacements_before_acquisition() -> Result<()> {
        assert_purge_preserves_replacement("quarantining", "source_path")
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn purge_preserves_replacements_before_deletion() -> Result<()> {
        assert_purge_preserves_replacement("db-committed", "staged_path")
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn purge_preserves_symbolic_link_targets() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let target = temporary.path().join("target.flac");
        let retained = temporary.path().join("retained-link.flac");
        std::fs::write(&target, b"audio")?;
        std::os::unix::fs::symlink(&target, &retained)?;
        let identity = file_identity(&std::fs::symlink_metadata(&retained)?);
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(
            OperationKind::RemoveDelete,
            &[JournalFile {
                source: temporary.path().join("original-link.flac"),
                staged: retained.clone(),
                destination: temporary.path().join("original-link.flac"),
                content_hash: Some(journal_hash_path(&retained, "symlink")?),
                sha256: None,
                source_identity: Some(identity.clone()),
                owned_identity: Some(identity),
                role: "symlink".into(),
                state: "quarantined".into(),
            }],
        )?;
        library.set_file_state(&operation, 0, "quarantined")?;
        library.complete_operation(&operation)?;
        let purge = PurgePlan::build(&library, 0)?.approve(&library)?;
        assert_eq!(purge.len(), 1);
        let result = PurgeExecutor::new(&mut library).execute(&purge);
        assert!(
            result.is_ok(),
            "purge rejected a retained symbolic link: {result:?}"
        );
        assert!(!retained.is_symlink());
        assert_eq!(std::fs::read(&target)?, b"audio");
        assert_eq!(
            library.recover_pending()?.unresolved,
            [] as [std::string::String; 0]
        );
        assert_eq!(std::fs::read(target)?, b"audio");
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
            extended: crate::ExtendedMetadata::default(),
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
            singleton: false,
            extended: crate::ExtendedMetadata::default(),
        };
        library.commit_import(&operation, Some(&album), &[item])?;
        library.complete_operation(&operation)?;

        let plan = RemovalPlan::build(&library, &Query::all(), true)?.approve(&library)?;
        assert_eq!(plan.files.len(), 2);
        let report = RemovalExecutor::new(&mut library).execute(plan)?;
        assert_eq!(report.quarantined_files, 2);
        assert!(!track.exists());
        assert!(!artwork.exists());
        assert_eq!(PurgePlan::build(&library, 0)?.len(), 2);
        assert_eq!(library.audit()?.issues(), []);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn album_removal_after_artwork_replacement_preserves_archived_versions() -> Result<()> {
        use crate::artwork::{
            original_path, validate_artwork, ArtworkAssetMetadata, ArtworkProvenance, ArtworkRole,
        };
        use crate::artwork_projection::ExternalArtworkExecutor;

        fn png(color: [u8; 3]) -> Result<Vec<u8>> {
            let image = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
                4,
                3,
                image::Rgb(color),
            ));
            let mut output = std::io::Cursor::new(Vec::new());
            image.write_to(&mut output, image::ImageFormat::Png)?;
            Ok(output.into_inner())
        }

        let temporary = tempfile::tempdir()?;
        let track = temporary.path().join("track.flac");
        let cover = temporary.path().join("cover.png");
        let original_bytes = png([2, 4, 8])?;
        let previous_bytes = png([9, 7, 5])?;
        let validated = validate_artwork(&original_bytes)?;
        let original = original_path(temporary.path(), &validated);
        let original_parent = original
            .parent()
            .ok_or_else(|| Error::Artwork("original fixture has no parent".into()))?;
        std::fs::create_dir_all(original_parent)?;
        std::fs::write(&original, &original_bytes)?;
        std::fs::write(&cover, &previous_bytes)?;
        std::fs::write(&track, b"audio")?;
        let metadata = ArtworkAssetMetadata::from_validated(
            original.clone(),
            &validated,
            ArtworkProvenance {
                role: ArtworkRole::Front,
                source_provider: None,
                source_reference: None,
                provider_release_id: None,
                exact_release: false,
                rights: None,
            },
        );
        let mut library = library_with_artwork_at_root(
            &track,
            Some(temporary.path()),
            Some(&cover),
            Some(&metadata),
        )?;
        let target_id: String =
            library
                .conn
                .query_row("SELECT id FROM assets WHERE role = 'artwork'", [], |row| {
                    row.get(0)
                })?;
        let original_id: String = library.conn.query_row(
            "SELECT id FROM assets WHERE role = 'artwork-original'",
            [],
            |row| row.get(0),
        )?;
        let projection = library.plan_external_artwork(&target_id, Some(&original_id))?;
        library.approve_external_artwork(&projection)?;
        let receipt = ExternalArtworkExecutor::new(&mut library).execute(&projection)?;

        let result = RemovalPlan::build(&library, &Query::all(), true)
            .and_then(|plan| plan.approve(&library));
        assert!(
            result.is_ok(),
            "artwork replacement blocked whole-album removal: {result:?}"
        );
        let plan = result?;
        assert_eq!(plan.files.len(), 2);
        let report = RemovalExecutor::new(&mut library).execute(plan)?;
        assert_eq!(report.quarantined_files, 2);
        assert!(library.query_items(&Query::all())?.is_empty());
        assert!(!track.exists());
        assert!(!cover.exists());
        assert_eq!(std::fs::read(&original)?, original_bytes);
        assert_eq!(std::fs::read(receipt.retained_previous())?, previous_bytes);
        let purge = PurgePlan::build(&library, 0)?;
        assert_eq!(purge.len(), 2);
        assert!(purge
            .paths()
            .all(|path| path != original && path != receipt.retained_previous()));
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
