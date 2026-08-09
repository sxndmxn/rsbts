# Research: Accurate, Safe, Large-Scale Music Library Management

> Research snapshot: 2026-08-08
>
> Repository version reviewed: `rsbts` 0.2.0, commit `61409896c3bf682e697dfd71a815ac48f5736937`
>
> Normative companion: [Media-library requirements](./media-library-requirements.md)

This document records the evidence, experiments, primary-source research, and
design rationale behind the companion requirements. It is explanatory rather
than normative: when wording differs, the requirements document is the product
contract.

## 1. Current readiness and verdict

`rsbts` is an unusually thoughtful safety-oriented alpha, but it is not yet
trustworthy as the system of record for a large archival music collection.
Overall readiness is **C**.

Its strongest work is architectural intent: preview and approval are separate
from execution; mutations are journaled; destinations are not silently
clobbered; imports commit database rows before deleting move sources; removal
uses same-directory quarantine; migrations are backup-first; configuration is
strict; mutating queries are parsed before opening the database; SQL values are
bound; paths must round-trip through UTF-8; and dry runs do not modify the real
database or library. The Rust code also forbids unsafe code and denies common
panic-producing patterns.

The gaps that matter most for the project's stated purpose are persistent file
ownership, concurrent filesystem safety, exact-release identification,
provenance-rich metadata, managed tag writing, artwork lifecycle, and
million-item operation.

Until the P0 safety work is complete, the conservative operating profile is:

- Copy-only imports, one `rsbts` process at a time.
- Independent, tested backups of the database and media.
- No `--move`, `rm --delete`, or unattended `--yes` for valuable files.
- Treat the database as a rebuildable index rather than proof of ownership.
- Prefer explicit release IDs or MusicBrainz Picard for initial exact tagging,
  while recognizing that the current schema still merges or discards edition
  semantics.

| Area | Grade | Assessment |
|---|---:|---|
| Rust implementation discipline | B+ | Strict lints, unsafe forbidden, clean error handling, good tests |
| Safety architecture | B | Strong planning, approval, staging, and journal concepts |
| Proven file ownership and concurrency | D+ | No collection lease, persistent digest, or syscall-atomic acquisition |
| Catalog and provenance model | D | Flat album/item schema loses essential release semantics |
| Automated matching | D+ | Candidate incompleteness and false-confidence paths |
| Tagging and formats | C- | Useful reader, but extension-driven and no managed tag projection |
| Artwork management | D | Front image download only; no durable ownership or audit |
| Million-item operation | D | Whole-database snapshots and eager materialization |
| Release engineering | B- | Strong checks; weak portability, fuzzing, and release automation |

## 2. Safety, ownership, and audit

### 2.1 Persistent ownership is missing

The operation journal records BLAKE3 identities while work is active, but
successful completion deletes the journal row in
[`Library::complete_operation`](../src/db.rs#L651). A durable
[`Item`](../src/lib.rs#L95) stores size and mtime, not a content digest, asset
state, managed/unmanaged status, acquisition record, or last verified identity.

A disposable experiment demonstrated the resulting ambiguity:

1. Import a file.
2. Replace the cataloged destination externally with unrelated bytes.
3. Create a removal plan.
4. Confirm removal.

The removal planner fingerprints the new occupant and the executor deletes it.
There is no durable evidence with which to prove that the occupant is the asset
originally acquired by `rsbts`.

Completed journal entries therefore need to become compact append-only
operation history. Every managed asset needs persistent byte identity, audio
essence identity, verification state, ownership state, and acquisition
provenance.

### 2.2 There is no collection-wide writer lease

SQLite transactions serialize database writes, but filesystem manipulation
happens outside those transactions. Two processes can independently plan and
execute against the same paths.

Rust 1.89 provides stable
[`File::lock` and `File::try_lock`](https://doc.rust-lang.org/1.89.0/std/fs/struct.File.html#method.lock).
A permanent companion lock file should be authoritative:

- Shared lock for read-only browsing and planning.
- Exclusive lock before recovery or mutation.
- Close-on-crash releases the OS lock; PID text is diagnostic only.
- Filesystems without trustworthy locking and no-replace semantics require a
  constrained capability mode or refusal of destructive operations.

### 2.3 Narrow race windows remain

Removal validates source and quarantine state, then separately unlinks the
source in [`remove.rs`](../src/remove.rs#L178). Move imports similarly validate
and later unlink in [`import.rs`](../src/import.rs#L1323). Recovery checks paths
and then performs an ordinary rename in [`db.rs`](../src/db.rs#L1118), which can
overwrite a newcomer on Unix. Parent-symlink containment is also a pathname
check followed by later pathname I/O.

Existing replacement tests are valuable, but they replace a path before a call
or checkpoint. They do not interpose in the final validation-to-syscall window.

The safer pattern is acquire before trust:

- Anchor traversal to directory descriptors.
- Quarantine with `renameat2(RENAME_NOREPLACE)` or equivalent no-clobber
  semantics.
- Validate the acquired object.
- If it is wrong, restore no-replace or preserve both objects for review.
- Apply the same rule to rollback and recovery.
- On Linux, use constrained `openat2` resolution such as `RESOLVE_BENEATH` and
  `RESOLVE_NO_SYMLINKS`, preferably behind `rustix` or a capability-oriented
  abstraction.

### 2.4 Audit verifies catalog shape, not asset identity

[`Library::audit`](../src/db.rs#L193) checks whether each path exists and whether
its stored size is null. It does not compare actual size, mtime, digest, audio
properties, tags, or artwork projection.

In a disposable collection, an imported destination was truncated from 20,465
bytes to 128 bytes. The database retained the original size and `audit` returned
“no issues found.” Importing a source with mode `0600`, an old mtime, and an
xattr produced a destination with a different mode, current mtime, and no xattr,
while the database retained the source mtime. Normalization may be valid policy,
but the catalog must describe the state that was actually materialized.

Removing the album also left a managed `cover.jpg` orphan. The resulting empty
catalog passed audit. Audit must cover audio, art, sidecars, fixity, projection
state, and orphan ownership.

## 3. Matching and catalog accuracy

### 3.1 Candidate completeness is discarded

The MusicBrainz provider expands search hits into release details. It silently
discards failed detail lookups whenever at least one lookup succeeds in
[`musicbrainz.rs`](../src/musicbrainz.rs#L170). The matcher therefore treats a
partial candidate set as complete.

A single candidate is compared with a synthetic runner-up score of zero in
[`import.rs`](../src/import.rs#L891), so its runner-up margin becomes its entire
score. Configuration permits `search_limit = 1`. Dry run also selects candidate
zero in [`cli.rs`](../src/cli.rs#L232) rather than simulating the same acceptance
decision used by unattended execution.

A provider result needs to preserve completeness explicitly, for example:

```rust
SearchPage {
    candidates,
    requested,
    resolved,
    errors,
    complete,
}
```

Partial results and missing runner-up information must force abstention.

### 3.2 Search relevance is not edition confidence

A live MusicBrainz search for `Black Sabbath / Paranoid` returned 88 releases.
The first five all had search score 100 despite representing different German,
US, British, Italian, and later editions. Artist, title, and tracklist are not
enough to distinguish a specific issue.

MusicBrainz correctly distinguishes [Release](https://musicbrainz.org/doc/Release),
[Track](https://musicbrainz.org/doc/Track),
[Recording](https://musicbrainz.org/doc/Recording),
[Release Group](https://musicbrainz.org/doc/Release_Group), and
[Disc ID](https://musicbrainz.org/doc/Disc_ID). `rsbts` currently collapses much
of that structure:

- Candidate releases omit country, medium, label, catalog number, barcode,
  packaging, and disambiguation.
- The CLI displays little beyond artist, title, and year.
- Assignment cost emphasizes title and duration; disc and track numbers are
  mainly a post-assignment gate.
- Same-title tracks can be assigned incorrectly.
- Existing embedded MusicBrainz identifiers are not read.
- Credited-as artist names are replaced with canonical names.
- A recording ID is stored in a field named as a track ID, losing release-track
  identity.
- Printed positions such as `A1` and partial dates are discarded.

### 3.3 Matching must express levels of identity

Candidate retrieval should be followed by an edition-specific evidence diff.
The UI and API should report separate confidence for:

1. Recording identity.
2. Release-group identity.
3. Exact-release identity.

Hard exact-edition evidence includes embedded IDs, explicit user-selected IDs,
barcode plus label/catalog number, Disc ID/TOC, matrix/runout, exact digital
receipt/source, and trustworthy package or rip evidence. Acoustic fingerprints
provide recording-level candidates only.

Medium, position, printed position, track count, data/hidden tracks, pregaps, and
medium boundaries must participate in assignment. Manual decisions should be
saved as provenance-backed locks.

Before fuzzy exact-release matching becomes unattended, evaluate it against a
hard-negative corpus stratified by release group, reissues, territories, and
media. A useful initial gate is zero false accepts in at least 30,000 independent
hard-negative cases, which supports an approximate one-sided 95% false-accept
bound below 0.01%. Coverage is secondary; abstention is correct behavior.

## 4. Metadata authorities and provider policy

The best attainable catalog does not copy one provider into a flat row. It
maintains evidence from several authorities, resolves field-level claims under a
versioned policy, and preserves provenance.

| Source | Appropriate authority | Important limit |
|---|---|---|
| Local files, CUE/logs, receipts and package scans | Exact owned bytes and edition evidence | Existing tags can still be wrong |
| MusicBrainz | Artist credits, releases, recordings, works, relationships and stable IDs | Search relevance is not exact-edition probability |
| Cover Art Archive | Release-linked artwork and image roles | Release-group fallback may not match the owned edition |
| Discogs | Formats, labels/catalog numbers, identifiers/runouts, credits, country and styles | API image/user/marketplace data has different terms |
| AcoustID/Chromaprint | Recording candidate generation | Cannot prove release, edition, or mastering |
| RateYourMusic/Sonemic | Manual genres, descriptors, lists and personal curation | No supported public API; do not scrape |
| User claims | Final authority for selected fields | Retain provenance and allow unlocking |

### 4.1 MusicBrainz and Cover Art Archive

MusicBrainz should provide canonical artist-credit, release, recording, work,
and relationship identities. Its core data is CC0 while supplementary data uses
different terms; the catalog must preserve that boundary. See the
[MusicBrainz data license](https://musicbrainz.org/doc/About/Data_License).

For large-scale enrichment, prefer direct identifiers, caching, and
[MusicBrainz database downloads](https://musicbrainz.org/doc/MusicBrainz_Database/Download)
over repeatedly driving the public search API. Exact-release artwork should use
the role and release information exposed by the
[Cover Art Archive API](https://musicbrainz.org/doc/Cover_Art_Archive/API), not
only the `/front` byte endpoint. Picard's
[cover-art options](https://picard-docs.musicbrainz.org/en/latest/config/options_cover.html)
also make the distinction between exact-release and release-group fallback
explicit.

### 4.2 Discogs

Discogs is especially useful for physical and digital issue facts: quantity,
format name and descriptions, labels, catalog numbers, identifiers and runouts,
country, date, credits, genres, and styles. Preserve every value rather than
flattening to the first format or label. Keep general genres separate from more
specific styles; the [Discogs format guidelines](https://support.discogs.com/hc/en-us/articles/360005006654-Database-Guidelines-6-Format)
illustrate the structured distinctions that would otherwise be lost.

Prefer the [monthly CC0 data dumps](https://data.discogs.com/) for durable factual
ingestion. User, marketplace, and image content is governed separately by the
[Discogs API terms](https://support.discogs.com/hc/en-us/articles/360009334593-API-Terms-of-Use),
so it should not be assumed to have the same archival or redistribution rights.
This is a technical licensing boundary, not legal advice.

### 4.3 AcoustID and Chromaprint

The [AcoustID service](https://acoustid.org/webservice) and
[Chromaprint](https://github.com/acoustid/chromaprint) are useful for retrieving
recording candidates. Chromaprint deliberately trades some precision for speed
and robustness. Neither a match nor a near-identical fingerprint proves exact
release, mastering, pressing, or byte identity.

### 4.4 RateYourMusic and Sonemic

As of the research snapshot, [Sonemic](https://sonemic.com/) says an API is
planned after Sonemic is complete; there is no supported public integration.
RateYourMusic's [robots policy](https://rateyourmusic.com/robots.txt) prohibits
using automated crawling as a substitute.

Supported routes should therefore be limited to:

- A manually supplied RYM/Sonemic URL or ID.
- A lawful user-provided export.
- Manual genre, descriptor, language, list, and personal-rating claims.
- A future official API or explicit partnership.

Community ratings and descriptors are curation evidence, not release identity.

## 5. Target catalog and provenance architecture

The target data flow is:

```text
local files, package evidence, provider snapshots, manual claims
                              │
                              ▼
                 immutable observations/claims
                              │
                policy + confidence + field locks
                              ▼
                   resolved canonical catalog
                     │          │          │
                     ▼          ▼          ▼
                    tags       paths      artwork
                       journaled projections
                              │
                              ▼
           persistent asset fixity, audit, and recovery
```

The main musical graph should be:

```text
ReleaseGroup → Release → Medium → ReleaseTrack → Recording → Work
```

Supporting entities include:

- Ordered artist credits with credited names and join phrases.
- Artists, labels, catalog numbers, release events, territories, formats,
  packaging, barcodes, matrix/runout, and other identifiers.
- Credits with typed roles.
- `FileAsset` separate from musical identity, plus audio streams or segments.
- Typed provider/entity identifiers.
- Artwork blobs, roles, provenance, and generated derivatives.
- Root UUID plus relative path and filesystem capability profile.
- Ancillary CUE, rip log, checksum, PDF, lyric, and booklet assets.

Keep normalized materialized current-state tables for fast browsing, but also
retain compressed raw provider snapshots and immutable field claims. Model
`Unknown`, `Absent`, `NotApplicable`, and `Conflict` explicitly rather than
turning “Unknown” into canonical metadata. User edits are first-class, lockable
claims. Provider refresh produces a reviewable diff and never silently rewrites
files.

During migration, legacy files start as `unverified` and are hashed
incrementally. They cannot be deleted until verified. Lost edition semantics
must remain unknown rather than being invented. Existing external “track” IDs
should be migrated as recording IDs where that is what the old provider stored.

## 6. Tags and file formats

[`read_tags`](../src/tags.rs#L16) forces the parser from the filename extension
and derives the stored format from that extension. Disposable fixtures produced
these results:

- FLAC, MP3, Vorbis, Opus, and AAC parsed.
- ALAC in `.m4a` was incorrectly reported as AAC.
- WavPack tags parsed but the format became `Unknown`; scanning skipped it.
- WMA and MKA failed.
- Valid FLAC named `.mp3` failed.
- Opus renamed `.ogg` failed; `.oga` parsed but was reported only as Ogg.
- Vorbis renamed `.opus` failed.
- Repeated FLAC `ARTIST` and `GENRE` values collapsed to the first value.

Lofty 0.24 already understands WavPack, APE, Musepack, Speex, and MP4 codec
differences. Expansion should use content probing and typed media properties,
not a wider extension switch. Container, codec, and tag dialect are separate
properties.

Advertised support needs a versioned capability matrix per
`container + codec + tag dialect`, covering read, native write, multivalue
behavior, unknown-tag/attachment preservation, embedded artwork, round-trip
validation, and unchanged audio essence.

Initial full-support targets are FLAC, MP3, Ogg Vorbis/Opus/Speex, MP4 AAC/ALAC,
standalone AAC, WAV/BWF, AIFF, WavPack, APE, and Musepack. WMA/ASF,
Matroska/MKA/WebM audio, CAF, DSF/DFF, TTA, and TAK can begin read-only or later.
Unsupported media should remain opaque and sidecar-manageable, never mislabeled.

### 6.1 Canonical tag model and profiles

Use a canonical internal model and render it through explicit profiles:

- Archival/native-rich.
- Picard/Navidrome interoperability.
- ID3v2.3 legacy compatibility.
- Portable-player compatibility.

Store a display `ARTIST` and repeated `ARTISTS`, with equivalent album-artist
fields. Preserve multiple genres and roles, compilation status, track/disc
totals, release and original dates, MusicBrainz release/release-group/recording/
release-track/work/artist IDs, label, catalog number, country, media, barcode,
classical work/movement/conductor/composer fields, and derived ReplayGain or
loudness values.

Xiph comments explicitly support repeated fields in the
[Vorbis comment specification](https://xiph.org/vorbis/doc/v-comment.html).
[Picard's tag mapping](https://picard-docs.musicbrainz.org/en/latest/appendices/tag_mapping.html)
and [Navidrome's multivalue guidance](https://www.navidrome.org/docs/usage/library/tagging/)
are useful interoperability baselines.

Native tags and attachments unknown to the canonical model must be preserved
unless the selected profile explicitly removes them. Lofty's
[`SplitTag`](https://docs.rs/lofty/latest/lofty/tag/trait.SplitTag.html) can help
separate native and generic state.

Tag projection should use the generic journal:

1. Parse and inventory all native tags and attachments.
2. Write a sibling temporary file.
3. Sync and reopen it.
4. Validate canonical fields, unknown-field preservation, and audio essence.
5. Publish no-clobber.
6. Retain recovery state until durable completion.

## 7. Artwork management

The current provider fetches `/front`, applies a byte limit and magic check, and
writes a conventional path. It does not retain CAA JSON, role, exact-release
provenance, dimensions, decode validity, rights information, hash, or ownership.
[`plan_artwork`](../src/import.rs#L345) can also assign the album art path before
discovering that an existing file is unowned.

Preferred source order is:

1. Exact local scan or package artwork.
2. Approved Cover Art Archive art attached to the exact release.
3. Explicitly labeled release-group fallback.
4. User-approved external art.

Retain the original image content-addressably with role, source release,
provider, edit/approval state, MIME, decoded dimensions, digest, and applicable
rights metadata. Support front, back, booklet, disc, obi, spine, and other roles.
Decode under bounded pixel and resource limits rather than trusting magic bytes.

Generate reproducible sRGB player derivatives—approximately 1200 px by default,
configurable—without automatic crop, upscale, or generative modification.
External `cover.*` and optional embedded-front derivatives are projections.
Artwork must participate in ownership, audit, removal, rollback, and recovery.

## 8. Naming, paths, and roots

The current sanitizer in [`pathformat.rs`](../src/pathformat.rs#L296) handles
separators and dot components, but it lacks a full Unicode policy,
case/normalization collision keys, Windows reserved names and trailing-period
rules, byte/component limits, and deterministic collision suffixes. Templates
also lack the edition fields necessary for stable disambiguation.

Database identity should be `RootId + RelativePath`, not an absolute path. Each
root needs online/offline/read-only/degraded state and a capability contract for
case sensitivity, normalization, locking, atomic rename, no-replace publication,
hardlinks, xattrs, and timestamp behavior.

A stable default layout is:

```text
Album Artist/
  Release Year - Release [Country; Label Catalog; Medium]/
    Disc-Track - Track Artist - Title.ext
```

Examples:

```text
Black Sabbath/
  1987 - Paranoid [US; Creative Sounds 6007; CD]/
    01-01 - War Pigs.flac

Various Artists/
  1994 - Pulp Fiction [US; MCA MCAD-11103; CD]/
    01-01 - Dick Dale & His Del-Tones - Misirlou.flac
```

Only append a short stable provider-ID digest when the edition signature itself
collides. Never depend on collection or import order.

Use NFC for stored/display names and NFKC plus full case folding only for search
and collision comparison. Truncate on grapheme boundaries and append a stable
hash. Provide portable, native-filesystem, and archival naming profiles.
Renaming is a previewable, journaled projection, not an automatic consequence of
provider refresh.

## 9. Million-item scale research

A synthetic benchmark used one million tracks and 100,000 albums on a Ryzen
5900X with SQLite on tmpfs. These numbers are diagnostic, not a product SLA. An
early fixture included exploratory candidate indexes; the final
current-schema-like copy removed those indexes and was vacuumed.

| Operation | Observed result |
|---|---:|
| Current-schema-like database | 564.3 MB / 538.1 MiB |
| One `PRAGMA integrity_check` | 3.21 s mean |
| Effective open floor from checks before and after migration | At least 6.4 s |
| CLI statistics | 10.20 s mean |
| Equivalent raw statistics SQL | 316.6 ms mean |
| CLI FTS search | 9.62 s |
| Equivalent raw FTS query | About 3 ms |
| `ls` peak RSS | About 683 MiB |
| Dry-run empty removal | 16.92 s, about 697 MiB RSS |
| Audit of one million missing paths | 13.0 s, about 135 MiB before output |
| Keyset page near row 900,000 | 1.8 ms |
| Equivalent `OFFSET` page | 19.6 ms |

[`open_snapshot`](../src/db.rs#L147) copies the whole source database into memory
for dry runs. [`run_migrations`](../src/migrations.rs#L39) executes a full
integrity check on every open and again after the migration path, even when no
migration is needed. A full integrity check alone averaged 3.21 seconds on the
final fixture.

The initial schema declares `path TEXT UNIQUE` and also creates
`idx_items_path`, duplicating SQLite's unique autoindex. On the compact fixture,
each path index occupied 65.9 MB; dropping the redundant explicit index saves
one copy.

The scale design should therefore use:

- Read-only opens for ordinary reads and current-schema dry runs.
- Migration work only when schema inspection says it is needed.
- Explicit quick versus full integrity audit.
- Streaming reports and bounded result limits.
- Keyset pagination rather than deep `OFFSET` browsing.
- Deliberate indexes for `album_id`, common browse order, and partial unique
  provider IDs; remove the redundant path index.
- `ANALYZE`/optimization after major changes.
- WAL only if representative measurement supports it.

Provider scale is equally important. For 100,000 albums, one search, five detail
lookups, and one artwork request per album is 700,000 calls. At a one-second
limiter the theoretical lower bound is 8.10 days, excluding retries and network
time. Direct IDs, persistent raw-response cache, local dumps, resumable queues,
and provider-specific batching are required.

## 10. Beets repository comparison

The beets comparison used commit
[`74c2d98ee5c30d6dd6d4ffd4e6389984827a9f02`](https://github.com/beetbox/beets/tree/74c2d98ee5c30d6dd6d4ffd4e6389984827a9f02)
from 2026-08-05.

Useful ideas to borrow:

- Rich `AlbumInfo` and `TrackInfo` representations.
- Matching weights for label, catalog number, country, medium, and IDs.
- Unequal track assignment.
- Provider plugins and direct/batch-ID hooks.
- Resumable import sessions.
- Expressive queries, path profiles, and native tag mappings.
- The purpose of `%aunique`: distinguishing otherwise colliding editions.

The beets [autotagger](https://beets.readthedocs.io/en/stable/guides/tagger.html)
has substantially richer distance handling than `rsbts`, including strong and
medium recommendations and missing/unmatched penalties. It remains a heuristic
distance model, not a calibrated exact-edition probability.

Semantics not to copy:

- `%aunique` is import-order-dependent; the first edition remains unsuffixed.
  `rsbts` should use a deterministic edition signature.
- The importer adds database items before filesystem manipulation in
  [`stages.py`](https://github.com/beetbox/beets/blob/74c2d98ee5c30d6dd6d4ffd4e6389984827a9f02/beets/importer/stages.py#L269-L304).
- Tag writing can catch/log failure and continue in
  [`models.py`](https://github.com/beetbox/beets/blob/74c2d98ee5c30d6dd6d4ffd4e6389984827a9f02/beets/library/models.py#L989-L1005).
- The Discogs plugin flattens rich format, label, style, and artwork information.

These are different durability semantics, not a criticism that beets fails its
own contract. `rsbts` explicitly aspires to stronger ownership and recoverability
guarantees.

## 11. Preservation strategy

Use BLAKE3 for fast operational verification and SHA-256 for archival
interoperability and exported manifests. Keep whole-file byte identity distinct
from decoded audio-essence identity. Only byte-identical assets may be
auto-deduplicated; fingerprints can suggest duplicates but never authorize
deletion. Avoid hardlink deduplication where later retagging could mutate more
than one catalog entry.

Retain original CUE sheets, rip logs, checksums, AccurateRip evidence, and other
package context. The Library of Congress recommends early fixity and auditable
integrity workflows in its
[Data Integrity Management guidance](https://www.loc.gov/programs/digital-collections-management/inventory-and-custody/data-integrity-management/).
Provide [BagIt](https://www.rfc-editor.org/info/rfc8493/) manifests for exchange
and borrow immutable-version and fixity ideas from
[OCFL](https://ocfl.io/1.1/spec/) without necessarily implementing full OCFL.

Removal should default to retained quarantine and require a separate explicit
purge. Quick and deep audits should be schedulable and resumable, and backup
restore drills should be a normal operational requirement.

## 12. Rust repository and release engineering

The repository contains approximately 7,101 Rust lines including tests and 82
tests. Positive engineering properties include:

- `unsafe` forbidden.
- Production `unwrap`, `expect`, `panic`, `todo`, and `unimplemented` denied.
- Strict Clippy configuration in [`Cargo.toml`](../Cargo.toml#L1).
- Locked dependency resolution and good package metadata.
- Clear README, MIT license, and repository-specific agent guidance.
- Stronger coverage in database, migrations, and CLI integration than in
  provider and orchestration code.

The largest modules combine several concerns. Keep one crate until the domain
stabilizes, but separate internal responsibilities conceptually as:

```text
storage/{schema,queries,migration}
fsops/{lease,executor,recovery}
metadata/{model,claims,resolution,matching,providers}
media/{probe,tags,art}
cli
```

Use validated newtypes for `RootId`, release/recording IDs, content hashes,
relative paths, partial dates, scores, and operation states. Avoid stringly typed
roles and state machines. Provider APIs need direct/batch ID hooks, explicit
completeness, and streaming or resumable results. A generic filesystem-operations
trait can support deterministic failpoints.

The current public API in [`lib.rs`](../src/lib.rs#L1) exposes many concrete
fields, exhaustive enums, and dependency error types. A documentation audit
found 296 missing-public-documentation items. Before 1.0, prefer private fields,
validated constructors/accessors, `#[non_exhaustive]` extensible enums, and a
small stable facade. The
[Rust API Guidelines](https://rust-lang.github.io/api-guidelines/future-proofing.html)
and [Cargo SemVer guidance](https://doc.rust-lang.org/stable/cargo/reference/semver.html)
support doing this redesign before compatibility commitments harden.

Current CI in [`.github/workflows/rust.yml`](../.github/workflows/rust.yml#L1) is
Linux-only and the MSRV job runs `check` rather than the full tests. Add Windows,
macOS, and full MSRV tests; format golden fixtures; property/fuzz tests; crash
failpoints; mutation testing for safety logic; coverage; SemVer checking;
dependency license/source policy; and automated dependency updates.

GitHub Actions should be pinned to reviewed immutable SHAs, consistent with
[GitHub's security guidance](https://docs.github.com/en/actions/how-tos/security-for-github-actions/security-guides/security-hardening-for-github-actions).
Tag-driven releases should include checksums, SBOM, provenance, and
[artifact attestations](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations).

`cargo audit` found no known vulnerability, but reported the unmaintained
transitive `paste` advisory
[RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html), reached
through Lofty and without a direct safe upgrade. Track upstream. `reqwest` also
uses broad default features that bring native TLS and several system/runtime
dependencies; narrow features deliberately for portable releases.

## 13. Prioritized roadmap and release gates

### P0: v0.3 safety boundary

1. Disable unattended fuzzy autoacceptance and clearly mark destructive commands
   experimental.
2. Add a collection lease.
3. Introduce root and asset identity, persistent hashes, ownership, and
   verification state; migrate legacy rows as unverified.
4. Implement descriptor-anchored, no-replace acquisition, rollback, and
   recovery behind a generic operation state machine.
5. Add deterministic failpoints and subprocess crash/race tests.
6. Audit actual file state, art, and ancillary assets; default removal to
   retained quarantine.
7. Fix partial/single-candidate confidence, dry-run selection, and artwork
   planning.
8. Remove the redundant path index and stop cloning the full database for dry
   runs.

### P1: exact catalog

1. Add the normalized release/recording/work/artist-credit graph and immutable
   provenance claims.
2. Group imports directory-first with compilation, multidisc, box-set, CUE, and
   sidecar support.
3. Add deterministic portable naming and root capability profiles.
4. Add MusicBrainz direct IDs, raw-response caching, resumable queues, and local
   bulk-data support.

### P2: controlled projections and curation

1. Build and calibrate the hard-negative matching corpus.
2. Add generic journaled tag, path, and artwork projections.
3. Publish and test the media capability matrix.
4. Add Discogs CC0 facts, AcoustID recording candidates, and manual RYM claims.

### P3: archival operations and scale

1. Add content-addressed art originals and deterministic derivatives.
2. Add background fixity, manifests, backup verification, and restore drills.
3. Add loudness analysis, JSON/JSONL streaming, durable plans, review queues, and
   resumable operations.
4. Revisit SQLite journal mode only after representative measurement.

Release gates are:

- No lost or overwritten unowned bytes under the failure and race matrix.
- Recovery is idempotent after every syscall, sync, and commit boundary.
- A second writer stops before filesystem I/O.
- No destructive operation is allowed for a legacy-unverified asset.
- Incomplete provider results always abstain.
- At least 30,000 hard-negative exact-edition cases with zero fuzzy false
  accepts before unattended fuzzy matching.
- Every advertised media tuple passes read/write/round-trip, unknown-data
  preservation, and audio-essence tests.
- No normal million-item operation uses memory proportional to catalog size.
- Reference p95 targets: open below 250 ms, browse page below 200 ms, statistics
  below one second, and dry-run RSS below 128 MiB.
- Backup restores and exported manifests are verified routinely.

## 14. Verification record

The research pass left the repository clean and ran the complete release checks:

- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `cargo test --locked --all-targets` — 82 tests
- `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`
- `cargo package --locked`
- `cargo audit` — no known vulnerabilities; one unmaintained transitive advisory
- Locked dependency metadata validation

Measured line coverage was 76.42%. CLI orchestration and MusicBrainz behavior
were the thinnest important areas; database, migrations, and integration tests
were stronger.

The autonomous investigation ran for 4 hours, 0 minutes, and 29 seconds. Its
normative output is maintained in the adjacent
[media-library requirements](./media-library-requirements.md).
