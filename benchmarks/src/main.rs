use std::process::Command;
use std::time::{Duration, Instant};

use rsbts::db::Library;
use rsbts::query::Query;
use rsbts::remove::RemovalPlan;
use rusqlite::{params, Connection};

const DEFAULT_TRACKS: u64 = 1_000_000;
const SAMPLE_COUNT: usize = 30;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(database) = std::env::var("RSBTS_SCALE_CHILD_DB") {
        return measure_dry_run_rss(std::path::Path::new(&database));
    }

    let track_count = std::env::var("RSBTS_SCALE_TRACKS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TRACKS);
    let temporary = if let Some(directory) = std::env::var_os("RSBTS_SCALE_DIR") {
        tempfile::tempdir_in(directory)?
    } else {
        tempfile::tempdir()?
    };
    let database = temporary.path().join("library.db");
    build_dataset(&database, track_count)?;

    let open = samples(SAMPLE_COUNT, || {
        let library = Library::open_read_only(&database)?;
        drop(library);
        Ok(())
    })?;
    let library = Library::open_read_only(&database)?;
    let browse = samples(SAMPLE_COUNT, || {
        let page = library.query_items_page(&Query::all(), None, 100)?;
        if page.len() != track_count.min(100) as usize {
            return Err("browse page returned the wrong number of rows".into());
        }
        Ok(())
    })?;
    let statistics = samples(SAMPLE_COUNT, || {
        let stats = library.stats()?;
        if stats.tracks != track_count {
            return Err("cached statistics are inconsistent".into());
        }
        Ok(())
    })?;
    drop(library);
    let dry_run_rss = child_dry_run_rss(&database)?;

    let report = serde_json::json!({
        "schema": "rsbts-scale-benchmark-v1",
        "tracks": track_count,
        "albums": track_count.div_ceil(100),
        "samples": SAMPLE_COUNT,
        "open_p95_ms": milliseconds(p95(&open)),
        "browse_page_p95_ms": milliseconds(p95(&browse)),
        "statistics_p95_ms": milliseconds(p95(&statistics)),
        "dry_run_rss_mib": dry_run_rss as f64 / 1_048_576.0,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);

    if track_count == DEFAULT_TRACKS {
        if p95(&open) >= Duration::from_millis(250)
            || p95(&browse) >= Duration::from_millis(200)
            || p95(&statistics) >= Duration::from_secs(1)
            || dry_run_rss >= 128 * 1_048_576
        {
            return Err("one or more OPS-005 release thresholds failed".into());
        }
    }
    Ok(())
}

fn build_dataset(
    database: &std::path::Path,
    track_count: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    drop(Library::open(database)?);
    let mut connection = Connection::open(database)?;
    connection.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -65536;",
    )?;
    let transaction = connection.transaction()?;
    {
        let album_count = track_count.div_ceil(100);
        let mut albums = transaction.prepare(
            "INSERT INTO albums (id, album, albumartist, year, added)
             VALUES (?1, ?2, ?3, 2026, '2026-01-01T00:00:00Z')",
        )?;
        for album in 1..=album_count {
            albums.execute(params![
                album,
                format!("Album {album:07}"),
                format!("Artist {:05}", album % 10_000)
            ])?;
        }
        let mut items = transaction.prepare(
            "INSERT INTO items
             (id, album_id, path, title, artist, album, albumartist, genre,
              year, track, disc, format, bitrate, length, added, mtime, file_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5, 'Benchmark', 2026, ?7, 1,
                     'FLAC', 900, 240.0, '2026-01-01T00:00:00Z',
                     '2026-01-01T00:00:00Z', 30000000)",
        )?;
        for index in 1..=track_count {
            let album_id = (index - 1) / 100 + 1;
            items.execute(params![
                index,
                album_id,
                format!("/benchmark/{album_id:07}/{index:09}.flac"),
                format!("Track {index:09}"),
                format!("Artist {:05}", album_id % 10_000),
                format!("Album {album_id:07}"),
                (index - 1) % 100 + 1,
            ])?;
        }
    }
    transaction.commit()?;
    connection.execute_batch("PRAGMA optimize; ANALYZE;")?;
    Ok(())
}

fn samples(
    count: usize,
    mut operation: impl FnMut() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    let mut values = Vec::with_capacity(count);
    for _sample in 0..count {
        let started = Instant::now();
        operation()?;
        values.push(started.elapsed());
    }
    values.sort_unstable();
    Ok(values)
}

fn p95(values: &[Duration]) -> Duration {
    let index = ((values.len() * 95).div_ceil(100))
        .saturating_sub(1)
        .min(values.len().saturating_sub(1));
    values[index]
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn child_dry_run_rss(database: &std::path::Path) -> Result<u64, Box<dyn std::error::Error>> {
    let output = Command::new(std::env::current_exe()?)
        .env("RSBTS_SCALE_CHILD_DB", database)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "dry-run memory child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().parse()?)
}

#[cfg(target_os = "linux")]
fn measure_dry_run_rss(database: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let library = Library::open_snapshot(database)?;
    let query = Query::parse("title:=__rsbts_no_match__")?;
    let plan = RemovalPlan::build(&library, &query, true)?;
    if !plan.items.is_empty() {
        return Err("dry-run sentinel unexpectedly selected an item".into());
    }
    let status = std::fs::read_to_string("/proc/self/status")?;
    let rss_kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .ok_or("VmRSS is absent from /proc/self/status")?
        .parse::<u64>()?;
    println!("{}", rss_kib * 1_024);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn measure_dry_run_rss(_database: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    Err("dry-run RSS benchmark requires Linux /proc".into())
}
