use std::io::BufReader;
use std::path::Path;

use chrono::Utc;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::Accessor;

use crate::db::file_identity;
use crate::{AudioFormat, Error, Item, Result};

/// Read audio metadata tags from a file.
///
/// # Errors
/// Returns an error if the file cannot be read or probed for tags.
pub fn read_tags(path: &Path) -> Result<Item> {
    let file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    let before_identity = file_identity(&before);
    let probe = Probe::new(BufReader::new(file));
    let probe = if let Some(file_type) = FileType::from_path(path) {
        probe.set_file_type(file_type)
    } else {
        probe.guess_file_type()?
    };
    let tagged_file = probe.read()?;

    let properties = tagged_file.properties();
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .map_or(AudioFormat::Unknown, AudioFormat::from_extension);

    let metadata = std::fs::metadata(path)?;
    if file_identity(&metadata) != before_identity {
        return Err(Error::Import(format!(
            "file changed while reading tags: {}",
            path.display()
        )));
    }
    let mtime = metadata.modified()?.into();
    let file_size = metadata.len();

    let (title, artist, album, albumartist, genre, year, track, disc) = tag.map_or_else(
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
            )
        },
        |tag| {
            (
                tag.title().map(|s| s.to_string()).unwrap_or_default(),
                tag.artist().map(|s| s.to_string()).unwrap_or_default(),
                tag.album().map(|s| s.to_string()).unwrap_or_default(),
                tag.get_string(lofty::tag::ItemKey::AlbumArtist)
                    .map(String::from),
                tag.genre().map(|s| s.to_string()),
                tag.date().map(|date| i32::from(date.year)),
                tag.track(),
                tag.disk(),
            )
        },
    );

    // Use filename as title if missing
    let title = if title.is_empty() {
        path.file_stem()
            .and_then(|s| s.to_str())
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

    Ok(Item {
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
        track_external_id: None,
        release_external_id: None,
        added: Utc::now(),
        mtime,
        singleton: false,
        extended: crate::ExtendedMetadata::default(),
    })
}

#[must_use]
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_lowercase().as_str(),
                "mp3"
                    | "flac"
                    | "ogg"
                    | "oga"
                    | "opus"
                    | "m4a"
                    | "aac"
                    | "alac"
                    | "wav"
                    | "aiff"
                    | "aif"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(item.file_size, Some(bytes.len() as u64));
        Ok(())
    }
}
