//! Previewed, approved, journaled audio-tag projections.

use std::path::PathBuf;

use crate::db::{file_identity, hash_path, Library};
use crate::query::Query;
use crate::tag_projection::TagProjectionExecutor;
use crate::tags::{CanonicalTags, TagProfile};
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
        for item in library.query_items_bounded(query, 9_999)? {
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
        let projection = self.library.plan_tag_projection(
            item_id,
            canonical_tags(&plan.item)?,
            TagProfile::ArchivalNativeRich,
        )?;
        self.library.approve_tag_projection(&projection)?;
        TagProjectionExecutor::new(self.library)
            .execute(&projection)
            .map(|_receipt| ())
    }
}

pub(crate) fn canonical_tags(item: &Item) -> Result<CanonicalTags> {
    crate::validate_item_metadata(item)?;
    let artists = if item.extended.artists.is_empty() {
        vec![item.artist.clone()]
    } else {
        item.extended.artists.clone()
    };
    let album_artists = if item.extended.album_artists.is_empty() {
        vec![item
            .albumartist
            .clone()
            .unwrap_or_else(|| item.artist.clone())]
    } else {
        item.extended.album_artists.clone()
    };
    let genres = if item.extended.genres.is_empty() {
        item.genre.iter().cloned().collect()
    } else {
        item.extended.genres.clone()
    };
    let recording_date = partial_date_text(&item.extended.date)
        .or_else(|| item.year.map(|year| format!("{year:04}")));
    let mut tags = CanonicalTags::new(&item.title, artists, &item.album, album_artists)?
        .with_display_credits(item.artist.clone(), item.effective_albumartist().to_owned())?
        .with_positions(
            item.track.map(|number| (number, item.extended.track_total)),
            item.disc.map(|number| (number, item.extended.disc_total)),
        )?
        .with_dates(
            recording_date,
            partial_date_text(&item.extended.original_date),
        )?
        .with_release_facts(
            item.extended.label.iter().cloned().collect(),
            item.extended.catalog_number.iter().cloned().collect(),
            item.extended.country.clone(),
            item.extended.media.clone(),
            None,
        )?
        .with_genres(genres)?
        .with_classical(
            item.extended.composers.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
        )?;
    let mut recordings = Vec::new();
    let mut release_tracks = Vec::new();
    let mut releases = Vec::new();
    let mut release_groups = Vec::new();
    let mut artist_ids = Vec::new();
    let mut album_artist_ids = Vec::new();
    let mut works = Vec::new();
    for id in item
        .extended
        .external_ids
        .iter()
        .chain(item.track_external_id.iter())
        .chain(item.release_external_id.iter())
        .filter(|id| id.provider() == "musicbrainz")
    {
        let values = match id.kind() {
            "recording" | "track" => &mut recordings,
            "release-track" | "release_track" => &mut release_tracks,
            "release" | "legacy" => &mut releases,
            "release-group" | "release_group" => &mut release_groups,
            "artist" => &mut artist_ids,
            "album-artist" => &mut album_artist_ids,
            "work" => &mut works,
            _ => continue,
        };
        if !values.iter().any(|value| value == id.value()) {
            values.push(id.value().to_owned());
        }
    }
    tags = tags.with_musicbrainz_ids(
        recordings,
        release_tracks,
        releases,
        release_groups,
        artist_ids,
        album_artist_ids,
        works,
    )?;
    tags.with_extended_fields(&item.extended)
}

fn partial_date_text(date: &crate::PartialDate) -> Option<String> {
    date.year.map(|year| match (date.month, date.day) {
        (Some(month), Some(day)) => format!("{year:04}-{month:02}-{day:02}"),
        (Some(month), None) => format!("{year:04}-{month:02}"),
        _ => format!("{year:04}"),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::OperationKind;
    use crate::{AudioFormat, ExtendedMetadata, PartialDate};
    use chrono::Utc;

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
    fn canonical_reconstruction_preserves_independent_totals_and_typed_fields() -> Result<()> {
        use crate::FlexibleValue;

        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.wav");
        std::fs::write(&path, minimal_wav())?;
        let mut item = crate::tags::read_tags(&path)?;
        item.extended.track_total = Some(12);
        item.extended.disc_total = Some(2);
        for (key, value) in [
            (
                "labels",
                FlexibleValue::StringList(vec!["One".into(), "Two".into()]),
            ),
            (
                "catalog_numbers",
                FlexibleValue::StringList(vec!["A".into(), "B".into()]),
            ),
            ("barcode", FlexibleValue::String("123456789".into())),
            (
                "conductors",
                FlexibleValue::StringList(vec!["Conductor".into()]),
            ),
            (
                "lyricists",
                FlexibleValue::StringList(vec!["Lyricist".into()]),
            ),
            (
                "performers",
                FlexibleValue::StringList(vec!["Performer".into()]),
            ),
            ("work", FlexibleValue::String("Work".into())),
            ("movement", FlexibleValue::String("Movement".into())),
            ("movement_total", FlexibleValue::Integer(4)),
            (
                "replaygain_track_gain",
                FlexibleValue::String("-3 dB".into()),
            ),
            ("replaygain_track_peak", FlexibleValue::String("0.9".into())),
            (
                "replaygain_album_gain",
                FlexibleValue::String("-4 dB".into()),
            ),
            ("replaygain_album_peak", FlexibleValue::String("0.8".into())),
        ] {
            item.extended.flexible_fields.insert(key.into(), value);
        }
        let encoded = serde_json::to_value(canonical_tags(&item)?)?;
        assert_eq!(
            encoded["track_total"], 12,
            "an unknown track number erased its total"
        );
        assert_eq!(encoded["disc_total"], 2);
        assert_eq!(encoded["movement_total"], 4);
        for key in [
            "labels",
            "catalog_numbers",
            "barcode",
            "conductors",
            "lyricists",
            "performers",
            "work",
            "movement",
            "movement_total",
            "replaygain_track_gain",
            "replaygain_track_peak",
            "replaygain_album_gain",
            "replaygain_album_peak",
        ] {
            let expected = serde_json::to_value(&item.extended.flexible_fields[key])?;
            assert_eq!(encoded[key], expected["value"], "lost {key}");
        }
        Ok(())
    }

    #[test]
    fn canonical_reconstruction_rejects_malformed_typed_metadata() -> Result<()> {
        use crate::FlexibleValue;

        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.wav");
        std::fs::write(&path, minimal_wav())?;
        let original = crate::tags::read_tags(&path)?;
        let mut accepted = Vec::new();
        for (key, value) in [
            ("barcode", FlexibleValue::Integer(123)),
            ("conductors", FlexibleValue::String("Conductor".into())),
            ("movement_number", FlexibleValue::Float(1.5)),
            ("movement_total", FlexibleValue::Integer(0)),
            (
                "replaygain_track_gain",
                FlexibleValue::String("bad\nvalue".into()),
            ),
        ] {
            let mut item = original.clone();
            item.extended.flexible_fields.insert(key.into(), value);
            if canonical_tags(&item).is_ok() {
                accepted.push(key);
            }
        }
        let mut invalid_date = original;
        invalid_date.extended.date.month = Some(2);
        if canonical_tags(&invalid_date).is_ok() {
            accepted.push("date");
        }
        assert!(
            accepted.is_empty(),
            "malformed typed metadata passed: {accepted:?}"
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn native_workflow_fixture() -> Result<(tempfile::TempDir, PathBuf, CanonicalTags)> {
        use lofty::config::WriteOptions;
        use lofty::file::{AudioFile, TaggedFileExt};
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::probe::Probe;
        use lofty::tag::ItemKey;

        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("workflow.flac");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/formats/flac.flac"),
            &path,
        )?;
        let tags = CanonicalTags::new(
            "Title",
            vec!["Artist One".into(), "Artist Two".into()],
            "Album",
            vec!["Album Artist".into()],
        )?
        .with_positions(Some((1, Some(12))), Some((1, Some(2))))?
        .with_dates(Some("2024-02-29".into()), Some("1980-03".into()))?
        .with_genres(vec!["Rock".into(), "Metal".into()])?
        .with_release_facts(
            vec!["Label One".into(), "Label Two".into()],
            vec!["A".into(), "B".into()],
            Some("GB".into()),
            Some("CD".into()),
            Some("123456789".into()),
        )?
        .with_classical(
            vec!["Composer".into()],
            vec!["Conductor".into()],
            vec!["Lyricist".into()],
            vec!["Performer".into()],
            Some("Work".into()),
            Some("Movement".into()),
            Some((1, Some(4))),
        )?
        .with_replaygain(
            Some("-3 dB".into()),
            Some("0.9".into()),
            Some("-4 dB".into()),
            Some("0.8".into()),
        )?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        crate::tags::rewrite_tags(&mut file, &path, &tags, TagProfile::ArchivalNativeRich)?;
        drop(file);
        let mut native = Probe::open(&path)?.read()?;
        let tag = native
            .primary_tag_mut()
            .ok_or_else(|| Error::Import("test file has no tag".into()))?;
        assert!(tag.insert_text(ItemKey::Comment, "Unrelated native comment".into()));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2).write_to(&mut png, image::ImageFormat::Png)?;
        tag.push_picture(
            Picture::unchecked(png.into_inner())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .description("Preserved artwork")
                .build(),
        );
        native.save_to_path(&path, WriteOptions::new().remove_others(false))?;
        Ok((temporary, path, tags))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn read_import_update_modify_write_preserves_native_metadata() -> Result<()> {
        use serde_json::json;

        let (_temporary, path, tags) = native_workflow_fixture()?;
        let before = crate::tags::read_tag_snapshot(&path)?;
        let essence = crate::media::decoded_audio_essence_hash(&path)?;

        let mut item = crate::tags::read_tags(&path)?;
        item.singleton = true;
        item.extended
            .flexible_fields
            .insert("user_rating".into(), crate::FlexibleValue::Integer(42));
        let mut library = Library::open_in_memory()?;
        let import = library.create_operation(OperationKind::ImportInPlace, &[])?;
        library.commit_import(&import, None, &[item])?;
        library.complete_operation(&import)?;
        let stored = library.query_items(&Query::all())?;
        let id = stored
            .first()
            .and_then(|item| item.id)
            .ok_or_else(|| Error::Import("test import has no item".into()))?;
        library.update_items(&[(id, crate::tags::read_tags(&path)?)])?;
        library.modify_items(
            &[id],
            &[
                "title=Edited title".into(),
                "artist=Edited artist".into(),
                "genre=Edited genre".into(),
                "year=2025".into(),
            ],
        )?;
        let stored = library.query_items(&Query::all())?;
        assert_eq!(
            stored[0].extended.flexible_fields.get("user_rating"),
            Some(&crate::FlexibleValue::Integer(42))
        );
        let plan = TagWritePlan::build(&library, &Query::all())?;
        let report = TagWriteExecutor::new(&mut library).execute(plan);
        assert_eq!(report.written, 1, "failures: {:?}", report.failures);
        let reread = crate::tags::read_tags(&path)?;
        let mut expected = serde_json::to_value(tags)?;
        expected["title"] = json!("Edited title");
        expected["track_artists"] = json!(["Edited artist"]);
        expected["genres"] = json!(["Edited genre"]);
        expected["recording_date"] = json!("2025");
        assert_eq!(
            serde_json::to_value(canonical_tags(&reread)?)?,
            expected,
            "normal workflow lost supported metadata"
        );
        let after = crate::tags::read_tag_snapshot(&path)?;
        assert_eq!(after.unprojected_digest(), before.unprojected_digest());
        assert_eq!(after.picture_digest(), before.picture_digest());
        assert_eq!(crate::media::decoded_audio_essence_hash(&path)?, essence);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn explicit_write_replaces_tags_and_commits_the_new_fingerprint() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("single.wav");
        std::fs::write(&path, minimal_wav())?;
        let original = std::fs::read(&path)?;
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
        let retained = std::fs::read_dir(temporary.path())?
            .filter_map(std::result::Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().contains("tag-original"))
            .ok_or_else(|| Error::Recovery("tag write did not retain its original".into()))?;
        assert_eq!(std::fs::read(retained.path())?, original);
        Ok(())
    }
}
