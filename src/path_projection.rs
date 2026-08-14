//! Previewable, journaled path projections for verified managed assets.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::asset::digest_reader;
use crate::db::{file_identity, JournalFile, Library, OperationKind};
use crate::fsops::AnchoredRoot;
use crate::naming::{collision_key, sanitize_relative_path, NamingProfile};
use crate::operations::{PlanId, PlanKind, PlanState};
use crate::roots::{RootCapabilities, RootId};
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathProjectionPlan {
    id: PlanId,
    asset_id: String,
    root_id: RootId,
    root_path: PathBuf,
    source_relative: PathBuf,
    destination_relative: PathBuf,
    source: PathBuf,
    destination: PathBuf,
    role: String,
    byte_size: u64,
    blake3: String,
    sha256: String,
    entry_identity: String,
    profile: NamingProfile,
}

impl PathProjectionPlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathProjectionReceipt {
    plan_id: PlanId,
    operation_id: String,
    asset_id: String,
    source: PathBuf,
    destination: PathBuf,
}

impl Library {
    /// Persist a path preview without changing the filesystem or catalog path.
    pub fn plan_path_projection(
        &self,
        asset_id: &str,
        requested_relative: &Path,
        profile: NamingProfile,
    ) -> Result<PathProjectionPlan> {
        self.build_path_projection(asset_id, requested_relative, profile, true)
    }

    /// Run the exact rename validation without persisting a durable plan.
    pub fn preview_path_projection(
        &self,
        asset_id: &str,
        requested_relative: &Path,
        profile: NamingProfile,
    ) -> Result<PathProjectionPlan> {
        self.build_path_projection(asset_id, requested_relative, profile, false)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the preview performs one ordered path-safety validation protocol"
    )]
    fn build_path_projection(
        &self,
        asset_id: &str,
        requested_relative: &Path,
        profile: NamingProfile,
        persist: bool,
    ) -> Result<PathProjectionPlan> {
        uuid::Uuid::parse_str(asset_id)
            .map_err(|error| Error::Operation(format!("invalid asset ID: {error}")))?;
        let destination_relative = sanitize_relative_path(requested_relative, profile)?;
        if destination_relative != requested_relative {
            return Err(Error::PathFormat(format!(
                "requested path is not already valid for {profile:?}; preview the sanitized path explicitly: {}",
                destination_relative.display()
            )));
        }
        let evidence = self
            .conn
            .query_row(
                "SELECT a.root_id, lr.path, lr.state, lr.capabilities_json,
                        a.relative_path, a.absolute_path, a.role, a.byte_size,
                        a.blake3, a.sha256, a.entry_identity,
                        a.verification_state, a.managed
                 FROM assets a JOIN library_roots lr ON lr.id = a.root_id
                 WHERE a.id = ?1",
                [asset_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        PathBuf::from(row.get::<_, String>(1)?),
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        PathBuf::from(row.get::<_, String>(4)?),
                        PathBuf::from(row.get::<_, String>(5)?),
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<u64>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, String>(11)?,
                        row.get::<_, bool>(12)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Error::Operation("asset does not exist".into()))?;
        let (
            root_id,
            root_path,
            root_state,
            capabilities,
            source_relative,
            source,
            role,
            byte_size,
            blake3,
            sha256,
            entry_identity,
            verification_state,
            managed,
        ) = evidence;
        if !managed || verification_state != "verified" || root_state != "online" {
            return Err(Error::Operation(
                "only verified managed assets on an online root may be renamed".into(),
            ));
        }
        serde_json::from_str::<RootCapabilities>(&capabilities)?.require_safe_mutation()?;
        if root_path.join(&source_relative) != source {
            return Err(Error::Recovery(
                "asset root-relative and compatibility paths disagree".into(),
            ));
        }
        if source.extension() != destination_relative.extension() {
            return Err(Error::PathFormat(
                "path projection must preserve the file extension".into(),
            ));
        }
        if source_relative == destination_relative {
            return Err(Error::PathFormat(
                "path projection does not change the path".into(),
            ));
        }
        let destination = root_path.join(&destination_relative);
        let root = AnchoredRoot::open(&root_path)?;
        require_absent(&root, &destination)?;
        reject_collision_key(&self.conn, &root_id, asset_id, &destination_relative)?;
        let byte_size =
            byte_size.ok_or_else(|| Error::Recovery("verified asset has no byte size".into()))?;
        let blake3 =
            blake3.ok_or_else(|| Error::Recovery("verified asset has no BLAKE3 digest".into()))?;
        let sha256 =
            sha256.ok_or_else(|| Error::Recovery("verified asset has no SHA-256 digest".into()))?;
        let entry_identity = entry_identity
            .ok_or_else(|| Error::Recovery("verified asset has no entry identity".into()))?;
        revalidate(&root, &source, byte_size, &blake3, &sha256, &entry_identity)?;
        let request = json!({
            "asset_id": asset_id,
            "destination_relative": destination_relative,
            "profile": profile,
        });
        let preview = json!({
            "source": source,
            "destination": destination,
            "byte_size": byte_size,
            "blake3": blake3,
            "sha256": sha256,
            "no_clobber": true,
        });
        let id = if persist {
            let id =
                self.create_durable_plan(PlanKind::PathProjection, &request, &preview, Some(1))?;
            let now = Utc::now().to_rfc3339();
            let transaction = self.conn.unchecked_transaction()?;
            transaction.execute(
                "INSERT INTO projection_plans
                 (id, projection_type, profile, policy_version)
                 VALUES (?1, 'paths', ?2, 1)",
                params![id.as_str(), profile_name(profile)],
            )?;
            transaction.execute(
                "INSERT INTO asset_projection_steps
                 (plan_id, asset_id, before_json, after_json, state, evidence_json)
                 VALUES (?1, ?2, ?3, ?4, 'planned', ?5)",
                params![
                    id.as_str(),
                    asset_id,
                    serde_json::to_string(&json!({"relative_path": source_relative}))?,
                    serde_json::to_string(&json!({"relative_path": destination_relative}))?,
                    serde_json::to_string(&json!({
                        "planned_at": now,
                        "entry_identity": entry_identity,
                        "blake3": blake3,
                        "sha256": sha256,
                    }))?
                ],
            )?;
            transaction.commit()?;
            id
        } else {
            PlanId::new()
        };
        Ok(PathProjectionPlan {
            id,
            asset_id: asset_id.to_owned(),
            root_id: RootId::parse(root_id)?,
            root_path,
            source_relative,
            destination_relative,
            source,
            destination,
            role,
            byte_size,
            blake3,
            sha256,
            entry_identity,
            profile,
        })
    }

    pub fn approve_path_projection(&self, plan: &PathProjectionPlan) -> Result<()> {
        self.approve_durable_plan(plan.id())?;
        self.conn.execute(
            "UPDATE projection_plans SET approved_at = ?2 WHERE id = ?1",
            params![plan.id().as_str(), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn execute_path_projection(
        &mut self,
        plan: &PathProjectionPlan,
    ) -> Result<PathProjectionReceipt> {
        self.start_durable_plan(plan.id())?;
        let root = AnchoredRoot::open(&plan.root_path)?;
        if let Err(error) = revalidate(
            &root,
            &plan.source,
            plan.byte_size,
            &plan.blake3,
            &plan.sha256,
            &plan.entry_identity,
        )
        .and_then(|()| require_absent(&root, &plan.destination))
        {
            let _ =
                self.finish_durable_plan(plan.id(), PlanState::Failed, Some(&error.to_string()));
            return Err(error);
        }
        root.create_parent_all(&plan.destination)?;
        let staged = sibling_path(&plan.source, plan.id())?;
        let journal = JournalFile {
            source: plan.source.clone(),
            staged: staged.clone(),
            destination: plan.destination.clone(),
            content_hash: Some(plan.blake3.clone()),
            sha256: Some(plan.sha256.clone()),
            source_identity: Some(plan.entry_identity.clone()),
            owned_identity: Some(plan.entry_identity.clone()),
            role: "path-projection".into(),
            state: "prepared".into(),
        };
        let operation = match self.create_operation_for_plan(
            OperationKind::PathWrite,
            &[journal],
            Some(plan.id().as_str()),
            Some(&json!({
                "asset_id": plan.asset_id,
                "profile": plan.profile,
                "source_relative": plan.source_relative,
                "destination_relative": plan.destination_relative,
            })),
        ) {
            Ok(operation) => operation,
            Err(error) => return Err(self.fail_projection(plan.id(), error)),
        };
        self.conn.execute(
            "UPDATE operation_files
             SET root_id = ?2, source_relative_path = ?3,
                 staged_relative_path = ?4, destination_relative_path = ?5
             WHERE operation_id = ?1 AND ordinal = 0",
            params![
                operation,
                plan.root_id.as_str(),
                path_text(&plan.source_relative)?,
                path_text(staged.strip_prefix(&plan.root_path).map_err(|_error| {
                    Error::Recovery("path projection stage escaped its root".into())
                })?)?,
                path_text(&plan.destination_relative)?
            ],
        )?;
        match self.execute_path_journal(plan, &root, &operation, &staged) {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(self.fail_projection_operation(plan.id(), &operation, error)),
        }
    }

    fn execute_path_journal(
        &self,
        plan: &PathProjectionPlan,
        root: &AnchoredRoot,
        operation: &str,
        staged: &Path,
    ) -> Result<PathProjectionReceipt> {
        self.set_operation_state(operation, "quarantining", None)?;
        root.rename_noreplace(&plan.source, staged)?;
        self.set_file_state(operation, 0, "quarantined")?;
        revalidate(
            root,
            staged,
            plan.byte_size,
            &plan.blake3,
            &plan.sha256,
            &plan.entry_identity,
        )?;
        root.rename_noreplace(staged, &plan.destination)?;
        self.set_file_state(operation, 0, "published")?;
        revalidate(
            root,
            &plan.destination,
            plan.byte_size,
            &plan.blake3,
            &plan.sha256,
            &plan.entry_identity,
        )?;
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        if transaction.execute(
            "UPDATE assets
             SET relative_path = ?2, absolute_path = ?3,
                 projection_state = 'current', last_verified_at = ?4
             WHERE id = ?1 AND root_id = ?5 AND relative_path = ?6
               AND entry_identity = ?7 AND blake3 = ?8 AND sha256 = ?9",
            params![
                plan.asset_id,
                path_text(&plan.destination_relative)?,
                path_text(&plan.destination)?,
                now,
                plan.root_id.as_str(),
                path_text(&plan.source_relative)?,
                plan.entry_identity,
                plan.blake3,
                plan.sha256,
            ],
        )? != 1
        {
            return Err(Error::Recovery(
                "asset catalog changed after path projection preview".into(),
            ));
        }
        transaction.execute(
            "UPDATE items SET path = ?2
             WHERE id IN (SELECT item_id FROM item_assets WHERE asset_id = ?1)",
            params![plan.asset_id, path_text(&plan.destination)?],
        )?;
        transaction.execute(
            "UPDATE albums SET artpath = ?2
             WHERE artpath = ?3
               AND id IN (SELECT album_id FROM album_assets WHERE asset_id = ?1)",
            params![
                plan.asset_id,
                path_text(&plan.destination)?,
                path_text(&plan.source)?
            ],
        )?;
        transaction.execute(
            "UPDATE asset_projection_steps
             SET state = 'published', evidence_json = ?3
             WHERE plan_id = ?1 AND asset_id = ?2 AND state = 'planned'",
            params![
                plan.id().as_str(),
                plan.asset_id,
                serde_json::to_string(&json!({
                    "published_at": now,
                    "entry_identity": plan.entry_identity,
                    "blake3": plan.blake3,
                    "sha256": plan.sha256,
                }))?
            ],
        )?;
        transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?2
             WHERE id = ?1",
            params![operation, now],
        )?;
        transaction.execute(
            "UPDATE durable_plans
             SET state = 'complete', progress_current = 1,
                 updated_at = ?2, completed_at = ?2
             WHERE id = ?1 AND state = 'running'",
            params![plan.id().as_str(), now],
        )?;
        transaction.commit()?;
        self.complete_operation(operation)?;
        Ok(PathProjectionReceipt {
            plan_id: plan.id.clone(),
            operation_id: operation.to_owned(),
            asset_id: plan.asset_id.clone(),
            source: plan.source.clone(),
            destination: plan.destination.clone(),
        })
    }

    fn fail_projection(&mut self, plan_id: &PlanId, error: Error) -> Error {
        let _ = self.finish_durable_plan(plan_id, PlanState::Failed, Some(&error.to_string()));
        let _ = self.recover_pending();
        error
    }

    fn fail_projection_operation(
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

fn revalidate(
    root: &AnchoredRoot,
    path: &Path,
    size: u64,
    blake3: &str,
    sha256: &str,
    identity: &str,
) -> Result<()> {
    let before = root.entry_metadata(path)?;
    if before.len() != size || file_identity(&before) != identity {
        return Err(Error::Operation(format!(
            "path-projection source identity changed: {}",
            path.display()
        )));
    }
    let digest = digest_reader(root.open_file(path)?)?;
    let after = root.entry_metadata(path)?;
    if digest.byte_size() != size
        || digest.blake3() != blake3
        || digest.sha256() != sha256
        || file_identity(&after) != identity
    {
        return Err(Error::Operation(format!(
            "path-projection source bytes changed: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_absent(root: &AnchoredRoot, path: &Path) -> Result<()> {
    match root.entry_metadata(path) {
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(Error::Operation(format!(
            "path-projection destination exists: {}",
            path.display()
        ))),
    }
}

fn reject_collision_key(
    connection: &rusqlite::Connection,
    root_id: &str,
    asset_id: &str,
    destination: &Path,
) -> Result<()> {
    let requested = collision_key(path_text(destination)?);
    let mut statement = connection.prepare(
        "SELECT relative_path FROM assets WHERE root_id = ?1 AND id != ?2 AND managed = 1",
    )?;
    let mut rows = statement.query(params![root_id, asset_id])?;
    while let Some(row) = rows.next()? {
        let existing = row.get::<_, String>(0)?;
        if collision_key(&existing) == requested {
            return Err(Error::PathFormat(format!(
                "path collides under compatibility case folding: {existing}"
            )));
        }
    }
    Ok(())
}

const fn profile_name(profile: NamingProfile) -> &'static str {
    match profile {
        NamingProfile::Portable => "portable",
        NamingProfile::NativeFilesystem => "native-filesystem",
        NamingProfile::Archival => "archival",
    }
}

fn sibling_path(source: &Path, plan_id: &PlanId) -> Result<PathBuf> {
    let name = source
        .file_name()
        .ok_or_else(|| Error::Operation("path-projection source has no filename".into()))?;
    let mut staged = std::ffi::OsString::from(".");
    staged.push(name);
    staged.push(format!(".rsbts-{}.path-original", plan_id.as_str()));
    Ok(source.with_file_name(staged))
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::PathFormat("path is not valid UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failpoints;

    fn library_with_asset(root: &Path, relative: &Path) -> Result<(Library, String)> {
        let path = root.join(relative);
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| Error::Operation("test asset has no parent".into()))?,
        )?;
        std::fs::write(&path, b"path projection bytes")?;
        let digests = digest_reader(std::fs::File::open(&path)?)?;
        let metadata = std::fs::metadata(&path)?;
        let library = Library::open_in_memory()?;
        let root_id = RootId::new();
        let capabilities = RootCapabilities::detect(root)?;
        let now = Utc::now().to_rfc3339();
        library.conn.execute(
            "INSERT INTO library_roots
             (id, path, state, capabilities_json, created_at, updated_at)
             VALUES (?1, ?2, 'online', ?3, ?4, ?4)",
            params![
                root_id.as_str(),
                path_text(root)?,
                serde_json::to_string(&capabilities)?,
                now
            ],
        )?;
        let asset_id = uuid::Uuid::new_v4().to_string();
        library.conn.execute(
            "INSERT INTO assets
             (id, root_id, relative_path, absolute_path, role, managed,
              verification_state, byte_size, blake3, sha256, entry_identity,
              projection_state, first_seen_at, last_verified_at)
             VALUES (?1, ?2, ?3, ?4, 'ancillary', 1, 'verified', ?5, ?6, ?7,
                     ?8, 'current', ?9, ?9)",
            params![
                asset_id,
                root_id.as_str(),
                path_text(relative)?,
                path_text(&path)?,
                digests.byte_size(),
                digests.blake3(),
                digests.sha256(),
                file_identity(&metadata),
                now
            ],
        )?;
        Ok((library, asset_id))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn path_projection_requires_approval_and_updates_root_relative_identity() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("library");
        std::fs::create_dir(&root)?;
        let (mut library, asset_id) = library_with_asset(&root, Path::new("old/file.cue"))?;
        let plan = library.plan_path_projection(
            &asset_id,
            Path::new("new/file.cue"),
            NamingProfile::Portable,
        )?;
        assert!(library.execute_path_projection(&plan).is_err());
        library.approve_path_projection(&plan)?;
        library.execute_path_projection(&plan)?;
        assert!(!plan.source().exists());
        assert_eq!(std::fs::read(plan.destination())?, b"path projection bytes");
        let stored: String = library.conn.query_row(
            "SELECT relative_path FROM assets WHERE id = ?1",
            [&asset_id],
            |row| row.get(0),
        )?;
        assert_eq!(stored, "new/file.cue");
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn every_path_projection_boundary_preserves_one_verified_copy() -> Result<()> {
        let recording = tempfile::tempdir()?;
        let root = recording.path().join("library");
        std::fs::create_dir(&root)?;
        let (mut library, asset_id) = library_with_asset(&root, Path::new("old/file.cue"))?;
        let plan = library.plan_path_projection(
            &asset_id,
            Path::new("new/file.cue"),
            NamingProfile::Portable,
        )?;
        library.approve_path_projection(&plan)?;
        let (result, boundaries) =
            failpoints::run_recording(|| library.execute_path_projection(&plan));
        result?;

        for fail_at in 0..boundaries.len() {
            let temporary = tempfile::tempdir()?;
            let root = temporary.path().join("library");
            std::fs::create_dir(&root)?;
            let (mut library, asset_id) = library_with_asset(&root, Path::new("old/file.cue"))?;
            let plan = library.plan_path_projection(
                &asset_id,
                Path::new("new/file.cue"),
                NamingProfile::Portable,
            )?;
            library.approve_path_projection(&plan)?;
            let (_result, _hits) =
                failpoints::run_failing(fail_at, || library.execute_path_projection(&plan));
            let first = library.recover_pending()?;
            let second = library.recover_pending()?;
            assert!(first.unresolved.is_empty(), "boundary {fail_at}");
            assert!(second.unresolved.is_empty(), "boundary {fail_at}");
            let source = plan.source().exists();
            let destination = plan.destination().exists();
            assert_ne!(source, destination, "boundary {fail_at}");
            let retained = if source {
                plan.source()
            } else {
                plan.destination()
            };
            assert_eq!(std::fs::read(retained)?, b"path projection bytes");
        }
        Ok(())
    }
}
