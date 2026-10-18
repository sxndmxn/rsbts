use std::collections::{BTreeMap, HashSet};
use std::io::BufReader;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use chrono::Utc;
use lofty::aac::AacFile;
use lofty::ape::{ApeFile, ApeTag, APE_PICTURE_TYPES};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::flac::FlacFile;
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::musepack::MpcFile;
use lofty::ogg::{OpusFile, SpeexFile, VorbisFile};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::{MergeTag as _, SplitTag as _};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagItem, TagType};
use lofty::wavpack::WavPackFile;
use serde::{Deserialize, Serialize};

use crate::db::file_identity;
use crate::failpoints;
use crate::{AudioFormat, Error, ExtendedMetadata, FlexibleValue, Item, Result};

fn native_parse_options() -> ParseOptions {
    // Some files have padding after an ID3 tag, before the first audio frame.
    ParseOptions::new().max_junk_bytes(64 * 1024)
}

pub(crate) fn probe_native<R: Read + Seek>(mut reader: R) -> std::io::Result<Probe<R>> {
    let position = reader.stream_position()?;
    let mut header = [0_u8; 3];
    let starts_with_id3 = match reader.read_exact(&mut header) {
        Ok(()) => &header == b"ID3",
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => false,
        Err(error) => return Err(error),
    };
    reader.seek(SeekFrom::Start(position))?;
    let options = if starts_with_id3 {
        native_parse_options()
    } else {
        ParseOptions::new()
    };
    Probe::new(reader).options(options).guess_file_type()
}

pub(crate) fn is_native_flexible_field(field: &str) -> bool {
    matches!(
        field,
        "labels"
            | "catalog_numbers"
            | "barcode"
            | "conductors"
            | "lyricists"
            | "performers"
            | "work"
            | "movement"
            | "movement_number"
            | "movement_total"
            | "replaygain_track_gain"
            | "replaygain_track_peak"
            | "replaygain_album_gain"
            | "replaygain_album_peak"
    )
}

fn flexible_text(metadata: &ExtendedMetadata, key: &str) -> Result<Option<String>> {
    match metadata.flexible_fields.get(key) {
        None => Ok(None),
        Some(FlexibleValue::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(Error::Import(format!(
            "native field {key} must be a string"
        ))),
    }
}

fn flexible_list(metadata: &ExtendedMetadata, key: &str) -> Result<Option<Vec<String>>> {
    match metadata.flexible_fields.get(key) {
        None => Ok(None),
        Some(FlexibleValue::StringList(value)) => Ok(Some(value.clone())),
        Some(_) => Err(Error::Import(format!(
            "native field {key} must be a string list"
        ))),
    }
}

fn flexible_number(metadata: &ExtendedMetadata, key: &str) -> Result<Option<u32>> {
    match metadata.flexible_fields.get(key) {
        None => Ok(None),
        Some(FlexibleValue::Integer(value)) => u32::try_from(*value).map(Some).map_err(|error| {
            Error::Import(format!(
                "native field {key} must fit a positive u32: {error}"
            ))
        }),
        Some(_) => Err(Error::Import(format!(
            "native field {key} must be an integer"
        ))),
    }
}

/// Versioned tag-projection policy profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TagProfile {
    ArchivalNativeRich,
    PicardNavidrome,
    Id3v23Legacy,
    PortablePlayer,
}

impl TagProfile {
    #[must_use]
    pub const fn policy_version(self) -> u32 {
        1
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArchivalNativeRich => "archival-native-rich",
            Self::PicardNavidrome => "picard-navidrome",
            Self::Id3v23Legacy => "id3v2.3-legacy",
            Self::PortablePlayer => "portable-player",
        }
    }

    const fn keeps_native_multivalue(self) -> bool {
        matches!(self, Self::ArchivalNativeRich | Self::PicardNavidrome)
    }
}

/// Canonical metadata approved for materialization into one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalTags {
    title: String,
    track_artists: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track_artist_credit: Option<String>,
    album: String,
    album_artists: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_artist_credit: Option<String>,
    genres: Vec<String>,
    recording_date: Option<String>,
    original_release_date: Option<String>,
    track_number: Option<u32>,
    track_total: Option<u32>,
    disc_number: Option<u32>,
    disc_total: Option<u32>,
    labels: Vec<String>,
    catalog_numbers: Vec<String>,
    territory: Option<String>,
    medium: Option<String>,
    barcode: Option<String>,
    composers: Vec<String>,
    conductors: Vec<String>,
    lyricists: Vec<String>,
    performers: Vec<String>,
    work: Option<String>,
    movement: Option<String>,
    movement_number: Option<u32>,
    movement_total: Option<u32>,
    musicbrainz_recording_ids: Vec<String>,
    musicbrainz_release_track_ids: Vec<String>,
    musicbrainz_release_ids: Vec<String>,
    musicbrainz_release_group_ids: Vec<String>,
    musicbrainz_artist_ids: Vec<String>,
    musicbrainz_album_artist_ids: Vec<String>,
    musicbrainz_work_ids: Vec<String>,
    replaygain_track_gain: Option<String>,
    replaygain_track_peak: Option<String>,
    replaygain_album_gain: Option<String>,
    replaygain_album_peak: Option<String>,
}

impl<'de> Deserialize<'de> for CanonicalTags {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct UncheckedCanonicalTags {
            title: String,
            track_artists: Vec<String>,
            track_artist_credit: Option<String>,
            album: String,
            album_artists: Vec<String>,
            album_artist_credit: Option<String>,
            genres: Vec<String>,
            recording_date: Option<String>,
            original_release_date: Option<String>,
            track_number: Option<u32>,
            track_total: Option<u32>,
            disc_number: Option<u32>,
            disc_total: Option<u32>,
            labels: Vec<String>,
            catalog_numbers: Vec<String>,
            territory: Option<String>,
            medium: Option<String>,
            barcode: Option<String>,
            composers: Vec<String>,
            conductors: Vec<String>,
            lyricists: Vec<String>,
            performers: Vec<String>,
            work: Option<String>,
            movement: Option<String>,
            movement_number: Option<u32>,
            movement_total: Option<u32>,
            musicbrainz_recording_ids: Vec<String>,
            musicbrainz_release_track_ids: Vec<String>,
            musicbrainz_release_ids: Vec<String>,
            musicbrainz_release_group_ids: Vec<String>,
            musicbrainz_artist_ids: Vec<String>,
            musicbrainz_album_artist_ids: Vec<String>,
            musicbrainz_work_ids: Vec<String>,
            replaygain_track_gain: Option<String>,
            replaygain_track_peak: Option<String>,
            replaygain_album_gain: Option<String>,
            replaygain_album_peak: Option<String>,
        }

        let value = UncheckedCanonicalTags::deserialize(deserializer)?;
        let tags = Self {
            title: value.title,
            track_artists: value.track_artists,
            track_artist_credit: value.track_artist_credit,
            album: value.album,
            album_artists: value.album_artists,
            album_artist_credit: value.album_artist_credit,
            genres: value.genres,
            recording_date: value.recording_date,
            original_release_date: value.original_release_date,
            track_number: value.track_number,
            track_total: value.track_total,
            disc_number: value.disc_number,
            disc_total: value.disc_total,
            labels: value.labels,
            catalog_numbers: value.catalog_numbers,
            territory: value.territory,
            medium: value.medium,
            barcode: value.barcode,
            composers: value.composers,
            conductors: value.conductors,
            lyricists: value.lyricists,
            performers: value.performers,
            work: value.work,
            movement: value.movement,
            movement_number: value.movement_number,
            movement_total: value.movement_total,
            musicbrainz_recording_ids: value.musicbrainz_recording_ids,
            musicbrainz_release_track_ids: value.musicbrainz_release_track_ids,
            musicbrainz_release_ids: value.musicbrainz_release_ids,
            musicbrainz_release_group_ids: value.musicbrainz_release_group_ids,
            musicbrainz_artist_ids: value.musicbrainz_artist_ids,
            musicbrainz_album_artist_ids: value.musicbrainz_album_artist_ids,
            musicbrainz_work_ids: value.musicbrainz_work_ids,
            replaygain_track_gain: value.replaygain_track_gain,
            replaygain_track_peak: value.replaygain_track_peak,
            replaygain_album_gain: value.replaygain_album_gain,
            replaygain_album_peak: value.replaygain_album_peak,
        };
        tags.validate().map_err(serde::de::Error::custom)?;
        Ok(tags)
    }
}

impl CanonicalTags {
    /// Start an invariant-checked projection value. Optional fields are added
    /// through typed setters so callers cannot bypass validation.
    pub fn new(
        title: impl Into<String>,
        track_artists: Vec<String>,
        album: impl Into<String>,
        album_artists: Vec<String>,
    ) -> Result<Self> {
        let value = Self {
            title: title.into(),
            track_artists,
            track_artist_credit: None,
            album: album.into(),
            album_artists,
            album_artist_credit: None,
            genres: Vec::new(),
            recording_date: None,
            original_release_date: None,
            track_number: None,
            track_total: None,
            disc_number: None,
            disc_total: None,
            labels: Vec::new(),
            catalog_numbers: Vec::new(),
            territory: None,
            medium: None,
            barcode: None,
            composers: Vec::new(),
            conductors: Vec::new(),
            lyricists: Vec::new(),
            performers: Vec::new(),
            work: None,
            movement: None,
            movement_number: None,
            movement_total: None,
            musicbrainz_recording_ids: Vec::new(),
            musicbrainz_release_track_ids: Vec::new(),
            musicbrainz_release_ids: Vec::new(),
            musicbrainz_release_group_ids: Vec::new(),
            musicbrainz_artist_ids: Vec::new(),
            musicbrainz_album_artist_ids: Vec::new(),
            musicbrainz_work_ids: Vec::new(),
            replaygain_track_gain: None,
            replaygain_track_peak: None,
            replaygain_album_gain: None,
            replaygain_album_peak: None,
        };
        value.validate()?;
        Ok(value)
    }

    /// Preserve display credits independently of the separate artist names.
    pub fn with_display_credits(mut self, track: String, album: String) -> Result<Self> {
        self.track_artist_credit = (track != self.track_artists.join("; ")).then_some(track);
        self.album_artist_credit = (album != self.album_artists.join("; ")).then_some(album);
        self.validate()?;
        Ok(self)
    }

    fn display_track_artist(&self) -> String {
        self.track_artist_credit
            .clone()
            .unwrap_or_else(|| self.track_artists.join("; "))
    }

    fn display_album_artist(&self) -> String {
        self.album_artist_credit
            .clone()
            .unwrap_or_else(|| self.album_artists.join("; "))
    }

    pub fn with_positions(
        mut self,
        track: Option<(u32, Option<u32>)>,
        disc: Option<(u32, Option<u32>)>,
    ) -> Result<Self> {
        if let Some((number, total)) = track {
            self.track_number = Some(number);
            self.track_total = total;
        }
        if let Some((number, total)) = disc {
            self.disc_number = Some(number);
            self.disc_total = total;
        }
        self.validate()?;
        Ok(self)
    }

    pub fn with_dates(
        mut self,
        recording_date: Option<String>,
        original_release_date: Option<String>,
    ) -> Result<Self> {
        self.recording_date = recording_date;
        self.original_release_date = original_release_date;
        self.validate()?;
        Ok(self)
    }

    pub fn with_release_facts(
        mut self,
        labels: Vec<String>,
        catalog_numbers: Vec<String>,
        territory: Option<String>,
        medium: Option<String>,
        barcode: Option<String>,
    ) -> Result<Self> {
        self.labels = labels;
        self.catalog_numbers = catalog_numbers;
        self.territory = territory;
        self.medium = medium;
        self.barcode = barcode;
        self.validate()?;
        Ok(self)
    }

    pub fn with_genres(mut self, genres: Vec<String>) -> Result<Self> {
        self.genres = genres;
        self.validate()?;
        Ok(self)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_classical(
        mut self,
        composers: Vec<String>,
        conductors: Vec<String>,
        lyricists: Vec<String>,
        performers: Vec<String>,
        work: Option<String>,
        movement: Option<String>,
        movement_position: Option<(u32, Option<u32>)>,
    ) -> Result<Self> {
        self.composers = composers;
        self.conductors = conductors;
        self.lyricists = lyricists;
        self.performers = performers;
        self.work = work;
        self.movement = movement;
        if let Some((number, total)) = movement_position {
            self.movement_number = Some(number);
            self.movement_total = total;
        }
        self.validate()?;
        Ok(self)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_musicbrainz_ids(
        mut self,
        recordings: Vec<String>,
        release_tracks: Vec<String>,
        releases: Vec<String>,
        release_groups: Vec<String>,
        artists: Vec<String>,
        album_artists: Vec<String>,
        works: Vec<String>,
    ) -> Result<Self> {
        self.musicbrainz_recording_ids = recordings;
        self.musicbrainz_release_track_ids = release_tracks;
        self.musicbrainz_release_ids = releases;
        self.musicbrainz_release_group_ids = release_groups;
        self.musicbrainz_artist_ids = artists;
        self.musicbrainz_album_artist_ids = album_artists;
        self.musicbrainz_work_ids = works;
        self.validate()?;
        Ok(self)
    }

    pub fn with_replaygain(
        mut self,
        track_gain: Option<String>,
        track_peak: Option<String>,
        album_gain: Option<String>,
        album_peak: Option<String>,
    ) -> Result<Self> {
        self.replaygain_track_gain = track_gain;
        self.replaygain_track_peak = track_peak;
        self.replaygain_album_gain = album_gain;
        self.replaygain_album_peak = album_peak;
        self.validate()?;
        Ok(self)
    }

    pub(crate) fn with_extended_fields(mut self, metadata: &ExtendedMetadata) -> Result<Self> {
        self.track_total = metadata.track_total;
        self.disc_total = metadata.disc_total;
        if let Some(labels) = flexible_list(metadata, "labels")? {
            self.labels = labels;
        }
        if let Some(numbers) = flexible_list(metadata, "catalog_numbers")? {
            self.catalog_numbers = numbers;
        }
        self.barcode = flexible_text(metadata, "barcode")?;
        self.conductors = flexible_list(metadata, "conductors")?.unwrap_or_default();
        self.lyricists = flexible_list(metadata, "lyricists")?.unwrap_or_default();
        self.performers = flexible_list(metadata, "performers")?.unwrap_or_default();
        self.work = flexible_text(metadata, "work")?;
        self.movement = flexible_text(metadata, "movement")?;
        self.movement_number = flexible_number(metadata, "movement_number")?;
        self.movement_total = flexible_number(metadata, "movement_total")?;
        self.replaygain_track_gain = flexible_text(metadata, "replaygain_track_gain")?;
        self.replaygain_track_peak = flexible_text(metadata, "replaygain_track_peak")?;
        self.replaygain_album_gain = flexible_text(metadata, "replaygain_album_gain")?;
        self.replaygain_album_peak = flexible_text(metadata, "replaygain_album_peak")?;
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<()> {
        validate_tag_text(&self.title, "title")?;
        validate_tag_text(&self.album, "album")?;
        for (name, values) in [
            ("track artist", &self.track_artists),
            ("album artist", &self.album_artists),
            ("genre", &self.genres),
            ("label", &self.labels),
            ("catalog number", &self.catalog_numbers),
            ("composer", &self.composers),
            ("conductor", &self.conductors),
            ("lyricist", &self.lyricists),
            ("performer", &self.performers),
        ] {
            validate_tag_values(values, name)?;
        }
        if self.track_artists.is_empty() || self.album_artists.is_empty() {
            return Err(Error::Operation(
                "tag projection requires track and album artist credits".into(),
            ));
        }
        for (name, value) in [
            ("track artist credit", self.track_artist_credit.as_deref()),
            ("album artist credit", self.album_artist_credit.as_deref()),
            ("territory", self.territory.as_deref()),
            ("medium", self.medium.as_deref()),
            ("barcode", self.barcode.as_deref()),
            ("work", self.work.as_deref()),
            ("movement", self.movement.as_deref()),
        ] {
            if let Some(value) = value {
                validate_tag_text(value, name)?;
            }
        }
        for date in [&self.recording_date, &self.original_release_date]
            .into_iter()
            .flatten()
        {
            crate::catalog::PartialDate::parse(date.clone())?;
        }
        if [
            (self.track_number, self.track_total),
            (self.disc_number, self.disc_total),
        ]
        .into_iter()
        .any(|(number, total)| {
            total == Some(0)
                || number
                    .is_some_and(|number| number == 0 || total.is_some_and(|total| total < number))
        }) {
            return Err(Error::Operation(
                "tag positions must be positive and no greater than their totals".into(),
            ));
        }
        if self.movement_total == Some(0)
            || self.movement_number.is_some_and(|number| {
                number == 0 || self.movement_total.is_some_and(|total| total < number)
            })
        {
            return Err(Error::Operation("invalid movement position".into()));
        }
        for id in self
            .musicbrainz_recording_ids
            .iter()
            .chain(&self.musicbrainz_release_track_ids)
            .chain(&self.musicbrainz_release_ids)
            .chain(&self.musicbrainz_release_group_ids)
            .chain(&self.musicbrainz_artist_ids)
            .chain(&self.musicbrainz_album_artist_ids)
            .chain(&self.musicbrainz_work_ids)
        {
            uuid::Uuid::parse_str(id).map_err(|error| {
                Error::Operation(format!("invalid MusicBrainz UUID {id:?}: {error}"))
            })?;
        }
        for value in [
            &self.replaygain_track_gain,
            &self.replaygain_track_peak,
            &self.replaygain_album_gain,
            &self.replaygain_album_peak,
        ]
        .into_iter()
        .flatten()
        {
            validate_tag_text(value, "loudness value")?;
        }
        Ok(())
    }
}

/// Logical tag evidence used to prove reread and preservation behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSnapshot {
    tag_type: String,
    projected_values: BTreeMap<String, Vec<String>>,
    unprojected_digest: String,
    picture_digest: String,
    #[serde(default)]
    unprojected_count: usize,
    #[serde(default)]
    picture_count: usize,
}

impl TagSnapshot {
    #[must_use]
    pub fn unprojected_digest(&self) -> &str {
        &self.unprojected_digest
    }

    #[must_use]
    pub fn picture_digest(&self) -> &str {
        &self.picture_digest
    }

    #[must_use]
    pub const fn unprojected_count(&self) -> usize {
        self.unprojected_count
    }

    #[must_use]
    pub const fn picture_count(&self) -> usize {
        self.picture_count
    }
}

/// Read all canonical multivalues plus fingerprints for unknown items and pictures.
pub fn read_tag_snapshot(path: &Path) -> Result<TagSnapshot> {
    read_tag_snapshot_from_file(std::fs::File::open(path)?)
}

pub(crate) fn rewrite_tags(
    file: &mut std::fs::File,
    path_hint: &Path,
    tags: &CanonicalTags,
    profile: TagProfile,
) -> Result<()> {
    tags.validate()?;
    let mut probe_reader = file.try_clone()?;
    probe_reader.seek(SeekFrom::Start(0))?;
    let file_type = probe_native(BufReader::new(probe_reader))?
        .file_type()
        .ok_or_else(|| {
            Error::Operation(format!(
                "{} has no supported content-derived media type",
                path_hint.display()
            ))
        })?;
    let mut options = WriteOptions::new()
        .remove_others(false)
        .respect_read_only(true)
        .preferred_padding(WriteOptions::DEFAULT_PREFERRED_PADDING);
    if profile == TagProfile::Id3v23Legacy {
        options = options.use_id3v23(true);
    }
    options = options.lossy_text_encoding(false);

    rewrite_native_tag(file, file_type, options, false, &mut |generic| {
        materialize_tag(generic, tags, profile)
    })?;
    failpoints::hit("fs.write-tag-staging")?;
    file.sync_all()?;
    failpoints::hit("fs.sync-tag-staging")?;
    Ok(())
}

/// Replace only the embedded front-cover projection while preserving native
/// metadata remainder and all other pictures.
pub(crate) fn rewrite_embedded_front(
    file: &mut std::fs::File,
    path_hint: &Path,
    artwork: Option<(&[u8], &str)>,
) -> Result<()> {
    let picture = artwork
        .map(|(bytes, mime)| {
            let mime = match mime {
                "image/jpeg" => MimeType::Jpeg,
                "image/png" => MimeType::Png,
                "image/gif" => MimeType::Gif,
                _ => {
                    return Err(Error::Artwork(format!(
                        "{mime} cannot be represented as embedded artwork"
                    )));
                }
            };
            Ok(Picture::unchecked(bytes.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(mime)
                .description("Front cover projected by rsbts")
                .build())
        })
        .transpose()?;
    let mut probe_reader = file.try_clone()?;
    probe_reader.seek(SeekFrom::Start(0))?;
    let file_type = probe_native(BufReader::new(probe_reader))?
        .file_type()
        .ok_or_else(|| {
            Error::Operation(format!(
                "{} has no supported content-derived media type",
                path_hint.display()
            ))
        })?;
    let options = WriteOptions::new()
        .remove_others(false)
        .respect_read_only(true)
        .preferred_padding(WriteOptions::DEFAULT_PREFERRED_PADDING)
        .lossy_text_encoding(false);
    rewrite_native_tag(file, file_type, options, true, &mut |generic| {
        generic.remove_picture_type(PictureType::CoverFront);
        if generic.tag_type() == TagType::Mp4Ilst {
            // MP4 `covr` has no role field; all decoded ilst artwork is Other.
            generic.remove_picture_type(PictureType::Other);
        }
        if let Some(picture) = picture.clone() {
            generic.push_picture(picture);
        }
        Ok(())
    })?;
    failpoints::hit("fs.write-artwork-staging")?;
    file.sync_all()?;
    failpoints::hit("fs.sync-artwork-staging")?;
    Ok(())
}

fn rewrite_native_tag<F>(
    file: &mut std::fs::File,
    file_type: FileType,
    options: WriteOptions,
    remove_native_front: bool,
    mutate: &mut F,
) -> Result<()>
where
    F: FnMut(&mut Tag) -> Result<()>,
{
    // Native split/merge is essential here: a generic `TaggedFile` conversion
    // discards dialect-specific fields it cannot model (notably APEv2 cover
    // items). Splitting retains that remainder and merges it back unchanged.
    macro_rules! rewrite_optional_native {
        ($file_ty:path, $remove:ident, $set:ident) => {{
            let mut reader = file.try_clone()?;
            reader.seek(SeekFrom::Start(0))?;
            let mut media =
                <$file_ty as AudioFile>::read_from(&mut reader, native_parse_options())?;
            let native = media.$remove().unwrap_or_default();
            let (remainder, mut generic) = native.split_tag();
            mutate(&mut generic)?;
            media.$set(remainder.merge_tag(generic));
            file.seek(SeekFrom::Start(0))?;
            media.save_to(file, options)?;
        }};
    }
    macro_rules! rewrite_required_native {
        ($file_ty:path, $remove:ident, $set:ident) => {{
            let mut reader = file.try_clone()?;
            reader.seek(SeekFrom::Start(0))?;
            let mut media =
                <$file_ty as AudioFile>::read_from(&mut reader, native_parse_options())?;
            let native = media.$remove();
            let (remainder, mut generic) = native.split_tag();
            mutate(&mut generic)?;
            media.$set(remainder.merge_tag(generic));
            file.seek(SeekFrom::Start(0))?;
            media.save_to(file, options)?;
        }};
    }
    macro_rules! rewrite_optional_ape {
        ($file_ty:path) => {{
            let mut reader = file.try_clone()?;
            reader.seek(SeekFrom::Start(0))?;
            let mut media =
                <$file_ty as AudioFile>::read_from(&mut reader, native_parse_options())?;
            let mut native = media.remove_ape().unwrap_or_default();
            if remove_native_front {
                native.remove("Cover Art (Front)");
            }
            let (remainder, mut generic) = native.split_tag();
            mutate(&mut generic)?;
            media.set_ape(remainder.merge_tag(generic));
            file.seek(SeekFrom::Start(0))?;
            media.save_to(file, options)?;
        }};
    }

    match file_type {
        FileType::Flac => {
            rewrite_optional_native!(FlacFile, remove_vorbis_comments, set_vorbis_comments);
        }
        FileType::Mpeg => rewrite_optional_native!(MpegFile, remove_id3v2, set_id3v2),
        FileType::Vorbis => {
            rewrite_required_native!(VorbisFile, remove_vorbis_comments, set_vorbis_comments);
        }
        FileType::Opus => {
            rewrite_required_native!(OpusFile, remove_vorbis_comments, set_vorbis_comments);
        }
        FileType::Speex => {
            rewrite_required_native!(SpeexFile, remove_vorbis_comments, set_vorbis_comments);
        }
        FileType::Mp4 => rewrite_optional_native!(Mp4File, remove_ilst, set_ilst),
        FileType::Aac => rewrite_optional_native!(AacFile, remove_id3v2, set_id3v2),
        FileType::Wav => rewrite_optional_native!(WavFile, remove_id3v2, set_id3v2),
        FileType::Aiff => rewrite_optional_native!(AiffFile, remove_id3v2, set_id3v2),
        FileType::WavPack => rewrite_optional_ape!(WavPackFile),
        FileType::Ape => rewrite_optional_ape!(ApeFile),
        FileType::Mpc => rewrite_optional_ape!(MpcFile),
        _ => {
            return Err(Error::Operation(format!(
                "{} has no native tag writer in capability matrix version {}",
                "media file",
                crate::media::FORMAT_CAPABILITY_VERSION
            )));
        }
    }
    Ok(())
}

pub(crate) fn read_tag_snapshot_from_file(file: std::fs::File) -> Result<TagSnapshot> {
    let mut generic_reader = file.try_clone()?;
    generic_reader.seek(SeekFrom::Start(0))?;
    let tagged = probe_native(BufReader::new(generic_reader))?.read()?;
    let mut snapshot = snapshot_tagged(&tagged);
    if matches!(
        tagged.file_type(),
        FileType::Ape | FileType::Mpc | FileType::WavPack
    ) {
        let mut native_reader = file;
        native_reader.seek(SeekFrom::Start(0))?;
        let native = match tagged.file_type() {
            FileType::Ape => {
                ApeFile::read_from(&mut native_reader, ParseOptions::new())?.remove_ape()
            }
            FileType::Mpc => {
                MpcFile::read_from(&mut native_reader, ParseOptions::new())?.remove_ape()
            }
            FileType::WavPack => {
                WavPackFile::read_from(&mut native_reader, ParseOptions::new())?.remove_ape()
            }
            _ => None,
        };
        if let Some(native) = native {
            add_ape_remainder_evidence(&mut snapshot, &native);
        }
    }
    Ok(snapshot)
}

pub(crate) fn embedded_picture_sha256s_from_file(file: std::fs::File) -> Result<Vec<String>> {
    let mut generic_reader = file.try_clone()?;
    generic_reader.seek(SeekFrom::Start(0))?;
    let tagged = probe_native(BufReader::new(generic_reader))?.read()?;
    let mut digests = tagged
        .tags()
        .iter()
        .flat_map(lofty::tag::Tag::pictures)
        .map(|picture| crate::asset::sha256_bytes(picture.data()))
        .collect::<Vec<_>>();
    if matches!(
        tagged.file_type(),
        FileType::Ape | FileType::Mpc | FileType::WavPack
    ) {
        let mut native_reader = file;
        native_reader.seek(SeekFrom::Start(0))?;
        let native = match tagged.file_type() {
            FileType::Ape => {
                ApeFile::read_from(&mut native_reader, ParseOptions::new())?.remove_ape()
            }
            FileType::Mpc => {
                MpcFile::read_from(&mut native_reader, ParseOptions::new())?.remove_ape()
            }
            FileType::WavPack => {
                WavPackFile::read_from(&mut native_reader, ParseOptions::new())?.remove_ape()
            }
            _ => None,
        };
        if let Some(native) = native {
            for item in &native {
                if APE_PICTURE_TYPES.contains(&item.key()) {
                    if let ItemValue::Binary(bytes) = item.value() {
                        if let Ok(picture) = Picture::from_ape_bytes(item.key(), bytes) {
                            digests.push(crate::asset::sha256_bytes(picture.data()));
                        }
                    }
                }
            }
        }
    }
    digests.sort_unstable();
    digests.dedup();
    Ok(digests)
}

pub(crate) fn validate_materialized_snapshot(
    snapshot: &TagSnapshot,
    tags: &CanonicalTags,
    profile: TagProfile,
) -> Result<()> {
    let expected = expected_projected_values(tags, profile);
    if snapshot.projected_values == expected {
        Ok(())
    } else {
        Err(Error::Operation(format!(
            "tag reread did not match the approved projection; expected {expected:?}, observed {:?}",
            snapshot.projected_values
        )))
    }
}

fn expected_projected_values(
    values: &CanonicalTags,
    profile: TagProfile,
) -> BTreeMap<String, Vec<String>> {
    let mut expected = BTreeMap::new();
    expected_one(&mut expected, ItemKey::TrackTitle, Some(&values.title));
    expected_one(&mut expected, ItemKey::AlbumTitle, Some(&values.album));
    expected_one(
        &mut expected,
        ItemKey::TrackArtist,
        Some(&values.display_track_artist()),
    );
    expected_one(
        &mut expected,
        ItemKey::AlbumArtist,
        Some(&values.display_album_artist()),
    );
    if profile.keeps_native_multivalue() {
        expected_many(&mut expected, ItemKey::TrackArtists, &values.track_artists);
        expected_many(&mut expected, ItemKey::AlbumArtists, &values.album_artists);
    }
    for (key, list) in [
        (ItemKey::Genre, &values.genres),
        (ItemKey::Label, &values.labels),
        (ItemKey::CatalogNumber, &values.catalog_numbers),
        (ItemKey::Composer, &values.composers),
        (ItemKey::Conductor, &values.conductors),
        (ItemKey::Lyricist, &values.lyricists),
        (ItemKey::Performer, &values.performers),
        (
            ItemKey::MusicBrainzRecordingId,
            &values.musicbrainz_recording_ids,
        ),
        (
            ItemKey::MusicBrainzTrackId,
            &values.musicbrainz_release_track_ids,
        ),
        (
            ItemKey::MusicBrainzReleaseId,
            &values.musicbrainz_release_ids,
        ),
        (
            ItemKey::MusicBrainzReleaseGroupId,
            &values.musicbrainz_release_group_ids,
        ),
        (ItemKey::MusicBrainzArtistId, &values.musicbrainz_artist_ids),
        (
            ItemKey::MusicBrainzReleaseArtistId,
            &values.musicbrainz_album_artist_ids,
        ),
        (ItemKey::MusicBrainzWorkId, &values.musicbrainz_work_ids),
    ] {
        if profile.keeps_native_multivalue() {
            expected_many(&mut expected, key, list);
        } else if !list.is_empty() {
            expected_one(&mut expected, key, Some(&list.join("; ")));
        }
    }
    for (key, value) in [
        (ItemKey::RecordingDate, values.recording_date.as_ref()),
        (
            ItemKey::OriginalReleaseDate,
            values.original_release_date.as_ref(),
        ),
        (ItemKey::ReleaseCountry, values.territory.as_ref()),
        (ItemKey::OriginalMediaType, values.medium.as_ref()),
        (ItemKey::Barcode, values.barcode.as_ref()),
        (ItemKey::Work, values.work.as_ref()),
        (ItemKey::Movement, values.movement.as_ref()),
        (
            ItemKey::ReplayGainTrackGain,
            values.replaygain_track_gain.as_ref(),
        ),
        (
            ItemKey::ReplayGainTrackPeak,
            values.replaygain_track_peak.as_ref(),
        ),
        (
            ItemKey::ReplayGainAlbumGain,
            values.replaygain_album_gain.as_ref(),
        ),
        (
            ItemKey::ReplayGainAlbumPeak,
            values.replaygain_album_peak.as_ref(),
        ),
    ] {
        expected_one(&mut expected, key, value);
    }
    for (key, value) in [
        (ItemKey::TrackNumber, values.track_number),
        (ItemKey::TrackTotal, values.track_total),
        (ItemKey::DiscNumber, values.disc_number),
        (ItemKey::DiscTotal, values.disc_total),
        (ItemKey::MovementNumber, values.movement_number),
        (ItemKey::MovementTotal, values.movement_total),
    ] {
        if let Some(value) = value {
            expected_one(&mut expected, key, Some(&value.to_string()));
        }
    }
    expected
}

fn expected_one(
    expected: &mut BTreeMap<String, Vec<String>>,
    key: ItemKey,
    value: Option<&String>,
) {
    if let Some(value) = value {
        expected.insert(format!("{key:?}"), vec![value.clone()]);
    }
}

fn expected_many(expected: &mut BTreeMap<String, Vec<String>>, key: ItemKey, values: &[String]) {
    if !values.is_empty() {
        expected.insert(format!("{key:?}"), values.to_vec());
    }
}

fn snapshot_tagged(tagged: &lofty::file::TaggedFile) -> TagSnapshot {
    let primary = tagged.primary_tag().or_else(|| tagged.first_tag());
    let mut projected_values = BTreeMap::new();
    if let Some(tag) = primary {
        for key in projected_keys() {
            let values = tag
                .get_strings(*key)
                .flat_map(|value| {
                    if tag.tag_type() == TagType::Ape {
                        value.split('\0').map(str::to_owned).collect::<Vec<_>>()
                    } else {
                        vec![value.to_owned()]
                    }
                })
                .collect::<Vec<_>>();
            if !values.is_empty() {
                projected_values.insert(format!("{key:?}"), values);
            }
        }
    }

    let projected = projected_keys().iter().copied().collect::<HashSet<_>>();
    let mut unknown = Vec::new();
    let mut pictures = Vec::new();
    for tag in tagged.tags() {
        for item in tag.items().filter(|item| {
            !projected.contains(&item.key()) && item.key() != ItemKey::EncoderSoftware
        }) {
            let mut hasher = blake3::Hasher::new();
            hash_part(&mut hasher, format!("{:?}", tag.tag_type()).as_bytes());
            hash_part(&mut hasher, format!("{:?}", item.key()).as_bytes());
            hash_part(&mut hasher, item.lang());
            hash_part(&mut hasher, item.description().as_bytes());
            match item.value() {
                ItemValue::Text(value) => {
                    hash_part(&mut hasher, b"text");
                    hash_part(&mut hasher, value.as_bytes());
                }
                ItemValue::Locator(value) => {
                    hash_part(&mut hasher, b"locator");
                    hash_part(&mut hasher, value.as_bytes());
                }
                ItemValue::Binary(value) => {
                    hash_part(&mut hasher, b"binary");
                    hash_part(&mut hasher, value);
                }
            }
            unknown.push(hasher.finalize().to_hex().to_string());
        }
        for picture in tag.pictures() {
            pictures.push(picture_fingerprint(tag.tag_type(), picture));
        }
    }
    unknown.sort_unstable();
    pictures.sort_unstable();
    TagSnapshot {
        tag_type: primary.map_or_else(|| "none".into(), |tag| format!("{:?}", tag.tag_type())),
        projected_values,
        unprojected_digest: hash_strings(&unknown),
        picture_digest: hash_strings(&pictures),
        unprojected_count: unknown.len(),
        picture_count: pictures.len(),
    }
}

fn add_ape_remainder_evidence(snapshot: &mut TagSnapshot, tag: &ApeTag) {
    let mut unknown = Vec::new();
    let mut pictures = Vec::new();
    for item in tag {
        if APE_PICTURE_TYPES.contains(&item.key()) {
            if let ItemValue::Binary(bytes) = item.value() {
                if let Ok(picture) = Picture::from_ape_bytes(item.key(), bytes) {
                    pictures.push(picture_fingerprint(TagType::Ape, &picture));
                }
            }
        } else if ItemKey::from_key(TagType::Ape, item.key()).is_none() {
            let mut hasher = blake3::Hasher::new();
            hash_part(&mut hasher, b"Ape");
            hash_part(&mut hasher, item.key().as_bytes());
            match item.value() {
                ItemValue::Text(value) => {
                    hash_part(&mut hasher, b"text");
                    hash_part(&mut hasher, value.as_bytes());
                }
                ItemValue::Locator(value) => {
                    hash_part(&mut hasher, b"locator");
                    hash_part(&mut hasher, value.as_bytes());
                }
                ItemValue::Binary(value) => {
                    hash_part(&mut hasher, b"binary");
                    hash_part(&mut hasher, value);
                }
            }
            unknown.push(hasher.finalize().to_hex().to_string());
        }
    }
    unknown.sort_unstable();
    pictures.sort_unstable();
    snapshot.unprojected_digest = combine_digests(&snapshot.unprojected_digest, &unknown);
    snapshot.picture_digest = combine_digests(&snapshot.picture_digest, &pictures);
    snapshot.unprojected_count += unknown.len();
    snapshot.picture_count += pictures.len();
}

fn picture_fingerprint(tag_type: TagType, picture: &Picture) -> String {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, format!("{tag_type:?}").as_bytes());
    hash_part(&mut hasher, &[picture.pic_type().as_u8()]);
    if let Some(mime) = picture.mime_type() {
        hash_part(&mut hasher, format!("{mime:?}").as_bytes());
    }
    if let Some(description) = picture.description() {
        hash_part(&mut hasher, description.as_bytes());
    }
    hash_part(&mut hasher, picture.data());
    hasher.finalize().to_hex().to_string()
}

fn combine_digests(existing: &str, additions: &[String]) -> String {
    if additions.is_empty() {
        return existing.to_owned();
    }
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, existing.as_bytes());
    for value in additions {
        hash_part(&mut hasher, value.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

#[allow(clippy::too_many_lines)]
fn materialize_tag(tag: &mut Tag, values: &CanonicalTags, profile: TagProfile) -> Result<()> {
    for key in projected_keys() {
        tag.remove_key(*key);
    }
    put_one(tag, ItemKey::TrackTitle, &values.title, "title")?;
    put_one(tag, ItemKey::AlbumTitle, &values.album, "album")?;
    put_credit(
        tag,
        ItemKey::TrackArtist,
        ItemKey::TrackArtists,
        &values.display_track_artist(),
        &values.track_artists,
        profile,
        "track artists",
    )?;
    put_credit(
        tag,
        ItemKey::AlbumArtist,
        ItemKey::AlbumArtists,
        &values.display_album_artist(),
        &values.album_artists,
        profile,
        "album artists",
    )?;
    put_many(tag, ItemKey::Genre, &values.genres, profile, "genres")?;
    put_optional(
        tag,
        ItemKey::RecordingDate,
        values.recording_date.as_deref(),
        "recording date",
    )?;
    put_optional(
        tag,
        ItemKey::OriginalReleaseDate,
        values.original_release_date.as_deref(),
        "original release date",
    )?;
    put_number(
        tag,
        ItemKey::TrackNumber,
        values.track_number,
        "track number",
    )?;
    put_number(tag, ItemKey::TrackTotal, values.track_total, "track total")?;
    put_number(tag, ItemKey::DiscNumber, values.disc_number, "disc number")?;
    put_number(tag, ItemKey::DiscTotal, values.disc_total, "disc total")?;
    put_many(tag, ItemKey::Label, &values.labels, profile, "labels")?;
    put_many(
        tag,
        ItemKey::CatalogNumber,
        &values.catalog_numbers,
        profile,
        "catalog numbers",
    )?;
    put_optional(
        tag,
        ItemKey::ReleaseCountry,
        values.territory.as_deref(),
        "release territory",
    )?;
    put_optional(
        tag,
        ItemKey::OriginalMediaType,
        values.medium.as_deref(),
        "medium",
    )?;
    put_optional(tag, ItemKey::Barcode, values.barcode.as_deref(), "barcode")?;
    put_many(
        tag,
        ItemKey::Composer,
        &values.composers,
        profile,
        "composers",
    )?;
    put_many(
        tag,
        ItemKey::Conductor,
        &values.conductors,
        profile,
        "conductors",
    )?;
    put_many(
        tag,
        ItemKey::Lyricist,
        &values.lyricists,
        profile,
        "lyricists",
    )?;
    put_many(
        tag,
        ItemKey::Performer,
        &values.performers,
        profile,
        "performers",
    )?;
    put_optional(tag, ItemKey::Work, values.work.as_deref(), "work")?;
    put_optional(
        tag,
        ItemKey::Movement,
        values.movement.as_deref(),
        "movement",
    )?;
    put_number(
        tag,
        ItemKey::MovementNumber,
        values.movement_number,
        "movement number",
    )?;
    put_number(
        tag,
        ItemKey::MovementTotal,
        values.movement_total,
        "movement total",
    )?;
    put_many(
        tag,
        ItemKey::MusicBrainzRecordingId,
        &values.musicbrainz_recording_ids,
        profile,
        "MusicBrainz recording IDs",
    )?;
    put_many(
        tag,
        ItemKey::MusicBrainzTrackId,
        &values.musicbrainz_release_track_ids,
        profile,
        "MusicBrainz release-track IDs",
    )?;
    put_many(
        tag,
        ItemKey::MusicBrainzReleaseId,
        &values.musicbrainz_release_ids,
        profile,
        "MusicBrainz release IDs",
    )?;
    put_many(
        tag,
        ItemKey::MusicBrainzReleaseGroupId,
        &values.musicbrainz_release_group_ids,
        profile,
        "MusicBrainz release-group IDs",
    )?;
    put_many(
        tag,
        ItemKey::MusicBrainzArtistId,
        &values.musicbrainz_artist_ids,
        profile,
        "MusicBrainz artist IDs",
    )?;
    put_many(
        tag,
        ItemKey::MusicBrainzReleaseArtistId,
        &values.musicbrainz_album_artist_ids,
        profile,
        "MusicBrainz album-artist IDs",
    )?;
    put_many(
        tag,
        ItemKey::MusicBrainzWorkId,
        &values.musicbrainz_work_ids,
        profile,
        "MusicBrainz work IDs",
    )?;
    for (key, value, label) in [
        (
            ItemKey::ReplayGainTrackGain,
            values.replaygain_track_gain.as_deref(),
            "track gain",
        ),
        (
            ItemKey::ReplayGainTrackPeak,
            values.replaygain_track_peak.as_deref(),
            "track peak",
        ),
        (
            ItemKey::ReplayGainAlbumGain,
            values.replaygain_album_gain.as_deref(),
            "album gain",
        ),
        (
            ItemKey::ReplayGainAlbumPeak,
            values.replaygain_album_peak.as_deref(),
            "album peak",
        ),
    ] {
        put_optional(tag, key, value, label)?;
    }
    Ok(())
}

fn put_credit(
    tag: &mut Tag,
    display_key: ItemKey,
    values_key: ItemKey,
    display_value: &str,
    values: &[String],
    profile: TagProfile,
    label: &str,
) -> Result<()> {
    put_one(tag, display_key, display_value, label)?;
    if profile.keeps_native_multivalue() {
        put_repeated(tag, values_key, values, label)?;
    }
    Ok(())
}

fn put_many(
    tag: &mut Tag,
    key: ItemKey,
    values: &[String],
    profile: TagProfile,
    label: &str,
) -> Result<()> {
    if values.is_empty() {
        return Ok(());
    }
    if profile.keeps_native_multivalue() {
        put_repeated(tag, key, values, label)
    } else {
        put_one(tag, key, &values.join("; "), label)
    }
}

fn put_repeated(tag: &mut Tag, key: ItemKey, values: &[String], label: &str) -> Result<()> {
    // APEv2 represents multiple text values in one item separated by NUL;
    // duplicate keys are not permitted and Lofty's merge step would retain
    // only the final duplicate. Keep that native representation explicit.
    if tag.tag_type() == TagType::Ape {
        return put_one(tag, key, &values.join("\0"), label);
    }
    for value in values {
        if !tag.push(TagItem::new(key, ItemValue::Text(value.clone()))) {
            return Err(Error::Operation(format!(
                "tag dialect {:?} cannot represent {label}",
                tag.tag_type()
            )));
        }
    }
    Ok(())
}

fn put_optional(tag: &mut Tag, key: ItemKey, value: Option<&str>, label: &str) -> Result<()> {
    value.map_or(Ok(()), |value| put_one(tag, key, value, label))
}

fn put_number(tag: &mut Tag, key: ItemKey, value: Option<u32>, label: &str) -> Result<()> {
    value.map_or(Ok(()), |value| put_one(tag, key, &value.to_string(), label))
}

fn put_one(tag: &mut Tag, key: ItemKey, value: &str, label: &str) -> Result<()> {
    if tag.insert_text(key, value.to_owned()) {
        Ok(())
    } else {
        Err(Error::Operation(format!(
            "tag dialect {:?} cannot represent {label}",
            tag.tag_type()
        )))
    }
}

const fn projected_keys() -> &'static [ItemKey] {
    &[
        ItemKey::TrackTitle,
        ItemKey::AlbumTitle,
        ItemKey::TrackArtist,
        ItemKey::TrackArtists,
        ItemKey::AlbumArtist,
        ItemKey::AlbumArtists,
        ItemKey::Genre,
        ItemKey::RecordingDate,
        ItemKey::OriginalReleaseDate,
        ItemKey::TrackNumber,
        ItemKey::TrackTotal,
        ItemKey::DiscNumber,
        ItemKey::DiscTotal,
        ItemKey::Label,
        ItemKey::CatalogNumber,
        ItemKey::ReleaseCountry,
        ItemKey::OriginalMediaType,
        ItemKey::Barcode,
        ItemKey::Composer,
        ItemKey::Conductor,
        ItemKey::Lyricist,
        ItemKey::Performer,
        ItemKey::Work,
        ItemKey::Movement,
        ItemKey::MovementNumber,
        ItemKey::MovementTotal,
        ItemKey::MusicBrainzRecordingId,
        ItemKey::MusicBrainzTrackId,
        ItemKey::MusicBrainzReleaseId,
        ItemKey::MusicBrainzReleaseGroupId,
        ItemKey::MusicBrainzArtistId,
        ItemKey::MusicBrainzReleaseArtistId,
        ItemKey::MusicBrainzWorkId,
        ItemKey::ReplayGainTrackGain,
        ItemKey::ReplayGainTrackPeak,
        ItemKey::ReplayGainAlbumGain,
        ItemKey::ReplayGainAlbumPeak,
    ]
}

fn hash_part(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn hash_strings(values: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in values {
        hash_part(&mut hasher, value.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn validate_tag_values(values: &[String], label: &str) -> Result<()> {
    let mut unique = HashSet::new();
    for value in values {
        validate_tag_text(value, label)?;
        if !unique.insert(value) {
            return Err(Error::Operation(format!(
                "duplicate {label} value {value:?}"
            )));
        }
    }
    Ok(())
}

fn validate_tag_text(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 16 * 1024 || value.chars().any(char::is_control) {
        Err(Error::Operation(format!("invalid {label} tag value")))
    } else {
        Ok(())
    }
}

/// Read audio metadata tags from a file.
///
/// # Errors
/// Returns an error if the file cannot be read or probed for tags.
pub fn read_tags(path: &Path) -> Result<Item> {
    read_tags_with_title_fallback(path, None)
}

/// Read updated tags, retaining the catalog title when the file has no title tag.
pub fn read_tags_for_update(item: &Item) -> Result<Item> {
    read_tags_with_title_fallback(&item.path, Some(&item.title))
}

#[allow(clippy::too_many_lines)]
fn read_tags_with_title_fallback(path: &Path, fallback_title: Option<&str>) -> Result<Item> {
    let file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    let before_identity = file_identity(&before);
    // Always inspect content. The extension is an optional output hint, never
    // authority for the parser or stored media type.
    let probe = probe_native(BufReader::new(file))?;
    let tagged_file = probe.read()?;

    let properties = tagged_file.properties();
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let format = audio_format(path, tagged_file.file_type())?;

    let metadata = std::fs::metadata(path)?;
    if file_identity(&metadata) != before_identity {
        return Err(Error::Import(format!(
            "file changed while reading tags: {}",
            path.display()
        )));
    }
    let mtime = metadata.modified()?.into();
    let file_size = metadata.len();

    let (
        title,
        artist,
        album,
        albumartist,
        genre,
        year,
        track,
        disc,
        track_external_id,
        release_external_id,
    ) = tag.map_or_else(
        || {
            (
                String::new(),
                String::new(),
                String::new(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        },
        |tag| {
            (
                tag.title().map(|s| s.to_string()).unwrap_or_default(),
                tag.artist().map(|s| s.to_string()).unwrap_or_default(),
                tag.album().map(|s| s.to_string()).unwrap_or_default(),
                tag.get_string(lofty::tag::ItemKey::AlbumArtist)
                    .filter(|value| !value.trim().is_empty())
                    .map(String::from),
                tag.genre()
                    .filter(|value| !value.trim().is_empty())
                    .map(|s| s.to_string()),
                tag.date().map(|date| i32::from(date.year)),
                tag.track(),
                tag.disk(),
                tag.get_string(ItemKey::MusicBrainzRecordingId)
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| crate::ExternalId {
                        provider: "musicbrainz".into(),
                        kind: "recording".into(),
                        value: value.into(),
                    }),
                tag.get_string(ItemKey::MusicBrainzReleaseId)
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| crate::ExternalId {
                        provider: "musicbrainz".into(),
                        kind: "release".into(),
                        value: value.into(),
                    }),
            )
        },
    );
    let extended = tag.map_or_else(|| Ok(ExtendedMetadata::default()), read_extended_metadata)?;
    let year = extended
        .date
        .year
        .or_else(|| year.filter(|value| *value > 0));
    let track = tag.map_or(Ok(track), |tag| native_number(tag, ItemKey::TrackNumber))?;
    let disc = tag.map_or(Ok(disc), |tag| native_number(tag, ItemKey::DiscNumber))?;

    // Use filename as title if missing
    let title = if title.is_empty() {
        fallback_title
            .or_else(|| path.file_stem().and_then(|s| s.to_str()))
            .unwrap_or("Unknown")
            .to_string()
    } else {
        title
    };

    let artist = if artist.is_empty() {
        "Unknown Artist".to_string()
    } else {
        artist
    };

    let album = if album.is_empty() {
        "Unknown Album".to_string()
    } else {
        album
    };

    let item = Item {
        id: None,
        album_id: None,
        path: path.to_path_buf(),
        title,
        artist,
        album,
        albumartist,
        genre,
        year,
        track,
        disc,
        format,
        bitrate: properties.audio_bitrate().unwrap_or(0),
        length: properties.duration().as_secs_f64(),
        file_size: Some(file_size),
        track_external_id,
        release_external_id,
        added: Utc::now(),
        mtime,
        singleton: false,
        extended,
    };
    crate::write::canonical_tags(&item)?;
    Ok(item)
}

fn native_values(tag: &Tag, key: ItemKey) -> Result<Vec<String>> {
    let mut values = Vec::new();
    for item in tag.get_items(key) {
        let ItemValue::Text(text) = item.value() else {
            return Err(Error::Import(format!(
                "native field {key:?} must contain text"
            )));
        };
        if tag.tag_type() == TagType::Ape {
            values.extend(
                text.split('\0')
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned),
            );
        } else if !text.trim().is_empty() {
            values.push(text.clone());
        }
    }
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
    Ok(values)
}

fn native_one(tag: &Tag, key: ItemKey) -> Result<Option<String>> {
    let mut values = native_values(tag, key)?.into_iter();
    let first = values.next();
    if values.any(|value| Some(&value) != first.as_ref()) {
        return Err(Error::Import(format!(
            "native field {key:?} contains conflicting values"
        )));
    }
    Ok(first)
}

fn native_number(tag: &Tag, key: ItemKey) -> Result<Option<u32>> {
    let value = native_one(tag, key)?
        .map(|value| {
            value.parse::<u32>().map_err(|error| {
                Error::Import(format!(
                    "native field {key:?} must be a non-negative integer: {error}"
                ))
            })
        })
        .transpose()?;
    Ok(value.filter(|value| *value > 0))
}

fn native_date(tag: &Tag, key: ItemKey) -> Result<crate::PartialDate> {
    let Some(value) = native_one(tag, key)? else {
        return Ok(crate::PartialDate::default());
    };
    if matches!(value.as_str(), "" | "0" | "0000") {
        return Ok(crate::PartialDate::default());
    }
    crate::catalog::PartialDate::parse(&value)?;
    let timestamp = value.parse::<lofty::tag::items::Timestamp>()?;
    Ok(crate::PartialDate {
        year: Some(i32::from(timestamp.year)),
        month: timestamp.month,
        day: timestamp.day,
    })
}

fn read_extended_metadata(tag: &Tag) -> Result<ExtendedMetadata> {
    let mut artists = native_values(tag, ItemKey::TrackArtists)?;
    if artists.is_empty() {
        artists = native_values(tag, ItemKey::TrackArtist)?;
    }
    let mut album_artists = native_values(tag, ItemKey::AlbumArtists)?;
    if album_artists.is_empty() {
        album_artists = native_values(tag, ItemKey::AlbumArtist)?;
    }
    let labels = native_values(tag, ItemKey::Label)?;
    let catalogs = native_values(tag, ItemKey::CatalogNumber)?;
    let date_key = if tag.get_items(ItemKey::RecordingDate).next().is_some() {
        ItemKey::RecordingDate
    } else {
        ItemKey::Year
    };
    let mut metadata = ExtendedMetadata {
        date: native_date(tag, date_key)?,
        original_date: native_date(tag, ItemKey::OriginalReleaseDate)?,
        track_total: native_number(tag, ItemKey::TrackTotal)?,
        disc_total: native_number(tag, ItemKey::DiscTotal)?,
        label: labels.first().cloned(),
        catalog_number: catalogs.first().cloned(),
        country: native_one(tag, ItemKey::ReleaseCountry)?,
        media: native_one(tag, ItemKey::OriginalMediaType)?,
        language: native_one(tag, ItemKey::Language)?,
        artists,
        album_artists,
        genres: native_values(tag, ItemKey::Genre)?,
        composers: native_values(tag, ItemKey::Composer)?,
        external_ids: native_external_ids(tag)?,
        ..ExtendedMetadata::default()
    };
    if let Some(value) = native_one(tag, ItemKey::FlagCompilation)? {
        metadata.compilation = Some(match value.as_str() {
            "1" | "true" => true,
            "0" | "false" => false,
            _ => {
                return Err(Error::Import(
                    "native compilation flag must be boolean".into(),
                ))
            }
        });
    }
    for (name, values) in [
        ("labels", labels),
        ("catalog_numbers", catalogs),
        ("conductors", native_values(tag, ItemKey::Conductor)?),
        ("lyricists", native_values(tag, ItemKey::Lyricist)?),
        ("performers", native_values(tag, ItemKey::Performer)?),
    ] {
        if !values.is_empty() {
            metadata
                .flexible_fields
                .insert(name.into(), FlexibleValue::StringList(values));
        }
    }
    for (name, key) in [
        ("barcode", ItemKey::Barcode),
        ("work", ItemKey::Work),
        ("movement", ItemKey::Movement),
        ("replaygain_track_gain", ItemKey::ReplayGainTrackGain),
        ("replaygain_track_peak", ItemKey::ReplayGainTrackPeak),
        ("replaygain_album_gain", ItemKey::ReplayGainAlbumGain),
        ("replaygain_album_peak", ItemKey::ReplayGainAlbumPeak),
    ] {
        if let Some(value) = native_one(tag, key)? {
            metadata
                .flexible_fields
                .insert(name.into(), FlexibleValue::String(value));
        }
    }
    for (name, key) in [
        ("movement_number", ItemKey::MovementNumber),
        ("movement_total", ItemKey::MovementTotal),
    ] {
        if let Some(value) = native_number(tag, key)? {
            metadata
                .flexible_fields
                .insert(name.into(), FlexibleValue::Integer(i64::from(value)));
        }
    }
    Ok(metadata)
}

fn native_external_ids(tag: &Tag) -> Result<Vec<crate::ExternalId>> {
    let mut ids = Vec::new();
    for (kind, key) in [
        ("recording", ItemKey::MusicBrainzRecordingId),
        ("release-track", ItemKey::MusicBrainzTrackId),
        ("release", ItemKey::MusicBrainzReleaseId),
        ("release-group", ItemKey::MusicBrainzReleaseGroupId),
        ("artist", ItemKey::MusicBrainzArtistId),
        ("album-artist", ItemKey::MusicBrainzReleaseArtistId),
        ("work", ItemKey::MusicBrainzWorkId),
    ] {
        for value in native_values(tag, key)? {
            uuid::Uuid::parse_str(&value).map_err(|error| {
                Error::Import(format!("invalid native MusicBrainz {kind} ID: {error}"))
            })?;
            ids.push(crate::ExternalId::new_typed("musicbrainz", kind, value)?);
        }
    }
    Ok(ids)
}

#[must_use]
pub fn is_audio_file(path: &Path) -> bool {
    std::fs::File::open(path)
        .ok()
        .and_then(|file| probe_native(BufReader::new(file)).ok())
        .and_then(|probe| probe.file_type())
        .is_some()
}

fn audio_format(path: &Path, file_type: FileType) -> Result<AudioFormat> {
    Ok(match file_type {
        FileType::Aac => AudioFormat::Aac,
        FileType::Aiff => AudioFormat::Aiff,
        FileType::Ape => AudioFormat::Ape,
        FileType::Flac => AudioFormat::Flac,
        FileType::Mpeg => AudioFormat::Mp3,
        FileType::Mp4 => mp4_audio_format(path)?,
        FileType::Mpc => AudioFormat::Musepack,
        FileType::Opus => AudioFormat::Opus,
        FileType::Vorbis => AudioFormat::Ogg,
        FileType::Speex => AudioFormat::Speex,
        FileType::Wav => AudioFormat::Wav,
        FileType::WavPack => AudioFormat::WavPack,
        _ => AudioFormat::Unknown,
    })
}

fn mp4_audio_format(path: &Path) -> Result<AudioFormat> {
    let mut file = std::fs::File::open(path)?;
    let mp4 = Mp4File::read_from(&mut file, ParseOptions::new())?;
    Ok(match mp4.properties().codec() {
        Mp4Codec::ALAC => AudioFormat::Alac,
        Mp4Codec::AAC => AudioFormat::Aac,
        _ => AudioFormat::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
    use lofty::ape::ApeItem;
    use lofty::picture::{MimeType, Picture, PictureType};

    use super::*;
    use crate::media::{
        decoded_audio_essence_hash, format_capabilities, probe_media, AudioCodec, Container,
        TagDialect,
    };

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

    fn canonical_test_tags() -> Result<CanonicalTags> {
        CanonicalTags::new(
            "Title",
            vec!["Artist".into()],
            "Album",
            vec!["Album Artist".into()],
        )?
        .with_positions(Some((1, Some(2))), Some((1, Some(2))))?
        .with_dates(Some("2024-02-29".into()), Some("1980".into()))?
        .with_release_facts(
            vec!["Label".into()],
            vec!["Catalog".into()],
            Some("GB".into()),
            Some("CD".into()),
            Some("123456789".into()),
        )?
        .with_genres(vec!["Rock".into()])?
        .with_classical(
            vec!["Composer".into()],
            vec!["Conductor".into()],
            vec!["Lyricist".into()],
            vec!["Performer".into()],
            Some("Work".into()),
            Some("Movement".into()),
            Some((1, Some(2))),
        )?
        .with_musicbrainz_ids(
            vec!["550e8400-e29b-41d4-a716-446655440001".into()],
            vec!["550e8400-e29b-41d4-a716-446655440002".into()],
            vec!["550e8400-e29b-41d4-a716-446655440003".into()],
            vec!["550e8400-e29b-41d4-a716-446655440004".into()],
            vec!["550e8400-e29b-41d4-a716-446655440005".into()],
            vec!["550e8400-e29b-41d4-a716-446655440006".into()],
            vec!["550e8400-e29b-41d4-a716-446655440007".into()],
        )?
        .with_replaygain(
            Some("-3.0 dB".into()),
            Some("0.9".into()),
            Some("-4.0 dB".into()),
            Some("0.8".into()),
        )
    }

    #[test]
    fn canonical_tag_deserialization_enforces_basic_validation() -> Result<()> {
        let tags = canonical_test_tags()?;
        let valid = serde_json::to_value(&tags)?;
        assert_eq!(
            serde_json::from_value::<CanonicalTags>(valid.clone())?,
            tags
        );
        for (field, invalid) in [
            ("title", serde_json::json!("")),
            ("track_artists", serde_json::json!([])),
            ("genres", serde_json::json!(["Rock", "Rock"])),
        ] {
            let mut value = valid.clone();
            value[field] = invalid;
            assert!(
                serde_json::from_value::<CanonicalTags>(value).is_err(),
                "constructor-invalid {field} passed JSON validation"
            );
        }
        Ok(())
    }

    #[test]
    fn canonical_tag_validation_enforces_builder_invariants() -> Result<()> {
        let valid = canonical_test_tags()?;
        let mut invalid = valid.clone();
        invalid.recording_date = Some("2024-02-30".into());
        assert!(
            invalid.validate().is_err(),
            "invalid date passed validation"
        );
        let mut invalid = valid.clone();
        invalid.original_release_date = Some("+123".into());
        assert!(invalid.validate().is_err());
        for position in ["track", "disc", "movement"] {
            let mut invalid = valid.clone();
            match position {
                "track" => invalid.track_total = Some(0),
                "disc" => invalid.disc_total = Some(0),
                _ => invalid.movement_total = Some(0),
            }
            assert!(
                invalid.validate().is_err(),
                "invalid {position} total passed validation"
            );
        }
        let mut invalid = valid.clone();
        invalid.musicbrainz_recording_ids = vec!["not-a-uuid".into()];
        assert!(
            invalid.validate().is_err(),
            "invalid MusicBrainz ID passed validation"
        );
        let mut invalid = valid;
        invalid.replaygain_track_gain = Some("bad\nvalue".into());
        assert!(
            invalid.validate().is_err(),
            "invalid loudness value passed validation"
        );
        Ok(())
    }

    #[test]
    fn canonical_tag_totals_must_be_positive_when_positions_are_unknown() -> Result<()> {
        for position in ["track", "disc", "movement"] {
            let mut tags = canonical_test_tags()?;
            match position {
                "track" => {
                    tags.track_number = None;
                    tags.track_total = Some(2);
                }
                "disc" => {
                    tags.disc_number = None;
                    tags.disc_total = Some(2);
                }
                _ => {
                    tags.movement_number = None;
                    tags.movement_total = Some(2);
                }
            }
            tags.validate()?;
            assert_eq!(
                serde_json::from_value::<CanonicalTags>(serde_json::to_value(&tags)?)?,
                tags
            );
            match position {
                "track" => tags.track_total = Some(0),
                "disc" => tags.disc_total = Some(0),
                _ => tags.movement_total = Some(0),
            }
            assert!(
                tags.validate().is_err(),
                "zero {position} total passed validation with an unknown position"
            );
            assert!(serde_json::from_value::<CanonicalTags>(serde_json::to_value(tags)?).is_err());
        }
        Ok(())
    }

    fn add_golden_noncanonical_metadata(path: &Path) -> Result<TagSnapshot> {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 2, Rgb([1, 2, 3])));
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, ImageFormat::Png)?;
        let picture = Picture::unchecked(encoded.into_inner())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .description("rsbts golden front")
            .build();

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let reader = file.try_clone()?;
        let mut tagged = Probe::new(BufReader::new(reader))
            .guess_file_type()?
            .read()?;
        let primary_type = tagged.primary_tag_type();
        if tagged.primary_tag().is_none() {
            tagged.insert_tag(Tag::new(primary_type));
        }
        let tag = tagged
            .primary_tag_mut()
            .ok_or_else(|| Error::Operation("golden file has no writable tag".into()))?;
        if !tag.insert_text(ItemKey::Comment, "rsbts-golden-unknown".into()) {
            return Err(Error::Operation(format!(
                "{:?} cannot store the golden unknown tag",
                tag.tag_type()
            )));
        }
        tag.push_picture(picture.clone());
        let file_type = tagged.file_type();
        file.seek(SeekFrom::Start(0))?;
        tagged.save_to(&mut file, WriteOptions::new().remove_others(false))?;
        file.sync_all()?;
        drop(file);

        if matches!(file_type, FileType::Ape | FileType::Mpc | FileType::WavPack) {
            let item = || {
                ApeItem::new(
                    "Cover Art (Front)".into(),
                    ItemValue::Binary(picture.as_ape_bytes()),
                )
            };
            macro_rules! add_ape_picture {
                ($file_ty:path) => {{
                    let mut file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(path)?;
                    let mut media =
                        <$file_ty as AudioFile>::read_from(&mut file, ParseOptions::new())?;
                    let mut native = media.remove_ape().unwrap_or_default();
                    native.insert(item()?);
                    media.set_ape(native);
                    file.seek(SeekFrom::Start(0))?;
                    media.save_to(&mut file, WriteOptions::new().remove_others(false))?;
                    file.sync_all()?;
                }};
            }
            match file_type {
                FileType::Ape => add_ape_picture!(ApeFile),
                FileType::Mpc => add_ape_picture!(MpcFile),
                FileType::WavPack => add_ape_picture!(WavPackFile),
                _ => {}
            }
        }

        let snapshot = read_tag_snapshot(path)?;
        if snapshot.unprojected_count() == 0 || snapshot.picture_count() == 0 {
            return Err(Error::Operation(format!(
                "golden setup persisted {} unknown items and {} pictures for {}",
                snapshot.unprojected_count(),
                snapshot.picture_count(),
                path.display(),
            )));
        }
        Ok(snapshot)
    }

    fn golden_tag_debug(path: &Path) -> Result<Vec<String>> {
        let tagged = Probe::new(BufReader::new(std::fs::File::open(path)?))
            .guess_file_type()?
            .read()?;
        Ok(tagged
            .tags()
            .iter()
            .flat_map(|tag| {
                tag.items().map(move |item| {
                    format!(
                        "{:?}:{:?}:{:?}:{:?}",
                        tag.tag_type(),
                        item.key(),
                        item.description(),
                        item.value()
                    )
                })
            })
            .collect())
    }

    #[test]
    fn reads_an_untagged_wav_with_safe_fallbacks() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("Fallback Title.wav");
        let bytes = minimal_wav();
        std::fs::write(&path, &bytes)?;

        let item = read_tags(&path)?;

        assert_eq!(item.title, "Fallback Title");
        assert_eq!(item.artist, "Unknown Artist");
        assert_eq!(item.album, "Unknown Album");
        assert_eq!(item.format, AudioFormat::Wav);
        assert_eq!(item.file_size, u64::try_from(bytes.len()).ok());
        Ok(())
    }

    #[test]
    fn content_probe_ignores_a_misleading_extension() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("audio.mp3");
        std::fs::write(&path, minimal_wav())?;

        let item = read_tags(&path)?;

        assert_eq!(item.format, AudioFormat::Wav);
        assert!(is_audio_file(&path));
        Ok(())
    }

    #[test]
    fn read_tags_retains_supported_extended_metadata() -> Result<()> {
        for name in ["flac.flac", "wavpack.wv", "aac.m4a"] {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join(name);
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/formats")
                    .join(name),
                &path,
            )?;
            let mut tags = canonical_test_tags()?;
            tags.track_artists.push("Second artist".into());
            tags.album_artists.push("Second album artist".into());
            tags.labels.push("Second label".into());
            tags.catalog_numbers.push("Second catalog".into());
            if name == "aac.m4a" {
                tags.performers.clear();
            }
            if name == "wavpack.wv" {
                tags.movement_total = None;
            }
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)?;
            rewrite_tags(&mut file, &path, &tags, TagProfile::ArchivalNativeRich)?;
            drop(file);
            let item = read_tags(&path).map_err(|error| {
                Error::Import(format!(
                    "{name}: {error}; tags: {:?}",
                    golden_tag_debug(&path)
                ))
            })?;
            assert_eq!(
                item.extended.date,
                crate::PartialDate {
                    year: Some(2024),
                    month: Some(2),
                    day: Some(29)
                },
                "{name} lost recording-date precision"
            );
            assert_eq!(item.extended.original_date.year, Some(1980));
            assert_eq!(item.extended.original_date.month, None);
            assert_eq!(item.extended.track_total, Some(2));
            assert_eq!(item.extended.disc_total, Some(2));
            assert_eq!(item.extended.artists, tags.track_artists);
            assert_eq!(item.extended.album_artists, tags.album_artists);
            assert_eq!(item.extended.genres, tags.genres);
            assert_eq!(item.extended.composers, tags.composers);
            assert_eq!(item.extended.label.as_deref(), Some("Label"));
            assert_eq!(item.extended.catalog_number.as_deref(), Some("Catalog"));
            assert_eq!(item.extended.country.as_deref(), Some("GB"));
            assert_eq!(item.extended.media.as_deref(), Some("CD"));
            assert_eq!(item.extended.external_ids.len(), 7);
            let projected = crate::write::canonical_tags(&item)?;
            assert_eq!(projected, tags, "{name} lost supported projection values");
        }
        Ok(())
    }

    #[test]
    fn read_tags_rejects_malformed_native_typed_values() -> Result<()> {
        let mut accepted = Vec::new();
        for (key, value) in [
            (ItemKey::RecordingDate, "2024-02-30"),
            (ItemKey::OriginalReleaseDate, "+123"),
            (ItemKey::TrackTotal, "not-a-number"),
            (ItemKey::DiscTotal, "-1"),
            (ItemKey::MovementNumber, "-1"),
            (ItemKey::MusicBrainzWorkId, "not-a-uuid"),
        ] {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join("invalid.flac");
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats/flac.flac"),
                &path,
            )?;
            let mut media = Probe::open(&path)?.read()?;
            let mut tag = Tag::new(TagType::VorbisComments);
            assert!(tag.insert_text(key, value.into()));
            media.insert_tag(tag);
            media.save_to_path(&path, WriteOptions::new())?;
            if read_tags(&path).is_ok() {
                accepted.push(format!("{key:?}={value:?}"));
            }
        }
        assert!(
            accepted.is_empty(),
            "malformed native values passed: {accepted:?}"
        );
        Ok(())
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one golden matrix test demonstrates every advertised container/codec/tag/profile tuple"
    )]
    fn every_advertised_tuple_has_a_golden_native_round_trip() -> Result<()> {
        let fixtures = [
            (
                "flac.flac",
                Container::Flac,
                AudioCodec::Flac,
                TagDialect::VorbisComments,
            ),
            (
                "mp3.mp3",
                Container::Mpeg,
                AudioCodec::Mp3,
                TagDialect::Id3v2,
            ),
            (
                "vorbis.ogg",
                Container::Ogg,
                AudioCodec::Vorbis,
                TagDialect::VorbisComments,
            ),
            (
                "opus.opus",
                Container::Ogg,
                AudioCodec::Opus,
                TagDialect::VorbisComments,
            ),
            (
                "speex.spx",
                Container::Ogg,
                AudioCodec::Speex,
                TagDialect::VorbisComments,
            ),
            (
                "aac.m4a",
                Container::Mp4,
                AudioCodec::Aac,
                TagDialect::Mp4Ilst,
            ),
            (
                "alac.m4a",
                Container::Mp4,
                AudioCodec::Alac,
                TagDialect::Mp4Ilst,
            ),
            (
                "aac.aac",
                Container::Adts,
                AudioCodec::Aac,
                TagDialect::Id3v2,
            ),
            (
                "pcm.wav",
                Container::Wave,
                AudioCodec::Pcm,
                TagDialect::Id3v2,
            ),
            (
                "pcm.aiff",
                Container::Aiff,
                AudioCodec::Pcm,
                TagDialect::Id3v2,
            ),
            (
                "wavpack.wv",
                Container::WavPack,
                AudioCodec::WavPack,
                TagDialect::ApeV2,
            ),
            (
                "monkey.ape",
                Container::Ape,
                AudioCodec::MonkeyAudio,
                TagDialect::ApeV2,
            ),
            (
                "musepack.mpc",
                Container::Musepack,
                AudioCodec::Musepack,
                TagDialect::ApeV2,
            ),
        ];
        assert_eq!(fixtures.len(), format_capabilities().len());
        let profiles = [
            TagProfile::ArchivalNativeRich,
            TagProfile::PicardNavidrome,
            TagProfile::Id3v23Legacy,
            TagProfile::PortablePlayer,
        ];
        for (name, container, codec, dialect) in fixtures {
            for profile in profiles {
                let temporary = tempfile::tempdir()?;
                let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/formats")
                    .join(name);
                let path = temporary.path().join(name);
                std::fs::copy(source, &path)?;
                let before_tags = add_golden_noncanonical_metadata(&path)
                    .map_err(|error| Error::Operation(format!("{name}: {error}")))?;
                let before_debug = golden_tag_debug(&path)?;
                let before_essence = decoded_audio_essence_hash(&path)
                    .map_err(|error| Error::Media(format!("{name}: {error}")))?;
                let tags = CanonicalTags::new(
                    "Golden title",
                    vec!["Artist One".into(), "Artist Two".into()],
                    "Golden album",
                    vec!["Album Artist One".into(), "Album Artist Two".into()],
                )?
                .with_genres(vec!["Rock".into(), "Metal".into()])?
                .with_positions(Some((1, Some(2))), Some((1, Some(1))))?;
                let mut file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)?;
                rewrite_tags(&mut file, &path, &tags, profile)?;
                drop(file);
                let after_tags = read_tag_snapshot(&path)?;
                validate_materialized_snapshot(&after_tags, &tags, profile).map_err(|error| {
                    Error::Operation(format!("{name} {}: {error}", profile.as_str()))
                })?;
                assert_eq!(
                    before_tags.unprojected_digest(),
                    after_tags.unprojected_digest(),
                    "{name} {} unknown metadata; before={before_debug:?}; after={:?}",
                    profile.as_str(),
                    golden_tag_debug(&path)?,
                );
                assert_eq!(
                    before_tags.picture_digest(),
                    after_tags.picture_digest(),
                    "{name} {} pictures",
                    profile.as_str()
                );
                assert_eq!(
                    before_essence,
                    decoded_audio_essence_hash(&path)?,
                    "{name} {} audio essence",
                    profile.as_str()
                );
                let media = probe_media(&path)?;
                assert_eq!(media.container(), container, "{name}");
                assert_eq!(media.codec(), codec, "{name}");
                assert_eq!(media.tag_dialect(), dialect, "{name}");
            }
        }
        Ok(())
    }
}
