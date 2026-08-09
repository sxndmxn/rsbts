//! Journaled SHA-256 manifests, `BagIt` export, and exercised restore verification.

use std::collections::HashSet;
use std::io::{BufRead as _, BufReader, BufWriter, Write as _};
use std::path::{Component, Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::asset::{digest_reader, FileDigests};
use crate::db::{file_identity, file_object_identity, JournalFile, Library, OperationKind};
use crate::fsops::AnchoredRoot;
use crate::operations::{append_plan_event, PlanId, PlanKind, PlanState};
use crate::roots::RootId;
use crate::{Error, Result};

const PAGE_SIZE: u32 = 512;

#[derive(Debug, Clone)]
struct ManifestAsset {
    id: String,
    relative_path: PathBuf,
    absolute_path: PathBuf,
    byte_size: u64,
    sha256: String,
    entry_identity: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ManifestFormat {
    Sha256,
    BagIt,
}

impl ManifestFormat {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::BagIt => "bagit",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestPlan {
    id: PlanId,
    root_id: RootId,
    root_path: PathBuf,
    format: ManifestFormat,
    output: PathBuf,
    asset_count: u64,
    byte_count: u64,
}

impl ManifestPlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    #[must_use]
    pub const fn asset_count(&self) -> u64 {
        self.asset_count
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlan {
    id: PlanId,
    bag: PathBuf,
    destination: PathBuf,
    manifest_sha256: String,
    file_count: u64,
    byte_count: u64,
}

impl RestorePlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestReceipt {
    id: String,
    operation_id: String,
    path: PathBuf,
    sha256: String,
    asset_count: u64,
    byte_count: u64,
}

impl ManifestReceipt {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestVerification {
    checked: u64,
    failures: Vec<String>,
}

impl ManifestVerification {
    #[must_use]
    pub const fn checked(&self) -> u64 {
        self.checked
    }

    #[must_use]
    pub fn failures(&self) -> &[String] {
        &self.failures
    }

    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreReceipt {
    id: String,
    operation_id: String,
    restored_files: u64,
    restored_bytes: u64,
    destination: PathBuf,
}

impl RestoreReceipt {
    #[must_use]
    pub const fn restored_files(&self) -> u64 {
        self.restored_files
    }

    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }
}

impl Library {
    /// Resolve a configured root identity by its path.
    pub fn root_id_for_path(&self, path: &Path) -> Result<RootId> {
        let stored = path_text(path)?;
        self.conn
            .query_row(
                "SELECT id FROM library_roots WHERE path = ?1 AND state != 'legacy'",
                [stored],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| Error::Preservation("library root is not registered".into()))
            .and_then(RootId::parse)
    }

    /// Persist a side-effect-free manifest or `BagIt` preview.
    pub fn plan_manifest(
        &self,
        root_id: &RootId,
        format: ManifestFormat,
        output: &Path,
    ) -> Result<ManifestPlan> {
        require_absolute_absent(output, "manifest output")?;
        let (root_path, state) = self.root_record(root_id)?;
        if state == "offline" {
            return Err(Error::Preservation("library root is offline".into()));
        }
        let (managed, verified, byte_count) = self.conn.query_row(
            "SELECT COUNT(*),
                    SUM(CASE WHEN verification_state = 'verified'
                                  AND byte_size IS NOT NULL AND sha256 IS NOT NULL
                                  AND entry_identity IS NOT NULL THEN 1 ELSE 0 END),
                    COALESCE(SUM(byte_size), 0)
             FROM assets WHERE root_id = ?1 AND managed = 1",
            [root_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            },
        )?;
        if managed != verified {
            return Err(Error::Preservation(format!(
                "{managed} managed assets exist but only {verified} have complete verified fixity"
            )));
        }
        let request = json!({
            "root_id": root_id,
            "format": format,
            "output": output,
        });
        let preview = json!({
            "asset_count": managed,
            "byte_count": byte_count,
            "output_must_remain_absent": true,
            "journaled": true,
        });
        let id = self.create_durable_plan(PlanKind::Manifest, &request, &preview, Some(managed))?;
        Ok(ManifestPlan {
            id,
            root_id: root_id.clone(),
            root_path,
            format,
            output: output.to_path_buf(),
            asset_count: managed,
            byte_count,
        })
    }

    pub fn approve_manifest(&self, plan: &ManifestPlan) -> Result<()> {
        self.approve_durable_plan(plan.id())
    }

    /// Execute an approved export through a typed journal and no-clobber publication.
    pub fn execute_manifest(&mut self, plan: &ManifestPlan) -> Result<ManifestReceipt> {
        if let Err(error) = self.start_durable_plan(plan.id()) {
            return self.fail_plan_without_operation(plan.id(), error);
        }
        require_absolute_absent(&plan.output, "manifest output")?;
        let current_root = self.root_record(&plan.root_id)?;
        if current_root.0 != plan.root_path || current_root.1 == "offline" {
            return self.fail_plan_without_operation(
                plan.id(),
                Error::Preservation("library root changed after manifest preview".into()),
            );
        }
        let operation = match self.create_operation_for_plan(
            OperationKind::ManifestWrite,
            &[],
            Some(plan.id().as_str()),
            Some(&json!({"format": plan.format, "root_id": plan.root_id})),
        ) {
            Ok(operation) => operation,
            Err(error) => return self.fail_plan_without_operation(plan.id(), error),
        };
        let result = match plan.format {
            ManifestFormat::Sha256 => self.execute_sha256_manifest(plan, &operation),
            ManifestFormat::BagIt => self.execute_bagit(plan, &operation),
        };
        match result {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(self.fail_preservation_operation(plan.id(), &operation, error)),
        }
    }

    /// Validate a bag and persist a restore preview without creating the destination.
    pub fn plan_restore(&self, bag: &Path, destination: &Path) -> Result<RestorePlan> {
        require_absolute_directory(bag, "bag")?;
        require_absolute_absent(destination, "restore destination")?;
        let manifest = bag.join("manifest-sha256.txt");
        let verification = verify_sha256_manifest(bag, &manifest)?;
        if !verification.is_valid() {
            return Err(Error::Preservation(format!(
                "source bag failed verification: {}",
                verification.failures().join("; ")
            )));
        }
        let manifest_sha256 = digest_reader(std::fs::File::open(&manifest)?)?
            .sha256()
            .to_owned();
        let (file_count, byte_count) = manifest_shape(bag, &manifest)?;
        let request = json!({
            "bag": bag,
            "destination": destination,
            "manifest_sha256": manifest_sha256,
        });
        let preview = json!({
            "file_count": file_count,
            "byte_count": byte_count,
            "destination_must_remain_absent": true,
            "source_will_be_reverified": true,
        });
        let id = self.create_durable_plan(
            PlanKind::BackupRestore,
            &request,
            &preview,
            Some(file_count),
        )?;
        Ok(RestorePlan {
            id,
            bag: bag.to_path_buf(),
            destination: destination.to_path_buf(),
            manifest_sha256,
            file_count,
            byte_count,
        })
    }

    pub fn approve_restore(&self, plan: &RestorePlan) -> Result<()> {
        self.approve_durable_plan(plan.id())
    }

    /// Exercise a restore into a new directory and verify every restored payload.
    pub fn execute_restore(&mut self, plan: &RestorePlan) -> Result<RestoreReceipt> {
        if let Err(error) = self.start_durable_plan(plan.id()) {
            return self.fail_plan_without_operation(plan.id(), error);
        }
        require_absolute_absent(&plan.destination, "restore destination")?;
        let manifest = plan.bag.join("manifest-sha256.txt");
        let digest = digest_reader(std::fs::File::open(&manifest)?)?;
        if digest.sha256() != plan.manifest_sha256 {
            return self.fail_plan_without_operation(
                plan.id(),
                Error::Preservation("source manifest changed after restore preview".into()),
            );
        }
        let verification = verify_sha256_manifest(&plan.bag, &manifest)?;
        if !verification.is_valid() {
            return self.fail_plan_without_operation(
                plan.id(),
                Error::Preservation("source bag changed after restore preview".into()),
            );
        }
        let operation = match self.create_operation_for_plan(
            OperationKind::RestoreCopy,
            &[],
            Some(plan.id().as_str()),
            Some(&json!({
                "manifest_sha256": plan.manifest_sha256,
                "files": plan.file_count,
                "bytes": plan.byte_count,
            })),
        ) {
            Ok(operation) => operation,
            Err(error) => return self.fail_plan_without_operation(plan.id(), error),
        };
        match self.execute_restore_inner(plan, &operation) {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(self.fail_preservation_operation(plan.id(), &operation, error)),
        }
    }

    fn execute_sha256_manifest(
        &self,
        plan: &ManifestPlan,
        operation: &str,
    ) -> Result<ManifestReceipt> {
        let output_parent = absolute_parent(&plan.output)?;
        let output_root = AnchoredRoot::open(output_parent)?;
        let source_root = AnchoredRoot::open(&plan.root_path)?;
        let staged = sibling_path(&plan.output, "manifest-stage")?;
        let file = JournalFile {
            source: plan.output.clone(),
            staged: staged.clone(),
            destination: plan.output.clone(),
            content_hash: None,
            sha256: None,
            source_identity: None,
            owned_identity: None,
            role: "sha256-manifest".into(),
            state: "prepared".into(),
        };
        let ordinal = self.append_operation_file(operation, &file)?;
        self.set_operation_state(operation, "staging", None)?;
        let mut written_assets = 0_u64;
        let mut written_bytes = 0_u64;
        output_root.write_new_stream_observed(
            &staged,
            |metadata| {
                self.set_acquired_file_identity(operation, ordinal, &file_object_identity(metadata))
            },
            |output| {
                let mut writer = BufWriter::new(output);
                let mut after = String::new();
                loop {
                    let page = self.managed_manifest_assets(&plan.root_id, &after, PAGE_SIZE)?;
                    if page.is_empty() {
                        break;
                    }
                    for asset in &page {
                        validate_manifest_asset(&source_root, &plan.root_path, asset)?;
                        writeln!(
                            writer,
                            "{}  {}",
                            asset.sha256,
                            portable_manifest_path(&asset.relative_path)?
                        )?;
                        written_assets = written_assets.saturating_add(1);
                        written_bytes = written_bytes.saturating_add(asset.byte_size);
                        after.clone_from(&asset.id);
                    }
                }
                writer.flush()?;
                Ok(())
            },
        )?;
        require_planned_shape(plan, written_assets, written_bytes)?;
        let evidence = digest_reader(output_root.open_file(&staged)?)?;
        let identity = file_identity(&output_root.entry_metadata(&staged)?);
        self.set_staged_file_full_evidence(
            operation,
            ordinal,
            &identity,
            evidence.blake3(),
            evidence.sha256(),
        )?;
        output_root.rename_noreplace(&staged, &plan.output)?;
        self.set_file_state(operation, ordinal, "published")?;
        self.commit_manifest(
            plan,
            operation,
            &plan.output,
            evidence.sha256(),
            written_assets,
            written_bytes,
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "ordered BagIt staging and validation is one recovery protocol"
    )]
    fn execute_bagit(&self, plan: &ManifestPlan, operation: &str) -> Result<ManifestReceipt> {
        let output_parent = absolute_parent(&plan.output)?;
        let output_root = AnchoredRoot::open(output_parent)?;
        let source_root = AnchoredRoot::open(&plan.root_path)?;
        let staged_bag = sibling_path(&plan.output, "bag-stage")?;
        output_root.create_parent_all(&staged_bag.join("placeholder"))?;
        self.set_operation_state(operation, "staging", None)?;

        let bagit = staged_bag.join("bagit.txt");
        let final_bagit = plan.output.join("bagit.txt");
        let bagit_digest = self.stage_bytes(
            &output_root,
            operation,
            &bagit,
            &final_bagit,
            "bagit-declaration",
            b"BagIt-Version: 1.0\nTag-File-Character-Encoding: UTF-8\n",
        )?;
        let info = staged_bag.join("bag-info.txt");
        let final_info = plan.output.join("bag-info.txt");
        let info_bytes = format!(
            "Bagging-Date: {}\nPayload-Oxum: {}.{}\n",
            Utc::now().date_naive(),
            plan.byte_count,
            plan.asset_count
        );
        let info_digest = self.stage_bytes(
            &output_root,
            operation,
            &info,
            &final_info,
            "bag-info",
            info_bytes.as_bytes(),
        )?;

        let manifest = staged_bag.join("manifest-sha256.txt");
        let final_manifest = plan.output.join("manifest-sha256.txt");
        let manifest_file = JournalFile {
            source: final_manifest.clone(),
            staged: manifest.clone(),
            destination: final_manifest.clone(),
            content_hash: None,
            sha256: None,
            source_identity: None,
            owned_identity: None,
            role: "bagit-payload-manifest".into(),
            state: "prepared".into(),
        };
        let manifest_ordinal = self.append_operation_file(operation, &manifest_file)?;
        let mut written_assets = 0_u64;
        let mut written_bytes = 0_u64;
        output_root.write_new_stream_observed(
            &manifest,
            |metadata| {
                self.set_acquired_file_identity(
                    operation,
                    manifest_ordinal,
                    &file_object_identity(metadata),
                )
            },
            |output| {
                let mut writer = BufWriter::new(output);
                let mut after = String::new();
                loop {
                    let page = self.managed_manifest_assets(&plan.root_id, &after, PAGE_SIZE)?;
                    if page.is_empty() {
                        break;
                    }
                    for asset in &page {
                        validate_manifest_asset(&source_root, &plan.root_path, asset)?;
                        let relative = safe_relative(&asset.relative_path)?;
                        let staged_payload = staged_bag.join("data").join(&relative);
                        let final_payload = plan.output.join("data").join(&relative);
                        output_root.create_parent_all(&staged_payload)?;
                        let mut input = source_root.open_file(&asset.absolute_path)?;
                        let copied = self.stage_stream(
                            &output_root,
                            operation,
                            &asset.absolute_path,
                            &staged_payload,
                            &final_payload,
                            "bagit-payload",
                            |file| {
                                std::io::copy(&mut input, file)?;
                                Ok(())
                            },
                        )?;
                        if copied.byte_size() != asset.byte_size || copied.sha256() != asset.sha256
                        {
                            return Err(Error::Preservation(format!(
                                "BagIt payload validation failed: {}",
                                asset.absolute_path.display()
                            )));
                        }
                        writeln!(
                            writer,
                            "{}  data/{}",
                            asset.sha256,
                            portable_manifest_path(&relative)?
                        )?;
                        written_assets = written_assets.saturating_add(1);
                        written_bytes = written_bytes.saturating_add(asset.byte_size);
                        after.clone_from(&asset.id);
                    }
                }
                writer.flush()?;
                Ok(())
            },
        )?;
        require_planned_shape(plan, written_assets, written_bytes)?;
        let manifest_digest = digest_reader(output_root.open_file(&manifest)?)?;
        let manifest_identity = file_identity(&output_root.entry_metadata(&manifest)?);
        self.set_staged_file_full_evidence(
            operation,
            manifest_ordinal,
            &manifest_identity,
            manifest_digest.blake3(),
            manifest_digest.sha256(),
        )?;

        let tagmanifest = staged_bag.join("tagmanifest-sha256.txt");
        let final_tagmanifest = plan.output.join("tagmanifest-sha256.txt");
        let tagmanifest_bytes = format!(
            "{}  bagit.txt\n{}  bag-info.txt\n{}  manifest-sha256.txt\n",
            bagit_digest.sha256(),
            info_digest.sha256(),
            manifest_digest.sha256()
        );
        self.stage_bytes(
            &output_root,
            operation,
            &tagmanifest,
            &final_tagmanifest,
            "bagit-tag-manifest",
            tagmanifest_bytes.as_bytes(),
        )?;
        let verification = verify_sha256_manifest(&staged_bag, &manifest)?;
        if !verification.is_valid() {
            return Err(Error::Preservation(format!(
                "staged BagIt export failed verification: {}",
                verification.failures().join("; ")
            )));
        }
        output_root.rename_noreplace(&staged_bag, &plan.output)?;
        self.mark_operation_files_published(operation)?;
        self.commit_manifest(
            plan,
            operation,
            &final_manifest,
            manifest_digest.sha256(),
            written_assets,
            written_bytes,
        )
    }

    fn execute_restore_inner(&self, plan: &RestorePlan, operation: &str) -> Result<RestoreReceipt> {
        let destination_parent = absolute_parent(&plan.destination)?;
        let output_root = AnchoredRoot::open(destination_parent)?;
        let bag_root = AnchoredRoot::open(&plan.bag)?;
        let staged_restore = sibling_path(&plan.destination, "restore-stage")?;
        output_root.create_parent_all(&staged_restore.join("placeholder"))?;
        self.set_operation_state(operation, "staging", None)?;
        let run_id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO backup_restore_runs
             (id, source_path, restore_path, state, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![
                run_id,
                path_text(&plan.bag)?,
                path_text(&plan.destination)?,
                Utc::now().to_rfc3339()
            ],
        )?;

        let manifest_path = plan.bag.join("manifest-sha256.txt");
        let manifest_file = bag_root.open_file(&manifest_path)?;
        let mut files = 0_u64;
        let mut bytes = 0_u64;
        for line in BufReader::new(manifest_file).lines() {
            let line = line?;
            let (expected, relative) = parse_manifest_line(&line)?;
            let relative = safe_relative(Path::new(relative))?;
            let payload_relative = relative.strip_prefix("data").map_err(|_error| {
                Error::Preservation("BagIt manifest path is outside data/".into())
            })?;
            let source = plan.bag.join(&relative);
            let staged = staged_restore.join(payload_relative);
            let destination = plan.destination.join(payload_relative);
            output_root.create_parent_all(&staged)?;
            let mut input = bag_root.open_file(&source)?;
            let copied = self.stage_stream(
                &output_root,
                operation,
                &source,
                &staged,
                &destination,
                "restore-payload",
                |file| {
                    std::io::copy(&mut input, file)?;
                    Ok(())
                },
            )?;
            if !copied.sha256().eq_ignore_ascii_case(expected) {
                return Err(Error::Preservation(format!(
                    "restored file failed verification: {}",
                    destination.display()
                )));
            }
            files = files.saturating_add(1);
            bytes = bytes.saturating_add(copied.byte_size());
        }
        if files != plan.file_count || bytes != plan.byte_count {
            return Err(Error::Preservation(
                "restore shape changed after preview".into(),
            ));
        }
        output_root.rename_noreplace(&staged_restore, &plan.destination)?;
        self.mark_operation_files_published(operation)?;
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "UPDATE backup_restore_runs
             SET state = 'complete', completed_at = ?2 WHERE id = ?1 AND state = 'running'",
            params![run_id, now],
        )?;
        transition_operation_and_plan(&transaction, operation, plan.id(), files, &now)?;
        transaction.commit()?;
        self.complete_operation(operation)?;
        Ok(RestoreReceipt {
            id: run_id,
            operation_id: operation.to_owned(),
            restored_files: files,
            restored_bytes: bytes,
            destination: plan.destination.clone(),
        })
    }

    fn stage_bytes(
        &self,
        root: &AnchoredRoot,
        operation: &str,
        staged: &Path,
        destination: &Path,
        role: &str,
        bytes: &[u8],
    ) -> Result<FileDigests> {
        self.stage_stream(
            root,
            operation,
            destination,
            staged,
            destination,
            role,
            |file| {
                file.write_all(bytes)?;
                Ok(())
            },
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "all three paths and the typed role are journal evidence"
    )]
    fn stage_stream(
        &self,
        root: &AnchoredRoot,
        operation: &str,
        source: &Path,
        staged: &Path,
        destination: &Path,
        role: &str,
        write: impl FnOnce(&mut std::fs::File) -> Result<()>,
    ) -> Result<FileDigests> {
        let journal = JournalFile {
            source: source.to_path_buf(),
            staged: staged.to_path_buf(),
            destination: destination.to_path_buf(),
            content_hash: None,
            sha256: None,
            source_identity: None,
            owned_identity: None,
            role: role.to_owned(),
            state: "prepared".into(),
        };
        let ordinal = self.append_operation_file(operation, &journal)?;
        root.write_new_stream_observed(
            staged,
            |metadata| {
                self.set_acquired_file_identity(operation, ordinal, &file_object_identity(metadata))
            },
            write,
        )?;
        let evidence = digest_reader(root.open_file(staged)?)?;
        let identity = file_identity(&root.entry_metadata(staged)?);
        self.set_staged_file_full_evidence(
            operation,
            ordinal,
            &identity,
            evidence.blake3(),
            evidence.sha256(),
        )?;
        Ok(evidence)
    }

    fn commit_manifest(
        &self,
        plan: &ManifestPlan,
        operation: &str,
        manifest_path: &Path,
        manifest_sha256: &str,
        asset_count: u64,
        byte_count: u64,
    ) -> Result<ManifestReceipt> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO preservation_manifests
             (id, root_id, format, manifest_path, manifest_sha256, asset_count,
              byte_count, created_at, verified_at, verification_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 'verified')",
            params![
                id,
                plan.root_id.as_str(),
                plan.format.as_str(),
                path_text(manifest_path)?,
                manifest_sha256,
                asset_count,
                byte_count,
                now
            ],
        )?;
        transition_operation_and_plan(&transaction, operation, plan.id(), asset_count, &now)?;
        transaction.commit()?;
        self.complete_operation(operation)?;
        Ok(ManifestReceipt {
            id,
            operation_id: operation.to_owned(),
            path: manifest_path.to_path_buf(),
            sha256: manifest_sha256.to_owned(),
            asset_count,
            byte_count,
        })
    }

    fn mark_operation_files_published(&self, operation: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operation_files SET state = 'published'
             WHERE operation_id = ?1 AND state IN ('staged', 'acquired')",
            [operation],
        )?;
        if changed == 0 {
            return Err(Error::Recovery(
                "published preservation operation had no journaled files".into(),
            ));
        }
        Ok(())
    }

    fn root_record(&self, root_id: &RootId) -> Result<(PathBuf, String)> {
        self.conn
            .query_row(
                "SELECT path, state FROM library_roots WHERE id = ?1 AND state != 'legacy'",
                [root_id.as_str()],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, String>(1)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Error::Preservation("library root is not registered".into()))
    }

    fn managed_manifest_assets(
        &self,
        root_id: &RootId,
        after: &str,
        limit: u32,
    ) -> Result<Vec<ManifestAsset>> {
        let mut statement = self.conn.prepare(
            "SELECT id, relative_path, absolute_path, byte_size, sha256, entry_identity
             FROM assets
             WHERE root_id = ?1 AND managed = 1 AND id > ?2
               AND verification_state = 'verified'
               AND byte_size IS NOT NULL AND sha256 IS NOT NULL
               AND entry_identity IS NOT NULL
             ORDER BY id LIMIT ?3",
        )?;
        let assets = statement
            .query_map(params![root_id.as_str(), after, limit], |row| {
                Ok(ManifestAsset {
                    id: row.get(0)?,
                    relative_path: PathBuf::from(row.get::<_, String>(1)?),
                    absolute_path: PathBuf::from(row.get::<_, String>(2)?),
                    byte_size: row.get(3)?,
                    sha256: row.get(4)?,
                    entry_identity: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into);
        assets
    }

    fn fail_plan_without_operation<T>(&mut self, plan_id: &PlanId, error: Error) -> Result<T> {
        if self
            .durable_plan(plan_id)
            .is_ok_and(|plan| plan.state() == PlanState::Running)
        {
            let _ = self.finish_durable_plan(plan_id, PlanState::Failed, Some(&error.to_string()));
        }
        match self.recover_pending() {
            Ok(report) if report.unresolved.is_empty() => Err(error),
            Ok(report) => Err(Error::Recovery(format!(
                "{error}; recovery requires review: {}",
                report.unresolved.join("; ")
            ))),
            Err(recovery) => Err(Error::Recovery(format!(
                "{error}; recovery could not run: {recovery}"
            ))),
        }
    }

    fn fail_preservation_operation(
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
        match self.recover_pending() {
            Ok(report) if report.unresolved.is_empty() => error,
            Ok(report) => Error::Recovery(format!(
                "{error}; preservation recovery requires review: {}",
                report.unresolved.join("; ")
            )),
            Err(recovery) => Error::Recovery(format!(
                "{error}; preservation recovery could not run: {recovery}"
            )),
        }
    }
}

fn transition_operation_and_plan(
    transaction: &rusqlite::Transaction<'_>,
    operation: &str,
    plan_id: &PlanId,
    progress: u64,
    now: &str,
) -> Result<()> {
    if transaction.execute(
        "UPDATE operation_journal SET state = 'db-committed', updated_at = ?2
         WHERE id = ?1 AND state NOT IN ('db-committed', 'complete')",
        params![operation, now],
    )? != 1
    {
        return Err(Error::Recovery(
            "preservation journal could not enter committed state".into(),
        ));
    }
    if transaction.execute(
        "UPDATE durable_plans
         SET state = 'complete', progress_current = ?2, updated_at = ?3, completed_at = ?3
         WHERE id = ?1 AND state = 'running'",
        params![plan_id.as_str(), progress, now],
    )? != 1
    {
        return Err(Error::Recovery(
            "preservation plan could not enter complete state".into(),
        ));
    }
    append_plan_event(
        transaction,
        plan_id,
        "complete",
        &json!({"progress_current": progress, "operation_id": operation}),
    )?;
    Ok(())
}

fn validate_manifest_asset(
    root: &AnchoredRoot,
    root_path: &Path,
    asset: &ManifestAsset,
) -> Result<()> {
    if root_path.join(&asset.relative_path) != asset.absolute_path {
        return Err(Error::Preservation(format!(
            "asset root-relative identity diverged: {}",
            asset.absolute_path.display()
        )));
    }
    let before = root.entry_metadata(&asset.absolute_path)?;
    if file_identity(&before) != asset.entry_identity || before.len() != asset.byte_size {
        return Err(Error::Preservation(format!(
            "asset changed before preservation export: {}",
            asset.absolute_path.display()
        )));
    }
    let digest = digest_reader(root.open_file(&asset.absolute_path)?)?;
    let after = root.entry_metadata(&asset.absolute_path)?;
    if digest.byte_size() != asset.byte_size
        || digest.sha256() != asset.sha256
        || file_identity(&after) != asset.entry_identity
    {
        return Err(Error::Preservation(format!(
            "asset changed during preservation export: {}",
            asset.absolute_path.display()
        )));
    }
    Ok(())
}

fn require_planned_shape(plan: &ManifestPlan, assets: u64, bytes: u64) -> Result<()> {
    if assets == plan.asset_count && bytes == plan.byte_count {
        Ok(())
    } else {
        Err(Error::Preservation(
            "managed asset set changed after preservation preview".into(),
        ))
    }
}

/// Verify a standard two-space-delimited SHA-256 manifest beneath `root`.
pub fn verify_sha256_manifest(root: &Path, manifest: &Path) -> Result<ManifestVerification> {
    require_absolute_directory(root, "manifest root")?;
    let anchored = AnchoredRoot::open(root)?;
    let manifest_root = AnchoredRoot::open(absolute_parent(manifest)?)?;
    let file = manifest_root.open_file(manifest)?;
    let mut report = ManifestVerification::default();
    let mut seen = HashSet::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        let (expected, relative) = parse_manifest_line(&line).map_err(|error| {
            Error::Preservation(format!("malformed manifest line {}: {error}", index + 1))
        })?;
        let relative = safe_relative(Path::new(relative))?;
        if !seen.insert(relative.clone()) {
            return Err(Error::Preservation(format!(
                "duplicate manifest path: {}",
                relative.display()
            )));
        }
        let path = root.join(&relative);
        report.checked = report.checked.saturating_add(1);
        match anchored.open_file(&path).and_then(digest_reader) {
            Ok(actual) if actual.sha256().eq_ignore_ascii_case(expected) => {}
            Ok(_) => report
                .failures
                .push(format!("digest mismatch: {}", relative.display())),
            Err(error) => report
                .failures
                .push(format!("{}: {error}", relative.display())),
        }
    }
    Ok(report)
}

fn manifest_shape(root: &Path, manifest: &Path) -> Result<(u64, u64)> {
    let anchored = AnchoredRoot::open(root)?;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for line in BufReader::new(anchored.open_file(manifest)?).lines() {
        let line = line?;
        let (_expected, relative) = parse_manifest_line(&line)?;
        let relative = safe_relative(Path::new(relative))?;
        let metadata = anchored.entry_metadata(&root.join(relative))?;
        if !metadata.is_file() {
            return Err(Error::Preservation(
                "manifest payload is not a regular file".into(),
            ));
        }
        files = files.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
    }
    Ok((files, bytes))
}

fn parse_manifest_line(line: &str) -> Result<(&str, &str)> {
    let (expected, relative) = line
        .split_once("  ")
        .ok_or_else(|| Error::Preservation("missing two-space delimiter".into()))?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Preservation("invalid SHA-256".into()));
    }
    Ok((expected, relative))
}

fn safe_relative(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() || path.as_os_str().is_empty() {
        return Err(Error::Preservation("manifest path must be relative".into()));
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => output.push(value),
            _ => return Err(Error::Preservation("manifest path escapes its root".into())),
        }
    }
    Ok(output)
}

fn portable_manifest_path(path: &Path) -> Result<String> {
    let path = safe_relative(path)?;
    let text = path
        .to_str()
        .ok_or_else(|| Error::Preservation("manifest path must be valid UTF-8".into()))?;
    if text.chars().any(char::is_control) || text.contains("  ") {
        return Err(Error::Preservation(
            "manifest paths cannot contain controls or two spaces".into(),
        ));
    }
    Ok(text.replace('\\', "/"))
}

fn absolute_parent(path: &Path) -> Result<&Path> {
    if !path.is_absolute() {
        return Err(Error::Preservation(format!(
            "path must be absolute: {}",
            path.display()
        )));
    }
    path.parent()
        .filter(|parent| parent.is_absolute())
        .ok_or_else(|| Error::Preservation("path has no absolute parent".into()))
}

fn require_absolute_absent(path: &Path, role: &str) -> Result<()> {
    let parent = absolute_parent(path)?;
    if !parent.is_dir() {
        return Err(Error::Preservation(format!(
            "{role} parent does not exist: {}",
            parent.display()
        )));
    }
    if path.exists() || path.is_symlink() {
        return Err(Error::Preservation(format!(
            "{role} already exists: {}",
            path.display()
        )));
    }
    path_text(path)?;
    Ok(())
}

fn require_absolute_directory(path: &Path, role: &str) -> Result<()> {
    if !path.is_absolute() || !path.is_dir() || path.is_symlink() {
        return Err(Error::Preservation(format!(
            "{role} must be an absolute real directory: {}",
            path.display()
        )));
    }
    path_text(path)?;
    Ok(())
}

fn sibling_path(path: &Path, role: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Preservation("output filename is invalid UTF-8".into()))?;
    Ok(path.with_file_name(format!(".{name}.rsbts-{}-{role}", uuid::Uuid::new_v4())))
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::Preservation("path must be valid UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failpoints;

    fn add_managed_asset(library: &Library, root: &Path, relative: &Path) -> Result<RootId> {
        let root_id = RootId::new();
        let path = root.join(relative);
        let digests = digest_reader(std::fs::File::open(&path)?)?;
        let metadata = std::fs::metadata(&path)?;
        let now = Utc::now().to_rfc3339();
        library.conn.execute(
            "INSERT INTO library_roots
             (id, path, state, capabilities_json, created_at, updated_at)
             VALUES (?1, ?2, 'online', '{}', ?3, ?3)",
            params![root_id.as_str(), path_text(root)?, now],
        )?;
        library.conn.execute(
            "INSERT INTO assets
             (id, root_id, relative_path, absolute_path, role, managed,
              verification_state, byte_size, blake3, sha256, entry_identity,
              projection_state, first_seen_at, last_verified_at)
             VALUES (?1, ?2, ?3, ?4, 'audio', 1, 'verified', ?5, ?6, ?7, ?8,
                     'current', ?9, ?9)",
            params![
                uuid::Uuid::new_v4().to_string(),
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
        Ok(root_id)
    }

    #[test]
    fn manifest_verification_rejects_escapes_and_detects_changes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let payload = temporary.path().join("track.flac");
        std::fs::write(&payload, b"audio")?;
        let sha = digest_reader(std::fs::File::open(&payload)?)?
            .sha256()
            .to_owned();
        let manifest = temporary.path().join("manifest-sha256.txt");
        std::fs::write(&manifest, format!("{sha}  track.flac\n"))?;
        assert!(verify_sha256_manifest(temporary.path(), &manifest)?.is_valid());
        std::fs::write(&payload, b"changed")?;
        assert_eq!(
            verify_sha256_manifest(temporary.path(), &manifest)?
                .failures()
                .len(),
            1
        );
        std::fs::write(&manifest, format!("{sha}  ../escape\n"))?;
        assert!(verify_sha256_manifest(temporary.path(), &manifest).is_err());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn manifests_and_restore_are_approved_journaled_and_exercised() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join("track.flac"), b"preserved audio")?;
        let mut library = Library::open_in_memory()?;
        let root_id = add_managed_asset(&library, &source, Path::new("track.flac"))?;

        let manifest_path = temporary.path().join("collection.sha256");
        let manifest_plan =
            library.plan_manifest(&root_id, ManifestFormat::Sha256, &manifest_path)?;
        assert!(library.execute_manifest(&manifest_plan).is_err());
        library.approve_manifest(&manifest_plan)?;
        let manifest_receipt = library.execute_manifest(&manifest_plan)?;
        assert_eq!(manifest_receipt.path(), manifest_path);
        assert!(verify_sha256_manifest(&source, &manifest_path)?.is_valid());

        let bag = temporary.path().join("collection.bag");
        let bag_plan = library.plan_manifest(&root_id, ManifestFormat::BagIt, &bag)?;
        library.approve_manifest(&bag_plan)?;
        let bag_receipt = library.execute_manifest(&bag_plan)?;
        assert_eq!(bag_receipt.path(), bag.join("manifest-sha256.txt"));
        assert!(verify_sha256_manifest(&bag, bag_receipt.path())?.is_valid());

        let restored = temporary.path().join("restored");
        let restore_plan = library.plan_restore(&bag, &restored)?;
        assert!(library.execute_restore(&restore_plan).is_err());
        library.approve_restore(&restore_plan)?;
        let restore = library.execute_restore(&restore_plan)?;
        assert_eq!(restore.restored_files(), 1);
        assert_eq!(
            std::fs::read(restored.join("track.flac"))?,
            b"preserved audio"
        );
        let completed: u64 = library.conn.query_row(
            "SELECT COUNT(*) FROM operation_journal
             WHERE kind IN ('manifest-write', 'restore-copy') AND state = 'complete'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(completed, 3);
        Ok(())
    }

    #[test]
    fn every_manifest_boundary_recovers_without_losing_source_bytes() -> Result<()> {
        let recording = tempfile::tempdir()?;
        let source = recording.path().join("source");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join("track.flac"), b"preserved audio")?;
        let mut library = Library::open_in_memory()?;
        let root_id = add_managed_asset(&library, &source, Path::new("track.flac"))?;
        let output = recording.path().join("manifest.sha256");
        let plan = library.plan_manifest(&root_id, ManifestFormat::Sha256, &output)?;
        library.approve_manifest(&plan)?;
        let (recorded, boundaries) = failpoints::run_recording(|| library.execute_manifest(&plan));
        recorded?;
        assert!(!boundaries.is_empty());

        for fail_at in 0..boundaries.len() {
            let temporary = tempfile::tempdir()?;
            let source = temporary.path().join("source");
            std::fs::create_dir(&source)?;
            std::fs::write(source.join("track.flac"), b"preserved audio")?;
            let mut library = Library::open_in_memory()?;
            let root_id = add_managed_asset(&library, &source, Path::new("track.flac"))?;
            let output = temporary.path().join("manifest.sha256");
            let plan = library.plan_manifest(&root_id, ManifestFormat::Sha256, &output)?;
            library.approve_manifest(&plan)?;
            let (_result, _hits) =
                failpoints::run_failing(fail_at, || library.execute_manifest(&plan));
            let first = library.recover_pending()?;
            let second = library.recover_pending()?;
            assert!(first.unresolved.is_empty(), "boundary {fail_at}");
            assert!(second.unresolved.is_empty(), "boundary {fail_at}");
            assert_eq!(
                std::fs::read(source.join("track.flac"))?,
                b"preserved audio",
                "boundary {fail_at}"
            );
            if output.exists() {
                assert!(
                    verify_sha256_manifest(&source, &output)?.is_valid(),
                    "boundary {fail_at}"
                );
            }
            let orphaned = std::fs::read_dir(temporary.path())?
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .any(|name| name.contains("manifest-stage"));
            assert!(!orphaned, "boundary {fail_at}");
        }
        Ok(())
    }
}
