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
use crate::fsops::{validate_absolute_root_path, AnchoredRoot, ReadRoot};
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

    fn request(&self) -> serde_json::Value {
        json!({
            "root_id": self.root_id,
            "root_path": self.root_path,
            "format": self.format,
            "output": self.output,
        })
    }

    fn preview(&self) -> serde_json::Value {
        json!({
            "asset_count": self.asset_count,
            "byte_count": self.byte_count,
            "output_must_remain_absent": true,
            "journaled": true,
        })
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

    fn request(&self) -> serde_json::Value {
        json!({
            "bag": self.bag,
            "destination": self.destination,
            "manifest_sha256": self.manifest_sha256,
        })
    }

    fn preview(&self) -> serde_json::Value {
        json!({
            "file_count": self.file_count,
            "byte_count": self.byte_count,
            "destination_must_remain_absent": true,
            "source_will_be_reverified": true,
        })
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
        self.registered_root_for_path(path)?
            .filter(|root| root.path() == path)
            .map(|root| root.id().clone())
            .ok_or_else(|| Error::Preservation("library root is not registered".into()))
    }

    /// Persist a side-effect-free manifest or `BagIt` preview.
    pub fn plan_manifest(
        &self,
        root_id: &RootId,
        format: ManifestFormat,
        output: &Path,
    ) -> Result<ManifestPlan> {
        require_absolute_absent(output, "manifest output")?;
        self.require_path_root_mutation(output)?;
        let (root_path, state) = self.root_record(root_id)?;
        validate_absolute_root_path(&root_path)?;
        if state == "offline" {
            return Err(Error::Preservation("library root is offline".into()));
        }
        let (managed, verified, byte_count) = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN verification_state = 'verified'
                                  AND byte_size IS NOT NULL AND sha256 IS NOT NULL
                                  AND entry_identity IS NOT NULL THEN 1 ELSE 0 END), 0),
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
            "root_path": root_path,
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
        self.require_saved_plan(
            plan.id(),
            PlanKind::Manifest,
            &plan.request(),
            &plan.preview(),
        )?;
        self.approve_durable_plan(plan.id())
    }

    /// Execute an approved export through a typed journal and no-clobber publication.
    pub fn execute_manifest(&mut self, plan: &ManifestPlan) -> Result<ManifestReceipt> {
        self.require_reviewed_plan(
            plan.id(),
            PlanKind::Manifest,
            &plan.request(),
            &plan.preview(),
        )?;
        if let Err(error) = self.start_durable_plan(plan.id()) {
            return self.fail_plan_without_operation(plan.id(), error);
        }
        let preflight = require_absolute_absent(&plan.output, "manifest output")
            .and_then(|()| self.require_path_root_mutation(&plan.output))
            .and_then(|()| self.root_record(&plan.root_id))
            .and_then(|(path, state)| {
                validate_absolute_root_path(&path)?;
                if path != plan.root_path || state == "offline" {
                    Err(Error::Preservation(
                        "library root changed after manifest preview".into(),
                    ))
                } else {
                    Ok(())
                }
            });
        if let Err(error) = preflight {
            return self.fail_plan_without_operation(plan.id(), error);
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
        self.require_path_root_mutation(destination)?;
        let manifest = bag.join("manifest-sha256.txt");
        let verification = verify_bagit_manifest(bag, &manifest)?;
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
        self.require_saved_plan(
            plan.id(),
            PlanKind::BackupRestore,
            &plan.request(),
            &plan.preview(),
        )?;
        self.approve_durable_plan(plan.id())
    }

    /// Exercise a restore into a new directory and verify every restored payload.
    pub fn execute_restore(&mut self, plan: &RestorePlan) -> Result<RestoreReceipt> {
        self.require_reviewed_plan(
            plan.id(),
            PlanKind::BackupRestore,
            &plan.request(),
            &plan.preview(),
        )?;
        if let Err(error) = self.start_durable_plan(plan.id()) {
            return self.fail_plan_without_operation(plan.id(), error);
        }
        let preflight = require_absolute_absent(&plan.destination, "restore destination")
            .and_then(|()| self.require_path_root_mutation(&plan.destination))
            .and_then(|()| {
                let manifest = plan.bag.join("manifest-sha256.txt");
                let digest = digest_reader(std::fs::File::open(&manifest)?)?;
                if digest.sha256() != plan.manifest_sha256 {
                    return Err(Error::Preservation(
                        "source manifest changed after restore preview".into(),
                    ));
                }
                let verification = verify_bagit_manifest(&plan.bag, &manifest)?;
                if !verification.is_valid() {
                    return Err(Error::Preservation(
                        "source bag changed after restore preview".into(),
                    ));
                }
                Ok(())
            });
        if let Err(error) = preflight {
            return self.fail_plan_without_operation(plan.id(), error);
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
                            "{}",
                            gnu_manifest_record(&asset.sha256, &asset.relative_path)?
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
        output_root.create_parent_all(&staged_bag.join("data/placeholder"))?;
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
                            bagit_manifest_path(&relative)?
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
        let verification = verify_bagit_manifest(&staged_bag, &manifest)?;
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
        // The operation ID links this run to recovery without another association column.
        let run_id = operation.to_owned();
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
            let (expected, relative) = parse_manifest_line(&line, ManifestFormat::BagIt)?;
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

/// Verify a GNU SHA-256 manifest beneath `root`.
///
/// Both GNU mode flags verify raw file bytes without text conversion.
pub fn verify_sha256_manifest(root: &Path, manifest: &Path) -> Result<ManifestVerification> {
    verify_manifest(root, manifest, ManifestFormat::Sha256)
}

/// Verify an RFC 8493 SHA-256 payload manifest beneath `root`.
pub fn verify_bagit_manifest(root: &Path, manifest: &Path) -> Result<ManifestVerification> {
    verify_manifest(root, manifest, ManifestFormat::BagIt)
}

fn verify_manifest(
    root: &Path,
    manifest: &Path,
    format: ManifestFormat,
) -> Result<ManifestVerification> {
    require_absolute_directory(root, "manifest root")?;
    let anchored = ReadRoot::open(root)?;
    let manifest_root = ReadRoot::open(absolute_parent(manifest)?)?;
    let file = manifest_root.open_file(manifest)?;
    let mut report = ManifestVerification::default();
    let mut seen = HashSet::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        let (expected, relative) = parse_manifest_line(&line, format).map_err(|error| {
            Error::Preservation(format!("malformed manifest line {}: {error}", index + 1))
        })?;
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
    let anchored = ReadRoot::open(root)?;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for line in BufReader::new(anchored.open_file(manifest)?).lines() {
        let line = line?;
        let (_expected, relative) = parse_manifest_line(&line, ManifestFormat::BagIt)?;
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

fn parse_manifest_line(line: &str, format: ManifestFormat) -> Result<(&str, PathBuf)> {
    let (line, escaped) = if format == ManifestFormat::Sha256 {
        line.strip_prefix('\\')
            .map_or((line, false), |line| (line, true))
    } else {
        (line, false)
    };
    let (expected, relative) = match format {
        ManifestFormat::Sha256 => {
            let (expected, flagged_path) = line
                .split_once(' ')
                .ok_or_else(|| Error::Preservation("missing GNU path delimiter".into()))?;
            let relative = flagged_path
                .strip_prefix([' ', '*'])
                .ok_or_else(|| Error::Preservation("invalid GNU checksum mode".into()))?;
            (expected, relative)
        }
        ManifestFormat::BagIt => {
            let (expected, relative) = line
                .split_once([' ', '\t'])
                .ok_or_else(|| Error::Preservation("missing BagIt path delimiter".into()))?;
            (expected, relative.trim_start_matches([' ', '\t']))
        }
    };
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Preservation("invalid SHA-256".into()));
    }
    let relative = match format {
        ManifestFormat::Sha256 if escaped => decode_gnu_manifest_path(relative)?,
        ManifestFormat::Sha256 => relative.to_owned(),
        ManifestFormat::BagIt => decode_bagit_manifest_path(relative)?,
    };
    #[cfg(windows)]
    if format == ManifestFormat::BagIt && relative.contains('\\') {
        return Err(Error::Preservation(
            "BagIt path contains a literal backslash unsupported on Windows".into(),
        ));
    }
    let relative = safe_relative(Path::new(&relative))?;
    if format == ManifestFormat::BagIt && !relative.starts_with("data") {
        return Err(Error::Preservation(
            "BagIt manifest path is outside data/".into(),
        ));
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
    path.iter()
        .map(|component| {
            component
                .to_str()
                .ok_or_else(|| Error::Preservation("manifest path must be valid UTF-8".into()))
        })
        .collect::<Result<Vec<_>>>()
        .map(|components| components.join("/"))
}

fn gnu_manifest_record(sha256: &str, path: &Path) -> Result<String> {
    let path = portable_manifest_path(path)?;
    if path.contains(['\\', '\n', '\r']) {
        let escaped = path
            .replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        Ok(format!("\\{sha256}  {escaped}"))
    } else {
        Ok(format!("{sha256}  {path}"))
    }
}

fn bagit_manifest_path(path: &Path) -> Result<String> {
    Ok(portable_manifest_path(path)?
        .replace('%', "%25")
        .replace('\n', "%0A")
        .replace('\r', "%0D"))
}

fn decode_gnu_manifest_path(value: &str) -> Result<String> {
    let mut decoded = String::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            decoded.push(match chars.next() {
                Some('\\') => '\\',
                Some('n') => '\n',
                Some('r') => '\r',
                _ => return Err(Error::Preservation("invalid GNU filename escape".into())),
            });
        } else {
            decoded.push(character);
        }
    }
    Ok(decoded)
}

fn decode_bagit_manifest_path(value: &str) -> Result<String> {
    let mut decoded = String::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character == '%' {
            decoded.push(match (chars.next(), chars.next()) {
                (Some('0'), Some('A' | 'a')) => '\n',
                (Some('0'), Some('D' | 'd')) => '\r',
                (Some('2'), Some('5')) => '%',
                _ => return Err(Error::Preservation("invalid BagIt filename escape".into())),
            });
        } else {
            decoded.push(character);
        }
    }
    Ok(decoded)
}

fn absolute_parent(path: &Path) -> Result<&Path> {
    validate_absolute_root_path(path)?;
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
    validate_absolute_root_path(path)?;
    if !path.is_dir() || path.is_symlink() {
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
    Ok(crate::fsops::bounded_sibling_path(
        path,
        format!(".{name}.rsbts-{}-{role}", uuid::Uuid::new_v4()),
    ))
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::Preservation("path must be valid UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failpoints;

    #[test]
    fn preservation_root_lookup_accepts_equivalent_paths() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("library");
        std::fs::create_dir(&path)?;
        let library = Library::open_in_memory()?;
        let root = library.register_root(&path)?;
        for equivalent in [
            PathBuf::from(format!("{}/", path.display())),
            temporary.path().join("./library"),
            path.join("."),
        ] {
            let result = library.root_id_for_path(&equivalent);
            assert!(
                result.as_ref().is_ok_and(|id| id == root.id()),
                "equivalent root path missed its stored ID: {}: {result:?}",
                equivalent.display()
            );
        }
        Ok(())
    }

    #[test]
    fn preservation_root_lookup_rejects_descendant_paths() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("library");
        std::fs::create_dir(&path)?;
        let library = Library::open_in_memory()?;
        let root = library.register_root(&path)?;

        assert!(
            library.root_id_for_path(&path.join("child")).is_err(),
            "a descendant resolved as its ancestor root"
        );
        assert_eq!(library.root_id_for_path(&path)?, *root.id());
        Ok(())
    }

    #[test]
    fn preservation_root_lookup_rejects_ambiguous_records() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("library");
        std::fs::create_dir(&path)?;
        let library = Library::open_in_memory()?;
        let root = library.register_root(&path)?;
        let alias = RootId::new();
        library.conn.execute(
            "INSERT INTO library_roots
             (id, path, state, capabilities_json, created_at, updated_at)
             SELECT ?2, ?3, state, capabilities_json, created_at, updated_at
             FROM library_roots WHERE id = ?1",
            params![
                root.id().as_str(),
                alias.as_str(),
                path_text(&path.join("."))?
            ],
        )?;

        let result = library.root_id_for_path(&path);
        assert!(
            result.is_err(),
            "ambiguous root records resolved without review"
        );
        assert!(result
            .err()
            .is_some_and(|error| error.to_string().contains("multiple roots")));
        Ok(())
    }

    #[test]
    fn preservation_root_lookup_rejects_parent_components() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("library");
        std::fs::create_dir(&path)?;
        let library = Library::open_in_memory()?;
        let root = library.register_root(&path)?;
        let unsafe_path = path.join("../library");
        library.conn.execute(
            "UPDATE library_roots SET path = ?2 WHERE id = ?1",
            params![root.id().as_str(), path_text(&unsafe_path)?],
        )?;

        assert!(
            library.root_id_for_path(&unsafe_path).is_err(),
            "an old root record authorized parent components"
        );
        assert!(path.is_dir());
        Ok(())
    }

    fn add_managed_asset(library: &Library, root: &Path, relative: &Path) -> Result<RootId> {
        let root_id = RootId::new();
        let path = root.join(relative);
        let digests = digest_reader(std::fs::File::open(&path)?)?;
        let metadata = std::fs::metadata(&path)?;
        let now = Utc::now().to_rfc3339();
        let capabilities = serde_json::to_string(&crate::roots::RootCapabilities::detect(root)?)?;
        library.conn.execute(
            "INSERT INTO library_roots
             (id, path, state, capabilities_json, created_at, updated_at)
             VALUES (?1, ?2, 'online', ?4, ?3, ?3)",
            params![root_id.as_str(), path_text(root)?, now, capabilities],
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

    fn collection_fixture(
        relative: &Path,
    ) -> Result<(tempfile::TempDir, Library, RootId, PathBuf)> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join(relative), b"preserved audio")?;
        let library = Library::open_in_memory()?;
        let root_id = add_managed_asset(&library, &source, relative)?;
        Ok((temporary, library, root_id, source))
    }

    fn restore_fixture() -> Result<(tempfile::TempDir, Library, PathBuf)> {
        let temporary = tempfile::tempdir()?;
        let bag = temporary.path().join("bag");
        std::fs::create_dir_all(bag.join("data"))?;
        let payload = bag.join("data/track.flac");
        std::fs::write(&payload, b"preserved audio")?;
        let digest = digest_reader(std::fs::File::open(payload)?)?;
        std::fs::write(
            bag.join("manifest-sha256.txt"),
            format!("{}  data/track.flac\n", digest.sha256()),
        )?;
        std::fs::write(
            bag.join("bagit.txt"),
            b"BagIt-Version: 1.0\nTag-File-Character-Encoding: UTF-8\n",
        )?;
        Ok((temporary, Library::open_in_memory()?, bag))
    }

    fn restore_run_state(library: &Library, id: &str) -> Result<(String, Option<String>)> {
        Ok(library.conn.query_row(
            "SELECT state, completed_at FROM backup_restore_runs WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    fn pending_restore_fixture(
        state: &str,
    ) -> Result<(tempfile::TempDir, Library, RestorePlan, String)> {
        let (temporary, library, bag) = restore_fixture()?;
        let plan = library.plan_restore(&bag, &temporary.path().join("restored"))?;
        library.approve_restore(&plan)?;
        library.start_durable_plan(plan.id())?;
        let operation = library.create_operation_for_plan(
            OperationKind::RestoreCopy,
            &[],
            Some(plan.id().as_str()),
            None,
        )?;
        library.set_operation_state(&operation, state, None)?;
        library.conn.execute(
            "INSERT INTO backup_restore_runs
             (id, source_path, restore_path, state, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![
                operation,
                path_text(&plan.bag)?,
                path_text(&plan.destination)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok((temporary, library, plan, operation))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn restore_failure_finishes_the_bound_run() -> Result<()> {
        let (temporary, mut library, bag) = restore_fixture()?;
        let plan = library.plan_restore(&bag, &temporary.path().join("restored"))?;
        library.approve_restore(&plan)?;
        library.conn.execute_batch(
            "CREATE TEMP TRIGGER reject_restore_payload BEFORE INSERT ON operation_files
             WHEN NEW.role = 'restore-payload'
             BEGIN SELECT RAISE(ABORT, 'restore payload blocked'); END;",
        )?;
        assert!(library.execute_restore(&plan).is_err());
        let (run_id, state): (String, String) =
            library
                .conn
                .query_row("SELECT id, state FROM backup_restore_runs", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
        assert_eq!(
            state, "failed",
            "ordinary restore failure left a running row"
        );
        let operation: String = library.conn.query_row(
            "SELECT id FROM operation_journal WHERE kind = 'restore-copy'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            run_id, operation,
            "restore run has no stable operation link"
        );
        assert!(restore_run_state(&library, &run_id)?.1.is_some());
        assert!(library.recover_pending()?.recovered_operations.is_empty());
        Ok(())
    }

    #[test]
    fn restore_recovery_finishes_running_rows_once() -> Result<()> {
        for state in ["prepared", "staging", "db-committed", "cleanup-pending"] {
            let (_temporary, mut library, plan, operation) = pending_restore_fixture(state)?;
            let expected = if matches!(state, "db-committed" | "cleanup-pending") {
                "complete"
            } else {
                "failed"
            };
            assert_eq!(
                library.recover_pending()?.recovered_operations.as_slice(),
                std::slice::from_ref(&operation)
            );
            let after = restore_run_state(&library, &operation)?;
            assert_eq!(
                after.0, expected,
                "{state} recovery left a running restore row"
            );
            assert!(after.1.is_some());
            let events = serde_json::to_value(library.plan_events(plan.id())?)?;
            assert!(library.recover_pending()?.recovered_operations.is_empty());
            assert_eq!(restore_run_state(&library, &operation)?, after);
            assert_eq!(
                serde_json::to_value(library.plan_events(plan.id())?)?,
                events
            );
        }
        Ok(())
    }

    #[test]
    fn restore_recovery_preserves_terminal_and_unbound_rows() -> Result<()> {
        for terminal in ["complete", "failed"] {
            let (_temporary, mut library, _plan, operation) = pending_restore_fixture("prepared")?;
            library.conn.execute(
                "UPDATE backup_restore_runs SET state = ?2, detail = 'original',
                 completed_at = 'original completion' WHERE id = ?1",
                params![operation, terminal],
            )?;
            library.conn.execute(
                "INSERT INTO backup_restore_runs
                 (id, source_path, restore_path, state, detail, started_at)
                 SELECT 'old-unbound-run', source_path, restore_path, 'running', 'old', started_at
                 FROM backup_restore_runs WHERE id = ?1",
                [&operation],
            )?;
            let before = restore_run_state(&library, &operation)?;
            library.recover_pending()?;
            assert_eq!(restore_run_state(&library, &operation)?, before);
            assert_eq!(
                restore_run_state(&library, "old-unbound-run")?,
                ("running".into(), None)
            );
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn restore_recovery_preserves_uncertain_rows() -> Result<()> {
        let (temporary, mut library, _plan, operation) = pending_restore_fixture("staging")?;
        let staged = temporary.path().join("uncertain.stage");
        std::fs::write(&staged, b"newcomer")?;
        library.append_operation_file(
            &operation,
            &JournalFile {
                source: temporary.path().join("source"),
                destination: temporary.path().join("destination"),
                staged: staged.clone(),
                content_hash: Some("wrong hash".into()),
                sha256: None,
                source_identity: None,
                owned_identity: None,
                role: "restore-payload".into(),
                state: "staged".into(),
            },
        )?;
        for _ in 0..2 {
            assert_eq!(library.recover_pending()?.unresolved.len(), 1);
            assert_eq!(
                restore_run_state(&library, &operation)?,
                ("running".into(), None)
            );
            assert_eq!(std::fs::read(&staged)?, b"newcomer");
        }
        Ok(())
    }

    #[test]
    fn restore_recovery_rolls_back_history_with_journal_failure() -> Result<()> {
        for state in ["prepared", "db-committed"] {
            let (_temporary, mut library, plan, operation) = pending_restore_fixture(state)?;
            library.conn.execute_batch(
                "CREATE TEMP TRIGGER reject_restore_completion
                 BEFORE UPDATE OF state ON operation_journal WHEN NEW.state = 'complete'
                 BEGIN SELECT RAISE(ABORT, 'restore completion blocked'); END;",
            )?;
            let events = serde_json::to_value(library.plan_events(plan.id())?)?;
            assert!(library.recover_pending().is_err());
            assert_eq!(
                restore_run_state(&library, &operation)?,
                ("running".into(), None)
            );
            assert_eq!(library.durable_plan(plan.id())?.state(), PlanState::Running);
            assert_eq!(
                serde_json::to_value(library.plan_events(plan.id())?)?,
                events
            );
            library
                .conn
                .execute_batch("DROP TRIGGER reject_restore_completion")?;
            library.recover_pending()?;
            let expected = if state == "db-committed" {
                "complete"
            } else {
                "failed"
            };
            assert_eq!(restore_run_state(&library, &operation)?.0, expected);
            assert!(library.recover_pending()?.recovered_operations.is_empty());
        }
        Ok(())
    }

    #[test]
    fn manifest_approval_rejects_changed_saved_content() -> Result<()> {
        let mut accepted = Vec::new();
        let mut transitioned = Vec::new();
        for field in [
            "root_id",
            "root_path",
            "format",
            "output",
            "asset_count",
            "byte_count",
            "id",
        ] {
            let (temporary, library, root_id, _source) =
                collection_fixture(Path::new("track.flac"))?;
            let output = temporary.path().join("manifest.sha256");
            let plan = library.plan_manifest(&root_id, ManifestFormat::Sha256, &output)?;
            let mut encoded = serde_json::to_value(&plan)?;
            let mut saved_id = plan.id().clone();
            encoded[field] = match field {
                "root_id" => json!(RootId::new()),
                "root_path" => json!(temporary.path()),
                "format" => json!(ManifestFormat::BagIt),
                "output" => json!(temporary.path().join("other-output")),
                "asset_count" | "byte_count" => json!(0),
                _ => {
                    saved_id = library.create_durable_plan(
                        PlanKind::Audit,
                        &json!({}),
                        &json!({}),
                        None,
                    )?;
                    json!(saved_id)
                }
            };
            let edited: ManifestPlan = serde_json::from_value(encoded)?;
            if library.approve_manifest(&edited).is_ok() {
                accepted.push(field);
            }
            if library.durable_plan(&saved_id)?.state() != PlanState::Planned {
                transitioned.push(field);
            }
            assert!(!output.exists());
        }
        assert!(
            accepted.is_empty(),
            "edited manifest approvals passed: {accepted:?}"
        );
        assert!(
            transitioned.is_empty(),
            "edited approvals changed state: {transitioned:?}"
        );
        Ok(())
    }

    #[test]
    fn restore_approval_rejects_changed_saved_content() -> Result<()> {
        let mut accepted = Vec::new();
        let mut transitioned = Vec::new();
        for field in [
            "bag",
            "destination",
            "manifest_sha256",
            "file_count",
            "byte_count",
            "id",
        ] {
            let (temporary, library, bag) = restore_fixture()?;
            let destination = temporary.path().join("restored");
            let plan = library.plan_restore(&bag, &destination)?;
            let mut encoded = serde_json::to_value(&plan)?;
            let mut saved_id = plan.id().clone();
            encoded[field] = match field {
                "bag" => json!(temporary.path()),
                "destination" => json!(temporary.path().join("other-output")),
                "manifest_sha256" => json!("0".repeat(64)),
                "file_count" | "byte_count" => json!(0),
                _ => {
                    saved_id = library.create_durable_plan(
                        PlanKind::Audit,
                        &json!({}),
                        &json!({}),
                        None,
                    )?;
                    json!(saved_id)
                }
            };
            let edited: RestorePlan = serde_json::from_value(encoded)?;
            if library.approve_restore(&edited).is_ok() {
                accepted.push(field);
            }
            if library.durable_plan(&saved_id)?.state() != PlanState::Planned {
                transitioned.push(field);
            }
            assert!(!destination.exists());
        }
        assert!(
            accepted.is_empty(),
            "edited restore approvals passed: {accepted:?}"
        );
        assert!(
            transitioned.is_empty(),
            "edited approvals changed state: {transitioned:?}"
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn check_manifest_output_root_policy(
        format: ManifestFormat,
        after_approval: bool,
    ) -> Result<()> {
        use crate::roots::RootState;

        let mut failures = Vec::new();
        for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
            let (temporary, mut library, root_id, source) =
                collection_fixture(Path::new("track.flac"))?;
            let output_directory = temporary.path().join("output");
            std::fs::create_dir(&output_directory)?;
            let output_root = library.register_root(&output_directory)?;
            let output = output_directory.join("export");
            let plan = if after_approval {
                let plan = library.plan_manifest(&root_id, format, &output)?;
                library.approve_manifest(&plan)?;
                Some(plan)
            } else {
                None
            };
            library.set_root_state(output_root.id(), state)?;
            let result = if let Some(plan) = &plan {
                library.execute_manifest(plan).map(|_| ())
            } else {
                library.plan_manifest(&root_id, format, &output).map(|_| ())
            };
            if result.is_ok() {
                failures.push(format!("accepted {state:?}"));
            }
            if let Some(plan) = plan {
                if library.durable_plan(plan.id())?.state() != PlanState::Failed {
                    failures.push(format!("did not fail execution for {state:?}"));
                }
            } else {
                let plans: u64 =
                    library
                        .conn
                        .query_row("SELECT COUNT(*) FROM durable_plans", [], |row| row.get(0))?;
                if plans != 0 {
                    failures.push(format!("stored a plan for {state:?}"));
                }
            }
            if std::fs::read_dir(&output_directory)?.next().is_some() {
                failures.push(format!("created output for {state:?}"));
            }
            let operations: u64 =
                library
                    .conn
                    .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
                        row.get(0)
                    })?;
            if operations != 0 {
                failures.push(format!("created a journal for {state:?}"));
            }
            assert_eq!(
                std::fs::read(source.join("track.flac"))?,
                b"preserved audio"
            );
        }
        assert!(
            failures.is_empty(),
            "{format:?} output policy failures: {failures:?}"
        );

        let (temporary, mut library, root_id, source) =
            collection_fixture(Path::new("track.flac"))?;
        library.set_root_state(&root_id, RootState::ReadOnly)?;
        let output = temporary.path().join("read-only-source-export");
        let plan = library.plan_manifest(&root_id, format, &output)?;
        library.approve_manifest(&plan)?;
        library.execute_manifest(&plan)?;
        assert_eq!(
            std::fs::read(source.join("track.flac"))?,
            b"preserved audio"
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn check_restore_output_root_policy(after_approval: bool) -> Result<()> {
        use crate::roots::RootState;

        let mut failures = Vec::new();
        for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
            let (temporary, mut library, bag) = restore_fixture()?;
            let output_directory = temporary.path().join("output");
            std::fs::create_dir(&output_directory)?;
            let output_root = library.register_root(&output_directory)?;
            let destination = output_directory.join("restored");
            let plan = if after_approval {
                let plan = library.plan_restore(&bag, &destination)?;
                library.approve_restore(&plan)?;
                Some(plan)
            } else {
                None
            };
            library.set_root_state(output_root.id(), state)?;
            let result = if let Some(plan) = &plan {
                library.execute_restore(plan).map(|_| ())
            } else {
                library.plan_restore(&bag, &destination).map(|_| ())
            };
            if result.is_ok() {
                failures.push(format!("accepted {state:?}"));
            }
            if let Some(plan) = plan {
                if library.durable_plan(plan.id())?.state() != PlanState::Failed {
                    failures.push(format!("did not fail execution for {state:?}"));
                }
            } else {
                let plans: u64 =
                    library
                        .conn
                        .query_row("SELECT COUNT(*) FROM durable_plans", [], |row| row.get(0))?;
                if plans != 0 {
                    failures.push(format!("stored a plan for {state:?}"));
                }
            }
            if std::fs::read_dir(&output_directory)?.next().is_some() {
                failures.push(format!("created output for {state:?}"));
            }
            let operations: u64 =
                library
                    .conn
                    .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
                        row.get(0)
                    })?;
            if operations != 0 {
                failures.push(format!("created a journal for {state:?}"));
            }
            assert_eq!(
                std::fs::read(bag.join("data/track.flac"))?,
                b"preserved audio"
            );
        }
        assert!(
            failures.is_empty(),
            "restore output policy failures: {failures:?}"
        );

        let (temporary, mut library, bag) = restore_fixture()?;
        let source_root = library.register_root(&bag)?;
        library.set_root_state(source_root.id(), RootState::ReadOnly)?;
        let destination = temporary.path().join("restored-read-only-source");
        let plan = library.plan_restore(&bag, &destination)?;
        library.approve_restore(&plan)?;
        library.execute_restore(&plan)?;
        assert_eq!(
            std::fs::read(destination.join("track.flac"))?,
            b"preserved audio"
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn sha256_planning_rejects_disabled_output_roots() -> Result<()> {
        check_manifest_output_root_policy(ManifestFormat::Sha256, false)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn sha256_execution_rejects_disabled_output_roots() -> Result<()> {
        check_manifest_output_root_policy(ManifestFormat::Sha256, true)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn bagit_planning_rejects_disabled_output_roots() -> Result<()> {
        check_manifest_output_root_policy(ManifestFormat::BagIt, false)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn bagit_execution_rejects_disabled_output_roots() -> Result<()> {
        check_manifest_output_root_policy(ManifestFormat::BagIt, true)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn restore_planning_rejects_disabled_output_roots() -> Result<()> {
        check_restore_output_root_policy(false)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn restore_execution_rejects_disabled_output_roots() -> Result<()> {
        check_restore_output_root_policy(true)
    }

    #[test]
    fn manifest_planning_rejects_parent_components_in_output() -> Result<()> {
        let (temporary, library, root_id, source) = collection_fixture(Path::new("track.flac"))?;
        let alias = temporary.path().join("alias");
        std::fs::create_dir(&alias)?;
        let output = alias.join("..").join("manifest.sha256");
        assert!(
            library
                .plan_manifest(&root_id, ManifestFormat::Sha256, &output)
                .is_err(),
            "manifest planning accepted an output with a parent component"
        );
        let plans: u64 =
            library
                .conn
                .query_row("SELECT COUNT(*) FROM durable_plans", [], |row| row.get(0))?;
        assert_eq!(plans, 0);
        assert!(!output.exists());
        assert_eq!(
            std::fs::read(source.join("track.flac"))?,
            b"preserved audio"
        );
        Ok(())
    }

    #[test]
    fn manifest_planning_rejects_parent_components_in_stored_root() -> Result<()> {
        let (temporary, library, root_id, source) = collection_fixture(Path::new("track.flac"))?;
        let alias = temporary.path().join("alias");
        std::fs::create_dir(&alias)?;
        let stored_root = alias.join("..").join("source");
        library.conn.execute(
            "UPDATE library_roots SET path = ?1 WHERE id = ?2",
            params![path_text(&stored_root)?, root_id.as_str()],
        )?;
        let output = temporary.path().join("manifest.sha256");
        assert!(
            library
                .plan_manifest(&root_id, ManifestFormat::Sha256, &output)
                .is_err(),
            "manifest planning accepted a stored root with a parent component"
        );
        let plans: u64 =
            library
                .conn
                .query_row("SELECT COUNT(*) FROM durable_plans", [], |row| row.get(0))?;
        assert_eq!(plans, 0);
        assert!(!output.exists());
        assert_eq!(
            std::fs::read(source.join("track.flac"))?,
            b"preserved audio"
        );
        Ok(())
    }

    #[test]
    fn restore_planning_rejects_parent_components_in_destination() -> Result<()> {
        let (temporary, library, bag) = restore_fixture()?;
        let alias = temporary.path().join("alias");
        std::fs::create_dir(&alias)?;
        let destination = alias.join("..").join("restored");
        assert!(
            library.plan_restore(&bag, &destination).is_err(),
            "restore planning accepted a destination with a parent component"
        );
        let plans: u64 =
            library
                .conn
                .query_row("SELECT COUNT(*) FROM durable_plans", [], |row| row.get(0))?;
        assert_eq!(plans, 0);
        assert!(!destination.exists());
        assert_eq!(
            std::fs::read(bag.join("data/track.flac"))?,
            b"preserved audio"
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn manifest_execution_rejects_edited_approved_plans() -> Result<()> {
        for field in [
            "output",
            "format",
            "root_path",
            "root_id",
            "asset_count",
            "byte_count",
        ] {
            let (temporary, mut library, root_id, source) =
                collection_fixture(Path::new("track.flac"))?;
            let output = temporary.path().join("approved.sha256");
            let unapproved = temporary.path().join("unapproved.sha256");
            let plan = library.plan_manifest(&root_id, ManifestFormat::Sha256, &output)?;
            library.approve_manifest(&plan)?;
            let mut encoded = serde_json::to_value(&plan)?;
            encoded[field] = match field {
                "output" => json!(unapproved),
                "format" => json!(ManifestFormat::BagIt),
                "root_path" => json!(temporary.path()),
                "root_id" => json!(RootId::new()),
                _ => json!(0),
            };
            let edited: ManifestPlan = serde_json::from_value(encoded)?;
            assert!(
                library.execute_manifest(&edited).is_err(),
                "edited manifest {field} bypassed approval"
            );
            assert_eq!(
                library.durable_plan(plan.id())?.state(),
                PlanState::Approved
            );
            assert!(!output.exists());
            assert!(!unapproved.exists());
            assert_eq!(
                std::fs::read(source.join("track.flac"))?,
                b"preserved audio"
            );
            let operations: u32 =
                library
                    .conn
                    .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
                        row.get(0)
                    })?;
            assert_eq!(operations, 0);
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn restore_execution_rejects_edited_approved_plans() -> Result<()> {
        for field in [
            "destination",
            "bag",
            "manifest_sha256",
            "file_count",
            "byte_count",
        ] {
            let (temporary, mut library, bag) = restore_fixture()?;
            let destination = temporary.path().join("approved-restore");
            let unapproved = temporary.path().join("unapproved-restore");
            let plan = library.plan_restore(&bag, &destination)?;
            library.approve_restore(&plan)?;
            let mut encoded = serde_json::to_value(&plan)?;
            encoded[field] = match field {
                "destination" => json!(unapproved),
                "bag" => json!(temporary.path().join("unapproved-bag")),
                "manifest_sha256" => json!("0".repeat(64)),
                _ => json!(0),
            };
            let edited: RestorePlan = serde_json::from_value(encoded)?;
            assert!(
                library.execute_restore(&edited).is_err(),
                "edited restore {field} bypassed approval"
            );
            assert_eq!(
                library.durable_plan(plan.id())?.state(),
                PlanState::Approved
            );
            assert!(!destination.exists());
            assert!(!unapproved.exists());
            assert_eq!(
                std::fs::read(bag.join("data/track.flac"))?,
                b"preserved audio"
            );
            let operations: u32 =
                library
                    .conn
                    .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
                        row.get(0)
                    })?;
            assert_eq!(operations, 0);
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn manifest_prejournal_errors_finish_the_durable_plan() -> Result<()> {
        for failure in ["occupied-output", "missing-root-record"] {
            let (temporary, mut library, root_id, source) =
                collection_fixture(Path::new("track.flac"))?;
            let output = temporary.path().join("manifest.sha256");
            let plan = library.plan_manifest(&root_id, ManifestFormat::Sha256, &output)?;
            library.approve_manifest(&plan)?;
            if failure == "occupied-output" {
                std::fs::write(&output, b"independent bytes")?;
            } else {
                library.conn.execute(
                    "UPDATE library_roots SET state = 'legacy' WHERE id = ?1",
                    [root_id.as_str()],
                )?;
            }
            assert!(library.execute_manifest(&plan).is_err());
            assert_eq!(
                library.durable_plan(plan.id())?.state(),
                PlanState::Failed,
                "{failure} left the manifest plan running"
            );
            assert_eq!(
                std::fs::read(source.join("track.flac"))?,
                b"preserved audio"
            );
            if output.exists() {
                assert_eq!(std::fs::read(output)?, b"independent bytes");
            }
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn restore_prejournal_errors_finish_the_durable_plan() -> Result<()> {
        for failure in ["occupied-destination", "missing-manifest"] {
            let (temporary, mut library, bag) = restore_fixture()?;
            let destination = temporary.path().join("restore");
            let plan = library.plan_restore(&bag, &destination)?;
            library.approve_restore(&plan)?;
            if failure == "occupied-destination" {
                std::fs::create_dir(&destination)?;
                std::fs::write(destination.join("independent"), b"independent bytes")?;
            } else {
                std::fs::remove_file(bag.join("manifest-sha256.txt"))?;
            }
            assert!(library.execute_restore(&plan).is_err());
            assert_eq!(
                library.durable_plan(plan.id())?.state(),
                PlanState::Failed,
                "{failure} left the restore plan running"
            );
            assert_eq!(
                std::fs::read(bag.join("data/track.flac"))?,
                b"preserved audio"
            );
            if destination.exists() {
                assert_eq!(
                    std::fs::read(destination.join("independent"))?,
                    b"independent bytes"
                );
            }
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn sha256_exports_preserve_literal_filename_characters() -> Result<()> {
        for (filename, encoded, escaped) in [
            ("literal\\name.flac", "literal\\\\name.flac", true),
            ("line\nfeed.flac", "line\\nfeed.flac", true),
            ("carriage\rreturn.flac", "carriage\\rreturn.flac", true),
            ("double  spaces.flac", "double  spaces.flac", false),
            ("literal%2Fname.flac", "literal%2Fname.flac", false),
        ] {
            let (temporary, mut library, root_id, source) =
                collection_fixture(Path::new(filename))?;
            let output = temporary.path().join("manifest.sha256");
            let plan = library.plan_manifest(&root_id, ManifestFormat::Sha256, &output)?;
            library.approve_manifest(&plan)?;
            let result = library.execute_manifest(&plan);
            assert!(
                result.is_ok(),
                "valid filename could not be exported: {filename:?}"
            );
            let digest = digest_reader(std::fs::File::open(source.join(filename))?)?;
            let marker = if escaped { "\\" } else { "" };
            assert_eq!(
                std::fs::read_to_string(&output)?,
                format!("{marker}{}  {encoded}\n", digest.sha256())
            );
            assert!(verify_sha256_manifest(&source, &output)?.is_valid());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn sha256_verification_decodes_gnu_escaped_filenames() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let filename = "literal\\name\nline\rreturn.flac";
        let payload = temporary.path().join(filename);
        std::fs::write(&payload, b"preserved audio")?;
        let digest = digest_reader(std::fs::File::open(payload)?)?;
        let manifest = temporary.path().join("manifest.sha256");
        std::fs::write(
            &manifest,
            format!(
                "\\{}  literal\\\\name\\nline\\rreturn.flac\n",
                digest.sha256()
            ),
        )?;
        let result = verify_sha256_manifest(temporary.path(), &manifest);
        assert!(
            matches!(&result, Ok(report) if report.is_valid() && report.checked() == 1),
            "a valid GNU escaped record failed verification: {result:?}"
        );
        Ok(())
    }

    #[test]
    fn sha256_verification_accepts_both_gnu_modes_for_raw_bytes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let payload = temporary.path().join("payload.bin");
        std::fs::write(&payload, b"line\r\nline\n\0")?;
        let digest = digest_reader(std::fs::File::open(payload)?)?;
        let manifest = temporary.path().join("manifest.sha256");
        for mode in ['*', ' '] {
            std::fs::write(
                &manifest,
                format!("{} {mode}payload.bin\n", digest.sha256()),
            )?;
            let result = verify_sha256_manifest(temporary.path(), &manifest);
            assert!(
                matches!(&result, Ok(report) if report.is_valid() && report.checked() == 1),
                "GNU mode {mode:?} did not verify raw file bytes: {result:?}"
            );
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn bagit_exports_and_restores_encoded_filename_characters() -> Result<()> {
        let filename = "literal%2F\\name\nline\r  end.flac";
        let (temporary, mut library, root_id, source) = collection_fixture(Path::new(filename))?;
        let bag = temporary.path().join("bag");
        let plan = library.plan_manifest(&root_id, ManifestFormat::BagIt, &bag)?;
        library.approve_manifest(&plan)?;
        let result = library.execute_manifest(&plan);
        assert!(
            result.is_ok(),
            "valid BagIt filename could not be exported: {result:?}"
        );
        let digest = digest_reader(std::fs::File::open(source.join(filename))?)?;
        assert_eq!(
            std::fs::read_to_string(bag.join("manifest-sha256.txt"))?,
            format!(
                "{}  data/literal%252F\\name%0Aline%0D  end.flac\n",
                digest.sha256()
            )
        );
        assert_eq!(
            std::fs::read(bag.join("data").join(filename))?,
            b"preserved audio"
        );
        assert!(verify_bagit_manifest(&bag, &bag.join("manifest-sha256.txt"))?.is_valid());
        let destination = temporary.path().join("restored");
        let restore = library.plan_restore(&bag, &destination)?;
        library.approve_restore(&restore)?;
        library.execute_restore(&restore)?;
        assert_eq!(
            std::fs::read(destination.join(filename))?,
            b"preserved audio"
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn empty_roots_produce_valid_empty_exports() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        std::fs::create_dir(&source)?;
        let mut library = Library::open_in_memory()?;
        let root_id = RootId::new();
        let now = Utc::now().to_rfc3339();
        library.conn.execute(
            "INSERT INTO library_roots
             (id, path, state, capabilities_json, created_at, updated_at)
             VALUES (?1, ?2, 'online', '{}', ?3, ?3)",
            params![root_id.as_str(), path_text(&source)?, now],
        )?;
        for format in [ManifestFormat::Sha256, ManifestFormat::BagIt] {
            let output = temporary.path().join(format.as_str());
            let result = library.plan_manifest(&root_id, format, &output);
            assert!(
                result.is_ok(),
                "an empty root could not be planned: {result:?}"
            );
            let plan = result?;
            assert_eq!(plan.asset_count(), 0);
            library.approve_manifest(&plan)?;
            let receipt = library.execute_manifest(&plan)?;
            assert!(std::fs::read(receipt.path())?.is_empty());
            if format == ManifestFormat::BagIt {
                assert!(
                    output.join("data").is_dir(),
                    "an empty bag has no payload directory"
                );
                assert!(verify_bagit_manifest(&output, receipt.path())?.is_valid());
            }
        }
        Ok(())
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
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
