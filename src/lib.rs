//! Safe, plan-first music library management.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

pub mod ancillary;
pub mod artwork;
pub mod artwork_projection;
pub mod asset;
pub mod catalog;
pub mod config;
pub mod db;
mod failpoints;
pub mod fixity;
mod fsops;
pub mod import;
mod lease;
pub mod matching_eval;
pub mod media;
pub mod migrations;
pub mod move_files;
pub mod musicbrainz;
pub mod naming;
pub mod operations;
pub mod path_projection;
pub mod pathformat;
pub mod preservation;
pub mod provider;
pub mod provider_policy;
pub mod query;
pub mod remove;
pub mod roots;
pub mod tag_projection;
pub mod tags;
pub mod write;

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Audio container/codec family recorded for a library item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AudioFormat {
    Mp3,
    Flac,
    Ogg,
    Opus,
    Aac,
    Alac,
    Wav,
    Aiff,
    WavPack,
    Ape,
    Musepack,
    Speex,
    Unknown,
}

impl AudioFormat {
    #[must_use]
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "mp3" => Self::Mp3,
            "flac" => Self::Flac,
            "ogg" | "oga" => Self::Ogg,
            "opus" => Self::Opus,
            "m4a" | "aac" => Self::Aac,
            "alac" => Self::Alac,
            "wav" => Self::Wav,
            "aiff" | "aif" => Self::Aiff,
            "wv" => Self::WavPack,
            "ape" => Self::Ape,
            "mpc" | "mp+" | "mpp" => Self::Musepack,
            "spx" | "speex" => Self::Speex,
            _ => Self::Unknown,
        }
    }

    /// Parse the stable database representation.
    #[must_use]
    pub fn from_storage(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "mp3" => Self::Mp3,
            "flac" => Self::Flac,
            "ogg" | "ogg vorbis" => Self::Ogg,
            "opus" => Self::Opus,
            "aac" => Self::Aac,
            "alac" => Self::Alac,
            "wav" => Self::Wav,
            "aiff" => Self::Aiff,
            "wavpack" => Self::WavPack,
            "ape" => Self::Ape,
            "musepack" => Self::Musepack,
            "speex" => Self::Speex,
            _ => Self::Unknown,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mp3 => "MP3",
            Self::Flac => "FLAC",
            Self::Ogg => "Ogg",
            Self::Opus => "Opus",
            Self::Aac => "AAC",
            Self::Alac => "ALAC",
            Self::Wav => "WAV",
            Self::Aiff => "AIFF",
            Self::WavPack => "WavPack",
            Self::Ape => "APE",
            Self::Musepack => "Musepack",
            Self::Speex => "Speex",
            Self::Unknown => "Unknown",
        }
    }
}

/// Provider-neutral external metadata identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalId {
    pub(crate) provider: String,
    pub(crate) value: String,
}

impl<'de> Deserialize<'de> for ExternalId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            provider: String,
            value: String,
        }

        let value = Wire::deserialize(deserializer)?;
        Self::new(value.provider, value.value).map_err(serde::de::Error::custom)
    }
}

impl ExternalId {
    pub fn new(provider: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let provider = provider.into();
        let value = value.into();
        if provider.trim().is_empty()
            || value.trim().is_empty()
            || provider.len() > 128
            || value.len() > 512
            || provider.chars().any(char::is_control)
            || value.chars().any(char::is_control)
        {
            return Err(Error::Import(
                "external ID provider and value must be non-empty, bounded, and control-free"
                    .into(),
            ));
        }
        Ok(Self { provider, value })
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A calendar date whose month and day may be unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialDate {
    pub year: Option<i32>,
    pub month: Option<u8>,
    pub day: Option<u8>,
}

/// A typed value preserved from a migrated library or a built-in metadata provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FlexibleValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Date(PartialDate),
    StringList(Vec<String>),
}

/// Metadata that extends the compact fields used by the common CLI path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtendedMetadata {
    pub date: PartialDate,
    pub original_date: PartialDate,
    pub track_total: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: Option<bool>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub country: Option<String>,
    pub media: Option<String>,
    pub language: Option<String>,
    pub artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub genres: Vec<String>,
    pub composers: Vec<String>,
    pub external_ids: Vec<ExternalId>,
    pub flexible_fields: BTreeMap<String, FlexibleValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: Option<i64>,
    pub album_id: Option<i64>,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub albumartist: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub format: AudioFormat,
    pub bitrate: u32,
    pub length: f64,
    pub file_size: Option<u64>,
    pub track_external_id: Option<ExternalId>,
    pub release_external_id: Option<ExternalId>,
    pub added: DateTime<Utc>,
    pub mtime: DateTime<Utc>,
    /// True when the item is intentionally not associated with an album.
    #[serde(default)]
    pub singleton: bool,
    #[serde(default)]
    pub extended: ExtendedMetadata,
}

impl Item {
    #[must_use]
    pub fn effective_albumartist(&self) -> &str {
        self.albumartist.as_deref().unwrap_or(&self.artist)
    }
}

pub(crate) fn validate_item_metadata(item: &Item) -> Result<()> {
    for (field, value) in [
        ("title", item.title.as_str()),
        ("artist", item.artist.as_str()),
        ("album", item.album.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(Error::Import(format!("item {field} cannot be empty")));
        }
    }
    if !item.length.is_finite() || item.length < 0.0 {
        return Err(Error::Import(
            "item length must be a finite, non-negative number".into(),
        ));
    }
    validate_external_id(item.track_external_id.as_ref())?;
    validate_external_id(item.release_external_id.as_ref())?;
    validate_extended_metadata(&item.extended)?;
    if let (Some(track), Some(release)) = (
        item.track_external_id.as_ref(),
        item.release_external_id.as_ref(),
    ) {
        if track.provider != release.provider {
            return Err(Error::Import(
                "track and release external IDs must use the same provider".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: Option<i64>,
    pub album: String,
    pub albumartist: String,
    pub year: Option<i32>,
    pub artpath: Option<PathBuf>,
    pub external_id: Option<ExternalId>,
    pub added: DateTime<Utc>,
    #[serde(default)]
    pub extended: ExtendedMetadata,
}

pub(crate) fn validate_album_metadata(album: &Album) -> Result<()> {
    if album.album.trim().is_empty() || album.albumartist.trim().is_empty() {
        return Err(Error::Import(
            "album name and album artist cannot be empty".into(),
        ));
    }
    validate_external_id(album.external_id.as_ref())?;
    validate_extended_metadata(&album.extended)
}

fn validate_external_id(external_id: Option<&ExternalId>) -> Result<()> {
    if external_id.is_some_and(|id| id.provider.trim().is_empty() || id.value.trim().is_empty()) {
        Err(Error::Import(
            "external ID provider and value cannot be empty".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_extended_metadata(metadata: &ExtendedMetadata) -> Result<()> {
    validate_partial_date(&metadata.date)?;
    validate_partial_date(&metadata.original_date)?;
    for id in &metadata.external_ids {
        validate_external_id(Some(id))?;
    }
    for (field, value) in &metadata.flexible_fields {
        if field.is_empty()
            || field == "__core"
            || !field
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(Error::Import(format!(
                "invalid flexible field name: {field}"
            )));
        }
        match value {
            FlexibleValue::Float(value) if !value.is_finite() => {
                return Err(Error::Import(format!(
                    "flexible field {field} must be finite"
                )))
            }
            FlexibleValue::Date(value) => validate_partial_date(value)?,
            FlexibleValue::String(_)
            | FlexibleValue::Integer(_)
            | FlexibleValue::Float(_)
            | FlexibleValue::Boolean(_)
            | FlexibleValue::StringList(_) => {}
        }
    }
    Ok(())
}

fn validate_partial_date(date: &PartialDate) -> Result<()> {
    if date.month.is_some_and(|month| !(1..=12).contains(&month))
        || date.day.is_some_and(|day| !(1..=31).contains(&day))
        || date.day.is_some() && date.month.is_none()
        || date.month.is_some() && date.year.is_none()
    {
        return Err(Error::Import("partial date is invalid".into()));
    }
    if let (Some(year), Some(month), Some(day)) = (date.year, date.month, date.day) {
        if chrono::NaiveDate::from_ymd_opt(year, u32::from(month), u32::from(day)).is_none() {
            return Err(Error::Import("partial date is invalid".into()));
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Database error: {0}")]
    Database(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Tag error: {0}")]
    Tag(String),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Import error: {0}")]
    Import(String),
    #[error("Metadata provider error: {0}")]
    Provider(String),
    #[error("Path format error: {0}")]
    PathFormat(String),
    #[error("Query error: {0}")]
    Query(String),
    #[error("Recovery required: {0}")]
    Recovery(String),
    #[error("Collection lease error: {0}")]
    Lease(String),
    #[error("Catalog error: {0}")]
    Catalog(String),
    #[error("JSON error: {0}")]
    Json(String),
    #[error("Media error: {0}")]
    Media(String),
    #[error("Library root error: {0}")]
    Root(String),
    #[error("Operation state error: {0}")]
    Operation(String),
    #[error("Artwork error: {0}")]
    Artwork(String),
    #[error("Image decoding error: {0}")]
    Image(String),
    #[error("Preservation error: {0}")]
    Preservation(String),
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<lofty::error::LoftyError> for Error {
    fn from(error: lofty::error::LoftyError) -> Self {
        Self::Tag(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

impl From<image::ImageError> for Error {
    fn from(error: image::ImageError) -> Self {
        Self::Image(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::AudioFormat;

    #[test]
    fn audio_format_storage_round_trip() {
        for format in [
            AudioFormat::Mp3,
            AudioFormat::Flac,
            AudioFormat::Ogg,
            AudioFormat::Opus,
            AudioFormat::Aac,
            AudioFormat::Alac,
            AudioFormat::Wav,
            AudioFormat::Aiff,
            AudioFormat::WavPack,
            AudioFormat::Ape,
            AudioFormat::Musepack,
            AudioFormat::Speex,
        ] {
            assert_eq!(AudioFormat::from_storage(format.as_str()), format);
        }
    }

    #[test]
    fn invariant_newtypes_validate_deserialization_boundaries() {
        use crate::catalog::{Confidence, EntityId, PartialDate};
        use crate::fixity::FixityScheduleId;
        use crate::operations::PlanId;
        use crate::provider_policy::{AcousticFingerprint, CommunityRating};
        use crate::roots::RootId;
        use crate::ExternalId;

        assert!(serde_json::from_str::<EntityId>(r#""not-a-uuid""#).is_err());
        assert!(serde_json::from_str::<PlanId>(r#""not-a-uuid""#).is_err());
        assert!(serde_json::from_str::<RootId>(r#""not-a-uuid""#).is_err());
        assert!(serde_json::from_str::<FixityScheduleId>(r#""not-a-uuid""#).is_err());
        assert!(serde_json::from_str::<Confidence>("1.01").is_err());
        assert!(serde_json::from_str::<CommunityRating>("5.01").is_err());
        assert!(serde_json::from_str::<PartialDate>(r#""2026-99""#).is_err());
        assert!(serde_json::from_str::<AcousticFingerprint>(r#""""#).is_err());
        assert!(
            serde_json::from_str::<ExternalId>(r#"{"provider":"", "value":"release"}"#).is_err()
        );
    }
}
