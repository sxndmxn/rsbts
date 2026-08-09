# rsbts

`rsbts` is a plan-first Rust CLI and library for cataloging, matching,
organizing, auditing, preserving, and removing a local music collection. It
treats ownership proof, recoverability, provenance, and bounded operation as
product behavior rather than implementation detail.

The normative product contract is [media-library-requirements.md](docs/media-library-requirements.md),
derived from [media-library-research.md](docs/media-library-research.md).

## Safety model

- One OS-backed collection lease excludes concurrent mutation and recovery.
- Every managed file has a path-independent asset UUID, BLAKE3 operational
  digest, SHA-256 archival digest, byte size, filesystem identity, and explicit
  verification state. Legacy rows are unverified until `verify` proves them.
- Preview, durable approval, and execution are separate. Execution revalidates
  source identity and destination state at the filesystem boundary.
- Linux mutations use directory-handle-anchored, symlink-rejecting I/O and
  atomic no-replace publication. A platform without those guarantees rejects
  mutation; read-only catalog operations remain available.
- Imports, removals, purge, tags, artwork, paths, ancillary files, manifests,
  and restores use typed persistent journals. Recovery is idempotent and
  preserves every candidate when ownership is uncertain.
- File-removing operations quarantine verified assets. Permanent deletion is a
  separately previewed and approved `purge` operation with an age policy.
- Dry runs open a current-schema read-only view. They do not create, copy, or
  modify the database, library, provider cache, journal, or recovery state.
- Mutating selections are bounded to 9,999 rows. Lists, histories, schedules,
  fixity work, and results use explicit limits or keyset pages.

## Install

Rust 1.89 or newer is required. Dependency resolution is locked.

```bash
cargo install rsbts --locked
```

For this checkout:

```bash
cargo install --path . --locked
```

## Quick start

Defaults are `~/Music` and `~/.local/share/rsbts/library.db`. Configuration
loading itself has no side effects.

```bash
rsbts stats
rsbts import --dry-run ~/Music/Incoming/album
rsbts import --existing-tags --dry-run ~/Music/Incoming/album
rsbts import --existing-tags --yes ~/Music/Incoming/album
rsbts import --in-place ~/Music/Already-Organized/album
rsbts ls --limit 100
rsbts audit
```

Create an explicit configuration only when needed:

```bash
mkdir -p ~/.config/rsbts
cp config.example.toml ~/.config/rsbts/config.toml
```

Relative paths in an explicit config resolve from that config's directory.
Missing explicit config files, unknown TOML keys, non-finite thresholds, and
invalid path templates fail closed.

### Migrating an existing Beets catalog

Migration reads Beets without modifying it, validates the translated catalog in
memory, and creates a new rsbts database with no-replace finalization. Missing
files remain cataloged for audit, flexible fields are retained as sourced
claims, and incompatible configuration is reported as a warning.

```bash
rsbts migrate beets \
  --beets-library ~/.config/beets/library.db \
  --beets-config ~/.config/beets/config.yaml \
  --output-database ~/.local/share/rsbts/library.db \
  --output-config ~/.config/rsbts/config.toml \
  --dry-run

# Remove --dry-run only after reviewing the plan. Both output paths must be new.
rsbts migrate beets \
  --beets-library ~/.config/beets/library.db \
  --beets-config ~/.config/beets/config.yaml \
  --output-database ~/.local/share/rsbts/library.db \
  --output-config ~/.config/rsbts/config.toml \
  --yes
```

## Matching and provenance

MusicBrainz direct IDs embedded in tags are used before text search. Candidate
review exposes recording, release-group, and exact-release confidence,
field-level evidence, contradictory evidence, candidate completeness, and
abstention reasons. Incomplete result sets and single candidates never receive
synthetic uniqueness evidence.

Unattended fuzzy exact-release acceptance is disabled unless a checked
evaluation attestation proves zero false accepts in at least 30,000 independent,
release-stratified hard negatives. Acoustic fingerprints produce recording
candidates only.

For unattended imports, `--yes` accepts provider metadata only when every
source track carries the same embedded release ID, the provider resolves that
exact ID, and all structural confidence gates pass. Everything else is skipped
with exit status 2 and a `requires-review` reason. Use `--existing-tags`
(`--as-is`) with `--yes` to explicitly import scanned embedded metadata instead;
the same selection policy applies during `--dry-run`.

Provider responses are cached as licensed raw snapshots. Canonical values are
materialized from immutable claims by an explicit resolution policy; manual
claims may be locked. Provider refresh produces a reviewable field diff and
does not rewrite tags or paths.

MusicBrainz is enabled by default. Discogs is an optional built-in provider:
add `discogs` to `providers.enabled` and supply `RSBTS_DISCOGS_TOKEN`. Direct,
provider-qualified IDs route only to their owning provider. Partial provider
results are disclosed and cannot satisfy unattended acceptance gates.

## Query and machine output

```bash
rsbts ls 'artist:"Black Sabbath" year:1969..1979 year+' --limit 100
rsbts ls --album paranoid --limit 50
rsbts --output json stats
rsbts --output jsonl ls --limit 100
rsbts update 'artist:=Beatles'
rsbts modify 'album:=Paranoid' genre=Metal year=1970
rsbts rm --dry-run 'artist:=Beatles'
rsbts rm --delete --yes 'artist:=Beatles'
rsbts purge --dry-run --older-than-days 30
```

Filters support literal substring `field:value`, exact `field:=value`, SQLite
glob `field::pattern`, ranges, `^` negation, relative added dates such as
`added:-7d`, and `+`/`-` sort suffixes. Values are bound SQL parameters. Empty
or malformed mutating queries are rejected before database open or recovery.

`--output json` and `--output jsonl` cover import/matching, audit, provider
refresh, tag projection, path projection, removal, plans, fixity, integrity,
lists, and statistics. A machine-readable mutation requires `--dry-run` or an
explicit non-interactive approval.

Exit status is 0 for success, 2 for a completed result containing issues or
partial work, and 1 for validation or fatal runtime failure. Clap uses status 2
for command-line syntax errors.

## Audit, fixity, and integrity

Quick audit compares catalog paths, sizes, mtimes, entry identities, media
properties, ownership, and projection state. Reports retain at most 4,096
issues and disclose omissions.

Deep audit is the durable, paged fixity workflow:

```bash
rsbts --output json audit --deep
rsbts fixity approve PLAN_ID
rsbts fixity run PLAN_ID --page-size 512
rsbts fixity results PLAN_ID --limit 512
rsbts plan status PLAN_ID
rsbts plan events PLAN_ID
rsbts plan cancel PLAN_ID
```

Each `fixity run` invocation handles one bounded page. Repeat until `complete`
is true; interruption resumes from the durable cursor. Persistent schedules and
auditable history are available through `fixity schedule`, `fixity due`,
`fixity schedules`, `fixity enable`, and `fixity history`.

Full SQLite integrity checking is intentionally explicit:

```bash
rsbts integrity
```

Library APIs also provide SHA-256 manifests, BagIt-compatible export,
manifest verification, and an exercised, journaled restore workflow.

## Projections and media

Tag, path, embedded-artwork, and external-artwork changes are reviewable,
journaled projections. Tag writing uses sibling output, durability sync,
reread validation, unknown-metadata comparison, decoded audio-essence
validation where available, and no-clobber publication. Original artwork is
fully decoded under resource limits and retained content-addressably; player
derivatives are deterministic sRGB PNGs that are never cropped or upscaled.

```bash
rsbts write --dry-run 'artist:=Black Sabbath'
rsbts write --yes 'artist:=Black Sabbath'
rsbts move --dry-run 'album:=Paranoid'
rsbts move --yes 'album:=Paranoid'
```

Explicit tag writes retain the prior file as a verified original. Managed-file
moves update the root-relative asset identity and both digests in the same
catalog transaction; recovery never deletes a source whose identity changed.

Capability contract version 2 covers:

- FLAC
- MP3
- Ogg Vorbis, Opus, and Speex
- MP4 AAC and ALAC
- standalone ADTS AAC
- WAV/BWF and AIFF PCM
- WavPack, Monkey's Audio/APE, and Musepack

Every advertised container/codec/tag tuple is exercised against every tag
profile for native writing, multivalue behavior, artwork, unknown preservation,
and audio-essence integrity. See [tag-capabilities.md](docs/tag-capabilities.md).
Unsupported files remain opaque catalog assets and are never rewritten through
an assumed dialect.

## Library API

The crate exposes typed queries, validated invariant newtypes, normalized
catalog entities and claims, provider jobs, durable plans and events, roots and
capabilities, projection executors, paged fixity, preservation workflows, and
explicit recovery. See [api-compatibility.md](docs/api-compatibility.md) for the
pre-1.0 compatibility contract.

## Development and release

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo package --locked
cargo deny --locked check
```

CI additionally runs Rust 1.89, Linux/macOS/Windows tests, property and fuzz
smoke suites, a 75% safety-core line-coverage gate, selected recovery mutation
tests, public API semver checks, and the published million-track benchmark.
Release artifacts include checksums, CycloneDX SBOM, and GitHub provenance
attestations. See [RELEASING.md](RELEASING.md) and
[performance.md](docs/performance.md).

CI compiles and tests supported behavior on Linux, macOS, and Windows. Mutating
filesystem workflows run only where the capability probe can prove anchored,
symlink-rejecting, atomic no-clobber semantics; unsupported platforms fail
closed while read-only catalog behavior remains available. Longer-term Beets
compatibility work is tracked separately in
[ARCHITECTURE_ROADMAP.md](ARCHITECTURE_ROADMAP.md) and cannot weaken the
normative requirements above.

## License

MIT
