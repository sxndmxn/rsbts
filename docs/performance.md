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

The generator creates a schema-v10 catalog containing:

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
| Current-schema read-only open | 1.694 ms | < 250 ms |
| 100-row keyset browse | 0.462 ms | < 200 ms |
| Exact cached statistics | 0.012 ms | < 1,000 ms |
| Read-only removal-preview RSS | 7.801 MiB | < 128 MiB |

The benchmark JSON schema is `rsbts-scale-benchmark-v1`. Hardware-dependent
results must always be published with the command, dataset size, filesystem,
cache state, sample count, and percentile rule above.

## Core command refinement

Measurement date: 2026-10-18.
The baseline uses commit `8e72b3f` before this refinement.
Both runs use Rust 1.92.0 with the release build profile.
The host runs Linux 7.2.9-arch1-1 on Btrfs.
The processor is an AMD Ryzen 9 9950X3D2 with 16 cores and 32 threads.
The host reports 60.2 GiB of memory.

```bash
cargo test --locked --release --test benchmark -- --ignored --nocapture
```

This benchmark creates 100,000 track rows with 1,000 artist labels and 500 album labels.
It does not create media files.
Each command runs in a new process against the same temporary catalog.
Each result is the median of five runs with a warm operating-system page cache.
The benchmark checks returned row counts before timing list commands.
The listing selects all 100,000 rows and writes text output to the null device.

| Measurement | Before | After |
|---|---:|---:|
| List 100,000 tracks | 1,166.743 ms | 498.329 ms |

The measured listing is about 2.3 times faster.
Extended metadata now uses two prepared queries per batch of 256 records.
The previous implementation used three queries per record.
JSON and JSONL output retain complete metadata.
These measurements cover catalog operations and exclude audio decoding or file copying.

The million-track benchmark also passes on this host with the same filesystem and build profile.
It uses the dataset, cache state, 30 samples, and percentile rule defined above.

```bash
RSBTS_SCALE_DIR=/home/sandman/code/repos/rsbts/target \
  cargo run --locked --manifest-path benchmarks/Cargo.toml --release --quiet
```

| Measurement | p95/result | Threshold |
|---|---:|---:|
| Current-schema read-only open | 0.850 ms | < 250 ms |
| 100-row keyset browse | 0.343 ms | < 200 ms |
| Exact cached statistics | 0.0042 ms | < 1,000 ms |
| Read-only removal-preview RSS | 8.094 MiB | < 128 MiB |

## Query regression proof

The query regression test detects exact matching in place of substring matching.
A temporary change from `FieldOperation::Substring` to `FieldOperation::Exact` made the focused test fail.
The assertion reported missing `%` markers in both search values.
Restoring substring matching made all 13 query unit tests pass.

```bash
# Failed with the temporary defect.
cargo test --locked --lib query::tests::rejects_unknown_fields_and_preserves_sort_like_search_terms

# Passed after restoration.
cargo test --locked --lib query::tests
```

The final checks also pass with the complete implementation.
The full suite runs 440 tests and leaves the release benchmark disabled by default.
The release benchmark above runs that test separately.
The disposable command workflow checks preview, import, query, audit, update, modification, removal preview, and confirmed cleanup.
The source audio remains unchanged after catalog removal.

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --quiet
RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps
cargo package --locked --allow-dirty
cargo deny --locked check
```

The dependency check passes with duplicate-version warnings from the existing dependency tree.
