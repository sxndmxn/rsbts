# Million-track performance contract

This document is the reproducible methodology and reference result for
OPS-005 and OPS-010. The benchmark is executable rather than a hand-maintained
spreadsheet:

```bash
RSBTS_SCALE_DIR=/path/on/the/filesystem-under-test \
  cargo run --locked --manifest-path benchmarks/Cargo.toml --release
```

The process exits unsuccessfully when the one-million-track run exceeds any
release threshold: current-schema open p95 250 ms, 100-row browse p95 200 ms,
statistics p95 one second, or read-only removal-preview RSS 128 MiB. CI runs
this command on every change.

## Dataset and measurements

The generator creates a schema-v9 catalog containing:

- 1,000,000 tracks in 10,000 albums, with 100 tracks per album;
- 10,000 distinct artist labels;
- realistic indexed title, artist, album, path, position, duration, size, and
  FTS values;
- all current catalog, ownership, journal, and projection tables and triggers;
- no materialized media payloads, because these timings measure catalog
  operations rather than fixity I/O.

Each timing is measured 30 times in one release-mode process. Durations are
sorted and p95 is the nearest-rank 95th percentile. Dataset construction,
schema migration, and `ANALYZE` are excluded. Measurements run with a warm
operating-system page cache; this is explicit because normal interactive use is
the target workload. Open creates a new read-only SQLite connection for every
sample. Browse uses the public 100-row keyset API. Statistics use the exact
transactionally maintained aggregate. The dry run occurs in a fresh child
process, performs the real empty removal selection and validation path against
the current schema, and reads Linux `VmRSS`.

## Reference environment and result

Reference run: 2026-08-09, Linux 7.1.6, x86_64, Btrfs, Rust 1.97.1
(`--release`), AMD Ryzen 9 5900X (12 cores/24 threads), 31 GiB RAM. The
database and WAL were placed on the Btrfs filesystem; the page cache was warm.

| Measurement | p95/result | Threshold |
|---|---:|---:|
| Current-schema read-only open | 117.655 ms | < 250 ms |
| 100-row keyset browse | 0.428 ms | < 200 ms |
| Exact cached statistics | 0.014 ms | < 1,000 ms |
| Read-only removal-preview RSS | 7.184 MiB | < 128 MiB |

The benchmark JSON schema is `rsbts-scale-benchmark-v1`. Hardware-dependent
results must always be published with the command, dataset size, filesystem,
cache state, sample count, and percentile rule above.
