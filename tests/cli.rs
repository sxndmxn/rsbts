use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};

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
fn existing_tags_import_skips_provider_lookups() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let directory = temporary.path().canonicalize()?;
    let config = directory.join("config.toml");
    let database = directory.join("library.db");
    let organized = directory.join("organized");
    let source = directory.join("track.mp3");
    let mut original = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats/mp3.mp3"),
    )?;
    assert_eq!(&original[..3], b"ID3");
    let tag_size = original[6..10]
        .iter()
        .fold(0_usize, |size, byte| (size << 7) | usize::from(*byte));
    original.splice(10 + tag_size..10 + tag_size, std::iter::repeat_n(0, 2048));
    std::fs::write(&source, &original)?;
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n[musicbrainz]\nmax_retries = 0\n",
    )?;
    let action = if cfg!(any(target_os = "linux", target_os = "android")) {
        "--copy"
    } else {
        "--in-place"
    };
    for approval in ["--dry-run", "--yes"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rsbts"))
            .args(["--config"])
            .arg(&config)
            .args([
                "--output",
                "json",
                "import",
                "--existing-tags",
                action,
                approval,
            ])
            .arg(&source)
            .env("HTTP_PROXY", "http://127.0.0.1:0")
            .env("HTTPS_PROXY", "http://127.0.0.1:0")
            .env("ALL_PROXY", "http://127.0.0.1:0")
            .env("NO_PROXY", "")
            .output()?;
        let result = parse_json_document(&output)?;
        let album = &result["plan"]["albums"][0];
        assert!(album["lookup_error"].is_null(), "{result}");
        assert_eq!(album["candidates"], serde_json::json!([]));
        assert_eq!(album["candidate_set_complete"], false);
        assert_eq!(result["plan"]["provider_warnings"], serde_json::json!([]));
        assert_eq!(std::fs::read(&source)?, original);
        if approval == "--dry-run" {
            assert!(!database.exists());
            assert!(!organized.exists());
        } else {
            assert_eq!(result["execution"][0]["imported_tracks"], 1);
        }
    }
    let listing = parse_json_document(&run(&config, &["--output", "json", "ls"])?)?;
    assert_eq!(listing.as_array().map(Vec::len), Some(1));
    assert_success(&run(&config, &["update"])?);
    let updated = parse_json_document(&run(&config, &["--output", "json", "ls"])?)?;
    assert_eq!(updated[0]["title"], listing[0]["title"]);
    assert_success(&run(&config, &["audit"])?);
    Ok(())
}

#[test]
fn import_reports_unreadable_audio_instead_of_silently_skipping(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    let input = temporary.path().join("input");
    std::fs::create_dir(&input)?;
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n",
    )?;
    for name in ["unsupported.wma", "damaged.MP3", "notes.txt"] {
        std::fs::write(input.join(name), b"unrecognized file content")?;
    }
    let output = Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(config)
        .args(["--output", "json", "import", "--existing-tags", "--dry-run"])
        .arg(&input)
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let issues = result["plan"]["scan_issues"]
        .as_array()
        .ok_or("missing scan issues")?;
    assert_eq!(issues.len(), 2, "{result}");
    for name in ["unsupported.wma", "damaged.MP3"] {
        let expected = input.join(name).canonicalize()?;
        assert!(issues
            .iter()
            .any(|issue| issue["path"] == expected.to_string_lossy().as_ref()));
    }
    assert!(!temporary.path().join("library.db").exists());
    assert!(!temporary.path().join("organized").exists());
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn long_unicode_names_survive_file_commands_and_prepared_journal_recovery(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    let database = temporary.path().join("library.db");
    let organized = temporary.path().join("organized");
    let source = temporary.path().join(format!("{}.wav", "é".repeat(110)));
    let original = minimal_wav();
    std::fs::write(&source, &original)?;
    let base = "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n";
    std::fs::write(&config, format!("{base}[paths]\nformat = '$title'\n"))?;
    let imported = Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(&config)
        .args(["import", "--existing-tags", "--move", "--yes"])
        .arg(&source)
        .output()?;
    assert_success(&imported);
    assert!(!source.exists());
    let listing = parse_json_document(&run(&config, &["--output", "json", "ls"])?)?;
    let path = Path::new(listing[0]["path"].as_str().ok_or("missing path")?);
    assert_eq!(std::fs::read(path)?, original);
    let audio = rsbts::media::decoded_audio_essence_hash(path)?;
    assert_success(&run(
        &config,
        &["modify", "--yes", "format:wav", "genre=Test"],
    )?);
    assert_success(&run(&config, &["write", "--all", "--yes"])?);
    assert_eq!(rsbts::media::decoded_audio_essence_hash(path)?, audio);
    std::fs::write(&config, format!("{base}[paths]\nformat = 'Moved/$title'\n"))?;
    assert_success(&run(&config, &["move", "--all", "--yes"])?);
    assert_success(&run(&config, &["rm", "--delete", "--yes", "format:wav"])?);
    assert_success(&run(
        &config,
        &["purge", "--older-than-days", "0", "--yes"],
    )?);
    assert_success(&run(&config, &["audit"])?);

    std::fs::write(&source, &original)?;
    let id = uuid::Uuid::new_v4().to_string();
    let staged = organized.join(format!(".{}.rsbts-{id}.stage", "é".repeat(110)));
    let destination = organized.join("uncreated.wav");
    let connection = Connection::open(&database)?;
    connection.execute(
        "INSERT INTO operation_journal (id, kind, state, created_at, updated_at)
         VALUES (?1, 'import-copy', 'prepared', ?2, ?2)",
        rusqlite::params![id, chrono::Utc::now().to_rfc3339()],
    )?;
    connection.execute(
        "INSERT INTO operation_files
         (operation_id, ordinal, source_path, staged_path, destination_path, role, state)
         VALUES (?1, 0, ?2, ?3, ?4, 'track', 'prepared')",
        rusqlite::params![
            id,
            source.to_str().ok_or("invalid source")?,
            staged.to_str().ok_or("invalid stage")?,
            destination.to_str().ok_or("invalid destination")?,
        ],
    )?;
    drop(connection);
    assert_success(&run(&config, &["audit"])?);
    assert_eq!(std::fs::read(&source)?, original);
    assert!(!destination.exists());
    let completed: bool = Connection::open(&database)?.query_row(
        "SELECT completed_at IS NOT NULL FROM operation_journal WHERE id = ?1",
        [id],
        |row| row.get(0),
    )?;
    assert!(completed);
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn file_command_fixture(
    directory: &Path,
) -> Result<(std::path::PathBuf, std::path::PathBuf), Box<dyn std::error::Error>> {
    let config = directory.join("config.toml");
    let database = directory.join("library.db");
    let library_directory = directory.join("organized");
    std::fs::create_dir(&library_directory)?;
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n",
    )?;
    let path = library_directory.join("track.wav");
    std::fs::write(&path, minimal_wav())?;
    let mut item = rsbts::tags::read_tags(&path)?;
    item.title = "Track".into();
    item.artist = "Artist".into();
    item.album = "Album".into();
    item.albumartist = Some("Artist".into());
    item.track = Some(1);
    item.extended.artists = vec!["Artist".into(), "Guest".into()];
    item.extended.genres = vec!["Rock".into(), "Metal".into()];
    let mut library = rsbts::db::Library::open(&database)?;
    library.import_migrated_groups(&[(None, vec![item])])?;
    assert_eq!(
        library
            .verify_items(&rsbts::query::Query::all(), &library_directory)?
            .verified,
        1
    );
    Ok((config, path))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn parse_file_command_result(
    output: &Output,
    format: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    if format == "json" {
        return Ok(serde_json::from_slice(&output.stdout)?);
    }
    let text = std::str::from_utf8(&output.stdout)?;
    let records = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<serde_json::Value>, _>>()?;
    assert_eq!(records.len(), 2, "JSONL must retain the preview and result");
    records
        .into_iter()
        .last()
        .ok_or_else(|| "missing result".into())
}

#[test]
fn extreme_relative_dates_fail_before_database_open() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'state/library.db'\n",
    )?;
    for value in [
        "added:-9223372036854775807d",
        "added:-9223372036854775807w",
        "added:-9223372036854775807m",
        "added:-9223372036854775807y",
        "added:-100000000d",
    ] {
        let output = run(&config, &["ls", value])?;
        assert_eq!(output.status.code(), Some(1), "query: {value}");
        assert!(String::from_utf8(output.stderr)?.contains("invalid relative date"));
        assert!(!temporary.path().join("state").exists());
    }
    Ok(())
}

#[test]
fn extreme_purge_retention_fails_before_database_open() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'state/library.db'\n",
    )?;
    for value in ["9223372036854775807", "18446744073709551615", "100000000"] {
        for approval in ["--dry-run", "--yes"] {
            let output = run(&config, &["purge", "--older-than-days", value, approval])?;
            assert_eq!(output.status.code(), Some(1), "retention: {value}");
            assert!(String::from_utf8(output.stderr)?.contains("purge retention"));
            assert!(!temporary.path().join("state").exists());
        }
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn modify_emits_machine_results_without_changing_media() -> Result<(), Box<dyn std::error::Error>> {
    for format in ["json", "jsonl"] {
        let temporary = tempfile::tempdir()?;
        let (config, path) = file_command_fixture(temporary.path())?;
        let original = std::fs::read(&path)?;
        for (query, count) in [("title:=Track", 1), ("title:=Missing", 0)] {
            let output = run(
                &config,
                &[
                    "--output",
                    format,
                    "modify",
                    "--yes",
                    query,
                    "title=Changed",
                ],
            )?;
            let result = parse_json_document(&output)?;
            assert_eq!(result["operation"], "modify");
            assert_eq!(result["modified"], count);
        }
        let library = rsbts::db::Library::open_read_only(&temporary.path().join("library.db"))?;
        assert_eq!(
            library.query_items(&rsbts::query::Query::all())?[0].title,
            "Changed"
        );
        assert_eq!(std::fs::read(path)?, original);
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn update_emits_machine_results_for_success_and_failure() -> Result<(), Box<dyn std::error::Error>>
{
    for format in ["json", "jsonl"] {
        let temporary = tempfile::tempdir()?;
        let (config, path) = file_command_fixture(temporary.path())?;
        let original = std::fs::read(&path)?;
        let output = run(&config, &["--output", format, "update"])?;
        let result = parse_json_document(&output)?;
        assert_eq!(result["operation"], "update");
        assert_eq!(result["updated"], 1);
        assert_eq!(result["failures"], serde_json::json!([]));
        assert_eq!(std::fs::read(&path)?, original);

        std::fs::write(&path, b"invalid test audio")?;
        let output = run(&config, &["--output", format, "update"])?;
        assert_eq!(output.status.code(), Some(2));
        let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(result["updated"], 0);
        assert_eq!(
            result["failures"]
                .as_array()
                .ok_or("missing failures")?
                .len(),
            1
        );
        assert_eq!(result["failures"][0]["path"], serde_json::json!(path));
        assert!(result["failures"][0]["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()));
        assert_eq!(std::fs::read(path)?, b"invalid test audio");
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn verify_emits_machine_results_for_success_and_missing_files(
) -> Result<(), Box<dyn std::error::Error>> {
    for format in ["json", "jsonl"] {
        let temporary = tempfile::tempdir()?;
        let (config, path) = file_command_fixture(temporary.path())?;
        let original = std::fs::read(&path)?;
        let output = run(&config, &["--output", format, "verify"])?;
        let result = parse_json_document(&output)?;
        assert_eq!(result["operation"], "verify");
        assert_eq!(result["verified"], 1);
        assert_eq!(result["skipped"], serde_json::json!([]));
        assert_eq!(std::fs::read(&path)?, original);

        std::fs::remove_file(&path)?;
        let output = run(&config, &["--output", format, "verify"])?;
        assert_eq!(output.status.code(), Some(2));
        let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(result["verified"], 0);
        assert_eq!(
            result["skipped"]
                .as_array()
                .ok_or("missing skipped items")?
                .len(),
            1
        );
        assert_eq!(result["skipped"][0]["path"], serde_json::json!(path));
        assert!(result["skipped"][0]["detail"]
            .as_str()
            .is_some_and(|detail| !detail.is_empty()));
        let library = rsbts::db::Library::open_read_only(&temporary.path().join("library.db"))?;
        assert_eq!(library.stats()?.tracks, 1);
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn purge_emits_machine_previews_and_results() -> Result<(), Box<dyn std::error::Error>> {
    for format in ["json", "jsonl"] {
        let temporary = tempfile::tempdir()?;
        let (config, path) = file_command_fixture(temporary.path())?;
        let original = std::fs::read(&path)?;
        assert_success(&run(&config, &["rm", "title:=Track", "--delete", "--yes"])?);
        let database = temporary.path().join("library.db");
        let output = run_preserving_database(
            &config,
            &database,
            &[
                "--output",
                format,
                "purge",
                "--older-than-days",
                "0",
                "--dry-run",
            ],
        )?;
        let preview = parse_json_document(&output)?;
        assert_eq!(preview["operation"], "purge");
        assert_eq!(preview["dry_run"], true);
        assert_eq!(preview["executed"], false);
        let paths = preview["paths"]
            .as_array()
            .ok_or("missing quarantine paths")?;
        assert_eq!(paths.len(), 1);
        let quarantine = Path::new(paths[0].as_str().ok_or("invalid quarantine path")?);
        assert!(quarantine.starts_with(temporary.path()));
        assert_eq!(std::fs::read(quarantine)?, original);

        let output = run(
            &config,
            &[
                "--output",
                format,
                "purge",
                "--older-than-days",
                "0",
                "--yes",
            ],
        )?;
        assert_success(&output);
        let result = parse_file_command_result(&output, format)?;
        assert_eq!(result["operation"], "purge");
        assert_eq!(result["executed"], true);
        assert_eq!(result["report"]["purged_files"], 1);
        assert_eq!(result["report"]["already_missing"], 0);
        assert!(!quarantine.exists());

        let output = run(
            &config,
            &[
                "--output",
                format,
                "purge",
                "--older-than-days",
                "0",
                "--yes",
            ],
        )?;
        let result = parse_json_document(&output)?;
        assert_eq!(result["executed"], false);
        assert_eq!(result["paths"], serde_json::json!([]));
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn confirmed_writes_emit_valid_machine_results() -> Result<(), Box<dyn std::error::Error>> {
    for format in ["json", "jsonl"] {
        let temporary = tempfile::tempdir()?;
        let (config, path) = file_command_fixture(temporary.path())?;
        let output = run(&config, &["--output", format, "write", "--all", "--yes"])?;
        assert_success(&output);
        let result = parse_file_command_result(&output, format)?;
        assert_eq!(result["operation"], "write");
        assert_eq!(result["written"], 1);
        assert_eq!(result["failures"], serde_json::json!([]));
        assert_eq!(rsbts::tags::read_tags(&path)?.title, "Track");
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn confirmed_moves_emit_valid_machine_results() -> Result<(), Box<dyn std::error::Error>> {
    for format in ["json", "jsonl"] {
        let temporary = tempfile::tempdir()?;
        let (config, source) = file_command_fixture(temporary.path())?;
        let original = std::fs::read(&source)?;
        let output = run(&config, &["--output", format, "move", "--all", "--yes"])?;
        assert_success(&output);
        let result = parse_file_command_result(&output, format)?;
        assert_eq!(result["operation"], "move");
        assert_eq!(result["moved"], 1);
        assert_eq!(result["failures"], serde_json::json!([]));
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(
                temporary
                    .path()
                    .join("organized/Artist/Album/01 - Track.wav")
            )?,
            original
        );
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn file_previews_leave_the_database_and_media_unchanged() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = tempfile::tempdir()?;
    let (config, path) = file_command_fixture(temporary.path())?;
    let database = temporary.path().join("library.db");
    let before = std::fs::read(&path)?;
    for format in ["json", "jsonl"] {
        for operation in ["write", "move"] {
            for selection in ["--all", "artist:=Missing"] {
                let output = run_preserving_database(
                    &config,
                    &database,
                    &["--output", format, operation, selection, "--dry-run"],
                )?;
                let preview = parse_json_document(&output)?;
                assert_eq!(preview["operation"], operation);
                assert_eq!(preview["dry_run"], true);
                let entries = if operation == "write" {
                    "files"
                } else {
                    "tracks"
                };
                let count = preview[entries]
                    .as_array()
                    .ok_or("missing preview entries")?
                    .len();
                assert_eq!(count, usize::from(selection == "--all"));
                assert_eq!(std::fs::read(&path)?, before);
            }
        }
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn failed_writes_preserve_changed_media_and_emit_valid_results(
) -> Result<(), Box<dyn std::error::Error>> {
    for format in ["json", "jsonl"] {
        let temporary = tempfile::tempdir()?;
        let (config, path) = file_command_fixture(temporary.path())?;
        let replacement = b"changed media must remain untouched";
        std::fs::write(&path, replacement)?;
        let output = run(&config, &["--output", format, "write", "--all", "--yes"])?;
        assert_eq!(output.status.code(), Some(2));
        let result = parse_file_command_result(&output, format)?;
        assert_eq!(result["written"], 0);
        assert_eq!(
            result["failures"]
                .as_array()
                .ok_or("missing failures")?
                .len(),
            1
        );
        assert_eq!(std::fs::read(path)?, replacement);
    }
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn bounded_file_plans_retain_multivalue_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let (_config, path) = file_command_fixture(temporary.path())?;
    let library = rsbts::db::Library::open_read_only(&temporary.path().join("library.db"))?;
    let query = rsbts::query::Query::all();
    let write = rsbts::write::TagWritePlan::build(&library, &query)?;
    assert_eq!(write.files.len(), 1);
    assert_eq!(write.files[0].item.extended.artists, ["Artist", "Guest"]);
    assert_eq!(write.files[0].item.extended.genres, ["Rock", "Metal"]);
    let movement = rsbts::move_files::MovePlan::build(
        &library,
        &query,
        &temporary.path().join("organized"),
        "$artist/$title",
    )?;
    assert_eq!(movement.tracks.len(), 1);
    assert_eq!(
        movement.tracks[0].item.extended.artists,
        ["Artist", "Guest"]
    );
    assert_eq!(movement.tracks[0].item.extended.genres, ["Rock", "Metal"]);
    assert!(path.exists());
    assert!(!movement.tracks[0].destination.exists());
    Ok(())
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
        vec!["ls", "--limit", "0"],
        vec!["ls", "--album", "--limit", "100001"],
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
    assert_ne!(String::from_utf8(results.stdout)?.trim(), "");

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

    let output = run(&config_path, &["verify", "artist:=Test Artist"])?;
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
            "--yes",
            "artist:=Test Artist",
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
            "--yes",
            "artist:=Test Artist",
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
        &["rm", "--delete", "--dry-run", "artist:=Test Artist"],
    )?;
    assert_success(&output);
    assert!(track_path.exists(), "dry-run must preserve the file");

    let output = run(
        &config_path,
        &["rm", "--delete", "--yes", "artist:=Test Artist"],
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

#[test]
fn a_closed_stdout_pipe_is_successful_and_never_panics() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config_path = temporary.path().join("config.toml");
    let database_path = temporary.path().join("library.db");
    std::fs::write(
        &config_path,
        format!(
            "[library]\ndirectory = 'organized'\ndatabase = '{}'\n",
            database_path.display()
        ),
    )?;
    assert_success(&run(&config_path, &["stats"])?);

    let connection = Connection::open(&database_path)?;
    connection.execute_batch(
        "WITH RECURSIVE records(id) AS (
             SELECT 1 UNION ALL SELECT id + 1 FROM records WHERE id < 4096
         )
         INSERT INTO items
             (path, title, artist, album, format, bitrate, length, added, mtime)
         SELECT printf('/missing/%d.flac', id), printf('Track %d', id),
                'Pipe Artist', 'Pipe Album', 'FLAC', 1, 1.0,
                '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z'
         FROM records;",
    )?;
    drop(connection);

    let mut child = Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(&config_path)
        .arg("ls")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("child process did not expose its piped stdout"))?;
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    assert!(reader.read_line(&mut first_line)? > 0);
    drop(reader);

    let output = child.wait_with_output()?;
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "unexpected panic output: {stderr}"
    );
    assert!(
        !stderr.contains("Broken pipe"),
        "unexpected pipe diagnostic: {stderr}"
    );
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one disposable CLI workflow covers native aliases, unknown values, and file preservation across three formats"
)]
fn native_tag_aliases_and_unknown_values_survive_cli_update_and_write(
) -> Result<(), Box<dyn std::error::Error>> {
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::ogg::VorbisComments;
    use lofty::prelude::TagExt;
    use lofty::probe::Probe;
    use lofty::tag::{ItemKey, Tag};

    for name in ["flac.flac", "mp3.mp3", "alac.m4a"] {
        let temporary = tempfile::tempdir()?;
        let config = temporary.path().join("config.toml");
        let database = temporary.path().join("library.db");
        let directory = temporary.path().join("organized");
        std::fs::create_dir(&directory)?;
        std::fs::write(
            &config,
            "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n",
        )?;
        let path = directory.join(name);
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/formats")
                .join(name),
            &path,
        )?;
        let item = rsbts::tags::read_tags(&path)?;
        rsbts::db::Library::open(&database)?.import_migrated_groups(&[(None, vec![item])])?;

        if name == "flac.flac" {
            let mut tag = VorbisComments::default();
            for (key, value) in [
                ("TITLE", "Track"),
                ("ARTIST", "Artist with Guest"),
                ("ALBUM", "Album"),
                ("TRACKNUMBER", "1"),
                ("TRACKTOTAL", "9"),
                ("TOTALTRACKS", "9"),
                ("DISCTOTAL", "1"),
                ("TOTALDISCS", "1"),
                ("DATE", "2024"),
            ] {
                tag.insert(key.into(), value.into());
            }
            for key in ["ALBUMARTIST", "COMPOSER"] {
                for value in ["Artist", "Guest", "Artist"] {
                    tag.push(key.into(), value.into());
                }
            }
            for value in ["Artist", "Guest"] {
                tag.push("ARTISTS".into(), value.into());
            }
            tag.save_to_path(&path, WriteOptions::new())?;
        } else {
            let mut media = Probe::open(&path)?.read()?;
            let mut tag = Tag::new(media.primary_tag_type());
            for (key, value) in [
                (ItemKey::TrackTitle, "Track"),
                (ItemKey::TrackArtist, "Artist"),
                (ItemKey::AlbumTitle, "Album"),
                (ItemKey::RecordingDate, "2024"),
                (ItemKey::OriginalReleaseDate, "0000"),
                (ItemKey::MusicBrainzReleaseId, ""),
                (ItemKey::Composer, ""),
                (ItemKey::TrackNumber, "1"),
                (ItemKey::TrackTotal, "0"),
                (ItemKey::DiscNumber, "0"),
                (ItemKey::DiscTotal, "0"),
            ] {
                assert!(tag.insert_text(key, value.into()));
            }
            media.insert_tag(tag);
            media.save_to_path(&path, WriteOptions::new())?;
        }

        let original = std::fs::read(&path)?;
        let audio = rsbts::media::decoded_audio_essence_hash(&path)?;
        let updated = parse_json_document(&run(&config, &["--output", "json", "update"])?)?;
        assert_eq!(updated["updated"], 1, "{name}: {updated}");
        assert_eq!(std::fs::read(&path)?, original);
        let item = rsbts::tags::read_tags(&path)?;
        assert_eq!(item.year, Some(2024));
        assert_eq!(item.track, Some(1));
        assert_eq!(item.extended.original_date.year, None);
        assert_eq!(
            item.extended.track_total,
            (name == "flac.flac").then_some(9)
        );
        assert_eq!(item.disc, None);
        assert_eq!(item.release_external_id, None);
        if name == "flac.flac" {
            assert_eq!(item.extended.album_artists, ["Artist", "Guest"]);
            assert_eq!(item.extended.composers, ["Artist", "Guest"]);
        } else {
            assert_eq!(item.extended.composers, [] as [String; 0]);
        }
        assert_success(&run(&config, &["verify"])?);
        assert_success(&run(
            &config,
            &["modify", "--yes", "title:=Track", "genre=Test Genre"],
        )?);
        run_preserving_database(&config, &database, &["write", "--all", "--dry-run"])
            .map(|output| assert_success(&output))?;
        assert_eq!(std::fs::read(&path)?, original);
        assert_success(&run(&config, &["write", "--all", "--yes"])?);
        let written_tags = rsbts::tags::read_tags(&path)?;
        assert_eq!(written_tags.artist, item.artist);
        assert_eq!(
            written_tags.effective_albumartist(),
            item.effective_albumartist()
        );
        assert_eq!(
            rsbts::tags::read_tags(&path)?.genre.as_deref(),
            Some("Test Genre")
        );
        assert_eq!(rsbts::media::decoded_audio_essence_hash(&path)?, audio);
        assert_success(&run(&config, &["audit"])?);
        if name == "flac.flac" {
            let mut media = Probe::open(&path)?.read()?;
            let tag = media.primary_tag_mut().ok_or("missing native tag")?;
            assert!(tag.push(lofty::tag::TagItem::new(
                ItemKey::TrackTotal,
                lofty::tag::ItemValue::Text("10".into()),
            )));
            media.save_to_path(&path, WriteOptions::new())?;
            let database_before = std::fs::read(&database)?;
            let output = run(&config, &["--output", "json", "update"])?;
            assert_eq!(output.status.code(), Some(2));
            let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(result["updated"], 0);
            assert!(result["failures"][0]["error"]
                .as_str()
                .is_some_and(|message| message.contains("conflicting values")));
            assert_eq!(std::fs::read(&database)?, database_before);
            std::fs::write(&path, original)?;
            assert_success(&run(&config, &["verify"])?);
        }
        assert_success(&run(&config, &["rm", "--delete", "--yes", "title:=Track"])?);
        assert!(!path.exists());
    }
    Ok(())
}
