# Changelog

All notable changes are documented here. The project follows semantic
versioning, with pre-1.0 minor releases allowed to contain documented API
breaks.

## [Unreleased]

### Core commands

The compatibility target now excludes plugins.
All query commands accept separate shell arguments and preserve quoted phrases.
Existing scripts must pass each filter and sort term separately.

Album lists now apply field filters and sort before the row limit.
Bare keywords now match substrings across core text fields.
Queries support `=~` exact matching without case and unqualified regular expressions.
Track IDs and album IDs can select and sort records.
Numeric fields now compare numbers instead of matching text fragments.
File size, singleton status, and album artwork paths are available as query fields.

Added `ls -p`, `ls -f`, and `ls -l` for paths, field formats, and row limits.
Added filtered statistics, configuration display, and field discovery.
Added `-l` and `-d` library path overrides before the command.

Listings now read extended metadata in batches of 256 records.
Batch reads preserve native metadata, custom fields, and stored provider identifiers.

### Fixed

Track and album JSON and JSONL listings now include stored extended metadata.
Original dates, separate artist lists, provider identifiers, and custom fields no longer appear empty.
Album imports now store shared release dates, album artists, genres, release facts, and album-scoped identifiers.
Missing or conflicting values stay unknown on the album. Tracks retain their individual metadata.
Per-disc track totals and recording identifiers remain attached to tracks.

Matching evaluation JSON now follows the same validation rules as checked construction.
Invalid cases, inconsistent reports, unknown policy versions, and failed acceptance gates now return errors.
Independent tests now detect a false accept and a duplicate case in otherwise valid evaluations.

Filesystem publication tests now exercise the production anchored rename method.
Removed the obsolete test-only rename implementation while preserving both tests.
I/O failure tests now inject operating-system errors inside the operation.
The fresh-process benchmark now checks listing row counts and requests the complete supported dataset.
Journal property and fuzz checks now verify approval order and preserve saved state after failed actions.
Recovery tests now detect digest disagreement despite unchanged file identity.
They also detect attempts to read a replacement named pipe before ownership validation.
Recovery tests now detect invalid link evidence and unverified move destinations.
CLI regression tests now use supported import actions on each platform and compare canonical file paths.
Assertions now show unexpected collection values when tests fail.
The API compatibility check now uses a tool version that supports Rust 1.99.

Existing-tag imports no longer query online metadata providers.
Preview and execution use the scanned tags directly.

Import scans now report unreadable audio files with recognized extensions.
They also report unreadable files supplied as explicit paths.
Native tag readers now accept up to 64 KiB of padding between ID3 tags and audio frames.

Updates now retain the catalog title when a file has no title tag.
An organized filename no longer replaces the original filename-derived title.

Long filenames now use bounded temporary and quarantine names across file operations.
Recovery can finish older prepared imports when the filesystem rejects an uncreated temporary name as too long.

Tag writes now preserve display artist credits separately from native artist lists.
Changing another tag no longer replaces a credited guest or a custom credit string.

Native tag reads now accept matching scalar values stored under multiple tag aliases.
Repeated native artist, composer, and other list values now retain one copy of each value.
Empty fields and zero dates, positions, or totals now represent unknown values.
These forms no longer prevent imports and updates of otherwise readable audio files.
Conflicting values, negative numbers, and malformed identifiers still fail validation.

- Unattended import now abstains from review-required provider matches instead
  of silently importing existing tags. `--existing-tags` (alias `--as-is`)
  provides the explicit non-interactive as-is workflow, and direct embedded
  release IDs remain eligible only after every structural confidence gate.

Relative date queries and purge retention now reject values outside the
supported range. Invalid purge ages and list limits fail before database open
or recovery.

Confirmed `write` and `move` commands now emit one JSON document. JSONL retains
its preview and result records.

Track listings now honor requested sort terms before applying the row limit.
Query pages ordered by item ID no longer repeat rows across alternative
filter groups.

Write and move plans now enforce the 9,999-row limit before reading media
files. Bounded selections retain metadata fields with multiple values.

Mutation commands now reject queries that contain only sort terms.
This prevents unintended changes to every item in the catalog.

Fixity schedules now validate intervals and dates before storage.
Invalid CLI intervals fail before database open. Invalid stored intervals
return an error without a crash or partial plans.

`purge`, `modify`, `update`, and `verify` now honor JSON and JSONL output.
Update failures and skipped verification items include structured details.

Purge now checks file identity after acquisition and again before deletion.
Changed files remain available for recovery review. Retained symbolic links
keep their file type during purge. Purge plans enforce the 9,999-file limit.

Provider job completion and failure now require the current attempt.
Old job handles cannot change a later retry. Claims validate stored fields
and attempt limits before changing state.

`plan events` now returns bounded pages. The default limit is 512 events.
`--after-sequence` selects the next page. Invalid limits and cursors fail
before database open.

Name shortening no longer creates parent-directory components, reserved
names, or forbidden trailing characters. Unicode collision checks include
equivalent names with combining marks.

Quoted queries now preserve backslashes in regular expressions and paths.
Exact and range queries now match numeric custom fields.

Purge recovery now acquires retained files before checking ownership.
It uses one parent directory handle through deletion.
Restored files remain eligible after rollback. Missing files receive completed
cleanup records. Validation and journal creation failures end the durable plan.

Successful recovery now finishes linked active plans with the journal in one transaction.
Committed work completes the plan. Rolled-back work fails the plan.
Recovery preserves terminal plans and avoids duplicate outcome events.
Purge history changes use the same transaction.

New restore history rows now share their operation ID.
Recovery finishes running restore history with the journal and linked plan.
Rollback marks the run failed. Committed cleanup marks the run complete.
Uncertain ownership leaves the run pending.
Older history without that association remains available for review.

Database symbolic links now share the target database's writer lock.
On Unix, databases with multiple hard links fail before SQLite writes.
This follows [SQLite's guidance on database aliases](https://www.sqlite.org/howtocorrupt.html#_multiple_links_to_the_same_file).

Import, move, tag, artwork, path, removal, and purge operations now check saved
root policy.
Offline, read-only, and degraded roots reject file changes.
Catalog-only import and removal remain available. Retained files keep their original
root association for later purge checks.

Moves now compare planned source evidence with the verified catalog asset.
Unverified assets and replacement files fail before file changes.
Execution also rechecks the asset's verification state.

Tag, artwork, path, and removal execution now compares the supplied plan with
its saved approval. Changed content and unrelated approvals fail before execution.
Path execution also checks for new case and Unicode collisions.

Ancillary import now binds every execution field to its saved preview.
Changed destinations and unrelated approvals fail before file changes.
Older incomplete previews require a new plan. Early failures no longer leave
ancillary plans running.

Typed approval methods now check the complete saved preview before changing state.
This applies to tag, artwork, path, ancillary, provider refresh, manifest,
restore, and fixity plans.

Equivalent root paths now reuse the saved root identity and policy.
Ambiguous duplicate root records require review.
Preservation root lookup follows the same path rules and rejects descendant paths.
Import, move, and ancillary operations check each actual destination.
Disabled child roots now reject these file changes before journaling.

Single-candidate matching now reports an unknown runner-up margin.
It no longer compares the candidate with a synthetic zero score.
Release and singleton matching now reject non-finite provider scores explicitly.
Other confidence gates cannot hide malformed score evidence.
Exact-release confidence now compares source edition facts with candidate facts.
Candidate metadata availability alone earns no credit.
Label and catalog checks include every supplied native value.
Conflicting secondary values require review. Missing candidate facts remain unknown.
Conflicting identifiers or edition facts require review, even with a shared release ID.
Release-ID conflict checks now include extended metadata and supported legacy aliases.
Equivalent MusicBrainz UUID spellings retain shared identity evidence.
Unattended acceptance and its diagnostics use the same source-ID checks.

Provider selection now keeps changed artist credits and years consistent with later tag writes.
Unchanged credits retain separate values. Unchanged dates retain their precision.
Missing provider years preserve known dates.
Selected identifiers replace older same-provider values within their identity type.
Other provider evidence remains stored when compact identifier fields cannot represent it.
Changed positions clear retained totals only when the new position exceeds the total.

Catalog dates now require `YYYY`, `YYYY-MM`, or `YYYY-MM-DD` syntax.
Years must be from 0001 through 9999. Entity, plan, and root identifiers use
one standard UUID form. JSON input now validates metadata claims, provider
identifiers, and canonical tags through the same checks as their builders.

Filesystem anchors now reject parent components before opening or creating
directories. This prevents a root path from selecting a different directory.

Manifest and restore execution now checks the saved request and preview.
Pre-journal errors finish the durable plan. Older manifest plans without
complete approval evidence require a new preview and approval.

SHA-256 and BagIt exports now preserve literal filename characters through
their separate encoding rules. Empty roots produce valid empty exports.
The new `verify_bagit_manifest` API handles encoded BagIt payload names.
Manifest output and restore destinations now respect registered root policy.
Read-only source roots remain usable for export and restore.

Removal now checks catalog ownership before approval and execution.
Plans cannot add files owned by unselected items or albums.
Regular-file recovery acquires a repeatable cleanup name and retains one parent
directory handle. Recovery also checks ownership before removing duplicate hard links.
Changed removal sources fail before the plan starts.
Committed start failures now leave removal plans in a terminal failure state.

Whole-album removal now works after external artwork replacement.
It preserves archived originals and retired covers.

Purge selection no longer scans the full purge history for each retained file.
Active purge operations still exclude their files. Completed rollbacks permit
later purge selection.

Missing and migrated dry-run snapshots now become read-only after setup.
They reject plan writes and recovery before filesystem changes.
Existing writable in-memory libraries remain supported.

Provider refresh now checks the complete saved approval before applying fields.
Query and transaction failures roll back field changes and finish the plan.
Claim replacement requires distinct active claims for the same entity and field.
Earlier evidence remains stored, and replacement across providers remains supported.

Fixity execution now checks saved review before cancellation, start, or resume.
Completed and cancelled plans reject further execution without changing history.
Fixity schedule identifiers now use the same standard UUID form as other identifiers.

Catalog updates now save extended tag metadata and preserve unrelated custom fields.
Absent native fields clear stale native values.
Changes to artist, album artist, genre, and year now update their extended values.
Invalid positions and totals roll back the complete change.
External identifiers retain the existing invalidation policy, including supported legacy aliases.

Native tag reads now populate extended metadata instead of leaving it empty.
Tag reconstruction retains supported repeated values, partial dates, independent totals,
release facts, classical fields, identifiers, and ReplayGain text.
Malformed typed fields and unsupported date precision return explicit errors.
The tag capability document now states the tested field scope and existing format limits.

Regular-expression queries now reuse compiled patterns through SQLite's argument cache.
Changed patterns retain correct results. NULL values and malformed patterns keep their previous behavior.

Updated `h2` to 0.4.16 to resolve
[RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258.html).
Replaced `serde_yaml` and its unmaintained `unsafe-libyaml` dependency with
`serde-saphyr` for Beets configuration parsing. This resolves
[RUSTSEC-2024-0449](https://rustsec.org/advisories/RUSTSEC-2024-0449.html).

## [0.4.0] - 2026-08-09

### Added

- Persistent asset ownership, dual digests, root-relative identities, root
  capability detection, exclusive collection leasing, and anchored no-clobber
  filesystem operations.
- Typed, durable, recoverable plans for import, removal, tag, artwork, path,
  ancillary, preservation, purge, provider refresh, and streamed fixity work.
- Normalized musical entities, immutable sourced claims, raw provider
  snapshots, explicit unknown/conflict states, and manual locks.
- Strict exact-edition matching gates, direct provider IDs, durable enrichment,
  Discogs/AcoustID/curation policy types, and the 30,000-case autoaccept
  attestation gate.
- Content-derived media probing and golden round-trip support for 13 advertised
  container/codec/tag tuples and four tag profiles.
- Content-addressed artwork originals, bounded sRGB derivatives, and journaled
  embedded and external artwork projection.
- SHA-256 manifests, BagIt export/restore, scheduled fixity history, explicit
  database integrity checking, machine-readable CLI output, pagination, and a
  one-million-track benchmark contract.
- A read-only, plan-first Beets migration command that preserves albums,
  singletons, external IDs, flexible fields, missing entries, and compatible
  configuration while creating only new rsbts output paths.
- Built-in MusicBrainz/Discogs aggregation with provider-qualified direct-ID
  routing, partial-result disclosure, and Discogs release/track identity
  separation.
- Explicit `write` and `move` commands routed through retained-original,
  identity-revalidating projection journals.
- Cross-platform/MSRV CI, fuzz, coverage, mutation, dependency-policy, SBOM,
  checksums, provenance, and multi-platform release automation.

### Changed

- The public API is hardened around validated newtypes, private invariant
  fields, non-exhaustive enums, and rsbts-owned error contracts.
- Deep audit is now a separately approved, resumable fixity plan rather than a
  synchronous scan.
- Removal defaults to retained quarantine; permanent deletion is an explicit
  purge plan.
- Schema version 10 integrates legacy Beets-compatible metadata with asset
  ownership, normalized provenance, durable operations, and constant-time
  statistics without conflating their external-ID models.

### Compatibility

- This is an intentional pre-1.0 minor-version API break from 0.2. See
  [API compatibility](docs/api-compatibility.md).

[Unreleased]: https://github.com/sxndmxn/rsbts/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/sxndmxn/rsbts/compare/v0.2.0...v0.4.0
