use std::path::Path;
use std::process::{Command, Output};

use rsbts::catalog::{Confidence, DataLicense, EntityId, EntityKind, MetadataClaim, ValueState};
use rusqlite::Connection;

fn run(config: &Path, arguments: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(config)
        .args(arguments)
        .output()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_preserving_database(
    config: &Path,
    database: &Path,
    arguments: &[&str],
) -> Result<Output, Box<dyn std::error::Error>> {
    let before = std::fs::read(database)?;
    let output = run(config, arguments)?;
    assert_eq!(
        std::fs::read(database)?,
        before,
        "command changed the database: {arguments:?}"
    );
    Ok(output)
}

fn stored_item_and_album_year(
    database: &Path,
) -> Result<(String, i32, i32), Box<dyn std::error::Error>> {
    let connection = Connection::open(database)?;
    let (title, item_year) = connection.query_row("SELECT title, year FROM items", [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    let album_year = connection.query_row("SELECT year FROM albums", [], |row| row.get(0))?;
    Ok((title, item_year, album_year))
}

fn minimal_wav() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&38_u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&44_100_u32.to_le_bytes());
    bytes.extend_from_slice(&88_200_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&2_u32.to_le_bytes());
    bytes.extend_from_slice(&0_i16.to_le_bytes());
    bytes
}

fn parse_json_document(output: &Output) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    assert_success(output);
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn import_dry_run_does_not_create_a_database_or_library() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = tempfile::tempdir()?;
    let config_path = temporary.path().join("config.toml");
    let database_path = temporary.path().join("state/library.db");
    let library_path = temporary.path().join("organized");
    let input_path = temporary.path().join("empty-input");
    std::fs::create_dir(&input_path)?;
    std::fs::write(
        &config_path,
        format!(
            "[library]\ndirectory = '{}'\ndatabase = '{}'\n[import]\nfetch_art = false\n",
            library_path.display(),
            database_path.display()
        ),
    )?;

    let output = run(
        &config_path,
        &["import", input_path.to_string_lossy().as_ref()],
    )?;
    assert_eq!(output.status.code(), Some(1));
    assert!(!database_path.exists());
    assert!(!library_path.exists());

    let output = run(
        &config_path,
        &["import", "--dry-run", input_path.to_string_lossy().as_ref()],
    )?;

    assert_success(&output);
    assert!(!database_path.exists());
    assert!(!library_path.exists());

    let missing_input = temporary.path().join("missing-input");
    let output = run(
        &config_path,
        &[
            "import",
            "--dry-run",
            missing_input.to_string_lossy().as_ref(),
        ],
    )?;
    assert_eq!(output.status.code(), Some(2));
    assert!(!database_path.exists());
    assert!(!library_path.exists());
    Ok(())
}

#[test]
fn invalid_commands_fail_before_opening_the_database() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config_path = temporary.path().join("config.toml");
    let database_path = temporary.path().join("state/library.db");
    std::fs::write(
        &config_path,
        "[library]\ndirectory = 'organized'\ndatabase = 'state/library.db'\n",
    )?;

    for arguments in [
        vec!["ls", "unknown:value"],
        vec!["update", ""],
        vec!["rm", "--yes", ""],
        vec!["modify", "artist:test", "year=not-a-year"],
    ] {
        let output = run(&config_path, &arguments)?;
        assert_eq!(output.status.code(), Some(1));
        assert!(
            !database_path.exists(),
            "invalid command unexpectedly opened the database: {arguments:?}"
        );
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
#[expect(
    clippy::too_many_lines,
    reason = "one disposable workflow verifies the complete machine-output and dry-run contract"
)]
fn machine_output_is_parseable_and_projection_previews_are_read_only(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config_path = temporary.path().join("config.toml");
    let database_path = temporary.path().join("library.db");
    let library_path = temporary.path().join("organized");
    std::fs::create_dir(&library_path)?;
    std::fs::write(
        &config_path,
        format!(
            "[library]\ndirectory = '{}'\ndatabase = '{}'\n[import]\nfetch_art = false\n",
            library_path.display(),
            database_path.display()
        ),
    )?;

    let stats = run(&config_path, &["--output", "json", "stats"])?;
    assert_eq!(parse_json_document(&stats)?["tracks"], 0);

    let track = library_path.join("track.wav");
    std::fs::write(&track, minimal_wav())?;
    let connection = Connection::open(&database_path)?;
    connection.execute(
        "INSERT INTO albums (album, albumartist, year, added)
         VALUES ('Album', 'Artist', 2026, '2026-01-01T00:00:00+00:00')",
        [],
    )?;
    let album_id = connection.last_insert_rowid();
    connection.execute(
        "INSERT INTO items
         (album_id, path, title, artist, album, albumartist, year, track, disc,
          format, bitrate, length, added, mtime, file_size)
         VALUES (?1, ?2, 'Track', 'Artist', 'Album', 'Artist', 2026, 1, 1,
                 'WAV', 705, 0.1, '2026-01-01T00:00:00+00:00',
                 '2026-01-01T00:00:00+00:00', ?3)",
        rusqlite::params![
            album_id,
            track.to_string_lossy(),
            std::fs::metadata(&track)?.len()
        ],
    )?;
    drop(connection);
    assert_success(&run(&config_path, &["verify"])?);

    let connection = Connection::open(&database_path)?;
    let (item_id, asset_id): (i64, String) = connection.query_row(
        "SELECT i.id, ia.asset_id FROM items i
         JOIN item_assets ia ON ia.item_id = i.id AND ia.relationship = 'audio'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    drop(connection);

    let library = rsbts::db::Library::open(&database_path)?;
    let entity = EntityId::new();
    library.append_metadata_claim(&MetadataClaim::new(
        EntityKind::Release,
        entity.clone(),
        "title",
        ValueState::Known(serde_json::json!("Provider Album")),
        "provider-api",
        Some("fixture".into()),
        Some("release-1".into()),
        Confidence::new(1.0)?,
        DataLicense::Cc0,
        false,
    )?)?;
    drop(library);

    let list = run(&config_path, &["--output", "jsonl", "ls", "--limit", "1"])?;
    assert_success(&list);
    for line in String::from_utf8(list.stdout)?.lines() {
        let _: serde_json::Value = serde_json::from_str(line)?;
    }

    let provider = run_preserving_database(
        &config_path,
        &database_path,
        &[
            "--output",
            "json",
            "provider-refresh",
            "release",
            entity.as_str(),
            "--dry-run",
        ],
    )?;
    assert!(parse_json_document(&provider)?["plan"]["diffs"].is_array());

    let tag = run_preserving_database(
        &config_path,
        &database_path,
        &[
            "--output",
            "json",
            "tag-project",
            &item_id.to_string(),
            "--title",
            "Projected Track",
            "--artist",
            "Artist",
            "--album",
            "Album",
            "--album-artist",
            "Artist",
            "--dry-run",
        ],
    )?;
    assert!(parse_json_document(&tag)?["plan"]["before"].is_object());

    let path = run_preserving_database(
        &config_path,
        &database_path,
        &[
            "--output",
            "json",
            "path-project",
            &asset_id,
            "Artist/Album/01 - Track.wav",
            "--dry-run",
        ],
    )?;
    assert_eq!(parse_json_document(&path)?["plan"]["asset_id"], asset_id);

    let removal = run_preserving_database(
        &config_path,
        &database_path,
        &[
            "--output",
            "json",
            "rm",
            "--delete",
            "--dry-run",
            "artist:=Artist",
        ],
    )?;
    assert_eq!(parse_json_document(&removal)?["plan"]["delete_files"], true);

    let integrity = run(&config_path, &["--output", "json", "integrity"])?;
    assert_success(&integrity);
    assert_eq!(parse_json_document(&integrity)?["truncated"], false);

    let deep = run(&config_path, &["--output", "json", "audit", "--deep"])?;
    assert_success(&deep);
    let plan_id = parse_json_document(&deep)?["id"]
        .as_str()
        .ok_or("deep audit did not emit a plan ID")?
        .to_owned();
    assert_success(&run(
        &config_path,
        &["--output", "json", "fixity", "approve", &plan_id],
    )?);
    let progress = run(
        &config_path,
        &[
            "--output",
            "json",
            "fixity",
            "run",
            &plan_id,
            "--page-size",
            "1",
        ],
    )?;
    assert_success(&progress);
    assert_eq!(parse_json_document(&progress)?["complete"], false);
    let completed = run(
        &config_path,
        &[
            "--output",
            "json",
            "fixity",
            "run",
            &plan_id,
            "--page-size",
            "1",
        ],
    )?;
    assert_success(&completed);
    assert_eq!(parse_json_document(&completed)?["complete"], true);
    let results = run(
        &config_path,
        &[
            "--output", "jsonl", "fixity", "results", &plan_id, "--limit", "10",
        ],
    )?;
    assert_success(&results);
    assert!(!String::from_utf8(results.stdout)?.trim().is_empty());

    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(clippy::too_many_lines)]
fn disposable_cli_workflow_is_atomic_and_confirmation_safe(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config_path = temporary.path().join("config.toml");
    let database_path = temporary.path().join("library.db");
    let library_path = temporary.path().join("organized");
    std::fs::write(
        &config_path,
        format!(
            "[library]\ndirectory = '{}'\ndatabase = '{}'\n[import]\nfetch_art = false\n",
            library_path.display(),
            database_path.display()
        ),
    )?;

    let output = run(&config_path, &["stats"])?;
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Tracks: 0"));

    let track_path = temporary.path().join("disposable-track.flac");
    std::fs::write(&track_path, b"synthetic test bytes")?;
    let connection = Connection::open(&database_path)?;
    connection.execute(
        "INSERT INTO albums (album, albumartist, year, added)
             VALUES ('Album', 'Test Artist', 2000, '2024-01-01T00:00:00+00:00')",
        [],
    )?;
    let album_id = connection.last_insert_rowid();
    connection.execute(
        "INSERT INTO items
             (album_id, path, title, artist, album, albumartist, genre, year, track, disc,
              format, bitrate, length, added, mtime, file_size)
             VALUES (?1, ?2, 'Original', 'Test Artist', 'Album', 'Test Artist', 'Rock', 2000, 1, 1,
                     'FLAC', 1, 1.0, '2024-01-01T00:00:00+00:00',
                     '2024-01-01T00:00:00+00:00', ?3)",
        rusqlite::params![
            album_id,
            track_path.to_string_lossy(),
            std::fs::metadata(&track_path)?.len()
        ],
    )?;
    drop(connection);

    let output = run_preserving_database(&config_path, &database_path, &["audit"])?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Missing managed asset"));

    let output = run(&config_path, &["verify", "artist:=\"Test Artist\""])?;
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Verified 1 asset(s)"));

    let output = run_preserving_database(&config_path, &database_path, &["audit"])?;
    assert_success(&output);

    let temporarily_missing = temporary.path().join("temporarily-missing.flac");
    std::fs::rename(&track_path, &temporarily_missing)?;
    let output = run(&config_path, &["audit"])?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Missing file: item"));
    assert!(String::from_utf8_lossy(&output.stdout).contains(&*track_path.to_string_lossy()));
    std::fs::rename(temporarily_missing, &track_path)?;

    let output = run(
        &config_path,
        &[
            "modify",
            "artist:=\"Test Artist\"",
            "title=Changed",
            "year=2024",
        ],
    )?;
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Modified 1 item(s)"));

    let output = run(
        &config_path,
        &[
            "modify",
            "artist:=\"Test Artist\"",
            "title=Must Not Persist",
            "year=not-a-year",
        ],
    )?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stored_item_and_album_year(&database_path)?,
        ("Changed".into(), 2024, 2024)
    );

    let output = run(&config_path, &["modify", "", "title=Must Not Persist"])?;
    assert_eq!(output.status.code(), Some(1));
    let output = run(&config_path, &["rm", "--delete", "--yes", ""])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(track_path.exists());

    let output = run_preserving_database(
        &config_path,
        &database_path,
        &["rm", "--delete", "--dry-run", "artist:=\"Test Artist\""],
    )?;
    assert_success(&output);
    assert!(track_path.exists(), "dry-run must preserve the file");

    let output = run(
        &config_path,
        &["rm", "--delete", "--yes", "artist:=\"Test Artist\""],
    )?;
    assert_success(&output);
    assert!(
        !track_path.exists(),
        "confirmed deletion should remove the file"
    );
    let connection = Connection::open(&database_path)?;
    let count: u64 = connection.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    Ok(())
}
