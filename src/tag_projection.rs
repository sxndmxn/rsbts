//! Previewable, journaled tag projections that retain the prior file.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::asset::{digest_reader, FileDigests};
use crate::db::{file_identity, file_object_identity, JournalFile, Library, OperationKind};
use crate::failpoints;
use crate::fsops::AnchoredRoot;
use crate::media::{decoded_audio_essence_hash_from_file, probe_media_from_file, MediaDescriptor};
use crate::operations::{PlanId, PlanKind, PlanState};
use crate::tags::{
    read_tag_snapshot_from_file, rewrite_tags, validate_materialized_snapshot, CanonicalTags,
    TagProfile, TagSnapshot,
};
use crate::{Error, Result};

/// Immutable evidence captured when a tag projection is previewed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagProjectionPlan {
    plan_id: PlanId,
    asset_id: String,
    path: PathBuf,
    source_identity: String,
    source_blake3: String,
    source_sha256: String,
    source_size: u64,
    audio_essence_hash: String,
    before: TagSnapshot,
    tags: CanonicalTags,
    profile: TagProfile,
}

impl TagProjectionPlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.plan_id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn before(&self) -> &TagSnapshot {
        &self.before
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagProjectionReceipt {
    plan_id: PlanId,
    operation_id: String,
    asset_id: String,
    path: PathBuf,
    retained_original: PathBuf,
    blake3: String,
    sha256: String,
    audio_essence_hash: String,
}

impl TagProjectionReceipt {
    #[must_use]
    pub fn retained_original(&self) -> &Path {
        &self.retained_original
    }
}

impl Library {
    /// Persist a tag preview. This performs deep source validation but does not
    /// change a media file; approval remains a separate transition.
    #[allow(clippy::too_many_lines)]
    pub fn plan_tag_projection(
        &self,
        item_id: i64,
        tags: CanonicalTags,
        profile: TagProfile,
    ) -> Result<TagProjectionPlan> {
        self.build_tag_projection(item_id, tags, profile, true)
    }

    /// Run the exact projection validation without persisting plan state.
    pub fn preview_tag_projection(
        &self,
        item_id: i64,
        tags: CanonicalTags,
        profile: TagProfile,
    ) -> Result<TagProjectionPlan> {
        self.build_tag_projection(item_id, tags, profile, false)
    }

    #[allow(clippy::too_many_lines)]
    fn build_tag_projection(
        &self,
        item_id: i64,
        tags: CanonicalTags,
        profile: TagProfile,
        persist: bool,
    ) -> Result<TagProjectionPlan> {
        let stored = self
            .conn
            .query_row(
                "SELECT a.id, a.absolute_path, a.byte_size, a.blake3, a.sha256,
                        a.entry_identity, a.audio_essence_hash, a.role,
                        a.verification_state, a.managed
                 FROM assets a
                 JOIN item_assets ia ON ia.asset_id = a.id
                 WHERE ia.item_id = ?1 AND ia.relationship = 'audio'",
                [item_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        PathBuf::from(row.get::<_, String>(1)?),
                        row.get::<_, Option<u64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, bool>(9)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Error::Operation(format!("item {item_id} has no audio asset")))?;
        let (asset_id, path, size, blake3, sha256, identity, stored_essence, role, state, managed) =
            stored;
        if !managed || state != "verified" || role != "audio" {
            return Err(Error::Operation(format!(
                "asset {asset_id} is not a verified managed regular audio asset"
            )));
        }
        let parent = anchored_parent(&path)?;
        let root = AnchoredRoot::open(parent)?;
        let metadata = root.entry_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(Error::Operation(
                "tag projection refuses symlinks and opaque non-regular assets".into(),
            ));
        }
        let digests = digest_reader(root.open_file(&path)?)?;
        let observed_identity = file_identity(&metadata);
        if size != Some(digests.byte_size())
            || blake3.as_deref() != Some(digests.blake3())
            || sha256.as_deref() != Some(digests.sha256())
            || identity.as_deref() != Some(observed_identity.as_str())
        {
            return Err(Error::Operation(format!(
                "asset {asset_id} changed since its last verification"
            )));
        }
        let essence = decoded_audio_essence_hash_from_file(root.open_file(&path)?, &path)?;
        if stored_essence
            .as_deref()
            .is_some_and(|stored| stored != essence)
        {
            return Err(Error::Operation(format!(
                "asset {asset_id} decoded audio does not match its catalog evidence"
            )));
        }
        let before = read_tag_snapshot_from_file(root.open_file(&path)?)?;
        let request = serde_json::to_value(&tags)?;
        let preview = json!({
            "asset_id": asset_id,
            "path": path,
            "profile": profile,
            "policy_version": profile.policy_version(),
            "before": before,
            "after": tags,
        });
        let plan_id = if persist {
            let id =
                self.create_durable_plan(PlanKind::TagProjection, &request, &preview, Some(1))?;
            self.conn.execute(
                "INSERT INTO projection_plans
                 (id, projection_type, profile, policy_version)
                 VALUES (?1, 'tags', ?2, ?3)",
                params![id.as_str(), profile.as_str(), profile.policy_version()],
            )?;
            self.conn.execute(
                "INSERT INTO asset_projection_steps
                 (plan_id, asset_id, before_json, after_json, state, evidence_json)
                 VALUES (?1, ?2, ?3, ?4, 'planned', ?5)",
                params![
                    id.as_str(),
                    asset_id,
                    serde_json::to_string(&before)?,
                    serde_json::to_string(&tags)?,
                    serde_json::to_string(&json!({
                        "source_identity": observed_identity,
                        "blake3": digests.blake3(),
                        "sha256": digests.sha256(),
                        "audio_essence_hash": essence,
                    }))?
                ],
            )?;
            id
        } else {
            PlanId::new()
        };
        Ok(TagProjectionPlan {
            plan_id,
            asset_id,
            path,
            source_identity: observed_identity,
            source_blake3: digests.blake3().to_owned(),
            source_sha256: digests.sha256().to_owned(),
            source_size: digests.byte_size(),
            audio_essence_hash: essence,
            before,
            tags,
            profile,
        })
    }

    /// Record explicit approval without starting filesystem execution.
    pub fn approve_tag_projection(&self, plan: &TagProjectionPlan) -> Result<()> {
        self.approve_durable_plan(plan.id())?;
        let changed = self.conn.execute(
            "UPDATE projection_plans SET approved_at = ?2 WHERE id = ?1 AND approved_at IS NULL",
            params![plan.id().as_str(), Utc::now().to_rfc3339()],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(Error::Operation(
                "tag projection was already approved or is unknown".into(),
            ))
        }
    }
}

pub struct TagProjectionExecutor<'a> {
    library: &'a mut Library,
}

impl<'a> TagProjectionExecutor<'a> {
    pub const fn new(library: &'a mut Library) -> Self {
        Self { library }
    }

    /// Execute an approved tag projection, retaining the verified original.
    pub fn execute(&mut self, plan: &TagProjectionPlan) -> Result<TagProjectionReceipt> {
        if self.library.durable_plan(plan.id())?.state() != PlanState::Approved {
            return Err(Error::Operation(
                "tag projection must be explicitly approved before execution".into(),
            ));
        }
        if let Err(error) = self.library.start_durable_plan(plan.id()) {
            if self
                .library
                .durable_plan(plan.id())
                .is_ok_and(|durable| durable.state() == PlanState::Running)
            {
                let _ = self.library.finish_durable_plan(
                    plan.id(),
                    PlanState::Failed,
                    Some(&error.to_string()),
                );
            }
            return Err(error);
        }
        let parent = anchored_parent(&plan.path)?;
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
        if let Err(error) = revalidate_source(&root, plan) {
            let _ = self.library.finish_durable_plan(
                plan.id(),
                PlanState::Failed,
                Some(&error.to_string()),
            );
            return Err(error);
        }

        let transfer = uuid::Uuid::new_v4();
        let rewrite = sibling_path(&plan.path, transfer, "tag-stage")?;
        let original = sibling_path(&plan.path, transfer, "tag-original")?;
        let journal_files = [
            JournalFile {
                source: plan.path.clone(),
                staged: rewrite.clone(),
                destination: plan.path.clone(),
                content_hash: None,
                sha256: None,
                source_identity: Some(plan.source_identity.clone()),
                owned_identity: None,
                role: "tag-rewrite".into(),
                state: "prepared".into(),
            },
            JournalFile {
                source: plan.path.clone(),
                staged: original.clone(),
                destination: plan.path.clone(),
                content_hash: Some(plan.source_blake3.clone()),
                sha256: Some(plan.source_sha256.clone()),
                source_identity: Some(plan.source_identity.clone()),
                owned_identity: Some(plan.source_identity.clone()),
                role: "tag-original".into(),
                state: "prepared".into(),
            },
        ];
        let decision = json!({
            "profile": plan.profile,
            "policy_version": plan.profile.policy_version(),
            "asset_id": plan.asset_id,
        });
        let operation_id = match self.library.create_operation_for_plan(
            OperationKind::TagWrite,
            &journal_files,
            Some(plan.id().as_str()),
            Some(&decision),
        ) {
            Ok(id) => id,
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
        match self.execute_journaled(&root, plan, &operation_id, &rewrite, &original) {
            Ok(receipt) => Ok(receipt),
            Err(error) => Err(self.rollback_failed(plan.id(), &operation_id, error)),
        }
    }

    fn execute_journaled(
        &mut self,
        root: &AnchoredRoot,
        plan: &TagProjectionPlan,
        operation_id: &str,
        rewrite: &Path,
        original: &Path,
    ) -> Result<TagProjectionReceipt> {
        self.library
            .set_operation_state(operation_id, "staging", None)?;
        root.copy_new_observed(&plan.path, rewrite, |metadata| {
            self.library.set_acquired_file_identity(
                operation_id,
                0,
                &file_object_identity(metadata),
            )
        })?;
        {
            let mut staged = root.open_file_read_write(rewrite)?;
            rewrite_tags(&mut staged, &plan.path, &plan.tags, plan.profile)?;
        }
        let after = read_tag_snapshot_from_file(root.open_file(rewrite)?)?;
        validate_materialized_snapshot(&after, &plan.tags, plan.profile)?;
        if after.unprojected_digest() != plan.before.unprojected_digest()
            || after.picture_digest() != plan.before.picture_digest()
        {
            return Err(Error::Operation(
                "tag rewrite changed unknown native metadata or embedded pictures".into(),
            ));
        }
        let essence = decoded_audio_essence_hash_from_file(root.open_file(rewrite)?, &plan.path)?;
        if essence != plan.audio_essence_hash {
            return Err(Error::Operation(
                "tag rewrite changed decoded audio essence".into(),
            ));
        }
        let digests = digest_reader(root.open_file(rewrite)?)?;
        let media = probe_media_from_file(root.open_file(rewrite)?, &plan.path)?;
        let staged_identity = file_identity(&root.entry_metadata(rewrite)?);
        self.library.set_staged_file_full_evidence(
            operation_id,
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
                plan.asset_id,
                serde_json::to_string(&json!({
                    "blake3": digests.blake3(),
                    "sha256": digests.sha256(),
                    "audio_essence_hash": essence,
                    "before": plan.before,
                    "after": after,
                }))?
            ],
        )?;

        self.library
            .set_operation_state(operation_id, "finalizing", None)?;
        root.rename_noreplace(&plan.path, original)?;
        self.library
            .set_file_state(operation_id, 1, "quarantined")?;
        root.rename_noreplace(rewrite, &plan.path)?;
        self.library.set_file_state(operation_id, 0, "finalized")?;
        let published_identity = file_identity(&root.entry_metadata(&plan.path)?);
        if published_identity != staged_identity {
            return Err(Error::Operation(
                "published tag projection lost its staged filesystem identity".into(),
            ));
        }
        self.commit_projection(
            root,
            plan,
            operation_id,
            &published_identity,
            &digests,
            &essence,
            &media,
            &after,
        )?;
        self.library.complete_operation(operation_id)?;
        Ok(TagProjectionReceipt {
            plan_id: plan.plan_id.clone(),
            operation_id: operation_id.to_owned(),
            asset_id: plan.asset_id.clone(),
            path: plan.path.clone(),
            retained_original: original.to_path_buf(),
            blake3: digests.blake3().to_owned(),
            sha256: digests.sha256().to_owned(),
            audio_essence_hash: essence,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_projection(
        &mut self,
        root: &AnchoredRoot,
        plan: &TagProjectionPlan,
        operation_id: &str,
        identity: &str,
        digests: &FileDigests,
        essence: &str,
        media: &MediaDescriptor,
        after: &TagSnapshot,
    ) -> Result<()> {
        let metadata = root.entry_metadata(&plan.path)?;
        let mtime: DateTime<Utc> = metadata.modified()?.into();
        let transaction = self.library.conn.transaction()?;
        let changed = transaction.execute(
            "UPDATE assets
             SET byte_size = ?1, blake3 = ?2, sha256 = ?3, mtime = ?4,
                 entry_identity = ?5, media_json = ?6, audio_essence_hash = ?7,
                 verification_state = 'verified', last_verified_at = ?8
             WHERE id = ?9 AND entry_identity = ?10 AND verification_state = 'verified'
               AND managed = 1",
            params![
                digests.byte_size(),
                digests.blake3(),
                digests.sha256(),
                mtime.to_rfc3339(),
                identity,
                serde_json::to_string(media)?,
                essence,
                Utc::now().to_rfc3339(),
                plan.asset_id,
                plan.source_identity,
            ],
        )?;
        if changed != 1 {
            return Err(Error::Operation(
                "tag projection asset row changed before commit".into(),
            ));
        }
        transaction.execute(
            "UPDATE items SET file_size = ?1, mtime = ?2
             WHERE id IN (SELECT item_id FROM item_assets WHERE asset_id = ?3)",
            params![digests.byte_size(), mtime.to_rfc3339(), plan.asset_id],
        )?;
        transaction.execute(
            "UPDATE operation_journal SET state = 'db-committed', updated_at = ?2
             WHERE id = ?1",
            params![operation_id, Utc::now().to_rfc3339()],
        )?;
        transaction.execute(
            "UPDATE asset_projection_steps
             SET state = 'published', after_json = ?3, evidence_json = ?4
             WHERE plan_id = ?1 AND asset_id = ?2 AND state = 'validated'",
            params![
                plan.id().as_str(),
                plan.asset_id,
                serde_json::to_string(after)?,
                serde_json::to_string(&json!({
                    "entry_identity": identity,
                    "blake3": digests.blake3(),
                    "sha256": digests.sha256(),
                    "audio_essence_hash": essence,
                    "retained_original": true,
                }))?
            ],
        )?;
        transaction.execute(
            "UPDATE durable_plans
             SET state = 'complete', progress_current = 1, resume_cursor = ?2,
                 updated_at = ?3, completed_at = ?3
             WHERE id = ?1 AND state = 'running'",
            params![plan.id().as_str(), plan.asset_id, Utc::now().to_rfc3339()],
        )?;
        transaction.commit()?;
        failpoints::hit("db.tag-projection-commit")?;
        Ok(())
    }

    fn rollback_failed(&mut self, plan_id: &PlanId, operation_id: &str, error: Error) -> Error {
        let _ = self
            .library
            .record_operation_failure(operation_id, &error.to_string());
        let recovery = self.library.recover_pending();
        let _ =
            self.library
                .finish_durable_plan(plan_id, PlanState::Failed, Some(&error.to_string()));
        match recovery {
            Ok(report) if report.unresolved.is_empty() => error,
            Ok(report) => Error::Recovery(format!(
                "{error}; tag rollback needs attention: {}",
                report.unresolved.join("; ")
            )),
            Err(recovery_error) => {
                Error::Recovery(format!("{error}; tag rollback failed: {recovery_error}"))
            }
        }
    }
}

fn revalidate_source(root: &AnchoredRoot, plan: &TagProjectionPlan) -> Result<()> {
    let metadata = root.entry_metadata(&plan.path)?;
    let digests = digest_reader(root.open_file(&plan.path)?)?;
    let essence = decoded_audio_essence_hash_from_file(root.open_file(&plan.path)?, &plan.path)?;
    let snapshot = read_tag_snapshot_from_file(root.open_file(&plan.path)?)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || file_identity(&metadata) != plan.source_identity
        || digests.byte_size() != plan.source_size
        || digests.blake3() != plan.source_blake3
        || digests.sha256() != plan.source_sha256
        || essence != plan.audio_essence_hash
        || snapshot != plan.before
    {
        return Err(Error::Operation(format!(
            "tag projection source changed after preview: {}",
            plan.path.display()
        )));
    }
    Ok(())
}

fn anchored_parent(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| {
        Error::Root(format!(
            "tag projection path has no anchored parent: {}",
            path.display()
        ))
    })
}

fn sibling_path(path: &Path, operation: uuid::Uuid, role: &str) -> Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        Error::Operation(format!(
            "tag projection path has no filename: {}",
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
    use super::*;
    use chrono::Utc;
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::{ItemKey, Tag};

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

    fn add_unknown_comment(path: &Path) -> Result<()> {
        let mut tagged = Probe::open(path)?.guess_file_type()?.read()?;
        if tagged.primary_tag().is_none() {
            tagged.insert_tag(Tag::new(tagged.primary_tag_type()));
        }
        let tag = tagged
            .primary_tag_mut()
            .ok_or_else(|| Error::Operation("test WAV has no primary tag".into()))?;
        if !tag.insert_text(ItemKey::Comment, "retain me".into()) {
            return Err(Error::Operation("cannot add test comment".into()));
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        tagged.save_to(&mut file, WriteOptions::default())?;
        file.sync_all()?;
        Ok(())
    }

    fn managed_library(path: &Path) -> Result<(Library, i64)> {
        let metadata = std::fs::metadata(path)?;
        let mut library = Library::open_in_memory()?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: "Before Album".into(),
            albumartist: "Before Artist".into(),
            year: Some(2024),
            artpath: None,
            external_id: None,
            added: Utc::now(),
            extended: crate::ExtendedMetadata::default(),
        };
        let item = Item {
            id: None,
            album_id: None,
            path: path.to_path_buf(),
            title: "Before Title".into(),
            artist: "Before Artist".into(),
            album: "Before Album".into(),
            albumartist: Some("Before Artist".into()),
            genre: None,
            year: Some(2024),
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
            singleton: false,
            extended: crate::ExtendedMetadata::default(),
        };
        library.commit_import(&operation, Some(&album), &[item])?;
        library.complete_operation(&operation)?;
        let id = library
            .query_items(&crate::query::Query::all())?
            .first()
            .and_then(|item| item.id)
            .ok_or_else(|| Error::Operation("test import has no item ID".into()))?;
        Ok((library, id))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn approved_projection_preserves_unknown_tags_audio_and_original() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.wav");
        std::fs::write(&path, wav(&[0, 1, -1, 2]))?;
        add_unknown_comment(&path)?;
        let source_bytes = std::fs::read(&path)?;
        let source_essence = crate::media::decoded_audio_essence_hash(&path)?;
        let (mut library, item_id) = managed_library(&path)?;
        let tags = CanonicalTags::new(
            "After Title",
            vec!["Artist One".into(), "Artist Two".into()],
            "After Album",
            vec!["Album Artist".into()],
        )?
        .with_positions(Some((1, Some(1))), Some((1, Some(1))))?
        .with_dates(Some("2025-03".into()), Some("1980".into()))?;
        let plan = library.plan_tag_projection(item_id, tags, TagProfile::PortablePlayer)?;
        assert_eq!(std::fs::read(&path)?, source_bytes);
        assert!(TagProjectionExecutor::new(&mut library)
            .execute(&plan)
            .is_err());
        library.approve_tag_projection(&plan)?;
        let receipt = TagProjectionExecutor::new(&mut library).execute(&plan)?;

        assert_eq!(std::fs::read(receipt.retained_original())?, source_bytes);
        assert_eq!(
            crate::media::decoded_audio_essence_hash(&path)?,
            source_essence
        );
        assert_eq!(crate::tags::read_tags(&path)?.title, "After Title");
        let retained =
            read_tag_snapshot_from_file(std::fs::File::open(receipt.retained_original())?)?;
        let projected = read_tag_snapshot_from_file(std::fs::File::open(&path)?)?;
        assert_eq!(
            retained.unprojected_digest(),
            projected.unprojected_digest()
        );
        assert_eq!(
            library.durable_plan(plan.id())?.state(),
            PlanState::Complete
        );
        Ok(())
    }

    #[test]
    fn source_replacement_after_preview_is_preserved() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.wav");
        std::fs::write(&path, wav(&[0, 1, 2]))?;
        let (mut library, item_id) = managed_library(&path)?;
        let tags = CanonicalTags::new(
            "Title",
            vec!["Artist".into()],
            "Album",
            vec!["Artist".into()],
        )?;
        let plan = library.plan_tag_projection(item_id, tags, TagProfile::PortablePlayer)?;
        library.approve_tag_projection(&plan)?;
        let replacement = wav(&[9, 8, 7]);
        std::fs::write(&path, &replacement)?;

        assert!(TagProjectionExecutor::new(&mut library)
            .execute(&plan)
            .is_err());
        assert_eq!(std::fs::read(path)?, replacement);
        assert_eq!(library.durable_plan(plan.id())?.state(), PlanState::Failed);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn every_tag_projection_boundary_preserves_original_or_verified_projection() -> Result<()> {
        fn one(
            fail_at: Option<usize>,
        ) -> Result<(Result<TagProjectionReceipt>, Vec<&'static str>)> {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join("track.wav");
            std::fs::write(&path, wav(&[0, 1, -1, 2]))?;
            add_unknown_comment(&path)?;
            let original = std::fs::read(&path)?;
            let (mut library, item_id) = managed_library(&path)?;
            let tags = CanonicalTags::new(
                "Projected",
                vec!["Artist".into()],
                "Album",
                vec!["Artist".into()],
            )?;
            let plan = library.plan_tag_projection(item_id, tags, TagProfile::PortablePlayer)?;
            library.approve_tag_projection(&plan)?;
            let (result, hits) = match fail_at {
                None => crate::failpoints::run_recording(|| {
                    TagProjectionExecutor::new(&mut library).execute(&plan)
                }),
                Some(index) => crate::failpoints::run_failing(index, || {
                    TagProjectionExecutor::new(&mut library).execute(&plan)
                }),
            };
            let recovery = library.recover_pending()?;
            assert!(recovery.unresolved.is_empty(), "{recovery:?}");
            assert!(library.recover_pending()?.unresolved.is_empty());

            let current = std::fs::read(&path)?;
            let mut retained_original = current == original;
            for entry in std::fs::read_dir(temporary.path())? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(!name.contains("tag-stage"), "orphaned tag stage {name}");
                if name.contains("tag-original") && std::fs::read(entry.path())? == original {
                    retained_original = true;
                }
            }
            assert!(retained_original, "original bytes were not retained");
            assert!(library.audit()?.issues().is_empty());
            assert!(matches!(
                library.durable_plan(plan.id())?.state(),
                PlanState::Complete | PlanState::Failed
            ));
            Ok((result, hits))
        }

        let (success, boundaries) = one(None)?;
        assert!(success.is_ok());
        assert!(boundaries.len() >= 15, "insufficient boundary coverage");
        for index in 0..boundaries.len() {
            let (result, observed) = one(Some(index))?;
            assert!(
                result.is_err(),
                "boundary {index} did not fail: {observed:?}"
            );
        }
        Ok(())
    }
}
