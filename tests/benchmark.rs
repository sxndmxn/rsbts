//! Ignored fresh-process benchmark harness.
//!
//! Run with:
//! `cargo test --release --test benchmark -- --ignored --nocapture`

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::Connection;

fn measured_process(
    executable: &Path,
    arguments: &[OsString],
    iterations: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let output = Command::new(executable)
            .args(arguments)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        let elapsed = started.elapsed();
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "{} {:?} exited with {}; stderr: {}",
                executable.display(),
                arguments,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        samples.push(elapsed);
    }
    samples.sort_unstable();
    samples
        .get(samples.len() / 2)
        .copied()
        .ok_or_else(|| io::Error::other("benchmark requires at least one iteration").into())
}

fn report(name: &str, duration: Duration, iterations: usize) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    writeln!(
        stdout,
        "{name}: median {:.3} ms over {iterations} fresh process(es)",
        duration.as_secs_f64() * 1_000.0
    )
}

fn initialize_catalog(
    rsbts: &Path,
    config_path: &Path,
    database_path: &Path,
    item_count: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(
        config_path,
        format!(
            "[library]\ndirectory = 'organized'\ndatabase = '{}'\n",
            database_path.display()
        ),
    )?;
    let initialize = Command::new(rsbts)
        .arg("--config")
        .arg(config_path)
        .arg("stats")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !initialize.status.success() {
        return Err(io::Error::other(format!(
            "could not initialize benchmark catalog: {}",
            String::from_utf8_lossy(&initialize.stderr)
        ))
        .into());
    }

    let connection = Connection::open(database_path)?;
    connection.execute_batch(&format!(
        "WITH RECURSIVE records(id) AS (
             SELECT 1 UNION ALL SELECT id + 1 FROM records WHERE id < {item_count}
         )
         INSERT INTO items
             (path, title, artist, album, format, bitrate, length, file_size, added, mtime)
         SELECT printf('/benchmark/%d.flac', id), printf('Track %d', id),
                printf('Artist %d', id % 1000), printf('Album %d', id % 500),
                'FLAC', 1000, 180.0, 1000000,
                '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z'
         FROM records;"
    ))?;
    Ok(())
}

#[test]
#[ignore = "release-mode benchmark; run explicitly"]
fn fresh_process_catalog_workloads() -> Result<(), Box<dyn std::error::Error>> {
    let iterations = std::env::var("RSBTS_BENCH_ITERATIONS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(5);
    if iterations == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RSBTS_BENCH_ITERATIONS must be positive",
        )
        .into());
    }
    let item_count = std::env::var("RSBTS_BENCH_ITEMS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(100_000);
    if item_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RSBTS_BENCH_ITEMS must be positive",
        )
        .into());
    }

    let temporary = tempfile::tempdir()?;
    let config_path = temporary.path().join("config.toml");
    let database_path = temporary.path().join("library.db");
    let rsbts = Path::new(env!("CARGO_BIN_EXE_rsbts"));
    initialize_catalog(rsbts, &config_path, &database_path, item_count)?;

    let config = config_path.as_os_str().to_os_string();
    let workloads = [
        ("version", vec![OsString::from("--version")]),
        (
            "stats",
            vec![
                OsString::from("--config"),
                config.clone(),
                OsString::from("stats"),
            ],
        ),
        (
            "exact-query",
            vec![
                OsString::from("--config"),
                config.clone(),
                OsString::from("ls"),
                OsString::from("artist:=\"Artist 42\""),
            ],
        ),
        (
            "full-list",
            vec![OsString::from("--config"), config, OsString::from("ls")],
        ),
    ];
    for (name, arguments) in workloads {
        report(
            &format!("rsbts {name}"),
            measured_process(rsbts, &arguments, iterations)?,
            iterations,
        )?;
    }

    if let Some(beets) = std::env::var_os("BEETS_BIN") {
        let beets = Path::new(&beets);
        report(
            "beets version",
            measured_process(beets, &[OsString::from("version")], iterations)?,
            iterations,
        )?;
    }
    Ok(())
}
