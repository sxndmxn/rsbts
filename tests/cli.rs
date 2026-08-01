use std::path::Path;
use std::process::{Command, Output};

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
