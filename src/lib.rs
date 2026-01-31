pub mod config;
pub mod db;
pub mod import;
pub mod musicbrainz;
pub mod pathformat;
pub mod query;
pub mod tags;

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
        match ext.to_lowercase().as_str() {
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

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Mp3 => "MP3",
            Self::Flac => "FLAC",
            Self::Ogg => "Ogg Vorbis",
            Self::Opus => "Opus",
            Self::Aac => "AAC",
            Self::Alac => "ALAC",
            Self::Wav => "WAV",
            Self::Aiff => "AIFF",
            Self::Unknown => "Unknown",
        }
    }
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
    pub mb_trackid: Option<String>,
    pub mb_albumid: Option<String>,
    pub added: DateTime<Utc>,
    pub mtime: DateTime<Utc>,
}

impl Item {
    #[must_use]
    pub fn effective_albumartist(&self) -> &str {
        self.albumartist.as_deref().unwrap_or(&self.artist)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: Option<i64>,
    pub album: String,
    pub albumartist: String,
    pub year: Option<i32>,
    pub artpath: Option<PathBuf>,
    pub mb_albumid: Option<String>,
    pub added: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Tag error: {0}")]
    Tag(#[from] lofty::error::LoftyError),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Import error: {0}")]
    Import(String),

    #[error("MusicBrainz error: {0}")]
    MusicBrainz(String),

    #[error("Path format error: {0}")]
    PathFormat(String),

    #[error("Query error: {0}")]
    Query(String),
}

pub type Result<T> = std::result::Result<T, Error>;
