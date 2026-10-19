# rsbts

`rsbts` is a Rust music library manager for the main beets workflow.
It imports albums, matches metadata, organizes files, searches the catalog, and edits tags.
It also provides file checks and recovery through its command line and library API.

The compatibility target is beets core behavior without plugins.
Full compatibility is still in progress.
The [compatibility manifest](compat/beets-2.11.toml) records known gaps against beets 2.11.

The normative product contract is [media-library-requirements.md](docs/media-library-requirements.md),
derived from [media-library-research.md](docs/media-library-research.md).

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
rsbts ls 'artist:Black Sabbath' year:1969..1979 year+
rsbts ls -a year:1970
rsbts ls -p 'album:Paranoid'
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

Use `-l DATABASE` and `-d DIRECTORY` before the command to override the configured library paths.
Relative override paths resolve from the current directory.

```bash
rsbts -l ./library.db -d ./Music stats
rsbts config
rsbts config --path
rsbts config --default
rsbts fields
```

`config` prints the effective TOML configuration.
`config --default` prints built-in defaults.
`config` and `fields` do not create a database or music directory.
`fields` includes stored custom field names when a database exists.
Use `flex.NAME` to query a custom field.

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

Unattended fuzzy exact-release acceptance remains disabled in the CLI.
The library requires a checked evaluation attestation for any future unattended path.
An attestation records zero false accepts across at least 30,000 independent cases from balanced release groups.
Each case compares different editions within one release group.
JSON loading checks constructor rules, report consistency, policy version, and acceptance rules.
Callers must verify the underlying evaluation data.
Acoustic fingerprints produce recording candidates only.

For unattended imports, `--yes` accepts provider metadata only when every
source track carries the same embedded release ID, the provider resolves that
exact ID, and all structural confidence gates pass. Everything else is skipped
with exit status 2 and a `requires-review` reason. Use `--existing-tags`
(`--as-is`) with `--yes` to explicitly import scanned embedded metadata instead;
the same selection policy applies during `--dry-run`.
Existing-tag imports skip online metadata lookups during preview and execution.
Unreadable files with recognized audio extensions produce scan warnings and exit status 2.
This also applies to unreadable files supplied as explicit paths.
Updates retain the catalog title when the file has no title tag.
Internal temporary and quarantine names remain within the 255-byte filename limit.
Tag writes preserve display artist credits separately from native artist lists.

Exact-release confidence compares source label, catalog number, country, and medium
with candidate facts. Candidate metadata availability alone earns no credit.
Label and catalog checks include repeated native values.
Missing candidate facts remain unknown.
Conflicting identifiers or edition facts require review, even with a shared release ID.
Non-finite provider scores always reject unattended acceptance.
Release-ID checks include extended metadata and supported legacy aliases.
Equivalent MusicBrainz UUID spellings represent the same identity.

Provider selection keeps changed artist credits and years consistent with later tag writes.
Unchanged credits retain their separate values. Missing provider years preserve known dates.
Selected identifiers replace older identifiers from the same provider and identity type.
Other provider evidence remains stored.

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
rsbts ls 'artist:Black Sabbath' year:1969..1979 year+ --limit 100
rsbts ls --album paranoid --limit 50
rsbts ls -af '$albumartist - $album ($year)' year:1970
rsbts ls -p 'artist:=~Black Sabbath'
rsbts --output json stats
rsbts --output json stats 'album:=Paranoid' length:180..
rsbts --output jsonl ls --limit 100
rsbts update --dry-run 'artist:=Beatles'
rsbts update 'artist:=Beatles'
rsbts modify --dry-run 'album:=Paranoid' genre=Metal year=1970
rsbts modify --yes 'album:=Paranoid' genre=Metal year=1970
rsbts rm --dry-run 'artist:=Beatles'
rsbts rm --delete --yes 'artist:=Beatles'
rsbts purge --dry-run --older-than-days 30
```

`update` replaces the catalog's native tag snapshot and preserves unrelated custom fields.
Absent native fields clear stale native values.
`update` does not adopt new external identifiers from tags.
Changes to identity fields can clear associated catalog identifiers.
`update --dry-run` (alias `--pretend`) lists title, artist, album, album artist, genre, year, track, and disc changes.
It does not list changes to other native tag fields.
`modify` keeps artist, album artist, genre, and year consistent with later tag writes.
`modify` shows each field change and asks for confirmation before it changes the database.
Use `--yes` to approve without a prompt, or `--dry-run` to preview only.
The preview also shows stored track or release identifiers that the change clears.
`modify` changes only fields that differ from the stored value.
Identifiers that depend on an unchanged field stay stored.

Filters support literal substrings `field:value`, exact values `field:=value`,
case-insensitive exact values `field:=~value`,
regular expressions `field::pattern`, SQLite globs `field:~pattern`, and ranges.
They also support `^` negation, relative added dates such as `added:-7d`, and
`+`/`-` sort suffixes. Separate alternative filter groups with `,`.
Values are bound SQL parameters. Empty or malformed mutating queries fail
before database open or recovery.

The shell handles quoting for command-line queries.
Pass each filter or sort term as a separate argument.
A quoted phrase forms one search term.
Backslashes and quote characters inside an argument remain literal.
This applies to listing, statistics, updates, verification, modification, writing, moving, and removal.

Bare keywords use literal substring matching across title, artist, album, album artist, and genre.
Use `=value` for exact matching and `=~value` for exact matching that ignores case.
Use `:pattern` for regular expression matching across those fields.
Album queries use album fields and can filter member tracks by fields such as `title`.

`ls -p` prints file paths or album directories.
`ls -f FORMAT` substitutes `$field` and `${field}` without changing metadata text.
`$$` prints one dollar sign. Unknown fields remain in the format.
Custom fields use `$NAME` or `${flex.NAME}`.
List formats currently support field substitution. Beets template functions remain a compatibility gap.
Path and format output require text output.

Exact and range filters compare numeric custom fields as numbers.
The same filters retain text comparison for custom fields stored as strings.

Numeric fields use equality unless the query supplies a range or an explicit matching operator.
For example, `track:1` does not match track 11.
`singleton:true` and `singleton:false` select tracks by album membership status.

For `rm`, `modify`, `write`, and `move`, an explicit query must contain a filter.
Sort terms alone fail before database open or recovery. Use `--all` to select
every item for `write` or `move`.

Track and album listings apply sort terms before `--limit`. The default limit is 1,000
rows. The allowed range is 1 to 100,000 rows. Invalid relative dates, purge
ages, and list limits fail before database open or recovery.

Track and album JSON and JSONL listings include stored extended metadata.
This includes original dates, separate artist lists, provider identifiers, and custom fields.
Each imported album group stores release metadata shared by all its tracks.
Tracks retain individual values when album values conflict or are missing.
Per-disc track totals and recording identifiers remain attached to tracks.

Listings load extended metadata in batches of 256 records.
Filtered statistics run directly in SQLite without loading media files or extended metadata.
Unfiltered statistics retain the cached aggregate.

Write and move plans reject selections above 9,999 rows before reading media
files. Narrow the query to select fewer rows.
Purge plans reject selections above 9,999 retained files.

Use `--output json` or `--output jsonl` for machine output.
Both formats cover import, matching, audit, provider refresh, tag projection,
path projection, removal, purge, plans, fixity, integrity, lists, and statistics.
Commands with confirmation prompts require `--dry-run` or explicit
non-interactive approval in these formats.

`modify`, `update`, and `verify` also support both formats. Each emits one
result object. `update` includes failure details for each affected item.
`verify` includes skipped items and their reasons.

For `write`, `move`, and `purge`, JSON emits one object. JSONL emits a preview
and, after execution, a result. Dry runs and empty selections emit only the
preview.

Exit status is 0 for success, 2 for a completed result containing issues or
partial work, and 1 for validation or fatal runtime failure. Clap uses status 2
for command-line syntax errors.

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

Purge checks file identity after acquisition and again before deletion.
Changed files remain available for recovery review.
Recovery keeps one parent directory handle while it acquires, checks, and
deletes a retained file. Missing retained files receive completed cleanup
records. Files restored during rollback remain eligible for a later purge.

Successful recovery updates the journal and linked active plan in one transaction.
Committed work completes the plan. Rolled-back work fails the plan.
Recovery preserves terminal plans and their existing events.
Uncertain file ownership leaves the operation pending for review.

Database symbolic links use the resolved database path for locking and writes.
On Unix, rsbts rejects database files with multiple hard links.

Registered roots must be online for import, move, tag, artwork, path, ancillary,
removal, and purge file changes.
Catalog-only import and removal remain available when a root is offline.
Purge checks the retained file's original root policy.
Import, move, and ancillary operations check each destination for disabled child roots.
Equivalent root paths reuse the saved identity and policy.

Root paths containing `..` fail before directory creation.
Moves require source files to match their stored verification evidence.
Unverified or replaced files require review and explicit verification before a move.

Tag, artwork, path, ancillary, removal, manifest, and restore execution must
match the saved approved plan.
Changes to the reviewed plan require a new preview and approval.
Previously saved plans with incomplete approval evidence also require a new
preview and approval.
Typed approval methods check the complete saved request and preview.

Dry-run snapshots remain read-only after in-memory migration.
They cannot create durable plans or run recovery.

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
Invalid schedule intervals fail before database open. Invalid stored intervals
return an error without creating partial plans.
Fixity execution checks the saved mode, asset count, and schedule before each page.
Completed and cancelled plans reject further execution without changing history.

`plan events` returns at most 512 events by default. Use `--limit` to select
between 1 and 4,096 events. Pass the last returned sequence to
`--after-sequence` for the next page.

Full SQLite integrity checking is intentionally explicit:

```bash
rsbts integrity
```

Library APIs also provide SHA-256 manifests, BagIt-compatible export,
manifest verification, and an exercised, journaled restore workflow.

Restore history links new runs to their operation IDs.
Recovery finishes the matching running record in the journal's completion transaction.
Automatic history repair applies to newly created runs.

`verify_sha256_manifest` checks GNU SHA-256 records against file bytes.
Both GNU mode flags use raw bytes without text conversion.
`verify_bagit_manifest` checks BagIt payload manifests and decodes their filename
escapes. Exports preserve spaces, percent signs, line breaks, and Unix
backslashes in filenames. Empty roots can export empty manifests and bags.

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

Tag reads retain supported repeated values, calendar dates, totals, release facts,
classical fields, identifiers, and ReplayGain text.
Empty tags and zero dates, positions, or totals represent unknown values.
Repeated scalar tags are accepted when their values agree. Conflicting values still require review.
Field support varies by format. See the [tag capability limits](docs/tag-capabilities.md).

Name shortening preserves path safety and portable filename rules.
Collision checks treat equivalent Unicode names and case variants as equal.

Capability contract version 2 covers:

- FLAC
- MP3
- Ogg Vorbis, Opus, and Speex
- MP4 AAC and ALAC
- standalone ADTS AAC
- WAV/BWF and AIFF PCM
- WavPack, Monkey's Audio/APE, and Musepack

Tests run each advertised format combination through every tag profile.
The tests check each test file's metadata, artwork, unknown fields, and audio preservation.
See [tag-capabilities.md](docs/tag-capabilities.md) for field and format limits.
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
