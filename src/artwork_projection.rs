//! Journaled embedded and external artwork projections derived from retained originals.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::artwork::validate_artwork;
use crate::asset::{digest_reader, FileDigests};
use crate::db::{file_identity, file_object_identity, JournalFile, Library, OperationKind};
use crate::failpoints;
use crate::fsops::AnchoredRoot;
use crate::media::{decoded_audio_essence_hash_from_file, probe_media_from_file, MediaDescriptor};
use crate::operations::{append_plan_event, PlanId, PlanKind, PlanState};
use crate::roots::RootId;
use crate::tags::{
    embedded_picture_sha256s_from_file, read_tag_snapshot_from_file, rewrite_embedded_front,
    TagSnapshot,
};
use crate::{Error, Result};

/// Immutable preview for replacing or removing an embedded front-cover projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedArtworkPlan {
    id: PlanId,
    audio_asset_id: String,
    audio_path: PathBuf,
    audio_identity: String,
    audio_blake3: String,
    audio_sha256: String,
    audio_size: u64,
    audio_essence: String,
    before: TagSnapshot,
    original: Option<OriginalArtwork>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OriginalArtwork {
    asset_id: String,
    path: PathBuf,
    identity: String,
    blake3: String,
    sha256: String,
    byte_size: u64,
    mime: String,
    artwork_role: String,
}

impl EmbeddedArtworkPlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    #[must_use]
    pub fn audio_path(&self) -> &Path {
        &self.audio_path
    }

    #[must_use]
    pub const fn removes_front_cover(&self) -> bool {
        self.original.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedArtworkReceipt {
    plan_id: PlanId,
    operation_id: String,
    audio_asset_id: String,
    retained_audio: PathBuf,
    projected_artwork_sha256: Option<String>,
}

/// Immutable preview for replacing or removing a managed external artwork projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalArtworkPlan {
    id: PlanId,
    target_asset_id: String,
    target_path: PathBuf,
    target_relative_path: PathBuf,
    target_root_id: RootId,
    root_path: PathBuf,
    target_identity: String,
    target_blake3: String,
    target_sha256: String,
    target_size: u64,
    target_mime: String,
    target_artwork_role: String,
    original: Option<OriginalArtwork>,
}

impl ExternalArtworkPlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    #[must_use]
    pub fn target_path(&self) -> &Path {
        &self.target_path
    }

    #[must_use]
    pub const fn removes_projection(&self) -> bool {
        self.original.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalArtworkReceipt {
    plan_id: PlanId,
    operation_id: String,
    retired_asset_id: String,
    replacement_asset_id: Option<String>,
    target_path: PathBuf,
    retained_previous: PathBuf,
}

impl ExternalArtworkReceipt {
    #[must_use]
    pub fn retained_previous(&self) -> &Path {
        &self.retained_previous
    }

    #[must_use]
    pub fn replacement_asset_id(&self) -> Option<&str> {
        self.replacement_asset_id.as_deref()
    }
}

impl EmbeddedArtworkReceipt {
    #[must_use]
    pub fn retained_audio(&self) -> &Path {
        &self.retained_audio
    }
}

impl Library {
    /// Preview an embedded front-cover replacement. `None` explicitly plans
    /// removal; the source artwork is always a verified retained original.
    #[allow(clippy::too_many_lines)]
    pub fn plan_embedded_front_artwork(
        &self,
        audio_asset_id: &str,
        original_artwork_asset_id: Option<&str>,
    ) -> Result<EmbeddedArtworkPlan> {
        let (audio_path, audio_size, audio_blake3, audio_sha256, audio_identity, role, state) =
            self.conn
                .query_row(
                    "SELECT absolute_path, byte_size, blake3, sha256, entry_identity,
                            role, verification_state
                     FROM assets WHERE id = ?1 AND managed = 1",
                    [audio_asset_id],
                    |row| {
                        Ok((
                            PathBuf::from(row.get::<_, String>(0)?),
                            row.get::<_, Option<u64>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| Error::Artwork("audio asset does not exist".into()))?;
        if role != "audio" || state != "verified" {
            return Err(Error::Artwork(
                "embedded artwork requires a verified managed audio asset".into(),
            ));
        }
        let audio_size = require_evidence(audio_size, "audio size")?;
        let audio_blake3 = require_evidence(audio_blake3, "audio BLAKE3")?;
        let audio_sha256 = require_evidence(audio_sha256, "audio SHA-256")?;
        let audio_identity = require_evidence(audio_identity, "audio entry identity")?;
        let audio_root = anchored_parent(&audio_path)?;
        let root = AnchoredRoot::open(audio_root)?;
        let observed = root.entry_metadata(&audio_path)?;
        let digests = digest_reader(root.open_file(&audio_path)?)?;
        if !observed.is_file()
            || observed.file_type().is_symlink()
            || file_identity(&observed) != audio_identity
            || digests.byte_size() != audio_size
            || digests.blake3() != audio_blake3
            || digests.sha256() != audio_sha256
        {
            return Err(Error::Artwork(
                "audio asset changed since its last verification".into(),
            ));
        }
        let audio_essence =
            decoded_audio_essence_hash_from_file(root.open_file(&audio_path)?, &audio_path)?;
        let before = read_tag_snapshot_from_file(root.open_file(&audio_path)?)?;
        let original = original_artwork_asset_id
            .map(|id| self.load_original_artwork(id))
            .transpose()?;
        if original
            .as_ref()
            .is_some_and(|value| value.artwork_role != "front")
        {
            return Err(Error::Artwork(
                "embedded front projection requires a front artwork original".into(),
            ));
        }
        if original.is_none() && before.picture_count() == 0 {
            return Err(Error::Artwork(
                "audio asset has no embedded artwork to remove".into(),
            ));
        }

        let request = json!({
            "audio_asset_id": audio_asset_id,
            "original_artwork_asset_id": original_artwork_asset_id,
            "projection": "embedded-front",
        });
        let preview = json!({
            "audio_path": audio_path,
            "before_picture_count": before.picture_count(),
            "action": if original.is_some() { "replace" } else { "remove" },
            "source_sha256": original.as_ref().map(|value| value.sha256.as_str()),
        });
        let id =
            self.create_durable_plan(PlanKind::ArtworkProjection, &request, &preview, Some(1))?;
        self.conn.execute(
            "INSERT INTO projection_plans
             (id, projection_type, profile, policy_version)
             VALUES (?1, 'artwork', 'embedded-front', 1)",
            [id.as_str()],
        )?;
        self.conn.execute(
            "INSERT INTO asset_projection_steps
             (plan_id, asset_id, before_json, after_json, state, evidence_json)
             VALUES (?1, ?2, ?3, ?4, 'planned', ?5)",
            params![
                id.as_str(),
                audio_asset_id,
                serde_json::to_string(&before)?,
                serde_json::to_string(&preview)?,
                serde_json::to_string(&json!({
                    "entry_identity": audio_identity,
                    "blake3": audio_blake3,
                    "sha256": audio_sha256,
                    "audio_essence": audio_essence,
                }))?,
            ],
        )?;
        Ok(EmbeddedArtworkPlan {
            id,
            audio_asset_id: audio_asset_id.into(),
            audio_path,
            audio_identity,
            audio_blake3,
            audio_sha256,
            audio_size,
            audio_essence,
            before,
            original,
        })
    }

    pub fn approve_embedded_front_artwork(&self, plan: &EmbeddedArtworkPlan) -> Result<()> {
        self.approve_durable_plan(plan.id())?;
        let changed = self.conn.execute(
            "UPDATE projection_plans SET approved_at = ?2
             WHERE id = ?1 AND approved_at IS NULL",
            params![plan.id().as_str(), Utc::now().to_rfc3339()],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(Error::Artwork(
                "artwork projection is unknown or already approved".into(),
            ))
        }
    }

    fn load_original_artwork(&self, asset_id: &str) -> Result<OriginalArtwork> {
        let value = self
            .conn
            .query_row(
                "SELECT a.absolute_path, a.byte_size, a.blake3, a.sha256,
                        a.entry_identity, a.role, a.verification_state, m.mime, m.role
                 FROM assets a JOIN artwork_metadata m ON m.asset_id = a.id
                 WHERE a.id = ?1 AND a.managed = 1 AND m.approval_state = 'approved'",
                [asset_id],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, Option<u64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Error::Artwork("retained artwork asset does not exist".into()))?;
        let (path, size, blake3, sha256, identity, role, state, mime, artwork_role) = value;
        if role != "artwork-original" || state != "verified" {
            return Err(Error::Artwork(
                "embedded projection source must be a verified artwork original".into(),
            ));
        }
        let original = OriginalArtwork {
            asset_id: asset_id.into(),
            path,
            identity: require_evidence(identity, "artwork entry identity")?,
            blake3: require_evidence(blake3, "artwork BLAKE3")?,
            sha256: require_evidence(sha256, "artwork SHA-256")?,
            byte_size: require_evidence(size, "artwork size")?,
            mime,
            artwork_role,
        };
        revalidate_original(&original)?;
        Ok(original)
    }

    /// Preview replacement or removal of an external artwork projection.
    #[allow(clippy::too_many_lines)]
    pub fn plan_external_artwork(
        &self,
        target_asset_id: &str,
        original_artwork_asset_id: Option<&str>,
    ) -> Result<ExternalArtworkPlan> {
        let value = self
            .conn
            .query_row(
                "SELECT a.absolute_path, a.relative_path, a.root_id, a.byte_size,
                        a.blake3, a.sha256, a.entry_identity, a.role,
                        a.verification_state, m.mime, m.role
                 FROM assets a JOIN artwork_metadata m ON m.asset_id = a.id
                 WHERE a.id = ?1 AND a.managed = 1",
                [target_asset_id],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        PathBuf::from(row.get::<_, String>(1)?),
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<u64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Error::Artwork("external artwork asset does not exist".into()))?;
        let (
            target_path,
            target_relative_path,
            root_id,
            target_size,
            target_blake3,
            target_sha256,
            target_identity,
            role,
            state,
            target_mime,
            target_artwork_role,
        ) = value;
        if role != "artwork" || state != "verified" {
            return Err(Error::Artwork(
                "external projection requires a verified managed artwork asset".into(),
            ));
        }
        let target_root_id = RootId::parse(root_id)?;
        let root_record = self.library_root(&target_root_id)?;
        root_record.capabilities().require_safe_mutation()?;
        let root_path = root_record.path().to_path_buf();
        if root_path.join(&target_relative_path) != target_path {
            return Err(Error::Artwork(
                "external artwork root-relative identity diverged".into(),
            ));
        }
        let target_size = require_evidence(target_size, "external artwork size")?;
        let target_blake3 = require_evidence(target_blake3, "external artwork BLAKE3")?;
        let target_sha256 = require_evidence(target_sha256, "external artwork SHA-256")?;
        let target_identity = require_evidence(target_identity, "external artwork identity")?;
        revalidate_external_target(
            &root_path,
            &target_path,
            &target_identity,
            target_size,
            &target_blake3,
            &target_sha256,
            &target_mime,
        )?;
        let original = original_artwork_asset_id
            .map(|id| self.load_original_artwork(id))
            .transpose()?;
        if let Some(source) = &original {
            if source.artwork_role != target_artwork_role {
                return Err(Error::Artwork(
                    "replacement artwork role does not match the external projection".into(),
                ));
            }
            if !source.path.starts_with(&root_path) {
                return Err(Error::Artwork(
                    "replacement source must belong to the same trusted root".into(),
                ));
            }
            require_compatible_artwork_extension(&target_path, &source.mime)?;
        }

        let request = json!({
            "target_asset_id": target_asset_id,
            "original_artwork_asset_id": original_artwork_asset_id,
            "projection": "external-artwork",
        });
        let preview = json!({
            "target_path": target_path,
            "action": if original.is_some() { "replace" } else { "remove" },
            "source_sha256": original.as_ref().map(|source| source.sha256.as_str()),
            "role": target_artwork_role,
        });
        let id =
            self.create_durable_plan(PlanKind::ArtworkProjection, &request, &preview, Some(1))?;
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO projection_plans
             (id, projection_type, profile, policy_version)
             VALUES (?1, 'artwork', 'external-artwork', 1)",
            [id.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO asset_projection_steps
             (plan_id, asset_id, before_json, after_json, state, evidence_json)
             VALUES (?1, ?2, ?3, ?4, 'planned', ?5)",
            params![
                id.as_str(),
                target_asset_id,
                serde_json::to_string(&json!({
                    "path": target_path,
                    "mime": target_mime,
                    "sha256": target_sha256,
                }))?,
                serde_json::to_string(&preview)?,
                serde_json::to_string(&json!({
                    "entry_identity": target_identity,
                    "blake3": target_blake3,
                    "sha256": target_sha256,
                }))?,
            ],
        )?;
        transaction.commit()?;
        Ok(ExternalArtworkPlan {
            id,
            target_asset_id: target_asset_id.into(),
            target_path,
            target_relative_path,
            target_root_id,
            root_path,
            target_identity,
            target_blake3,
            target_sha256,
            target_size,
            target_mime,
            target_artwork_role,
            original,
        })
    }

    pub fn approve_external_artwork(&self, plan: &ExternalArtworkPlan) -> Result<()> {
        self.approve_durable_plan(plan.id())?;
        let changed = self.conn.execute(
            "UPDATE projection_plans SET approved_at = ?2
             WHERE id = ?1 AND approved_at IS NULL",
            params![plan.id().as_str(), Utc::now().to_rfc3339()],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(Error::Artwork(
                "external artwork projection is unknown or already approved".into(),
            ))
        }
    }
}

pub struct EmbeddedArtworkExecutor<'a> {
    library: &'a mut Library,
}

impl<'a> EmbeddedArtworkExecutor<'a> {
    pub const fn new(library: &'a mut Library) -> Self {
        Self { library }
    }

    pub fn execute(&mut self, plan: &EmbeddedArtworkPlan) -> Result<EmbeddedArtworkReceipt> {
        if self.library.durable_plan(plan.id())?.state() != PlanState::Approved {
            return Err(Error::Artwork(
                "artwork projection must be approved before execution".into(),
            ));
        }
        if let Err(error) = self.library.start_durable_plan(plan.id()) {
            if self
                .library
                .durable_plan(plan.id())
                .is_ok_and(|value| value.state() == PlanState::Running)
            {
                let _ = self.library.finish_durable_plan(
                    plan.id(),
                    PlanState::Failed,
                    Some(&error.to_string()),
                );
            }
            return Err(error);
        }
        let parent = anchored_parent(&plan.audio_path)?;
        let root = match AnchoredRoot::open(parent) {
            Ok(root) => root,
            Err(error) => {
                let _ = self.library.finish_durable_plan(
                    plan.id(),
                    PlanState::Failed,
                    Some(&error.to_string()),
                );
                return Err(error);
            }
        };
        if let Err(error) = revalidate_audio(&root, plan)
            .and_then(|()| plan.original.as_ref().map_or(Ok(()), revalidate_original))
        {
            let _ = self.library.finish_durable_plan(
                plan.id(),
                PlanState::Failed,
                Some(&error.to_string()),
            );
            return Err(error);
        }
        let transfer = uuid::Uuid::new_v4();
        let rewrite = sibling_path(&plan.audio_path, transfer, "artwork-stage")?;
        let original_audio = sibling_path(&plan.audio_path, transfer, "artwork-original")?;
        let journal = [
            JournalFile {
                source: plan.audio_path.clone(),
                staged: rewrite.clone(),
                destination: plan.audio_path.clone(),
                content_hash: None,
                sha256: None,
                source_identity: Some(plan.audio_identity.clone()),
                owned_identity: None,
                role: "artwork-rewrite".into(),
                state: "prepared".into(),
            },
            JournalFile {
                source: plan.audio_path.clone(),
                staged: original_audio.clone(),
                destination: plan.audio_path.clone(),
                content_hash: Some(plan.audio_blake3.clone()),
                sha256: Some(plan.audio_sha256.clone()),
                source_identity: Some(plan.audio_identity.clone()),
                owned_identity: Some(plan.audio_identity.clone()),
                role: "artwork-original".into(),
                state: "prepared".into(),
            },
        ];
        let operation = match self.library.create_operation_for_plan(
            OperationKind::ArtworkWrite,
            &journal,
            Some(plan.id().as_str()),
            Some(&json!({
                "projection": "embedded-front",
                "source_artwork_asset_id": plan.original.as_ref().map(|value| &value.asset_id),
            })),
        ) {
            Ok(operation) => operation,
            Err(error) => {
                let _ = self.library.recover_pending();
                let _ = self.library.finish_durable_plan(
                    plan.id(),
                    PlanState::Failed,
                    Some(&error.to_string()),
                );
                return Err(error);
            }
        };
        match self.execute_journaled(plan, &root, &operation, &rewrite, &original_audio) {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(self.rollback_failed(plan.id(), &operation, error)),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn execute_journaled(
        &mut self,
        plan: &EmbeddedArtworkPlan,
        root: &AnchoredRoot,
        operation: &str,
        rewrite: &Path,
        original_audio: &Path,
    ) -> Result<EmbeddedArtworkReceipt> {
        self.library
            .set_operation_state(operation, "staging", None)?;
        root.copy_new_observed(&plan.audio_path, rewrite, |metadata| {
            self.library
                .set_acquired_file_identity(operation, 0, &file_object_identity(metadata))
        })?;
        let original_bytes = plan
            .original
            .as_ref()
            .map(|original| std::fs::read(&original.path))
            .transpose()?;
        if let (Some(bytes), Some(original)) = (&original_bytes, &plan.original) {
            let validated = validate_artwork(bytes)?;
            if validated.sha256() != original.sha256 || validated.mime() != original.mime {
                return Err(Error::Artwork(
                    "retained artwork changed during projection".into(),
                ));
            }
        }
        {
            let mut staged = root.open_file_read_write(rewrite)?;
            rewrite_embedded_front(
                &mut staged,
                &plan.audio_path,
                original_bytes
                    .as_deref()
                    .zip(plan.original.as_ref().map(|value| value.mime.as_str())),
            )?;
        }
        let after = read_tag_snapshot_from_file(root.open_file(rewrite)?)?;
        if after.unprojected_digest() != plan.before.unprojected_digest() {
            return Err(Error::Artwork(
                "artwork projection changed unprojected native metadata".into(),
            ));
        }
        let embedded = embedded_picture_sha256s_from_file(root.open_file(rewrite)?)?;
        if let Some(original) = &plan.original {
            if !embedded.iter().any(|digest| digest == &original.sha256) {
                return Err(Error::Artwork(
                    "reread did not contain the projected artwork bytes".into(),
                ));
            }
        } else if after.picture_count() >= plan.before.picture_count() {
            return Err(Error::Artwork(
                "reread did not prove embedded artwork removal".into(),
            ));
        }
        let essence =
            decoded_audio_essence_hash_from_file(root.open_file(rewrite)?, &plan.audio_path)?;
        if essence != plan.audio_essence {
            return Err(Error::Artwork(
                "embedded artwork projection changed audio essence".into(),
            ));
        }
        let digests = digest_reader(root.open_file(rewrite)?)?;
        let media = probe_media_from_file(root.open_file(rewrite)?, &plan.audio_path)?;
        let staged_identity = file_identity(&root.entry_metadata(rewrite)?);
        self.library.set_staged_file_full_evidence(
            operation,
            0,
            &staged_identity,
            digests.blake3(),
            digests.sha256(),
        )?;
        self.library.conn.execute(
            "UPDATE asset_projection_steps SET state = 'validated', evidence_json = ?3
             WHERE plan_id = ?1 AND asset_id = ?2 AND state = 'planned'",
            params![
                plan.id().as_str(),
                plan.audio_asset_id,
                serde_json::to_string(&json!({
                    "embedded_sha256": embedded,
                    "audio_essence": essence,
                    "blake3": digests.blake3(),
                    "sha256": digests.sha256(),
                }))?,
            ],
        )?;

        self.library
            .set_operation_state(operation, "finalizing", None)?;
        root.rename_noreplace(&plan.audio_path, original_audio)?;
        self.library.set_file_state(operation, 1, "quarantined")?;
        root.rename_noreplace(rewrite, &plan.audio_path)?;
        self.library.set_file_state(operation, 0, "finalized")?;
        let published_identity = file_identity(&root.entry_metadata(&plan.audio_path)?);
        if published_identity != staged_identity {
            return Err(Error::Artwork(
                "published artwork projection lost staged identity".into(),
            ));
        }
        self.commit_projection(
            plan,
            root,
            operation,
            &published_identity,
            &digests,
            &essence,
            &media,
            &after,
        )?;
        self.library.complete_operation(operation)?;
        Ok(EmbeddedArtworkReceipt {
            plan_id: plan.id.clone(),
            operation_id: operation.into(),
            audio_asset_id: plan.audio_asset_id.clone(),
            retained_audio: original_audio.into(),
            projected_artwork_sha256: plan.original.as_ref().map(|value| value.sha256.clone()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_projection(
        &mut self,
        plan: &EmbeddedArtworkPlan,
        root: &AnchoredRoot,
        operation: &str,
        identity: &str,
        digests: &FileDigests,
        essence: &str,
        media: &MediaDescriptor,
        after: &TagSnapshot,
    ) -> Result<()> {
        let metadata = root.entry_metadata(&plan.audio_path)?;
        let mtime: DateTime<Utc> = metadata.modified()?.into();
        let now = Utc::now().to_rfc3339();
        let transaction = self.library.conn.transaction()?;
        let changed = transaction.execute(
            "UPDATE assets SET byte_size = ?1, blake3 = ?2, sha256 = ?3,
                    mtime = ?4, entry_identity = ?5, media_json = ?6,
                    audio_essence_hash = ?7, last_verified_at = ?8,
                    verification_state = 'verified'
             WHERE id = ?9 AND entry_identity = ?10 AND verification_state = 'verified'",
            params![
                digests.byte_size(),
                digests.blake3(),
                digests.sha256(),
                mtime.to_rfc3339(),
                identity,
                serde_json::to_string(media)?,
                essence,
                now,
                plan.audio_asset_id,
                plan.audio_identity,
            ],
        )?;
        if changed != 1 {
            return Err(Error::Artwork(
                "audio asset changed before artwork commit".into(),
            ));
        }
        transaction.execute(
            "UPDATE items SET file_size = ?1, mtime = ?2
             WHERE id IN (SELECT item_id FROM item_assets WHERE asset_id = ?3)",
            params![digests.byte_size(), mtime.to_rfc3339(), plan.audio_asset_id],
        )?;
        transaction.execute(
            "DELETE FROM asset_relationships
             WHERE child_asset_id = ?1 AND relationship = 'embedded-front'",
            [plan.audio_asset_id.as_str()],
        )?;
        if let Some(original) = &plan.original {
            transaction.execute(
                "INSERT INTO asset_relationships
                 (parent_asset_id, child_asset_id, relationship)
                 VALUES (?1, ?2, 'embedded-front')",
                params![original.asset_id, plan.audio_asset_id],
            )?;
        }
        transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?2
             WHERE id = ?1",
            params![operation, now],
        )?;
        transaction.execute(
            "UPDATE asset_projection_steps SET state = 'published', after_json = ?3
             WHERE plan_id = ?1 AND asset_id = ?2 AND state = 'validated'",
            params![
                plan.id().as_str(),
                plan.audio_asset_id,
                serde_json::to_string(after)?,
            ],
        )?;
        transaction.execute(
            "UPDATE durable_plans SET state = 'complete', progress_current = 1,
                    updated_at = ?2, completed_at = ?2
             WHERE id = ?1 AND state = 'running'",
            params![plan.id().as_str(), now],
        )?;
        append_plan_event(
            &transaction,
            plan.id(),
            "complete",
            &json!({"audio_asset_id": plan.audio_asset_id}),
        )?;
        transaction.commit()?;
        failpoints::hit("db.artwork-projection-commit")?;
        Ok(())
    }

    fn rollback_failed(&mut self, plan: &PlanId, operation: &str, error: Error) -> Error {
        let _ = self
            .library
            .record_operation_failure(operation, &error.to_string());
        let recovery = self.library.recover_pending();
        if self
            .library
            .durable_plan(plan)
            .is_ok_and(|value| value.state() == PlanState::Running)
        {
            let _ =
                self.library
                    .finish_durable_plan(plan, PlanState::Failed, Some(&error.to_string()));
        }
        match recovery {
            Ok(report) if report.unresolved.is_empty() => error,
            Ok(report) => Error::Recovery(format!(
                "{error}; artwork rollback needs attention: {}",
                report.unresolved.join("; ")
            )),
            Err(recovery_error) => Error::Recovery(format!(
                "{error}; artwork rollback failed: {recovery_error}"
            )),
        }
    }
}

pub struct ExternalArtworkExecutor<'a> {
    library: &'a mut Library,
}

impl<'a> ExternalArtworkExecutor<'a> {
    pub const fn new(library: &'a mut Library) -> Self {
        Self { library }
    }

    pub fn execute(&mut self, plan: &ExternalArtworkPlan) -> Result<ExternalArtworkReceipt> {
        if self.library.durable_plan(plan.id())?.state() != PlanState::Approved {
            return Err(Error::Artwork(
                "external artwork projection must be approved before execution".into(),
            ));
        }
        revalidate_external_plan(plan)?;
        let root_record = self.library.library_root(&plan.target_root_id)?;
        root_record.capabilities().require_safe_mutation()?;
        if root_record.path() != plan.root_path {
            return Err(Error::Artwork(
                "external artwork root path changed after preview".into(),
            ));
        }
        let root = AnchoredRoot::open(&plan.root_path)?;
        if let Err(error) = self.library.start_durable_plan(plan.id()) {
            if self
                .library
                .durable_plan(plan.id())
                .is_ok_and(|value| value.state() == PlanState::Running)
            {
                let _ = self.library.finish_durable_plan(
                    plan.id(),
                    PlanState::Failed,
                    Some(&error.to_string()),
                );
            }
            return Err(error);
        }
        let transfer = uuid::Uuid::new_v4();
        let retained_previous = sibling_path(&plan.target_path, transfer, "artwork-original")?;
        let staged = plan
            .original
            .as_ref()
            .map(|_source| sibling_path(&plan.target_path, transfer, "artwork-stage"))
            .transpose()?;
        let mut files = Vec::with_capacity(usize::from(staged.is_some()) + 1);
        if let (Some(source), Some(stage)) = (&plan.original, &staged) {
            files.push(JournalFile {
                source: source.path.clone(),
                staged: stage.clone(),
                destination: plan.target_path.clone(),
                content_hash: Some(source.blake3.clone()),
                sha256: Some(source.sha256.clone()),
                source_identity: Some(source.identity.clone()),
                owned_identity: None,
                role: "artwork-rewrite".into(),
                state: "prepared".into(),
            });
        }
        let original_ordinal = files.len();
        files.push(JournalFile {
            source: plan.target_path.clone(),
            staged: retained_previous.clone(),
            destination: plan.target_path.clone(),
            content_hash: Some(plan.target_blake3.clone()),
            sha256: Some(plan.target_sha256.clone()),
            source_identity: Some(plan.target_identity.clone()),
            owned_identity: Some(plan.target_identity.clone()),
            role: "artwork-original".into(),
            state: "prepared".into(),
        });
        let operation = match self.library.create_operation_for_plan(
            OperationKind::ArtworkWrite,
            &files,
            Some(plan.id().as_str()),
            Some(&json!({
                "projection": "external-artwork",
                "target_asset_id": plan.target_asset_id,
                "source_artwork_asset_id": plan.original.as_ref().map(|source| &source.asset_id),
            })),
        ) {
            Ok(operation) => operation,
            Err(error) => return Err(self.fail(plan.id(), None, error)),
        };
        match self.execute_journaled(
            plan,
            &root,
            &operation,
            staged.as_deref(),
            &retained_previous,
            original_ordinal,
        ) {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(self.fail(plan.id(), Some(&operation), error)),
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn execute_journaled(
        &mut self,
        plan: &ExternalArtworkPlan,
        root: &AnchoredRoot,
        operation: &str,
        staged: Option<&Path>,
        retained_previous: &Path,
        original_ordinal: usize,
    ) -> Result<ExternalArtworkReceipt> {
        self.library
            .set_operation_state(operation, "staging", None)?;
        let replacement = if let (Some(source), Some(staged)) = (&plan.original, staged) {
            root.copy_new_observed(&source.path, staged, |metadata| {
                self.library.set_acquired_file_identity(
                    operation,
                    0,
                    &file_object_identity(metadata),
                )
            })?;
            let bytes = read_root_file(root, staged)?;
            let validated = validate_artwork(&bytes)?;
            if validated.mime() != source.mime
                || validated.sha256() != source.sha256
                || validated.blake3() != source.blake3
            {
                return Err(Error::Artwork(
                    "staged external artwork does not match its retained original".into(),
                ));
            }
            let metadata = root.entry_metadata(staged)?;
            let identity = file_identity(&metadata);
            let digests = digest_reader(root.open_file(staged)?)?;
            self.library.set_staged_file_full_evidence(
                operation,
                0,
                &identity,
                digests.blake3(),
                digests.sha256(),
            )?;
            Some((identity, digests))
        } else {
            None
        };

        self.library
            .set_operation_state(operation, "finalizing", None)?;
        root.rename_noreplace(&plan.target_path, retained_previous)?;
        self.library
            .set_file_state(operation, original_ordinal, "quarantined")?;
        if let Some(staged) = staged {
            root.rename_noreplace(staged, &plan.target_path)?;
            self.library.set_file_state(operation, 0, "finalized")?;
            let (expected_identity, expected_digests) = replacement
                .as_ref()
                .ok_or_else(|| Error::Artwork("replacement evidence was not retained".into()))?;
            let published = root.entry_metadata(&plan.target_path)?;
            let published_identity = file_identity(&published);
            let published_digests = digest_reader(root.open_file(&plan.target_path)?)?;
            let validated = validate_artwork(&read_root_file(root, &plan.target_path)?)?;
            if &published_identity != expected_identity
                || published_digests != *expected_digests
                || validated.mime() != plan.target_mime
            {
                return Err(Error::Artwork(
                    "published external artwork failed identity or decode validation".into(),
                ));
            }
        }
        let replacement_asset_id = self.commit_external_projection(
            plan,
            operation,
            retained_previous,
            replacement.as_ref(),
        )?;
        self.library.complete_operation(operation)?;
        Ok(ExternalArtworkReceipt {
            plan_id: plan.id.clone(),
            operation_id: operation.into(),
            retired_asset_id: plan.target_asset_id.clone(),
            replacement_asset_id,
            target_path: plan.target_path.clone(),
            retained_previous: retained_previous.into(),
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "asset retirement, replacement identity, album links, journal, and plan event commit atomically"
    )]
    fn commit_external_projection(
        &mut self,
        plan: &ExternalArtworkPlan,
        operation: &str,
        retained_previous: &Path,
        replacement: Option<&(String, FileDigests)>,
    ) -> Result<Option<String>> {
        let retained_relative = relative_utf8(&plan.root_path, retained_previous)?;
        let retained_absolute = path_utf8(retained_previous)?;
        let target_absolute = path_utf8(&plan.target_path)?;
        let target_relative = path_utf8(&plan.target_relative_path)?;
        let now = Utc::now().to_rfc3339();
        let retained_mtime =
            DateTime::<Utc>::from(std::fs::metadata(retained_previous)?.modified()?).to_rfc3339();
        let transaction = self.library.conn.transaction()?;
        let changed = transaction.execute(
            "UPDATE assets SET relative_path = ?2, absolute_path = ?3,
                    role = 'artwork-retired', mtime = ?4, last_verified_at = ?6
             WHERE id = ?1 AND entry_identity = ?5 AND verification_state = 'verified'",
            params![
                plan.target_asset_id,
                retained_relative,
                retained_absolute,
                retained_mtime,
                plan.target_identity,
                now,
            ],
        )?;
        if changed != 1 {
            return Err(Error::Artwork(
                "external artwork changed before projection commit".into(),
            ));
        }

        let replacement_asset_id =
            if let (Some(source), Some((identity, digests))) = (&plan.original, replacement) {
                let asset_id = uuid::Uuid::new_v4().to_string();
                let metadata = std::fs::metadata(&plan.target_path)?;
                let mtime: DateTime<Utc> = metadata.modified()?.into();
                transaction.execute(
                    "INSERT INTO assets
                 (id, root_id, relative_path, absolute_path, role, managed,
                  verification_state, byte_size, blake3, sha256, mtime,
                  entry_identity, projection_state, first_seen_at, last_verified_at)
                 VALUES (?1, ?2, ?3, ?4, 'artwork', 1, 'verified', ?5, ?6,
                         ?7, ?8, ?9, 'current', ?10, ?10)",
                    params![
                        asset_id,
                        plan.target_root_id.as_str(),
                        target_relative,
                        target_absolute,
                        digests.byte_size(),
                        digests.blake3(),
                        digests.sha256(),
                        mtime.to_rfc3339(),
                        identity,
                        now,
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO artwork_metadata
                 (asset_id, exact_release_id, release_group_id, potentially_inexact,
                  role, source_provider, source_reference, provider_release_id,
                  mime, width, height, approval_state, rights, original_asset_id,
                  transform_json)
                 SELECT ?1, exact_release_id, release_group_id, potentially_inexact,
                        role, source_provider, source_reference, provider_release_id,
                        mime, width, height, 'approved', rights, ?2,
                        json_object('kind', 'external-projection', 'policy_version', 1)
                 FROM artwork_metadata WHERE asset_id = ?2",
                    params![asset_id, source.asset_id],
                )?;
                transaction.execute(
                    "INSERT INTO album_assets (album_id, asset_id, relationship)
                 SELECT album_id, ?2, relationship FROM album_assets WHERE asset_id = ?1",
                    params![plan.target_asset_id, asset_id],
                )?;
                transaction.execute(
                    "UPDATE album_assets SET relationship = 'retired-' || relationship
                 WHERE asset_id = ?1",
                    [plan.target_asset_id.as_str()],
                )?;
                transaction.execute(
                    "INSERT INTO asset_relationships
                 (parent_asset_id, child_asset_id, relationship)
                 VALUES (?1, ?2, 'external-projection'),
                        (?3, ?2, 'previous-projection')",
                    params![source.asset_id, asset_id, plan.target_asset_id],
                )?;
                Some(asset_id)
            } else {
                transaction.execute(
                    "UPDATE albums SET artpath = NULL WHERE artpath = ?1",
                    [target_absolute],
                )?;
                transaction.execute(
                    "UPDATE album_assets SET relationship = 'retired-' || relationship
                 WHERE asset_id = ?1",
                    [plan.target_asset_id.as_str()],
                )?;
                None
            };
        transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?2
             WHERE id = ?1",
            params![operation, now],
        )?;
        transaction.execute(
            "UPDATE asset_projection_steps SET state = 'published', after_json = ?3
             WHERE plan_id = ?1 AND asset_id = ?2 AND state = 'planned'",
            params![
                plan.id().as_str(),
                plan.target_asset_id,
                serde_json::to_string(&json!({
                    "replacement_asset_id": replacement_asset_id,
                    "retained_previous": retained_previous,
                }))?,
            ],
        )?;
        transaction.execute(
            "UPDATE durable_plans SET state = 'complete', progress_current = 1,
                    updated_at = ?2, completed_at = ?2
             WHERE id = ?1 AND state = 'running'",
            params![plan.id().as_str(), now],
        )?;
        append_plan_event(
            &transaction,
            plan.id(),
            "complete",
            &json!({
                "retired_asset_id": plan.target_asset_id,
                "replacement_asset_id": replacement_asset_id,
            }),
        )?;
        transaction.commit()?;
        failpoints::hit("db.external-artwork-projection-commit")?;
        Ok(replacement_asset_id)
    }

    fn fail(&mut self, plan_id: &PlanId, operation: Option<&str>, error: Error) -> Error {
        if let Some(operation) = operation {
            let _ = self
                .library
                .record_operation_failure(operation, &error.to_string());
        }
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
                "{error}; external artwork recovery needs attention: {}",
                report.unresolved.join("; ")
            )),
            Err(recovery) => Error::Recovery(format!(
                "{error}; external artwork recovery failed: {recovery}"
            )),
        }
    }
}

fn revalidate_external_plan(plan: &ExternalArtworkPlan) -> Result<()> {
    revalidate_external_target(
        &plan.root_path,
        &plan.target_path,
        &plan.target_identity,
        plan.target_size,
        &plan.target_blake3,
        &plan.target_sha256,
        &plan.target_mime,
    )?;
    if let Some(original) = &plan.original {
        revalidate_original(original)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn revalidate_external_target(
    root_path: &Path,
    target_path: &Path,
    expected_identity: &str,
    expected_size: u64,
    expected_blake3: &str,
    expected_sha256: &str,
    expected_mime: &str,
) -> Result<()> {
    let root = AnchoredRoot::open(root_path)?;
    let metadata = root.entry_metadata(target_path)?;
    let digests = digest_reader(root.open_file(target_path)?)?;
    let artwork = validate_artwork(&read_root_file(&root, target_path)?)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || file_identity(&metadata) != expected_identity
        || digests.byte_size() != expected_size
        || digests.blake3() != expected_blake3
        || digests.sha256() != expected_sha256
        || artwork.mime() != expected_mime
    {
        return Err(Error::Artwork(
            "external artwork changed after its last verification".into(),
        ));
    }
    Ok(())
}

fn read_root_file(root: &AnchoredRoot, path: &Path) -> Result<Vec<u8>> {
    let mut file = root.open_file(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn require_compatible_artwork_extension(path: &Path, mime: &str) -> Result<()> {
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| Error::Artwork("external artwork target has no UTF-8 extension".into()))?;
    let compatible = match mime {
        "image/jpeg" => matches!(extension.as_str(), "jpg" | "jpeg"),
        "image/png" => extension == "png",
        "image/gif" => extension == "gif",
        "image/webp" => extension == "webp",
        _ => false,
    };
    if compatible {
        Ok(())
    } else {
        Err(Error::Artwork(
            "replacement artwork MIME does not match the target extension".into(),
        ))
    }
}

fn path_utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::Artwork("artwork path is not valid UTF-8".into()))
}

fn relative_utf8(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_error| Error::Artwork("artwork path escaped its registered root".into()))?;
    Ok(path_utf8(relative)?.to_owned())
}

fn revalidate_audio(root: &AnchoredRoot, plan: &EmbeddedArtworkPlan) -> Result<()> {
    let metadata = root.entry_metadata(&plan.audio_path)?;
    let digests = digest_reader(root.open_file(&plan.audio_path)?)?;
    let essence =
        decoded_audio_essence_hash_from_file(root.open_file(&plan.audio_path)?, &plan.audio_path)?;
    let snapshot = read_tag_snapshot_from_file(root.open_file(&plan.audio_path)?)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || file_identity(&metadata) != plan.audio_identity
        || digests.byte_size() != plan.audio_size
        || digests.blake3() != plan.audio_blake3
        || digests.sha256() != plan.audio_sha256
        || essence != plan.audio_essence
        || snapshot != plan.before
    {
        return Err(Error::Artwork(
            "audio asset changed after artwork preview".into(),
        ));
    }
    Ok(())
}

fn revalidate_original(original: &OriginalArtwork) -> Result<()> {
    let parent = anchored_parent(&original.path)?;
    let root = AnchoredRoot::open(parent)?;
    let metadata = root.entry_metadata(&original.path)?;
    let digests = digest_reader(root.open_file(&original.path)?)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || file_identity(&metadata) != original.identity
        || digests.byte_size() != original.byte_size
        || digests.blake3() != original.blake3
        || digests.sha256() != original.sha256
    {
        return Err(Error::Artwork(
            "retained artwork source changed after verification".into(),
        ));
    }
    let bytes = std::fs::read(&original.path)?;
    let validated = validate_artwork(&bytes)?;
    if validated.mime() != original.mime || validated.sha256() != original.sha256 {
        return Err(Error::Artwork(
            "retained artwork metadata does not describe its bytes".into(),
        ));
    }
    Ok(())
}

fn require_evidence<T>(value: Option<T>, label: &str) -> Result<T> {
    value.ok_or_else(|| Error::Artwork(format!("verified asset is missing {label}")))
}

fn anchored_parent(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| Error::Root(format!("artwork path has no parent: {}", path.display())))
}

fn sibling_path(path: &Path, operation: uuid::Uuid, role: &str) -> Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        Error::Artwork(format!(
            "artwork target has no filename: {}",
            path.display()
        ))
    })?;
    let mut staged = std::ffi::OsString::from(".");
    staged.push(name);
    staged.push(format!(".rsbts-{operation}.{role}"));
    Ok(path.with_file_name(staged))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use chrono::Utc;
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};

    use super::*;
    use crate::{Album, AudioFormat, Item};

    fn wav(samples: &[i16]) -> Vec<u8> {
        let data_len = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn png() -> Result<Vec<u8>> {
        png_color([2, 4, 8])
    }

    fn png_color(color: [u8; 3]) -> Result<Vec<u8>> {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(4, 3, Rgb(color)));
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, ImageFormat::Png)?;
        Ok(output.into_inner())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the fixture constructs a complete verified audio/original-artwork graph"
    )]
    fn managed_library(audio: &Path, artwork: &Path) -> Result<(Library, String, String)> {
        let metadata = std::fs::metadata(audio)?;
        let mut library = Library::open_in_memory()?;
        let root_path = audio
            .parent()
            .ok_or_else(|| Error::Artwork("test audio has no parent".into()))?;
        library.register_root(root_path)?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: "Album".into(),
            albumartist: "Artist".into(),
            year: Some(2026),
            artpath: None,
            external_id: None,
            added: Utc::now(),
        };
        let item = Item {
            id: None,
            album_id: None,
            path: audio.into(),
            title: "Track".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            albumartist: Some("Artist".into()),
            genre: None,
            year: Some(2026),
            track: Some(1),
            disc: Some(1),
            format: AudioFormat::Wav,
            bitrate: 128,
            length: 0.001,
            file_size: Some(metadata.len()),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: metadata.modified()?.into(),
        };
        library.commit_import_at_root_with_artwork(
            &operation,
            &album,
            &[item],
            Some(root_path),
            None,
        )?;
        library.complete_operation(&operation)?;
        let (audio_id, root_id) = library.conn.query_row(
            "SELECT a.id, a.root_id FROM assets a
             JOIN item_assets ia ON ia.asset_id = a.id LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;

        let art_id = uuid::Uuid::new_v4().to_string();
        let art_metadata = std::fs::metadata(artwork)?;
        let art_digest = crate::asset::digest_file(artwork)?;
        let validated = validate_artwork(&std::fs::read(artwork)?)?;
        let root_path = Path::new(&library.conn.query_row(
            "SELECT path FROM library_roots WHERE id = ?1",
            [&root_id],
            |row| row.get::<_, String>(0),
        )?)
        .to_path_buf();
        let relative = artwork
            .strip_prefix(&root_path)
            .map_err(|_strip_error| Error::Artwork("test artwork is outside root".into()))?;
        let now = Utc::now().to_rfc3339();
        library.conn.execute(
            "INSERT INTO assets
             (id, root_id, relative_path, absolute_path, role, managed,
              verification_state, byte_size, blake3, sha256, mtime,
              entry_identity, projection_state, first_seen_at, last_verified_at)
             VALUES (?1, ?2, ?3, ?4, 'artwork-original', 1, 'verified',
                     ?5, ?6, ?7, ?8, ?9, 'current', ?10, ?10)",
            params![
                art_id,
                root_id,
                relative.to_string_lossy(),
                artwork.to_string_lossy(),
                art_digest.byte_size(),
                art_digest.blake3(),
                art_digest.sha256(),
                DateTime::<Utc>::from(art_metadata.modified()?).to_rfc3339(),
                file_identity(&art_metadata),
                now,
            ],
        )?;
        library.conn.execute(
            "INSERT INTO artwork_metadata
             (asset_id, potentially_inexact, role, mime, width, height,
              approval_state)
             VALUES (?1, 0, 'front', ?2, ?3, ?4, 'approved')",
            params![
                art_id,
                validated.mime(),
                validated.dimensions().0,
                validated.dimensions().1,
            ],
        )?;
        library.conn.execute(
            "INSERT INTO album_assets (album_id, asset_id, relationship)
             SELECT i.album_id, ?1, 'artwork-original'
             FROM items i JOIN item_assets ia ON ia.item_id = i.id
             WHERE ia.asset_id = ?2 LIMIT 1",
            params![art_id, audio_id],
        )?;
        Ok((library, audio_id, art_id))
    }

    fn add_external_artwork(library: &Library, path: &Path) -> Result<String> {
        let (root_id, root_path, album_id) = library.conn.query_row(
            "SELECT a.root_id, lr.path, i.album_id
             FROM assets a
             JOIN library_roots lr ON lr.id = a.root_id
             JOIN item_assets ia ON ia.asset_id = a.id
             JOIN items i ON i.id = ia.item_id
             WHERE a.role = 'audio' LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        let relative = path
            .strip_prefix(&root_path)
            .map_err(|_error| Error::Artwork("test cover is outside root".into()))?;
        let metadata = std::fs::metadata(path)?;
        let digest = crate::asset::digest_file(path)?;
        let artwork = validate_artwork(&std::fs::read(path)?)?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        library.conn.execute(
            "INSERT INTO assets
             (id, root_id, relative_path, absolute_path, role, managed,
              verification_state, byte_size, blake3, sha256, mtime,
              entry_identity, projection_state, first_seen_at, last_verified_at)
             VALUES (?1, ?2, ?3, ?4, 'artwork', 1, 'verified', ?5, ?6, ?7,
                     ?8, ?9, 'current', ?10, ?10)",
            params![
                id,
                root_id,
                relative.to_string_lossy(),
                path.to_string_lossy(),
                digest.byte_size(),
                digest.blake3(),
                digest.sha256(),
                DateTime::<Utc>::from(metadata.modified()?).to_rfc3339(),
                file_identity(&metadata),
                now,
            ],
        )?;
        library.conn.execute(
            "INSERT INTO artwork_metadata
             (asset_id, potentially_inexact, role, mime, width, height,
              approval_state, transform_json)
             VALUES (?1, 0, 'front', ?2, ?3, ?4, 'approved',
                     json_object('kind', 'external-projection', 'policy_version', 1))",
            params![
                id,
                artwork.mime(),
                artwork.dimensions().0,
                artwork.dimensions().1,
            ],
        )?;
        library.conn.execute(
            "INSERT INTO album_assets (album_id, asset_id, relationship)
             VALUES (?1, ?2, 'front')",
            params![album_id, id],
        )?;
        library.conn.execute(
            "UPDATE albums SET artpath = ?1 WHERE id = ?2",
            params![path.to_string_lossy(), album_id],
        )?;
        Ok(id)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn external_artwork_replace_and_remove_retain_every_version() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let audio = temporary.path().join("track.wav");
        let original = temporary.path().join("original.png");
        let cover = temporary.path().join("cover.png");
        std::fs::write(&audio, wav(&[1, 2, 3, 4]))?;
        let source_bytes = png_color([2, 4, 8])?;
        let previous_bytes = png_color([9, 7, 5])?;
        std::fs::write(&original, &source_bytes)?;
        std::fs::write(&cover, &previous_bytes)?;
        let (mut library, _audio_id, original_id) = managed_library(&audio, &original)?;
        let target_id = add_external_artwork(&library, &cover)?;

        let plan = library.plan_external_artwork(&target_id, Some(&original_id))?;
        assert!(ExternalArtworkExecutor::new(&mut library)
            .execute(&plan)
            .is_err());
        library.approve_external_artwork(&plan)?;
        let replaced = ExternalArtworkExecutor::new(&mut library).execute(&plan)?;
        assert_eq!(std::fs::read(&cover)?, source_bytes);
        assert_eq!(std::fs::read(replaced.retained_previous())?, previous_bytes);
        let replacement_id = replaced
            .replacement_asset_id()
            .ok_or_else(|| Error::Artwork("replacement did not receive an asset ID".into()))?
            .to_owned();

        let removal = library.plan_external_artwork(&replacement_id, None)?;
        library.approve_external_artwork(&removal)?;
        let removed = ExternalArtworkExecutor::new(&mut library).execute(&removal)?;
        assert!(!cover.exists());
        assert_eq!(std::fs::read(removed.retained_previous())?, source_bytes);
        let audit_report = library.audit()?;
        assert!(
            audit_report.is_empty(),
            "audit issues: {:?}",
            audit_report.issues()
        );
        let artpath: Option<String> =
            library
                .conn
                .query_row("SELECT artpath FROM albums LIMIT 1", [], |row| row.get(0))?;
        assert!(artpath.is_none());
        assert_eq!(
            library.durable_plan(removal.id())?.state(),
            PlanState::Complete
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn embedded_front_replace_and_remove_are_approved_journaled_projections() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let audio = temporary.path().join("track.wav");
        let artwork = temporary.path().join("original.png");
        std::fs::write(&audio, wav(&[0, 1, -1, 2]))?;
        std::fs::write(&artwork, png()?)?;
        let source_audio = std::fs::read(&audio)?;
        let source_essence = crate::media::decoded_audio_essence_hash(&audio)?;
        let (mut library, audio_id, art_id) = managed_library(&audio, &artwork)?;

        let plan = library.plan_embedded_front_artwork(&audio_id, Some(&art_id))?;
        assert_eq!(std::fs::read(&audio)?, source_audio);
        assert!(EmbeddedArtworkExecutor::new(&mut library)
            .execute(&plan)
            .is_err());
        library.approve_embedded_front_artwork(&plan)?;
        let receipt = EmbeddedArtworkExecutor::new(&mut library).execute(&plan)?;
        assert_eq!(std::fs::read(receipt.retained_audio())?, source_audio);
        assert_eq!(
            crate::media::decoded_audio_essence_hash(&audio)?,
            source_essence
        );
        let art_sha = crate::asset::sha256_bytes(&std::fs::read(&artwork)?);
        assert!(
            embedded_picture_sha256s_from_file(std::fs::File::open(&audio)?)?.contains(&art_sha)
        );
        assert_eq!(
            library.durable_plan(plan.id())?.state(),
            PlanState::Complete
        );

        let remove = library.plan_embedded_front_artwork(&audio_id, None)?;
        library.approve_embedded_front_artwork(&remove)?;
        EmbeddedArtworkExecutor::new(&mut library).execute(&remove)?;
        assert!(
            !embedded_picture_sha256s_from_file(std::fs::File::open(&audio)?)?.contains(&art_sha)
        );
        let relationships: u64 = library.conn.query_row(
            "SELECT COUNT(*) FROM asset_relationships
             WHERE child_asset_id = ?1 AND relationship = 'embedded-front'",
            [&audio_id],
            |row| row.get(0),
        )?;
        assert_eq!(relationships, 0);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn every_embedded_artwork_boundary_recovers_without_losing_audio() -> Result<()> {
        fn one(fail_at: Option<usize>) -> Result<Vec<&'static str>> {
            let temporary = tempfile::tempdir()?;
            let audio = temporary.path().join("track.wav");
            let artwork = temporary.path().join("original.png");
            let original_audio = wav(&[0, 1, -1, 2]);
            std::fs::write(&audio, &original_audio)?;
            std::fs::write(&artwork, png()?)?;
            let (mut library, audio_id, art_id) = managed_library(&audio, &artwork)?;
            let plan = library.plan_embedded_front_artwork(&audio_id, Some(&art_id))?;
            library.approve_embedded_front_artwork(&plan)?;
            let (_result, hits) = match fail_at {
                None => crate::failpoints::run_recording(|| {
                    EmbeddedArtworkExecutor::new(&mut library).execute(&plan)
                }),
                Some(index) => crate::failpoints::run_failing(index, || {
                    EmbeddedArtworkExecutor::new(&mut library).execute(&plan)
                }),
            };
            let recovery = library.recover_pending()?;
            assert!(recovery.unresolved.is_empty(), "{recovery:?}");
            assert!(library.recover_pending()?.unresolved.is_empty());

            let current = std::fs::read(&audio)?;
            let mut original_retained = current == original_audio;
            for entry in std::fs::read_dir(temporary.path())? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(!name.contains("artwork-stage"), "orphaned stage {name}");
                if name.contains("artwork-original")
                    && std::fs::read(entry.path())? == original_audio
                {
                    original_retained = true;
                }
            }
            assert!(original_retained, "original audio was not retained");
            let audit_report = library.audit()?;
            assert!(
                audit_report.issues().is_empty(),
                "failure {fail_at:?} left audit issues: {:?}",
                audit_report.issues()
            );
            assert!(matches!(
                library.durable_plan(plan.id())?.state(),
                PlanState::Complete | PlanState::Failed
            ));
            Ok(hits)
        }

        let hits = one(None)?;
        assert!(!hits.is_empty());
        for index in 0..hits.len() {
            one(Some(index))?;
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn every_external_artwork_boundary_retains_the_previous_projection() -> Result<()> {
        fn one(fail_at: Option<usize>) -> Result<Vec<&'static str>> {
            let temporary = tempfile::tempdir()?;
            let audio = temporary.path().join("track.wav");
            let original = temporary.path().join("original.png");
            let cover = temporary.path().join("cover.png");
            let source = png_color([2, 4, 8])?;
            let previous = png_color([12, 10, 8])?;
            std::fs::write(&audio, wav(&[0, 1, -1, 2]))?;
            std::fs::write(&original, &source)?;
            std::fs::write(&cover, &previous)?;
            let (mut library, _audio_id, original_id) = managed_library(&audio, &original)?;
            let target_id = add_external_artwork(&library, &cover)?;
            let plan = library.plan_external_artwork(&target_id, Some(&original_id))?;
            library.approve_external_artwork(&plan)?;
            let (_result, hits) = match fail_at {
                None => crate::failpoints::run_recording(|| {
                    ExternalArtworkExecutor::new(&mut library).execute(&plan)
                }),
                Some(index) => crate::failpoints::run_failing(index, || {
                    ExternalArtworkExecutor::new(&mut library).execute(&plan)
                }),
            };
            let recovery = library.recover_pending()?;
            assert!(recovery.unresolved.is_empty(), "{recovery:?}");
            assert!(library.recover_pending()?.unresolved.is_empty());

            let mut previous_retained = cover.exists() && std::fs::read(&cover)? == previous;
            for entry in std::fs::read_dir(temporary.path())? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(!name.contains("artwork-stage"), "orphaned stage {name}");
                if name.contains("artwork-original") && std::fs::read(entry.path())? == previous {
                    previous_retained = true;
                }
            }
            assert!(
                previous_retained,
                "previous artwork bytes were not retained"
            );
            let audit_report = library.audit()?;
            assert!(
                audit_report.is_empty(),
                "failure {fail_at:?} left audit issues: {:?}",
                audit_report.issues()
            );
            assert!(matches!(
                library.durable_plan(plan.id())?.state(),
                PlanState::Complete | PlanState::Failed
            ));
            Ok(hits)
        }

        let hits = one(None)?;
        assert!(!hits.is_empty());
        for index in 0..hits.len() {
            one(Some(index))?;
        }
        Ok(())
    }
}
