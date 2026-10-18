use chrono::Utc;
use rsbts::catalog::DataLicense;
use rsbts::db::Library;
use rusqlite::{params, Connection};
use serde_json::json;

#[test]
fn stale_completion_cannot_complete_a_reclaimed_attempt() -> Result<(), Box<dyn std::error::Error>>
{
    let mut library = Library::open_in_memory()?;
    let request = json!({"release": "generated-fixture"});
    let id = library.enqueue_provider_job("test", "lookup-release", &request, Utc::now())?;
    let original = library.claim_provider_job("test")?.ok_or("missing job")?;
    let stale = original.clone();
    library.recover_pending()?;
    let current = library.claim_provider_job("test")?.ok_or("missing retry")?;
    assert_eq!(current.id(), original.id());
    let snapshot = library.store_provider_snapshot(
        "test",
        "generated-fixture",
        None,
        &DataLicense::UserOwned,
        b"{}",
        true,
    )?;

    assert!(
        library
            .complete_provider_job(&stale, snapshot.id().as_str())
            .is_err(),
        "an old attempt completed the current attempt"
    );
    library.complete_provider_job(&current, snapshot.id().as_str())?;
    assert!(library.claim_provider_job("test")?.is_none());
    assert_eq!(
        library.enqueue_provider_job("test", "lookup-release", &request, Utc::now())?,
        id
    );
    Ok(())
}

#[test]
fn stale_failure_cannot_change_a_reclaimed_attempt() -> Result<(), Box<dyn std::error::Error>> {
    for retriable in [false, true] {
        let mut library = Library::open_in_memory()?;
        library.enqueue_provider_job(
            "test",
            "lookup-release",
            &json!({"release": "generated-fixture"}),
            Utc::now(),
        )?;
        let original = library.claim_provider_job("test")?.ok_or("missing job")?;
        let stale = original.clone();
        library.recover_pending()?;
        let current = library.claim_provider_job("test")?.ok_or("missing retry")?;
        assert_eq!(current.id(), original.id());

        assert!(
            library
                .fail_provider_job(&stale, "old failure", retriable)
                .is_err(),
            "an old attempt changed the current attempt with retriable={retriable}"
        );
        library.fail_provider_job(&current, "current failure", false)?;
        assert!(library.claim_provider_job("test")?.is_none());
    }
    Ok(())
}

#[test]
fn exhausted_attempt_counter_does_not_claim_the_job() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let database = temporary.path().join("library.db");
    let library = Library::open(&database)?;
    let id = library.enqueue_provider_job("test", "lookup", &json!({}), Utc::now())?;
    let connection = Connection::open(&database)?;
    connection.execute(
        "UPDATE provider_jobs SET attempt_count = ?1 WHERE id = ?2",
        params![u32::MAX, id.as_str()],
    )?;

    assert!(
        library.claim_provider_job("test").is_err(),
        "a claim exceeded the attempt counter range"
    );
    let stored: (String, u64, Option<String>) = connection.query_row(
        "SELECT state, attempt_count, started_at FROM provider_jobs WHERE id = ?1",
        [id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(stored, ("queued".into(), u64::from(u32::MAX), None));
    Ok(())
}

#[test]
fn last_supported_attempt_can_complete() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let database = temporary.path().join("library.db");
    let library = Library::open(&database)?;
    let id = library.enqueue_provider_job("test", "lookup", &json!({}), Utc::now())?;
    let connection = Connection::open(&database)?;
    connection.execute(
        "UPDATE provider_jobs SET attempt_count = ?1 WHERE id = ?2",
        params![u32::MAX - 1, id.as_str()],
    )?;
    let job = library.claim_provider_job("test")?.ok_or("missing job")?;
    assert_eq!(serde_json::to_value(&job)?["attempt_count"], u32::MAX);
    let snapshot = library.store_provider_snapshot(
        "test",
        "generated-fixture",
        None,
        &DataLicense::UserOwned,
        b"{}",
        true,
    )?;
    library.complete_provider_job(&job, snapshot.id().as_str())?;
    assert!(library.claim_provider_job("test")?.is_none());
    Ok(())
}

#[test]
fn invalid_stored_fields_do_not_claim_the_job() -> Result<(), Box<dyn std::error::Error>> {
    for update in [
        "UPDATE provider_jobs SET id = 'invalid-id'",
        "UPDATE provider_jobs SET request_json = '1e9999'",
        "UPDATE provider_jobs SET operation = ''",
    ] {
        let temporary = tempfile::tempdir()?;
        let database = temporary.path().join("library.db");
        let library = Library::open(&database)?;
        library.enqueue_provider_job("test", "lookup", &json!({}), Utc::now())?;
        let connection = Connection::open(&database)?;
        connection.execute(update, [])?;

        assert!(library.claim_provider_job("test").is_err(), "{update}");
        let stored: (String, u32, Option<String>) = connection.query_row(
            "SELECT state, attempt_count, started_at FROM provider_jobs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(stored, ("queued".into(), 0, None), "{update}");
    }
    Ok(())
}
