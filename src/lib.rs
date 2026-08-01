//! Safe, plan-first music library management.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

pub mod config;
pub mod db;
pub mod import;
pub mod migrations;
pub mod musicbrainz;
pub mod pathformat;
pub mod provider;
pub mod query;
pub mod remove;
pub mod tags;

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Audio container/codec family recorded for a library item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioFormat {
    Mp3,
    Flac,
    Ogg,
    Opus,
    Aac,
    Alac,
    Wav,
    Aiff,
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
            Self::Unknown => "Unknown",
        }
    }
}

/// Provider-neutral external metadata identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalId {
    pub provider: String,
    pub value: String,
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
}

pub(crate) fn validate_album_metadata(album: &Album) -> Result<()> {
    if album.album.trim().is_empty() || album.albumartist.trim().is_empty() {
        return Err(Error::Import(
            "album name and album artist cannot be empty".into(),
        ));
    }
    validate_external_id(album.external_id.as_ref())
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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Tag error: {0}")]
    Tag(#[from] lofty::error::LoftyError),
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
        ] {
            assert_eq!(AudioFormat::from_storage(format.as_str()), format);
        }
    }
}
