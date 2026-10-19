use std::path::Path;
use std::process::{Command, Output};

use rsbts::db::Library;
use rusqlite::{params, Connection};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn run(config: &Path, arguments: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(config)
        .args(arguments)
        .output()
}

fn successful(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(directory: &Path) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let config = directory.join("config.toml");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'music'\ndatabase = 'library.db'\n",
    )?;
    let database = directory.join("library.db");
    drop(Library::open(&database)?);
    let connection = Connection::open(database)?;
    connection.execute_batch(
        "INSERT INTO albums (id, album, albumartist, year, added)
         VALUES (1, 'Late Album', 'Air Supply', 2001, '2026-01-01T00:00:00Z'),
                (2, 'Early Album', 'Air', 1970, '2026-01-01T00:00:00Z');",
    )?;
    for (id, album, artist, title, length, size) in [
        (1, 1, "Air Supply", "Northern Lights", 120.0, Some(100)),
        (2, 2, "AIR", "The Light", 180.0, Some(200)),
        (3, 2, "Air", "Northern Sky", 240.0, None),
        (4, 1, "Else", "100%_done / Mix", 60.0, Some(400)),
    ] {
        connection.execute(
            "INSERT INTO items
             (id, album_id, path, title, artist, album, albumartist, year,
              track, disc, format, bitrate, length, file_size, added, mtime)
             SELECT ?1, id, ?2, ?3, ?4, album, albumartist, year,
                    ?1, 1, 'FLAC', 900, ?5, ?6, added, added
             FROM albums WHERE id = ?7",
            params![
                id,
                format!("/missing/{id}.flac"),
                title,
                artist,
                length,
                size,
                album
            ],
        )?;
    }
    Ok(config)
}

fn ids(config: &Path, arguments: &[&str]) -> Result<Vec<i64>, Box<dyn std::error::Error>> {
    let mut all = vec!["--output", "json"];
    all.extend_from_slice(arguments);
    let output = run(config, &all)?;
    successful(&output);
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    rows.into_iter()
        .map(|row| row["id"].as_i64().ok_or_else(|| "missing row ID".into()))
        .collect()
}

#[test]
fn list_preserves_shell_phrases_and_combines_separate_filters() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    assert_eq!(
        ids(&config, &["ls", "artist:Air", "title:light", "track+"])?,
        [1, 2]
    );
    assert_eq!(ids(&config, &["ls", "Northern Lights"])?, [1]);
    assert_eq!(ids(&config, &["ls", "title:=Northern Lights"])?, [1]);
    assert_eq!(ids(&config, &["ls", "orth", "track+"])?, [1, 3]);
    assert_eq!(ids(&config, &["ls", "100%_done"])?, [4]);
    assert_eq!(ids(&config, &["ls", "artist:=~air", "track+"])?, [2, 3]);
    assert_eq!(ids(&config, &["ls", ":^Northern", "track+"])?, [1, 3]);
    assert_eq!(
        ids(
            &config,
            &[
                "ls",
                "title:=Northern Lights,",
                "title:=Northern Sky",
                "track+"
            ]
        )?,
        [1, 3]
    );
    Ok(())
}

#[test]
fn album_list_filters_and_sorts_before_limit() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    assert_eq!(ids(&config, &["ls", "-a", "year:1970"])?, [2]);
    assert_eq!(ids(&config, &["ls", "-a", "year+", "-l", "1"])?, [2]);
    assert_eq!(
        ids(&config, &["ls", "-a", "album:Late", "albumartist:Supply"])?,
        [1]
    );
    assert_eq!(ids(&config, &["ls", "-a", "title:=Northern Lights"])?, [1]);
    assert_eq!(ids(&config, &["ls", "-a", "Early Album"])?, [2]);
    Ok(())
}

#[test]
fn native_numeric_filters_compare_numbers_instead_of_substrings() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let connection = Connection::open(temporary.path().join("library.db"))?;
    connection.execute(
        "UPDATE items SET track = 11, singleton = 1 WHERE id = 4",
        [],
    )?;
    drop(connection);
    assert_eq!(ids(&config, &["ls", "track:1"])?, [1]);
    assert_eq!(ids(&config, &["ls", "year:70"])?, Vec::<i64>::new());
    assert_eq!(ids(&config, &["ls", "length:2"])?, Vec::<i64>::new());
    assert_eq!(ids(&config, &["ls", "file_size:100..200", "id+"])?, [1, 2]);
    assert_eq!(ids(&config, &["ls", "singleton:true"])?, [4]);
    assert_eq!(ids(&config, &["ls", "year::^1970$", "id+"])?, [2, 3]);
    Ok(())
}

#[test]
fn album_art_paths_filter_tracks_through_the_album() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let connection = Connection::open(temporary.path().join("library.db"))?;
    connection.execute(
        "UPDATE albums SET artpath = '/covers/early.jpg' WHERE id = 2",
        [],
    )?;
    drop(connection);
    assert_eq!(ids(&config, &["ls", "artpath:early", "id+"])?, [2, 3]);
    assert_eq!(ids(&config, &["ls", "-a", "artpath:early"])?, [2]);
    Ok(())
}

#[test]
fn list_paths_and_formats_preserve_metadata_text() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let output = run(&config, &["ls", "-p", "title:=Northern Lights"])?;
    successful(&output);
    assert_eq!(String::from_utf8(output.stdout)?, "/missing/1.flac\n");
    let output = run(
        &config,
        &["ls", "-f", "${artist}: $title ($year) $$", "track:=4"],
    )?;
    successful(&output);
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "Else: 100%_done / Mix (2001) $\n"
    );
    let output = run(
        &config,
        &["ls", "-a", "-f", "$albumartist - $album", "year:1970"],
    )?;
    successful(&output);
    assert_eq!(String::from_utf8(output.stdout)?, "Air - Early Album\n");
    Ok(())
}

#[test]
fn statistics_apply_filters_without_loading_or_reading_files() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let before = std::fs::read(temporary.path().join("library.db"))?;
    let output = run(
        &config,
        &[
            "--output",
            "json",
            "stats",
            "album:=Early Album",
            "length:180..240",
        ],
    )?;
    successful(&output);
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(stats["tracks"], 2);
    assert_eq!(stats["albums"], 1);
    assert_eq!(stats["artists"], 2);
    assert_eq!(stats["total_length"], 420.0);
    assert_eq!(stats["total_size"], 200);
    assert_eq!(stats["unknown_sizes"], 1);
    assert_eq!(std::fs::read(temporary.path().join("library.db"))?, before);
    let output = run(&config, &["--output", "json", "stats", "title:=absent"])?;
    successful(&output);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout)?["tracks"],
        0
    );
    Ok(())
}

#[test]
fn mutating_commands_share_shell_query_rules() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let before = std::fs::read(temporary.path().join("library.db"))?;
    let output = run(
        &config,
        &[
            "--output",
            "json",
            "rm",
            "--dry-run",
            "artist:Air",
            "title:light",
        ],
    )?;
    successful(&output);
    let preview: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(preview["plan"]["items"].as_array().map(Vec::len), Some(2));
    for command in ["write", "move", "update", "verify"] {
        let mut arguments = vec!["--output", "json", command, "artist:Air", "title:=absent"];
        if matches!(command, "write" | "move") {
            arguments.push("--dry-run");
        }
        successful(&run(&config, &arguments)?);
    }
    assert_eq!(std::fs::read(temporary.path().join("library.db"))?, before);
    let output = run(
        &config,
        &[
            "--output",
            "json",
            "modify",
            "--yes",
            "artist:Air",
            "title:light",
            "genre=Ambient",
        ],
    )?;
    successful(&output);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout)?["modified"],
        2
    );
    assert_eq!(ids(&config, &["ls", "genre:=Ambient", "track+"])?, [1, 2]);
    successful(&run(
        &config,
        &["modify", "--yes", "title:=100%_done / Mix", "genre=Rock"],
    )?);
    assert_eq!(ids(&config, &["ls", "genre:=Rock"])?, [4]);
    Ok(())
}

#[test]
fn global_library_paths_override_configuration() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let database = temporary.path().join("other.db");
    let directory = temporary.path().join("other-music");
    let output = Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(&config)
        .arg("-l")
        .arg(&database)
        .arg("-d")
        .arg(&directory)
        .args(["--output", "json", "stats"])
        .output()?;
    successful(&output);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout)?["tracks"],
        0
    );
    assert_eq!(ids(&config, &["ls"])?.len(), 4);
    assert!(!directory.exists());
    Ok(())
}

#[test]
fn config_and_field_discovery_do_not_create_a_library() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'music'\ndatabase = 'state/library.db'\n",
    )?;
    let output = run(&config, &["config"])?;
    successful(&output);
    let loaded: rsbts::config::Config = toml::from_str(std::str::from_utf8(&output.stdout)?)?;
    assert_eq!(
        loaded.library.database,
        temporary.path().join("state/library.db")
    );
    let output = run(&config, &["config", "-p"])?;
    successful(&output);
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        config.to_str().ok_or("config path is not UTF-8")?
    );
    let output = run(&config, &["--output", "json", "fields"])?;
    successful(&output);
    let fields: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(fields["item"]
        .as_array()
        .ok_or("missing item fields")?
        .contains(&serde_json::json!("title")));
    assert!(fields["album"]
        .as_array()
        .ok_or("missing album fields")?
        .contains(&serde_json::json!("albumartist")));
    assert!(!temporary.path().join("state").exists());
    assert!(!temporary.path().join("music").exists());
    Ok(())
}

#[test]
fn invalid_read_commands_fail_before_database_creation() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    std::fs::write(&config, "[library]\ndatabase = 'state/library.db'\n")?;
    for arguments in [
        vec!["ls", "-a", "unknown:value"],
        vec!["stats", "artist:"],
        vec!["stats", "year:NaN"],
        vec!["ls", "track:broken..3"],
        vec!["ls", "-f", "${title"],
        vec!["--output", "json", "ls", "-p"],
    ] {
        let output = run(&config, &arguments)?;
        assert!(
            !output.status.success(),
            "invalid command passed: {arguments:?}"
        );
        assert!(!temporary.path().join("state").exists());
    }
    Ok(())
}

fn page_metadata(id: i64) -> Result<rsbts::ExtendedMetadata, rsbts::Error> {
    if id % 11 == 0 {
        return Ok(rsbts::ExtendedMetadata::default());
    }
    let mut metadata = rsbts::ExtendedMetadata {
        artists: vec![format!("Artist {id}"), "Guest".into()],
        ..rsbts::ExtendedMetadata::default()
    };
    metadata
        .flexible_fields
        .insert("0rating".into(), rsbts::FlexibleValue::Integer(id));
    metadata
        .flexible_fields
        .insert("mood".into(), rsbts::FlexibleValue::String("Calm".into()));
    metadata.external_ids.push(rsbts::ExternalId::new_typed(
        "fixture",
        "release",
        format!("release-{id}"),
    )?);
    Ok(metadata)
}

#[test]
fn metadata_pages_keep_core_custom_fields_and_identifiers_separate() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let database = temporary.path().join("library.db");
    drop(Library::open(&database)?);
    let mut connection = Connection::open(&database)?;
    let transaction = connection.transaction()?;
    for id in 1..=600 {
        transaction.execute(
            "INSERT INTO albums (id, album, albumartist, added)
             VALUES (?1, 'Album', 'Artist', '2026-01-01T00:00:00Z')",
            [id],
        )?;
        transaction.execute(
            "INSERT INTO items (id, album_id, path, title, artist, album, format, bitrate, length, added, mtime)
             VALUES (?1, ?1, ?2, 'Track', 'Artist', 'Album', 'FLAC', 900, 1.0,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')", params![id, format!("/missing/{id}.flac")],
        )?;
        if id % 11 == 0 {
            continue;
        }
        let expected = page_metadata(id)?;
        let mut core = expected.clone();
        core.flexible_fields
            .insert("0rating".into(), rsbts::FlexibleValue::Integer(-1));
        core.external_ids = vec![rsbts::ExternalId::new_typed(
            "stale", "release", "obsolete",
        )?];
        for entity in ["item", "album"] {
            transaction.execute(
                "INSERT INTO entity_metadata (entity_type, entity_id, field, value_type, value_json)
                 VALUES (?1, ?2, '__core', 'string', ?3), (?1, ?2, '0rating', 'integer', ?4)",
                params![entity, id, serde_json::to_string(&core)?, serde_json::to_string(&expected.flexible_fields["0rating"])?],
            )?;
            transaction.execute(
                "INSERT INTO library_external_ids (entity_type, entity_id, provider, kind, value)
                 VALUES (?1, ?2, 'fixture', 'release', ?3)",
                params![entity, id, format!("release-{id}")],
            )?;
        }
    }
    transaction.commit()?;
    drop(connection);
    let library = Library::open_read_only(&database)?;
    let query = rsbts::query::Query::parse("id+")?;
    let mut item_count = 0;
    library.visit_items(&query, 600, |item| {
        item_count += 1;
        assert_eq!(item.id, Some(item_count));
        assert_eq!(item.extended, page_metadata(item_count)?);
        Ok(())
    })?;
    let mut album_count = 0;
    library.visit_albums(&query, 600, |album| {
        album_count += 1;
        assert_eq!(album.id, Some(album_count));
        assert_eq!(album.extended, page_metadata(album_count)?);
        Ok(())
    })?;
    assert_eq!((item_count, album_count), (600, 600));
    assert_eq!(
        library.query_items_bounded(&query, 600)?[599].extended,
        page_metadata(600)?
    );
    Ok(())
}

#[test]
fn modify_previews_changes_and_requires_explicit_approval() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let database = temporary.path().join("library.db");
    let before = std::fs::read(&database)?;

    let output = run(
        &config,
        &["modify", "--dry-run", "artist:Air", "genre=Ambient"],
    )?;
    successful(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Modification plan: 3 item(s) to change"),
        "{stdout}"
    );
    assert!(stdout.contains("genre: null -> \"Ambient\""), "{stdout}");
    assert!(stdout.contains("No changes made"), "{stdout}");
    assert_eq!(std::fs::read(&database)?, before);

    for arguments in [
        vec!["modify", "artist:Air", "genre=Ambient"],
        vec!["--output", "json", "modify", "artist:Air", "genre=Ambient"],
    ] {
        let output = run(&config, &arguments)?;
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert_eq!(std::fs::read(&database)?, before);
    }

    let modified = |config: &Path| -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let output = run(
            config,
            &[
                "--output",
                "json",
                "modify",
                "--yes",
                "artist:Air",
                "genre=Ambient",
            ],
        )?;
        successful(&output);
        Ok(serde_json::from_slice::<serde_json::Value>(&output.stdout)?["modified"].clone())
    };
    assert_eq!(modified(&config)?, 3);
    assert_eq!(
        ids(&config, &["ls", "genre:=Ambient", "track+"])?,
        [1, 2, 3]
    );
    assert_eq!(modified(&config)?, 0);
    Ok(())
}

#[test]
fn modify_keeps_identifiers_for_unchanged_fields() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let database = temporary.path().join("library.db");
    let recording = "8f3471b5-7e6a-48da-86a9-c1c07a0f47ae";
    Connection::open(&database)?.execute(
        "UPDATE items
         SET mb_trackid = ?1, external_track_id = ?1, metadata_provider = 'musicbrainz'
         WHERE id = 1",
        [recording],
    )?;
    let stored = || -> rusqlite::Result<Option<String>> {
        Connection::open(&database)?.query_row(
            "SELECT external_track_id FROM items WHERE id = 1",
            [],
            |row| row.get(0),
        )
    };

    successful(&run(
        &config,
        &[
            "modify",
            "--yes",
            "id:1",
            "title=Northern Lights",
            "genre=Pop",
        ],
    )?);
    assert_eq!(stored()?.as_deref(), Some(recording));

    let output = run(&config, &["modify", "--dry-run", "id:1", "title=Retitled"])?;
    successful(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("clears stored track identifiers"));
    successful(&run(
        &config,
        &["modify", "--yes", "id:1", "title=Retitled"],
    )?);
    assert_eq!(stored()?, None);
    Ok(())
}

#[test]
fn update_dry_run_leaves_the_catalog_unchanged() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let config = fixture(temporary.path())?;
    let database = temporary.path().join("library.db");
    let before = std::fs::read(&database)?;

    for flag in ["--dry-run", "--pretend"] {
        let output = run(&config, &["update", flag])?;
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("0 item(s) with core field changes; 4 failed"),
            "{stdout}"
        );
        assert!(stdout.contains("No changes made"), "{stdout}");
        assert_eq!(std::fs::read(&database)?, before);
    }
    Ok(())
}
