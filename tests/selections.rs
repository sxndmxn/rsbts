use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rsbts::db::Library;
use rsbts::query::Query;

fn catalog_fixture(
    directory: &Path,
    count: u32,
) -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let config = directory.join("config.toml");
    let database = directory.join("library.db");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n",
    )?;
    drop(Library::open(&database)?);
    let connection = rusqlite::Connection::open(&database)?;
    connection.execute(
        "WITH RECURSIVE records(id) AS (
             SELECT 1 UNION ALL SELECT id + 1 FROM records WHERE id < ?1
         )
         INSERT INTO items
         (path, title, artist, album, year, track, format, bitrate, length, added, mtime)
         SELECT ?2 || id || '.wav', printf('Track %05d', id),
                CASE WHEN id % 2 = 0 THEN 'B' ELSE 'A' END, 'Album',
                2000 + id % 3, id, 'WAV', 0, 0,
                '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'
         FROM records",
        rusqlite::params![count, directory.join("missing-").to_string_lossy()],
    )?;
    Ok((config, database))
}

fn run(config: &Path, arguments: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(config)
        .args(arguments)
        .output()
}

#[test]
fn sort_only_mutations_fail_before_database_open() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'state/library.db'\n",
    )?;
    for query in ["artist+", "year- title+", "  path- \n track+ "] {
        for arguments in [
            vec!["rm", query, "--yes"],
            vec!["rm", query, "--delete", "--dry-run"],
            vec!["modify", query, "title=Changed"],
            vec!["write", query, "--yes"],
            vec!["write", query, "--dry-run"],
            vec!["move", query, "--yes"],
            vec!["move", query, "--dry-run"],
        ] {
            let output = run(&config, &arguments)?;
            assert_eq!(output.status.code(), Some(1), "{arguments:?}: {output:?}");
            assert!(String::from_utf8(output.stderr)?.contains("explicit filter"));
            assert!(!temporary.path().join("state").exists());
            assert!(!temporary.path().join("organized").exists());
        }
    }
    Ok(())
}

#[test]
fn sorted_mutations_require_a_filter_and_keep_its_scope() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = tempfile::tempdir()?;
    let (config, database) = catalog_fixture(temporary.path(), 3)?;
    let before = std::fs::read(&database)?;
    let output = run(&config, &["modify", "artist+", "title=Changed"])?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(std::fs::read(&database)?, before);

    let output = run(&config, &["modify", "artist:=A title-", "title=Changed"])?;
    assert!(output.status.success(), "{output:?}");
    let library = Library::open_read_only(&database)?;
    let items = library.query_items_page(&Query::all(), None, 10)?;
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].title, "Changed");
    assert_eq!(items[1].title, "Track 00002");
    assert_eq!(items[2].title, "Changed");
    Ok(())
}

#[test]
fn disjunction_pages_never_repeat_or_omit_rows() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let (_config, database) = catalog_fixture(temporary.path(), 6)?;
    let library = Library::open_read_only(&database)?;
    let query = Query::parse("artist:=A , artist:=B")?;
    let mut after = None;
    let mut ids = Vec::new();
    for expected in 1..=6 {
        let page = library.query_items_page(&query, after, 1)?;
        assert_eq!(page.len(), 1);
        let id = page[0].id.ok_or("missing item ID")?;
        assert_eq!(id, expected);
        ids.push(id);
        after = Some(id);
    }
    assert!(library.query_items_page(&query, after, 1)?.is_empty());
    assert_eq!(ids, [1, 2, 3, 4, 5, 6]);
    Ok(())
}

#[test]
fn listings_apply_sort_terms_before_the_limit() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let (config, database) = catalog_fixture(temporary.path(), 1_005)?;
    let before = std::fs::read(&database)?;
    for format in ["json", "jsonl"] {
        for query in ["year+ title-", "year- title+", "artist+"] {
            let output = run(
                &config,
                &["--output", format, "ls", query, "--limit", "1001"],
            )?;
            assert!(output.status.success(), "{output:?}");
            let actual: Vec<serde_json::Value> = if format == "json" {
                serde_json::from_slice(&output.stdout)?
            } else {
                std::str::from_utf8(&output.stdout)?
                    .lines()
                    .map(serde_json::from_str)
                    .collect::<Result<_, _>>()?
            };
            let actual_ids = actual
                .iter()
                .map(|item| item["id"].as_i64().ok_or("missing item ID"))
                .collect::<Result<Vec<_>, _>>()?;
            let library = Library::open_read_only(&database)?;
            let expected_ids = library
                .query_items(&Query::parse(query)?)?
                .into_iter()
                .take(1_001)
                .map(|item| item.id.ok_or("missing expected item ID"))
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(actual_ids, expected_ids, "{format}: {query}");
        }
    }
    assert_eq!(std::fs::read(database)?, before);
    Ok(())
}

#[test]
fn listings_preserve_stored_extended_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let (config, database) = catalog_fixture(temporary.path(), 2)?;
    let mut expected = rsbts::ExtendedMetadata {
        date: rsbts::PartialDate {
            year: Some(2012),
            month: Some(4),
            day: Some(7),
        },
        original_date: rsbts::PartialDate {
            year: Some(1972),
            ..rsbts::PartialDate::default()
        },
        artists: vec!["Artist".into(), "Guest".into()],
        album_artists: vec!["Album Artist".into(), "Guest".into()],
        genres: vec!["Rock".into(), "Jazz".into()],
        ..rsbts::ExtendedMetadata::default()
    };
    let core = serde_json::to_string(&expected)?;
    let mood = rsbts::FlexibleValue::String("Calm".into());
    expected.flexible_fields.insert("mood".into(), mood.clone());
    expected.external_ids.push(rsbts::ExternalId::new_typed(
        "fixture",
        "release",
        "fixture-release",
    )?);
    let connection = rusqlite::Connection::open(&database)?;
    connection.execute(
        "INSERT INTO albums (id, album, albumartist, year, added)
         VALUES (1, 'Album', 'Album Artist', 2012, '2026-01-01T00:00:00+00:00')",
        [],
    )?;
    for (entity_type, id) in [("item", 2), ("album", 1)] {
        connection.execute(
            "INSERT INTO entity_metadata
             (entity_type, entity_id, field, ordinal, value_type, value_json)
             VALUES (?1, ?2, '__core', 0, 'string', ?3),
                    (?1, ?2, 'mood', 0, 'string', ?4)",
            rusqlite::params![entity_type, id, core, serde_json::to_string(&mood)?],
        )?;
        connection.execute(
            "INSERT INTO library_external_ids (entity_type, entity_id, provider, kind, value)
             VALUES (?1, ?2, 'fixture', 'release', 'fixture-release')",
            rusqlite::params![entity_type, id],
        )?;
    }
    drop(connection);
    let before = std::fs::read(&database)?;
    let mut failures = Vec::new();
    for format in ["json", "jsonl"] {
        for selection in ["title-", "--album"] {
            let output = run(
                &config,
                &["--output", format, "ls", selection, "--limit", "1"],
            )?;
            assert!(output.status.success(), "{output:?}");
            let records: Vec<serde_json::Value> = if format == "json" {
                serde_json::from_slice(&output.stdout)?
            } else {
                std::str::from_utf8(&output.stdout)?
                    .lines()
                    .map(serde_json::from_str)
                    .collect::<Result<_, _>>()?
            };
            assert_eq!(records.len(), 1);
            let actual: rsbts::ExtendedMetadata =
                serde_json::from_value(records[0]["extended"].clone())?;
            if actual != expected {
                failures.push(format!("{format} {selection}: {actual:?}"));
            }
            assert_eq!(std::fs::read(&database)?, before);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

#[test]
fn oversized_file_plans_fail_before_reading_media() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let (config, database) = catalog_fixture(temporary.path(), 10_000)?;
    let before = std::fs::read(&database)?;
    for operation in ["write", "move"] {
        let output = run(&config, &[operation, "--all", "--dry-run"])?;
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8(output.stderr)?
                .contains("selection exceeds the atomic command limit of 9999 items"),
            "operation: {operation}"
        );
        assert_eq!(std::fs::read(&database)?, before);
    }
    assert!(!temporary.path().join("organized").exists());
    Ok(())
}

#[test]
fn oversized_purge_plans_fail_without_changing_the_catalog(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let (config, database) = catalog_fixture(temporary.path(), 1)?;
    let connection = rusqlite::Connection::open(&database)?;
    connection.execute(
        "INSERT INTO operation_journal (id, kind, state, created_at, updated_at, completed_at)
         VALUES ('retained-fixture', 'remove-delete', 'complete', ?1, ?1, ?1)",
        ["2020-01-01T00:00:00+00:00"],
    )?;
    connection.execute(
        "WITH RECURSIVE records(id) AS (
             SELECT 1 UNION ALL SELECT id + 1 FROM records WHERE id < 10000
         )
         INSERT INTO operation_files
         (operation_id, ordinal, source_path, staged_path, destination_path, state)
         SELECT 'retained-fixture', id, ?1 || id, ?1 || id, ?1 || id, 'quarantined'
         FROM records",
        [temporary.path().join("quarantine-").to_string_lossy()],
    )?;
    drop(connection);
    let before = std::fs::read(&database)?;
    let output = run(&config, &["purge", "--older-than-days", "0", "--dry-run"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)?
        .contains("purge selection exceeds the atomic command limit of 9999 files"));
    assert_eq!(std::fs::read(&database)?, before);

    let connection = rusqlite::Connection::open(&database)?;
    connection.execute(
        "UPDATE operation_files SET state = 'purged' WHERE ordinal = 10000",
        [],
    )?;
    drop(connection);
    let library = Library::open_read_only(&database)?;
    let plan = rsbts::remove::PurgePlan::build(&library, 0)?;
    assert_eq!(plan.len(), 9_999);
    assert!(plan.paths().all(|path| path.starts_with(temporary.path())));
    Ok(())
}
