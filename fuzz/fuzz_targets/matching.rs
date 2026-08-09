#![no_main]

use chrono::Utc;
use libfuzzer_sys::fuzz_target;
use rsbts::import::assign_release_tracks;
use rsbts::provider::ProviderTrack;
use rsbts::{AudioFormat, Item};

fuzz_target!(|data: &[u8]| {
    let count = data.first().map_or(0, |value| usize::from(*value % 32));
    let items = (0..count)
        .map(|index| Item {
            id: None,
            album_id: None,
            path: format!("{index}.wav").into(),
            title: format!("track-{}", data.get(index + 1).copied().unwrap_or(0)),
            artist: "Artist".into(),
            album: "Album".into(),
            albumartist: None,
            genre: None,
            year: None,
            track: u32::try_from(index + 1).ok(),
            disc: Some(1),
            format: AudioFormat::Wav,
            bitrate: 0,
            length: index as f64,
            file_size: None,
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
        })
        .collect::<Vec<_>>();
    let tracks = (0..count)
        .map(|index| ProviderTrack {
            external_id: format!("recording-{index}"),
            release_track_external_id: Some(format!("track-{index}")),
            title: format!("track-{}", data.get(count + index + 1).copied().unwrap_or(0)),
            artist: "Artist".into(),
            number: u32::try_from(index + 1).ok(),
            printed_position: Some((index + 1).to_string()),
            disc: Some(1),
            length_ms: u64::try_from(index).ok().map(|value| value * 1_000),
            is_hidden: false,
            is_data_track: false,
            pregap_ms: None,
        })
        .collect::<Vec<_>>();
    let _assignments = assign_release_tracks(&items, &tracks);
});
