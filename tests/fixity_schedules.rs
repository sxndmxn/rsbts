use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rsbts::db::Library;
use rsbts::fixity::FixityMode;

fn run(config: &Path, arguments: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_rsbts"))
        .arg("--config")
        .arg(config)
        .args(arguments)
        .output()
}

#[test]
fn invalid_fixity_intervals_fail_before_database_open() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let config = temporary.path().join("config.toml");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'state/library.db'\n",
    )?;
    for seconds in [
        "0",
        "9223372036854775807",
        "18446744073709551615",
        "10000000000000",
    ] {
        let output = run(
            &config,
            &["fixity", "schedule", "--interval-seconds", seconds],
        )?;
        assert_eq!(output.status.code(), Some(1), "{seconds}: {output:?}");
        assert!(String::from_utf8(output.stderr)?.contains("fixity"));
        assert!(!temporary.path().join("state").exists(), "{seconds}");
    }
    Ok(())
}

#[test]
fn invalid_schedule_dates_do_not_persist() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let library = Library::open(&temporary.path().join("library.db"))?;
    for (seconds, first) in [
        (0, Utc::now()),
        (9_223_372_036_854_775_807, Utc::now()),
        (u64::MAX, Utc::now()),
        (10_000_000_000_000, Utc::now()),
        (1, DateTime::<Utc>::MAX_UTC),
    ] {
        assert!(
            library
                .schedule_fixity(FixityMode::Deep, Duration::from_secs(seconds), first)
                .is_err(),
            "accepted interval {seconds} at {first}"
        );
        assert!(library.fixity_schedules_page(None, 10)?.is_empty());
    }
    let now = Utc::now();
    let id = library.schedule_fixity(FixityMode::Quick, Duration::from_secs(60), now)?;
    assert_eq!(library.plan_due_fixity(now, 10)?.len(), 1);
    assert_eq!(
        library.plan_due_fixity(now, 10)?,
        [] as [rsbts::fixity::FixityPlan; 0]
    );
    let schedules = library.fixity_schedules_page(None, 10)?;
    assert_eq!(schedules[0].id(), &id);
    assert_eq!((schedules[0].next_run_at() - now).num_seconds(), 60);
    Ok(())
}

#[test]
fn invalid_stored_intervals_fail_without_panicking_or_partial_plans(
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let database = temporary.path().join("library.db");
    let config = temporary.path().join("config.toml");
    std::fs::write(
        &config,
        "[library]\ndirectory = 'organized'\ndatabase = 'library.db'\n",
    )?;
    let library = Library::open(&database)?;
    let earlier = "2020-01-01T00:00:00+00:00".parse::<DateTime<Utc>>()?;
    let later = "2021-01-01T00:00:00+00:00".parse::<DateTime<Utc>>()?;
    library.schedule_fixity(FixityMode::Quick, Duration::from_secs(60), earlier)?;
    let invalid = library.schedule_fixity(FixityMode::Quick, Duration::from_secs(60), later)?;
    drop(library);

    let connection = rusqlite::Connection::open(&database)?;
    connection.execute(
        "UPDATE fixity_schedules SET interval_seconds = ?1 WHERE id = ?2",
        rusqlite::params![i64::MAX, invalid.as_str()],
    )?;
    drop(connection);

    let output = run(&config, &["fixity", "due"])?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!String::from_utf8(output.stderr)?.contains("panicked"));
    let connection = rusqlite::Connection::open(&database)?;
    let count: u64 =
        connection.query_row("SELECT COUNT(*) FROM durable_plans", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    let advanced: u64 = connection.query_row(
        "SELECT COUNT(*) FROM fixity_schedules WHERE last_plan_id IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(advanced, 0);
    drop(connection);

    let library = Library::open(&database)?;
    library.set_fixity_schedule_enabled(&invalid, false)?;
    assert_eq!(library.plan_due_fixity(Utc::now(), 10)?.len(), 1);
    Ok(())
}
