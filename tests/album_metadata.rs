#![cfg(any(target_os = "linux", target_os = "android"))]

use std::path::Path;
use std::process::{Command, Output};

use lofty::config::WriteOptions;
use lofty::ogg::VorbisComments;
use lofty::prelude::TagExt;
use rsbts::{Album, FlexibleValue, Item, PartialDate};

const RELEASE_ID: &str = "550e8400-e29b-41d4-a716-446655440001";
const RELEASE_GROUP_ID: &str = "550e8400-e29b-41d4-a716-446655440002";
const ALBUM_ARTIST_ID: &str = "550e8400-e29b-41d4-a716-446655440003";
const TRACK_ID: &str = "550e8400-e29b-41d4-a716-446655440004";

fn run(config: &Path, arguments: &[&str]) -> Result<Output, Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(config)
        .args(arguments)
        .output()?;
    assert!(
        output.status.success(),
        "{arguments:?}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn write_track(
    path: &Path,
    album: &str,
    disc: u32,
    original_date: Option<&str>,
    conflict: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats/flac.flac"),
        path,
    )?;
    let mut tags = VorbisComments::default();
    for (key, value) in [
        ("ALBUM", album),
        ("ARTIST", "Track Artist"),
        ("ALBUMARTIST", "Album Artist"),
        ("TRACKNUMBER", "1"),
        ("TRACKTOTAL", "8"),
        ("DISCTOTAL", "2"),
        ("DATE", "2012-04-03"),
        ("COMPILATION", "1"),
        ("LABEL", "First Label"),
        ("CATALOGNUMBER", "First Catalog"),
        ("RELEASECOUNTRY", "GB"),
        ("MEDIA", "CD"),
        ("LANGUAGE", "eng"),
        ("BARCODE", "123456789"),
        ("REPLAYGAIN_ALBUM_GAIN", "-4.0 dB"),
        ("REPLAYGAIN_ALBUM_PEAK", "0.8"),
        ("REPLAYGAIN_TRACK_GAIN", "-3.0 dB"),
        ("REPLAYGAIN_TRACK_PEAK", "0.9"),
        ("COMPOSER", "Track Composer"),
        ("WORK", "Track Work"),
        ("MUSICBRAINZ_ALBUMID", RELEASE_ID),
        ("MUSICBRAINZ_RELEASEGROUPID", RELEASE_GROUP_ID),
        ("MUSICBRAINZ_ALBUMARTISTID", ALBUM_ARTIST_ID),
        ("MUSICBRAINZ_TRACKID", TRACK_ID),
        ("MUSICBRAINZ_RELEASETRACKID", TRACK_ID),
        ("MUSICBRAINZ_ARTISTID", TRACK_ID),
        ("MUSICBRAINZ_WORKID", TRACK_ID),
    ] {
        tags.insert(key.into(), value.into());
    }
    tags.insert("TITLE".into(), format!("Track {disc}"));
    tags.insert("DISCNUMBER".into(), disc.to_string());
    tags.push("LABEL".into(), "Second Label".into());
    tags.push("CATALOGNUMBER".into(), "Second Catalog".into());
    for value in [
        "Album Artist",
        if conflict { "Other Guest" } else { "Guest" },
    ] {
        tags.push("ALBUMARTISTS".into(), value.into());
    }
    for value in ["Rock", if conflict { "Jazz" } else { "Metal" }] {
        tags.push("GENRE".into(), value.into());
    }
    if let Some(value) = original_date {
        tags.insert("ORIGINALDATE".into(), value.into());
    }
    tags.save_to_path(path, WriteOptions::new())?;
    Ok(())
}

fn assert_album_scope(album: &Album) {
    let metadata = &album.extended;
    assert_eq!(
        metadata.track_total, None,
        "a per-disc total became an album total"
    );
    assert!(metadata.artists.is_empty());
    assert!(metadata.composers.is_empty());
    assert_eq!(metadata.external_ids.len(), 3);
    for (kind, value) in [
        ("release", RELEASE_ID),
        ("release-group", RELEASE_GROUP_ID),
        ("album-artist", ALBUM_ARTIST_ID),
    ] {
        assert!(metadata
            .external_ids
            .iter()
            .any(|id| id.kind() == kind && id.value() == value));
    }
    for name in ["work", "replaygain_track_gain", "replaygain_track_peak"] {
        assert!(!metadata.flexible_fields.contains_key(name));
    }
    assert_eq!(
        metadata.date,
        PartialDate {
            year: Some(2012),
            month: Some(4),
            day: Some(3)
        }
    );
    assert_eq!(metadata.disc_total, Some(2));
    assert_eq!(metadata.compilation, Some(true));
    for (actual, expected) in [
        (metadata.label.as_deref(), "First Label"),
        (metadata.catalog_number.as_deref(), "First Catalog"),
        (metadata.country.as_deref(), "GB"),
        (metadata.media.as_deref(), "CD"),
        (metadata.language.as_deref(), "eng"),
    ] {
        assert_eq!(actual, Some(expected));
    }
    for (name, expected) in [
        (
            "labels",
            FlexibleValue::StringList(vec!["First Label".into(), "Second Label".into()]),
        ),
        (
            "catalog_numbers",
            FlexibleValue::StringList(vec!["First Catalog".into(), "Second Catalog".into()]),
        ),
        ("barcode", FlexibleValue::String("123456789".into())),
        (
            "replaygain_album_gain",
            FlexibleValue::String("-4.0 dB".into()),
        ),
        ("replaygain_album_peak", FlexibleValue::String("0.8".into())),
    ] {
        assert_eq!(metadata.flexible_fields.get(name), Some(&expected));
    }
}

fn assert_imported_metadata(
    albums: &[Album],
    items: &[Item],
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(albums.len(), 3);
    assert_eq!(items.len(), 6);
    let shared = albums
        .iter()
        .find(|album| album.album == "Shared")
        .ok_or("missing shared album")?;
    assert_eq!(
        (
            shared.extended.original_date.year,
            shared.extended.album_artists.as_slice(),
            shared.extended.genres.as_slice()
        ),
        (
            Some(1972),
            ["Album Artist".into(), "Guest".into()].as_slice(),
            ["Rock".into(), "Metal".into()].as_slice()
        ),
        "import discarded shared original dates or album lists"
    );
    for album in albums {
        assert_album_scope(album);
        assert_eq!(album.year, Some(2012));
        if album.album != "Shared" {
            assert_eq!(album.extended.original_date, PartialDate::default());
        }
        if album.album == "Conflict" {
            assert!(album.extended.album_artists.is_empty());
            assert!(album.extended.genres.is_empty());
        }
    }
    for item in items {
        let disc = item.disc.ok_or("missing disc")?;
        let expected_year = match (item.album.as_str(), disc) {
            ("Conflict", 2) => Some(1984),
            ("Missing", 2) => None,
            _ => Some(1972),
        };
        assert_eq!(item.extended.original_date.year, expected_year);
        assert_eq!(item.extended.track_total, Some(8));
        for kind in ["recording", "release-track", "artist", "work"] {
            assert!(item
                .extended
                .external_ids
                .iter()
                .any(|id| id.kind() == kind));
        }
    }
    Ok(())
}

#[test]
fn imports_only_shared_album_metadata_and_preserves_track_facts(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    let input = temporary.path().join("input");
    std::fs::create_dir(&input)?;
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n[import]\nfetch_art = false\n",
    )?;
    let mut originals = Vec::new();
    for album in ["Shared", "Conflict", "Missing"] {
        let directory = input.join(album);
        std::fs::create_dir(&directory)?;
        for disc in [1, 2] {
            let original_date = match (album, disc) {
                ("Conflict", 2) => Some("1984"),
                ("Missing", 2) => None,
                _ => Some("1972"),
            };
            let path = directory.join(format!("disc-{disc}.flac"));
            write_track(
                &path,
                album,
                disc,
                original_date,
                album == "Conflict" && disc == 2,
            )?;
            originals.push((path.clone(), std::fs::read(path)?));
        }
    }
    let source = input.to_str().ok_or("invalid temporary input")?;
    run(
        &config,
        &["import", "--existing-tags", "--copy", "--dry-run", source],
    )?;
    assert!(!temporary.path().join("library.db").exists());
    assert!(!temporary.path().join("organized").exists());
    run(
        &config,
        &["import", "--existing-tags", "--copy", "--yes", source],
    )?;
    let albums: Vec<Album> =
        serde_json::from_slice(&run(&config, &["--output", "json", "ls", "--album"])?.stdout)?;
    let items: Vec<Item> =
        serde_json::from_slice(&run(&config, &["--output", "json", "ls"])?.stdout)?;
    assert_imported_metadata(&albums, &items)?;
    run(&config, &["audit"])?;
    for (path, bytes) in &originals {
        assert_eq!(std::fs::read(path)?, *bytes);
    }
    let query = format!("path:{}", temporary.path().join("organized").display());
    run(&config, &["rm", "--delete", "--dry-run", &query])?;
    run(&config, &["rm", "--delete", "--yes", &query])?;
    run(&config, &["purge", "--older-than-days", "0", "--yes"])?;
    run(&config, &["audit"])?;
    Ok(())
}
