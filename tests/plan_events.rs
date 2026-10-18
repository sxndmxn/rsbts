use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rsbts::db::Library;
use rsbts::operations::{PlanId, PlanKind};
use serde_json::json;

fn run(config: &Path, arguments: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(config)
        .args(arguments)
        .output()
}

fn create_plan(library: &Library, event_count: u64) -> rsbts::Result<PlanId> {
    let id = library.create_durable_plan(PlanKind::Audit, &json!({}), &json!({}), None)?;
    library.approve_durable_plan(&id)?;
    library.start_durable_plan(&id)?;
    for current in 3..event_count {
        library.update_plan_progress(&id, current, None)?;
    }
    Ok(id)
}

fn cli_fixture(
    directory: &Path,
    event_count: u64,
) -> Result<(PathBuf, PathBuf, PlanId), Box<dyn std::error::Error>> {
    let config = directory.join("config.toml");
    let database = directory.join("library.db");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n",
    )?;
    let library = Library::open(&database)?;
    let id = create_plan(&library, event_count)?;
    Ok((config, database, id))
}

#[test]
fn default_cli_event_page_is_bounded_in_every_output_format(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let (config, database, id) = cli_fixture(temporary.path(), 600)?;
    let before = std::fs::read(&database)?;
    for format in ["json", "jsonl", "text"] {
        let output = run(
            &config,
            &["--output", format, "plan", "events", id.as_str()],
        )?;
        assert!(output.status.success(), "{output:?}");
        let count = if format == "json" {
            serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)?.len()
        } else {
            std::str::from_utf8(&output.stdout)?.lines().count()
        };
        assert_eq!(count, 512, "unbounded {format} event output");
        assert_eq!(std::fs::read(&database)?, before);
    }
    Ok(())
}

#[test]
fn cli_event_cursor_and_limit_preserve_output_forms() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let (config, database, id) = cli_fixture(temporary.path(), 6)?;
    let before = std::fs::read(&database)?;
    for format in ["json", "jsonl", "text"] {
        let output = run(
            &config,
            &[
                "--output",
                format,
                "plan",
                "events",
                id.as_str(),
                "--after-sequence",
                "1",
                "--limit",
                "2",
            ],
        )?;
        assert!(output.status.success(), "{output:?}");
        let sequences = match format {
            "json" => serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)?
                .into_iter()
                .map(|event| event["sequence"].as_u64().ok_or("missing sequence"))
                .collect::<Result<Vec<_>, _>>()?,
            "jsonl" => std::str::from_utf8(&output.stdout)?
                .lines()
                .map(|line| -> Result<u64, Box<dyn std::error::Error>> {
                    let event = serde_json::from_str::<serde_json::Value>(line)?;
                    Ok(event["sequence"].as_u64().ok_or("missing sequence")?)
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => std::str::from_utf8(&output.stdout)?
                .lines()
                .map(|line| -> Result<u64, Box<dyn std::error::Error>> {
                    Ok(line
                        .split_once(' ')
                        .ok_or("missing event fields")?
                        .0
                        .parse()?)
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        assert_eq!(sequences, [2, 3], "{format}");
        assert_eq!(std::fs::read(&database)?, before);
    }
    Ok(())
}

#[test]
fn invalid_cli_event_bounds_fail_before_database_open() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'state/library.db'\n",
    )?;
    let id = PlanId::new();
    for (option, value) in [
        ("--limit", "0"),
        ("--limit", "4097"),
        ("--limit", "4294967295"),
        ("--after-sequence", "9223372036854775808"),
        ("--after-sequence", "18446744073709551615"),
    ] {
        let output = run(&config, &["plan", "events", id.as_str(), option, value])?;
        assert_eq!(
            output.status.code(),
            Some(1),
            "{option} {value}: {output:?}"
        );
        assert!(String::from_utf8(output.stderr)?.contains("plan event"));
        assert!(!temporary.path().join("state").exists());
    }
    Ok(())
}

#[test]
fn event_pages_use_exclusive_cursors_and_preserve_the_legacy_api(
) -> Result<(), Box<dyn std::error::Error>> {
    let library = Library::open_in_memory()?;
    let id = create_plan(&library, 6)?;
    create_plan(&library, 3)?;
    for (after, limit, expected) in [
        (None, 2, vec![0, 1]),
        (Some(0), 1, vec![1]),
        (Some(1), 2, vec![2, 3]),
        (Some(3), 4096, vec![4, 5]),
        (Some(5), 2, vec![]),
        (Some(i64::MAX.cast_unsigned()), 1, vec![]),
    ] {
        let events = library.plan_events_page(&id, after, limit)?;
        assert_eq!(
            events
                .iter()
                .map(rsbts::operations::PlanEvent::sequence)
                .collect::<Vec<_>>(),
            expected,
            "after {after:?}, limit {limit}"
        );
    }
    assert_eq!(library.plan_events(&id)?.len(), 6);
    for limit in [0, 4097, u32::MAX] {
        assert!(library.plan_events_page(&id, None, limit).is_err());
    }
    for after in [i64::MAX.cast_unsigned() + 1, u64::MAX] {
        assert!(library.plan_events_page(&id, Some(after), 1).is_err());
    }
    Ok(())
}

#[test]
fn event_page_does_not_read_events_outside_its_sequence_range(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let database = temporary.path().join("library.db");
    let library = Library::open(&database)?;
    let id = create_plan(&library, 6)?;
    let connection = rusqlite::Connection::open(&database)?;
    connection.execute(
        "UPDATE plan_events SET created_at = 'invalid' WHERE plan_id = ?1 AND sequence IN (0, 5)",
        [id.as_str()],
    )?;
    let page = library.plan_events_page(&id, Some(0), 2);
    assert!(
        page.is_ok(),
        "the page read events outside its sequence range: {page:?}"
    );
    assert_eq!(page?.len(), 2);
    assert!(library.plan_events(&id).is_err());
    Ok(())
}
