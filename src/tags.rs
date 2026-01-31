use std::path::Path;

use chrono::Utc;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::Accessor;

use crate::{AudioFormat, Item, Result};

/// Read audio metadata tags from a file.
///
/// # Errors
/// Returns an error if the file cannot be read or probed for tags.
pub fn read_tags(path: &Path) -> Result<Item> {
    let tagged_file = Probe::open(path)?.read()?;

    let properties = tagged_file.properties();
    let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag());

    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .map_or(AudioFormat::Unknown, AudioFormat::from_extension);

    let mtime = std::fs::metadata(path)?.modified()?.into();

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
                tag.get_string(&lofty::tag::ItemKey::AlbumArtist).map(String::from),
                tag.genre().map(|s| s.to_string()),
                tag.year(),
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

    #[allow(clippy::cast_possible_wrap)]
    let year = year.map(|y| y as i32);

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
        mb_trackid: None,
        mb_albumid: None,
        added: Utc::now(),
        mtime,
    })
}

#[must_use]
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_lowercase().as_str(),
                "mp3" | "flac" | "ogg" | "oga" | "opus" | "m4a" | "aac" | "wav" | "aiff" | "aif"
            )
        })
}
