#![no_main]

use chrono::Utc;
use libfuzzer_sys::fuzz_target;
use rsbts::pathformat::format_relative_path;
use rsbts::{AudioFormat, Item};

fuzz_target!(|data: &[u8]| {
    if let Ok(template) = std::str::from_utf8(data) {
        let item = Item {
            id: None,
            album_id: None,
            path: "fuzz.wav".into(),
            title: template.chars().take(64).collect(),
            artist: "Artist".into(),
            album: "Album".into(),
            albumartist: None,
            genre: None,
            year: None,
            track: Some(1),
            disc: Some(1),
            format: AudioFormat::Wav,
            bitrate: 0,
            length: 0.0,
            file_size: None,
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
        };
        let _path = format_relative_path(template, &item);
    }
});
