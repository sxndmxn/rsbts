#![cfg(any(target_os = "linux", target_os = "android"))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;

use rsbts::db::Library;
use rsbts::query::Query;
use rsbts::remove::{RemovalExecutor, RemovalPlan};
use rsbts::{Album, AudioFormat, ExtendedMetadata, Item};

fn fixture(directory: &Path) -> Result<Library, Box<dyn std::error::Error>> {
    let mut groups = Vec::new();
    for (name, tracks) in [("Selected", 2), ("Unselected", 1)] {
        let artwork = directory.join(format!("{name}.jpg"));
        std::fs::write(&artwork, format!("{name} artwork"))?;
        let album = Album {
            id: None,
            album: name.into(),
            albumartist: "Artist".into(),
            year: None,
            artpath: Some(artwork),
            external_id: None,
            added: Utc::now(),
            extended: ExtendedMetadata::default(),
        };
        let mut items = Vec::new();
        for index in 1..=tracks {
            let title = format!("{name}-{index}");
            let path = directory.join(format!("{title}.flac"));
            std::fs::write(&path, format!("{title} audio"))?;
            let metadata = std::fs::metadata(&path)?;
            items.push(Item {
                id: None,
                album_id: None,
                path,
                title,
                artist: "Artist".into(),
                album: name.into(),
                albumartist: None,
                genre: None,
                year: None,
                track: Some(index),
                disc: Some(1),
                format: AudioFormat::Flac,
                bitrate: 1,
                length: 1.0,
                file_size: Some(metadata.len()),
                track_external_id: None,
                release_external_id: None,
                added: Utc::now(),
                mtime: metadata.modified()?.into(),
                singleton: false,
                extended: ExtendedMetadata::default(),
            });
        }
        groups.push((Some(album), items));
    }
    let mut library = Library::open_in_memory()?;
    library.import_migrated_groups(&groups)?;
    Ok(library)
}

fn snapshot(directory: &Path) -> std::io::Result<BTreeMap<PathBuf, Vec<u8>>> {
    std::fs::read_dir(directory)?
        .map(|entry| {
            let path = entry?.path();
            let bytes = std::fs::read(&path)?;
            Ok((path, bytes))
        })
        .collect()
}

#[test]
fn approval_rejects_artwork_from_an_unselected_album() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let mut library = fixture(temporary.path())?;
    let before = snapshot(temporary.path())?;
    let mut selected = RemovalPlan::build(&library, &Query::parse("album:=Selected")?, true)?;
    let other = RemovalPlan::build(&library, &Query::parse("album:=Unselected")?, true)?;
    let artwork = other
        .files
        .iter()
        .find(|file| file.role == "artwork")
        .cloned()
        .ok_or("fixture has no unrelated artwork")?;
    selected.files.push(artwork);

    let result = selected
        .approve(&library)
        .and_then(|plan| RemovalExecutor::new(&mut library).execute(plan));
    assert!(
        result.is_err(),
        "removal accepted unrelated artwork: {result:?}"
    );
    assert_eq!(snapshot(temporary.path())?, before);
    assert_eq!(library.query_items(&Query::all())?.len(), 3);
    Ok(())
}

#[test]
fn approval_rejects_unrelated_audio_labeled_as_artwork() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let mut library = fixture(temporary.path())?;
    let before = snapshot(temporary.path())?;
    let mut selected = RemovalPlan::build(&library, &Query::parse("album:=Selected")?, true)?;
    let other = RemovalPlan::build(&library, &Query::parse("album:=Unselected")?, true)?;
    let mut audio = other
        .files
        .iter()
        .find(|file| file.role == "track")
        .cloned()
        .ok_or("fixture has no unrelated audio")?;
    audio.role = "artwork".into();
    selected.files.push(audio);

    let result = selected
        .approve(&library)
        .and_then(|plan| RemovalExecutor::new(&mut library).execute(plan));
    assert!(
        result.is_err(),
        "removal accepted relabeled audio: {result:?}"
    );
    assert_eq!(snapshot(temporary.path())?, before);
    assert_eq!(library.query_items(&Query::all())?.len(), 3);
    Ok(())
}

#[test]
fn approval_rejects_artwork_from_a_partially_selected_album(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let mut library = fixture(temporary.path())?;
    let before = snapshot(temporary.path())?;
    let mut selected = RemovalPlan::build(&library, &Query::parse("title:=Selected-1")?, true)?;
    let album = RemovalPlan::build(&library, &Query::parse("album:=Selected")?, true)?;
    let artwork = album
        .files
        .iter()
        .find(|file| file.role == "artwork")
        .cloned()
        .ok_or("fixture has no partially selected artwork")?;
    selected.files.push(artwork);

    let result = selected
        .approve(&library)
        .and_then(|plan| RemovalExecutor::new(&mut library).execute(plan));
    assert!(
        result.is_err(),
        "removal accepted partial-album artwork: {result:?}"
    );
    assert_eq!(snapshot(temporary.path())?, before);
    assert_eq!(library.query_items(&Query::all())?.len(), 3);
    Ok(())
}
