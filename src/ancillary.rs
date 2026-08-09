//! Typed ancillary-asset discovery and journaled import.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use walkdir::WalkDir;

use crate::asset::{digest_file, digest_reader};
use crate::db::{file_identity, file_object_identity, JournalFile, Library, OperationKind};
use crate::fsops::AnchoredRoot;
use crate::naming::{collision_key, sanitize_component, NamingProfile};
use crate::operations::{PlanId, PlanKind, PlanState};
use crate::roots::{RootId, RootState};
use crate::tags::is_audio_file;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AncillaryKind {
    CueSheet,
    RipLog,
    Checksum,
    Lyrics,
    Pdf,
    Scan,
    Booklet,
    DataFile,
    Other,
}

impl AncillaryKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CueSheet => "cue-sheet",
            Self::RipLog => "rip-log",
            Self::Checksum => "checksum",
            Self::Lyrics => "lyrics",
            Self::Pdf => "pdf",
            Self::Scan => "scan",
            Self::Booklet => "booklet",
            Self::DataFile => "data-file",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AncillaryCandidate {
    source: PathBuf,
    kind: AncillaryKind,
    media_type: String,
}

impl AncillaryCandidate {
    pub fn from_path(path: &Path) -> Result<Self> {
        let source = std::fs::canonicalize(path)?;
        if source.to_str().is_none() {
            return Err(Error::Import(
                "ancillary source path must be valid UTF-8".into(),
            ));
        }
        let metadata = std::fs::metadata(&source)?;
        if !metadata.is_file() {
            return Err(Error::Import(
                "ancillary source must be a regular file".into(),
            ));
        }
        if is_audio_file(&source) {
            return Err(Error::Import(
                "audio files must use the audio import workflow".into(),
            ));
        }
        let (kind, media_type) = classify(&source);
        Ok(Self {
            source,
            kind,
            media_type: media_type.into(),
        })
    }

    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    #[must_use]
    pub const fn kind(&self) -> AncillaryKind {
        self.kind
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AncillaryScan {
    candidates: Vec<AncillaryCandidate>,
    issues: Vec<String>,
}

impl AncillaryScan {
    #[must_use]
    pub fn candidates(&self) -> &[AncillaryCandidate] {
        &self.candidates
    }

    #[must_use]
    pub fn issues(&self) -> &[String] {
        &self.issues
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlannedAncillary {
    asset_id: String,
    source: PathBuf,
    destination_relative: PathBuf,
    destination: PathBuf,
    kind: AncillaryKind,
    media_type: String,
    original_filename: String,
    byte_size: u64,
    blake3: String,
    sha256: String,
    source_identity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AncillaryImportPlan {
    id: PlanId,
    root_id: RootId,
    root_path: PathBuf,
    parent_asset_id: Option<String>,
    relationship: String,
    files: Vec<PlannedAncillary>,
    byte_count: u64,
}

impl AncillaryImportPlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    pub fn destinations(&self) -> impl Iterator<Item = &Path> {
        self.files.iter().map(|file| file.destination.as_path())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AncillaryImportReceipt {
    plan_id: PlanId,
    operation_id: String,
    asset_ids: Vec<String>,
    byte_count: u64,
}

/// Discover every non-audio regular file, retaining unsupported files as `Other`.
#[must_use]
pub fn scan_ancillary_paths(paths: &[PathBuf], follow_symlinks: bool) -> AncillaryScan {
    let mut scan = AncillaryScan::default();
    let mut seen = HashSet::new();
    for input in paths {
        for entry in WalkDir::new(input).follow_links(follow_symlinks) {
            match entry {
                Ok(entry) if entry.file_type().is_file() => {
                    match AncillaryCandidate::from_path(entry.path()) {
                        Ok(candidate) if seen.insert(candidate.source.clone()) => {
                            scan.candidates.push(candidate);
                        }
                        Ok(_) => {}
                        Err(Error::Import(message))
                            if message == "audio files must use the audio import workflow" => {}
                        Err(error) => scan
                            .issues
                            .push(format!("{}: {error}", entry.path().to_string_lossy())),
                    }
                }
                Ok(_) => {}
                Err(error) => scan.issues.push(error.to_string()),
            }
        }
    }
    scan.candidates
        .sort_by(|left, right| left.source.cmp(&right.source));
    scan
}

impl Library {
    pub fn plan_ancillary_import(
        &self,
        root_id: &RootId,
        destination_directory: &Path,
        candidates: &[AncillaryCandidate],
        parent_asset_id: Option<&str>,
        relationship: &str,
        profile: NamingProfile,
    ) -> Result<AncillaryImportPlan> {
        if candidates.is_empty() {
            return Err(Error::Import("ancillary import has no files".into()));
        }
        validate_relationship(relationship)?;
        if let Some(parent) = parent_asset_id {
            uuid::Uuid::parse_str(parent)
                .map_err(|error| Error::Import(format!("invalid parent asset ID: {error}")))?;
            let exists = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM assets WHERE id = ?1)",
                [parent],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                return Err(Error::Import("parent asset does not exist".into()));
            }
        }
        let root = self.library_root(root_id)?;
        if root.state() != RootState::Online {
            return Err(Error::Root(
                "ancillary destination root must be online".into(),
            ));
        }
        root.capabilities().require_safe_mutation()?;
        let output_root = AnchoredRoot::open(root.path())?;
        let destination_directory =
            crate::naming::sanitize_relative_path(destination_directory, profile)?;
        let mut names = HashSet::new();
        let mut files = Vec::with_capacity(candidates.len());
        let mut byte_count = 0_u64;
        for candidate in candidates {
            let original_filename = candidate
                .source
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::Import("ancillary filename is invalid UTF-8".into()))?;
            let filename = sanitize_component(original_filename, profile)?;
            if !names.insert(collision_key(&filename)) {
                return Err(Error::Import(format!(
                    "ancillary filenames collide under the selected profile: {filename}"
                )));
            }
            let destination_relative = destination_directory.join(filename);
            let destination = root.path().join(&destination_relative);
            require_destination_absent(&output_root, &destination)?;
            let before = std::fs::metadata(&candidate.source)?;
            let digests = digest_file(&candidate.source)?;
            let after = std::fs::metadata(&candidate.source)?;
            let identity = file_identity(&before);
            if identity != file_identity(&after) || before.len() != digests.byte_size() {
                return Err(Error::Import(format!(
                    "ancillary source changed while planning: {}",
                    candidate.source.display()
                )));
            }
            byte_count = byte_count.saturating_add(digests.byte_size());
            files.push(PlannedAncillary {
                asset_id: uuid::Uuid::new_v4().to_string(),
                source: candidate.source.clone(),
                destination_relative,
                destination,
                kind: candidate.kind,
                media_type: candidate.media_type.clone(),
                original_filename: original_filename.into(),
                byte_size: digests.byte_size(),
                blake3: digests.blake3().into(),
                sha256: digests.sha256().into(),
                source_identity: identity,
            });
        }
        let request = json!({
            "root_id": root_id,
            "destination_directory": destination_directory,
            "parent_asset_id": parent_asset_id,
            "relationship": relationship,
            "profile": profile,
        });
        let preview = json!({
            "file_count": files.len(),
            "byte_count": byte_count,
            "kinds": files.iter().map(|file| file.kind).collect::<Vec<_>>(),
            "no_clobber": true,
        });
        let total = u64::try_from(files.len())
            .map_err(|error| Error::Import(format!("ancillary plan is too large: {error}")))?;
        let id =
            self.create_durable_plan(PlanKind::AncillaryImport, &request, &preview, Some(total))?;
        Ok(AncillaryImportPlan {
            id,
            root_id: root_id.clone(),
            root_path: root.path().to_path_buf(),
            parent_asset_id: parent_asset_id.map(str::to_owned),
            relationship: relationship.into(),
            files,
            byte_count,
        })
    }

    pub fn approve_ancillary_import(&self, plan: &AncillaryImportPlan) -> Result<()> {
        self.approve_durable_plan(plan.id())
    }

    pub fn execute_ancillary_import(
        &mut self,
        plan: &AncillaryImportPlan,
    ) -> Result<AncillaryImportReceipt> {
        self.start_durable_plan(plan.id())?;
        let root_record = self.library_root(&plan.root_id)?;
        if root_record.path() != plan.root_path || root_record.state() != RootState::Online {
            return Err(self.fail_ancillary(
                plan.id(),
                Error::Root("ancillary root changed after preview".into()),
            ));
        }
        root_record.capabilities().require_safe_mutation()?;
        let root = AnchoredRoot::open(&plan.root_path)?;
        for file in &plan.files {
            revalidate_source(file)?;
            require_destination_absent(&root, &file.destination)?;
        }
        let transfer = uuid::Uuid::new_v4();
        let journals = plan
            .files
            .iter()
            .map(|file| {
                Ok(JournalFile {
                    source: file.source.clone(),
                    staged: sibling_stage(&file.destination, transfer)?,
                    destination: file.destination.clone(),
                    content_hash: Some(file.blake3.clone()),
                    sha256: Some(file.sha256.clone()),
                    source_identity: Some(file.source_identity.clone()),
                    owned_identity: None,
                    role: format!("ancillary-{}", file.kind.as_str()),
                    state: "prepared".into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let operation = match self.create_operation_for_plan(
            OperationKind::AncillaryCopy,
            &journals,
            Some(plan.id().as_str()),
            Some(&json!({
                "root_id": plan.root_id,
                "parent_asset_id": plan.parent_asset_id,
                "relationship": plan.relationship,
            })),
        ) {
            Ok(operation) => operation,
            Err(error) => return Err(self.fail_ancillary(plan.id(), error)),
        };
        match self.execute_ancillary_journal(plan, &root, &operation, &journals) {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(self.fail_ancillary_operation(plan.id(), &operation, error)),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "ordered staging, publication, and catalog commit is one safety protocol"
    )]
    fn execute_ancillary_journal(
        &self,
        plan: &AncillaryImportPlan,
        root: &AnchoredRoot,
        operation: &str,
        journals: &[JournalFile],
    ) -> Result<AncillaryImportReceipt> {
        self.set_operation_state(operation, "staging", None)?;
        let mut identities: Vec<(String, chrono::DateTime<Utc>)> =
            Vec::with_capacity(plan.files.len());
        for (ordinal, (file, journal)) in plan.files.iter().zip(journals).enumerate() {
            root.create_parent_all(&journal.staged)?;
            root.copy_new_observed(&file.source, &journal.staged, |metadata| {
                self.set_acquired_file_identity(operation, ordinal, &file_object_identity(metadata))
            })?;
            let digest = digest_reader(root.open_file(&journal.staged)?)?;
            let metadata = root.entry_metadata(&journal.staged)?;
            if digest.byte_size() != file.byte_size
                || digest.blake3() != file.blake3
                || digest.sha256() != file.sha256
            {
                return Err(Error::Import(format!(
                    "ancillary staging validation failed: {}",
                    file.source.display()
                )));
            }
            let identity = file_identity(&metadata);
            self.set_staged_file_full_evidence(
                operation,
                ordinal,
                &identity,
                digest.blake3(),
                digest.sha256(),
            )?;
            self.conn.execute(
                "UPDATE operation_files
                 SET root_id = ?3, staged_relative_path = ?4,
                     destination_relative_path = ?5, asset_id = ?6
                 WHERE operation_id = ?1 AND ordinal = ?2",
                params![
                    operation,
                    ordinal,
                    plan.root_id.as_str(),
                    path_text(
                        journal
                            .staged
                            .strip_prefix(&plan.root_path)
                            .map_err(|_error| Error::Recovery(
                                "ancillary stage escaped root".into()
                            ))?
                    )?,
                    path_text(&file.destination_relative)?,
                    file.asset_id,
                ],
            )?;
            identities.push((identity, metadata.modified()?.into()));
        }
        self.set_operation_state(operation, "publishing", None)?;
        for (ordinal, journal) in journals.iter().enumerate() {
            root.rename_noreplace(&journal.staged, &journal.destination)?;
            self.set_file_state(operation, ordinal, "published")?;
        }
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        for (file, (identity, mtime)) in plan.files.iter().zip(&identities) {
            transaction.execute(
                "INSERT INTO assets
                 (id, root_id, relative_path, absolute_path, role, managed,
                  verification_state, byte_size, blake3, sha256, mtime,
                  entry_identity, projection_state, first_seen_at, last_verified_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, 'verified', ?6, ?7, ?8, ?9,
                         ?10, 'current', ?11, ?11)",
                params![
                    file.asset_id,
                    plan.root_id.as_str(),
                    path_text(&file.destination_relative)?,
                    path_text(&file.destination)?,
                    file.kind.as_str(),
                    file.byte_size,
                    file.blake3,
                    file.sha256,
                    mtime.to_rfc3339(),
                    identity,
                    now
                ],
            )?;
            transaction.execute(
                "INSERT INTO ancillary_metadata
                 (asset_id, kind, media_type, original_filename, imported_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    file.asset_id,
                    file.kind.as_str(),
                    file.media_type,
                    file.original_filename,
                    now
                ],
            )?;
            if let Some(parent) = &plan.parent_asset_id {
                transaction.execute(
                    "INSERT INTO asset_relationships
                     (parent_asset_id, child_asset_id, relationship)
                     VALUES (?1, ?2, ?3)",
                    params![parent, file.asset_id, plan.relationship],
                )?;
            }
        }
        transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?2
             WHERE id = ?1",
            params![operation, now],
        )?;
        transaction.execute(
            "UPDATE durable_plans
             SET state = 'complete', progress_current = progress_total,
                 updated_at = ?2, completed_at = ?2
             WHERE id = ?1 AND state = 'running'",
            params![plan.id().as_str(), now],
        )?;
        transaction.commit()?;
        self.complete_operation(operation)?;
        Ok(AncillaryImportReceipt {
            plan_id: plan.id.clone(),
            operation_id: operation.into(),
            asset_ids: plan
                .files
                .iter()
                .map(|file| file.asset_id.clone())
                .collect(),
            byte_count: plan.byte_count,
        })
    }

    fn fail_ancillary(&mut self, plan_id: &PlanId, error: Error) -> Error {
        let _ = self.finish_durable_plan(plan_id, PlanState::Failed, Some(&error.to_string()));
        let _ = self.recover_pending();
        error
    }

    fn fail_ancillary_operation(
        &mut self,
        plan_id: &PlanId,
        operation: &str,
        error: Error,
    ) -> Error {
        let _ = self.record_operation_failure(operation, &error.to_string());
        if self
            .durable_plan(plan_id)
            .is_ok_and(|plan| plan.state() == PlanState::Running)
        {
            let _ = self.finish_durable_plan(plan_id, PlanState::Failed, Some(&error.to_string()));
        }
        let _ = self.recover_pending();
        error
    }
}

fn revalidate_source(file: &PlannedAncillary) -> Result<()> {
    let before = std::fs::metadata(&file.source)?;
    if file_identity(&before) != file.source_identity || before.len() != file.byte_size {
        return Err(Error::Import(format!(
            "ancillary source changed after preview: {}",
            file.source.display()
        )));
    }
    let digest = digest_file(&file.source)?;
    let after = std::fs::metadata(&file.source)?;
    if digest.blake3() != file.blake3
        || digest.sha256() != file.sha256
        || file_identity(&after) != file.source_identity
    {
        return Err(Error::Import(format!(
            "ancillary source changed during revalidation: {}",
            file.source.display()
        )));
    }
    Ok(())
}

fn require_destination_absent(root: &AnchoredRoot, destination: &Path) -> Result<()> {
    match root.entry_metadata(destination) {
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(Error::Import(format!(
            "ancillary destination already exists: {}",
            destination.display()
        ))),
    }
}

fn sibling_stage(destination: &Path, transfer: uuid::Uuid) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .ok_or_else(|| Error::Import("ancillary destination has no filename".into()))?;
    let mut staged = std::ffi::OsString::from(".");
    staged.push(name);
    staged.push(format!(".rsbts-{transfer}.ancillary-stage"));
    Ok(destination.with_file_name(staged))
}

fn validate_relationship(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(Error::Import(
            "ancillary relationship must be 1-64 lowercase ASCII letters, digits, or hyphens"
                .into(),
        ));
    }
    Ok(())
}

fn classify(path: &Path) -> (AncillaryKind, &'static str) {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "cue" => (AncillaryKind::CueSheet, "application/x-cue"),
        "log" => (AncillaryKind::RipLog, "text/plain"),
        "md5" | "sfv" | "sha1" | "sha256" | "sha512" => (AncillaryKind::Checksum, "text/plain"),
        "lrc" => (AncillaryKind::Lyrics, "text/plain"),
        "pdf" if name.contains("booklet") => (AncillaryKind::Booklet, "application/pdf"),
        "pdf" => (AncillaryKind::Pdf, "application/pdf"),
        "jpg" | "jpeg" => (AncillaryKind::Scan, "image/jpeg"),
        "png" => (AncillaryKind::Scan, "image/png"),
        "tif" | "tiff" => (AncillaryKind::Scan, "image/tiff"),
        "bin" | "iso" => (AncillaryKind::DataFile, "application/octet-stream"),
        _ => (AncillaryKind::Other, "application/octet-stream"),
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::Import("path must be valid UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixity::{FixityMode, FixityResultState};

    #[test]
    fn ancillary_scan_classifies_known_files_and_retains_unknown_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        std::fs::write(temporary.path().join("disc.cue"), b"FILE track.flac WAVE")?;
        std::fs::write(temporary.path().join("rip.log"), b"Exact Audio Copy")?;
        std::fs::write(temporary.path().join("notes.xyz"), b"notes")?;
        let scan = scan_ancillary_paths(&[temporary.path().to_path_buf()], false);
        assert!(scan.issues().is_empty());
        assert_eq!(scan.candidates().len(), 3);
        assert!(scan
            .candidates()
            .iter()
            .any(|candidate| candidate.kind() == AncillaryKind::CueSheet));
        assert!(scan
            .candidates()
            .iter()
            .any(|candidate| candidate.kind() == AncillaryKind::Other));
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn ancillary_import_is_approved_related_and_included_in_fixity() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("disc.cue");
        std::fs::write(&source, b"FILE track.flac WAVE")?;
        let root = temporary.path().join("library");
        std::fs::create_dir(&root)?;
        let mut library = Library::open_in_memory()?;
        let root_id = library.register_root(&root)?.id().clone();
        let candidate = AncillaryCandidate::from_path(&source)?;
        let plan = library.plan_ancillary_import(
            &root_id,
            Path::new("Artist/Album"),
            &[candidate],
            None,
            "release-ancillary",
            NamingProfile::Portable,
        )?;
        assert!(library.execute_ancillary_import(&plan).is_err());
        library.approve_ancillary_import(&plan)?;
        let receipt = library.execute_ancillary_import(&plan)?;
        assert_eq!(receipt.asset_ids.len(), 1);
        assert_eq!(
            std::fs::read(root.join("Artist/Album/disc.cue"))?,
            b"FILE track.flac WAVE"
        );
        let fixity = library.plan_fixity(FixityMode::Deep)?;
        library.approve_fixity(&fixity)?;
        let progress = library.run_fixity_page(&fixity, 10)?;
        assert!(progress.complete());
        assert_eq!(progress.results()[0].state(), FixityResultState::Ok);
        Ok(())
    }
}
