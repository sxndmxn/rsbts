//! Previewed, journaled, explicit audio-tag writes.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagItem};

use crate::db::{
    file_identity, hash_path, remove_file_synced, sync_directory, JournalFile, Library,
    OperationKind,
};
use crate::query::Query;
use crate::{Error, Item, Result};

#[derive(Debug, Clone)]
pub struct PlannedTagWrite {
    pub item: Item,
    pub source_identity: String,
    pub source_hash: String,
    pub source_size: u64,
    pub source_modified: std::time::SystemTime,
}

#[derive(Debug, Clone, Default)]
pub struct TagWritePlan {
    pub files: Vec<PlannedTagWrite>,
}

impl TagWritePlan {
    pub fn build(library: &Library, query: &Query) -> Result<Self> {
        let mut files = Vec::new();
        for item in library.query_items(query)? {
            let metadata = std::fs::symlink_metadata(&item.path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(Error::Import(format!(
                    "tag writing requires a regular file: {}",
                    item.path.display()
                )));
            }
            files.push(PlannedTagWrite {
                source_identity: file_identity(&metadata),
                source_hash: hash_path(&item.path)?,
                source_size: metadata.len(),
                source_modified: metadata.modified()?,
                item,
            });
        }
        Ok(Self { files })
    }
}

#[derive(Debug, Clone)]
pub struct TagWriteFailure {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct TagWriteReport {
    pub written: usize,
    pub failures: Vec<TagWriteFailure>,
}

pub struct TagWriteExecutor<'a> {
    library: &'a mut Library,
}

impl<'a> TagWriteExecutor<'a> {
    pub const fn new(library: &'a mut Library) -> Self {
        Self { library }
    }

    pub fn execute(&mut self, plan: TagWritePlan) -> TagWriteReport {
        let mut report = TagWriteReport::default();
        for file in plan.files {
            match self.write_one(&file) {
                Ok(()) => report.written += 1,
                Err(error) => report.failures.push(TagWriteFailure {
                    path: file.item.path,
                    error: error.to_string(),
                }),
            }
        }
        report
    }

    fn write_one(&mut self, plan: &PlannedTagWrite) -> Result<()> {
        validate_source(plan)?;
        let item_id = plan
            .item
            .id
            .ok_or_else(|| Error::Import("tag-write item has no database ID".into()))?;
        let operation_uuid = uuid::Uuid::new_v4();
        let rewritten = sibling_path(&plan.item.path, operation_uuid, "write")?;
        let backup = sibling_path(&plan.item.path, operation_uuid, "backup")?;
        let journal = JournalFile {
            source: plan.item.path.clone(),
            staged: backup.clone(),
            destination: rewritten.clone(),
            content_hash: None,
            source_identity: Some(plan.source_identity.clone()),
            owned_identity: None,
            role: "tag-write".into(),
            state: "prepared".into(),
        };
        let operation_id = self
            .library
            .create_operation(OperationKind::TagWrite, &[journal])?;
        let result = self.perform_write(&operation_id, plan, &rewritten, &backup, item_id);
        if let Err(error) = result {
            // Preserve the last durable journal state. In particular, a failure
            // while removing the backup happens after the database commit; if
            // that state were overwritten with `failed`, recovery would treat
            // the write as uncommitted and incorrectly restore the old file.
            let recovery = self.library.recover_pending();
            return match recovery {
                Ok(report) if report.unresolved.is_empty() => Err(error),
                Ok(report) => Err(Error::Recovery(format!(
                    "{error}; tag-write rollback needs attention: {}",
                    report.unresolved.join("; ")
                ))),
                Err(recovery_error) => Err(Error::Recovery(format!(
                    "{error}; tag-write rollback failed: {recovery_error}"
                ))),
            };
        }
        Ok(())
    }

    fn perform_write(
        &mut self,
        operation_id: &str,
        plan: &PlannedTagWrite,
        rewritten: &Path,
        backup: &Path,
        item_id: i64,
    ) -> Result<()> {
        self.library
            .set_operation_state(operation_id, "staging", None)?;
        copy_new(&plan.item.path, rewritten)?;
        std::fs::set_permissions(rewritten, std::fs::metadata(&plan.item.path)?.permissions())?;
        write_item_tags(rewritten, &plan.item)?;
        verify_written_tags(rewritten, &plan.item)?;
        let rewritten_metadata = std::fs::symlink_metadata(rewritten)?;
        let rewritten_identity = file_identity(&rewritten_metadata);
        self.library
            .set_staged_file_identity(operation_id, 0, &rewritten_identity)?;

        validate_source(plan)?;
        std::fs::hard_link(&plan.item.path, backup)?;
        sync_parent(&plan.item.path)?;
        self.library
            .set_file_state(operation_id, 0, "quarantined")?;
        remove_file_synced(&plan.item.path)?;
        std::fs::hard_link(rewritten, &plan.item.path)?;
        sync_parent(&plan.item.path)?;
        remove_file_synced(rewritten)?;
        self.library.set_file_state(operation_id, 0, "finalized")?;

        let final_metadata = std::fs::metadata(&plan.item.path)?;
        if file_identity(&final_metadata) != rewritten_identity {
            return Err(Error::Recovery(format!(
                "rewritten file identity changed during finalization: {}",
                plan.item.path.display()
            )));
        }
        let modified: DateTime<Utc> = final_metadata.modified()?.into();
        self.library.commit_tag_write(
            operation_id,
            item_id,
            &plan.item.path,
            final_metadata.len(),
            modified,
        )?;
        let backup_metadata = std::fs::symlink_metadata(backup)?;
        if file_identity(&backup_metadata) != plan.source_identity {
            return Err(Error::Recovery(format!(
                "tag-write backup identity changed: {}",
                backup.display()
            )));
        }
        remove_file_synced(backup)?;
        self.library.complete_operation(operation_id)
    }
}

fn write_item_tags(path: &Path, item: &Item) -> Result<()> {
    let mut tagged = Probe::open(path)?.read()?;
    if tagged.primary_tag().is_none() {
        tagged.insert_tag(Tag::new(tagged.primary_tag_type()));
    }
    let tag = tagged.primary_tag_mut().ok_or_else(|| {
        Error::Import(format!("cannot create a writable tag: {}", path.display()))
    })?;
    tag.set_title(item.title.clone());
    tag.set_artist(item.artist.clone());
    tag.set_album(item.album.clone());
    if let Some(albumartist) = &item.albumartist {
        tag.insert(TagItem::new(
            ItemKey::AlbumArtist,
            ItemValue::Text(albumartist.clone()),
        ));
    } else {
        tag.remove_key(ItemKey::AlbumArtist);
    }
    if let Some(genre) = &item.genre {
        tag.set_genre(genre.clone());
    } else {
        tag.remove_genre();
    }
    if let Some(track) = item.track {
        tag.set_track(track);
    } else {
        tag.remove_track();
    }
    if let Some(total) = item.extended.track_total {
        tag.set_track_total(total);
    } else {
        tag.remove_track_total();
    }
    if let Some(disc) = item.disc {
        tag.set_disk(disc);
    } else {
        tag.remove_disk();
    }
    if let Some(total) = item.extended.disc_total {
        tag.set_disk_total(total);
    } else {
        tag.remove_disk_total();
    }
    if let Some(year) = item.year.and_then(|year| u16::try_from(year).ok()) {
        tag.set_date(lofty::tag::items::Timestamp {
            year,
            month: item.extended.date.month,
            day: item.extended.date.day,
            hour: None,
            minute: None,
            second: None,
        });
    } else {
        tag.remove_date();
    }
    tagged.save_to_path(path, WriteOptions::default())?;
    let mut output = OpenOptions::new().write(true).open(path)?;
    output.flush()?;
    output.sync_all()?;
    Ok(())
}

fn verify_written_tags(path: &Path, expected: &Item) -> Result<()> {
    let actual = crate::tags::read_tags(path)?;
    if actual.title != expected.title
        || actual.artist != expected.artist
        || actual.album != expected.album
        || actual.albumartist != expected.albumartist
        || actual.genre != expected.genre
        || actual.year != expected.year
        || actual.track != expected.track
        || actual.disc != expected.disc
    {
        Err(Error::Import(format!(
            "tag verification failed after writing {}",
            path.display()
        )))
    } else {
        Ok(())
    }
}

fn validate_source(plan: &PlannedTagWrite) -> Result<()> {
    let metadata = std::fs::symlink_metadata(&plan.item.path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() != plan.source_size
        || metadata.modified()? != plan.source_modified
        || file_identity(&metadata) != plan.source_identity
        || hash_path(&plan.item.path)? != plan.source_hash
    {
        Err(Error::Import(format!(
            "file changed after tag-write planning: {}",
            plan.item.path.display()
        )))
    } else {
        Ok(())
    }
}

fn copy_new(source: &Path, destination: &Path) -> Result<()> {
    let mut input = std::fs::File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn sibling_path(path: &Path, id: uuid::Uuid, role: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Import("tag-write filename is not valid UTF-8".into()))?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("audio");
    Ok(path.with_file_name(format!(".{name}.rsbts-{id}.{role}.{extension}")))
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::OperationKind;
    use crate::{AudioFormat, ExtendedMetadata, PartialDate};

    fn minimal_wav() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&38_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&0_i16.to_le_bytes());
        bytes
    }

    #[test]
    fn explicit_write_replaces_tags_and_commits_the_new_fingerprint() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("single.wav");
        std::fs::write(&path, minimal_wav())?;
        let metadata = std::fs::metadata(&path)?;
        let extended = ExtendedMetadata {
            date: PartialDate {
                year: Some(2025),
                month: Some(2),
                day: Some(3),
            },
            track_total: Some(1),
            disc_total: Some(1),
            ..ExtendedMetadata::default()
        };
        let item = Item {
            id: None,
            album_id: None,
            path: path.clone(),
            title: "Written Title".into(),
            artist: "Written Artist".into(),
            album: "Written Album".into(),
            albumartist: Some("Album Artist".into()),
            genre: Some("Metal".into()),
            year: Some(2025),
            track: Some(1),
            disc: Some(1),
            format: AudioFormat::Wav,
            bitrate: 0,
            length: 0.0,
            file_size: Some(metadata.len()),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: metadata.modified()?.into(),
            singleton: true,
            extended,
        };
        let mut library = Library::open_in_memory()?;
        let import = library.create_operation(OperationKind::ImportInPlace, &[])?;
        library.commit_import(&import, None, &[item])?;
        library.complete_operation(&import)?;

        let before_identity = file_identity(&std::fs::metadata(&path)?);
        let plan = TagWritePlan::build(&library, &Query::all())?;
        let report = TagWriteExecutor::new(&mut library).execute(plan);

        assert_eq!(report.written, 1, "failures: {:?}", report.failures);
        assert!(report.failures.is_empty());
        let actual = crate::tags::read_tags(&path)?;
        assert_eq!(actual.title, "Written Title");
        assert_eq!(actual.artist, "Written Artist");
        assert_eq!(actual.album, "Written Album");
        assert_eq!(actual.albumartist.as_deref(), Some("Album Artist"));
        assert_eq!(actual.genre.as_deref(), Some("Metal"));
        assert_eq!(actual.year, Some(2025));
        assert_eq!(actual.track, Some(1));
        assert_eq!(actual.disc, Some(1));
        assert_ne!(file_identity(&std::fs::metadata(&path)?), before_identity);
        let stored = library.query_items(&Query::all())?;
        assert_eq!(stored[0].file_size, Some(std::fs::metadata(&path)?.len()));
        assert!(std::fs::read_dir(temporary.path())?
            .filter_map(std::result::Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().contains(".rsbts-")));
        Ok(())
    }
}
