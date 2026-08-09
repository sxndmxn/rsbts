# Requirements: Accurate, Safe, Large-Scale Music Library Management

> Status: implemented product contract for rsbts 0.3, derived from the 2026-08-08 research snapshot
>
> Rationale and evidence: [Media-library research](./media-library-research.md)
>
> Implementation evidence: [Requirements traceability](./requirements-traceability.md)

This document defines the normative requirements for evolving `rsbts` into a
safe, provenance-rich manager for large music collections. **Shall** indicates a
required behavior. **Should** indicates a preferred behavior that may be
deferred only with a documented reason and an equivalent safety property.

Priorities are cumulative:

- **P0:** release-blocking safety work required before destructive workflows
  are considered trustworthy.
- **P1:** core catalog identity and exact metadata work.
- **P2:** managed providers, tags, artwork, and release engineering.
- **P3:** archival operations and million-item scale maturity.

## 1. Safety and ownership

Research basis: [Safety, ownership, and audit](./media-library-research.md#2-safety-ownership-and-audit).

| ID | Priority | Requirement |
|---|---:|---|
| SAF-001 | P0 | The system shall allow only one recovery or mutating operation per library at a time, enforced by an OS-backed collection lease. |
| SAF-002 | P0 | Every managed file shall have a persistent asset identity independent of its path. |
| SAF-003 | P0 | Persistent asset identity shall include a fast operational digest, SHA-256 archival digest, byte size, and verification state. |
| SAF-004 | P0 | The system shall distinguish whole-file byte identity from decoded audio-essence identity. |
| SAF-005 | P0 | Legacy catalog entries shall begin as `unverified` and shall not be destructively moved, replaced, purged, or deduplicated until verified. |
| SAF-006 | P0 | The system shall never overwrite an existing destination, quarantine path, rollback target, or recovery target. |
| SAF-007 | P0 | Filesystem mutation shall acquire or quarantine the target object atomically before trusting or deleting it. |
| SAF-008 | P0 | Filesystem traversal for mutation shall be anchored to a trusted root and shall reject symlink or path escapes at the I/O boundary. |
| SAF-009 | P0 | If ownership, identity, or recovery state is uncertain, the system shall preserve all candidate files and require review. |
| SAF-010 | P0 | Every file-creating, moving, tag-writing, artwork-writing, quarantining, restoring, and deleting operation shall use a persistent typed state-machine journal. |
| SAF-011 | P0 | Recovery shall be idempotent after interruption at any filesystem mutation, durability sync, or database commit boundary. |
| SAF-012 | P0 | Completed operations shall leave compact durable history containing affected assets, decisions, and final identities. |
| SAF-013 | P0 | Removal shall default to recoverable quarantine with a retention policy; permanent purge shall be a separate explicit operation. |
| SAF-014 | P0 | A dry run shall not write to the configured database, library, journal, provider cache, or recovery state. |
| SAF-015 | P0 | A dry run shall apply the same validation, candidate selection, confidence gates, and collision policy as real execution. |
| SAF-016 | P0 | A mutating plan shall be revalidated against current source identity and destination state immediately before execution. |
| SAF-017 | P0 | Imported database rows shall become durable before a move source is eligible for deletion. |
| SAF-018 | P0 | No rollback or recovery action shall sacrifice a newcomer occupying an expected path. |

## 2. Audit, backup, and preservation

Research basis: [Audit verifies catalog shape, not asset identity](./media-library-research.md#24-audit-verifies-catalog-shape-not-asset-identity)
and [Preservation strategy](./media-library-research.md#11-preservation-strategy).

| ID | Priority | Requirement |
|---|---:|---|
| AUD-001 | P0 | Audit shall compare cataloged and actual byte size, mtime, content digest, media properties, and managed projection state. |
| AUD-002 | P0 | Audit shall cover audio files, artwork, CUE sheets, rip logs, checksums, lyrics, PDFs, and other managed ancillary assets. |
| AUD-003 | P0 | Audit shall distinguish missing, modified, replaced, unverified, corrupt, orphaned, offline, and policy-divergent assets. |
| AUD-004 | P0 | Audit shall report database metadata that does not describe the actual materialized file. |
| AUD-005 | P0 | Quick state audit and deep fixity/integrity audit shall be separate modes. |
| AUD-006 | P1 | Only byte-identical assets may be automatically deduplicated. |
| AUD-007 | P1 | Acoustic fingerprints or audio-essence similarity shall never independently authorize deletion. |
| AUD-008 | P3 | Deep audit shall be streamed, resumable, cancellable, and bounded in memory. |
| AUD-009 | P3 | The system shall produce interoperable SHA-256 manifests and support BagIt-compatible export. |
| AUD-010 | P3 | Backup validity shall be demonstrated through repeatable restore and manifest-verification workflows. |
| AUD-011 | P3 | Scheduled fixity checks shall preserve an auditable history of results and failures. |

## 3. Catalog and provenance

Research basis: [Target catalog and provenance architecture](./media-library-research.md#5-target-catalog-and-provenance-architecture).

| ID | Priority | Requirement |
|---|---:|---|
| CAT-001 | P1 | The catalog shall represent Release Group, Release, Medium, Release Track, Recording, and Work as distinct entities. |
| CAT-002 | P1 | File assets shall be separate from musical entities so one recording or release track can map to multiple files or segments. |
| CAT-003 | P1 | Artist credits shall preserve artist IDs, credited names, ordering, join phrases, and typed roles. |
| CAT-004 | P1 | Releases shall support multiple labels, catalog numbers, release events, territories, identifiers, and media. |
| CAT-005 | P1 | Track positions shall preserve medium, numeric position, printed position, hidden/data-track status, pregap, and totals. |
| CAT-006 | P1 | Dates shall support partial precision and distinguish release date from original-release date. |
| CAT-007 | P1 | External identifiers shall be provider- and entity-typed; a recording ID shall not be stored as a release-track ID. |
| CAT-008 | P1 | Imported metadata shall be stored as immutable sourced claims with provider, retrieval time, confidence, and evidence. |
| CAT-009 | P1 | Provider responses used for resolution shall be retained as versioned raw snapshots, subject to source licensing policy. |
| CAT-010 | P1 | Canonical fields shall be materialized from claims under an explicit, versioned resolution policy. |
| CAT-011 | P1 | Manual corrections shall be first-class claims that can be locked against provider refresh. |
| CAT-012 | P1 | `Unknown`, `Absent`, `NotApplicable`, and `Conflict` shall be represented explicitly rather than encoded as placeholder text. |
| CAT-013 | P1 | Provider refresh shall produce a reviewable field-level diff and shall not silently rewrite canonical fields or media files. |
| CAT-014 | P1 | CUE sheets, rip logs, checksums, PDFs, lyrics, scans, and booklets shall be modelable as related assets rather than ignored files. |
| CAT-015 | P1 | The catalog shall preserve compilation, multidisc, box-set, image+CUE, and multi-segment relationships without flattening their identity. |

## 4. Matching and review

Research basis: [Matching and catalog accuracy](./media-library-research.md#3-matching-and-catalog-accuracy).

| ID | Priority | Requirement |
|---|---:|---|
| MAT-001 | P0 | Unattended fuzzy exact-release acceptance shall remain disabled until MAT-012 is satisfied. |
| MAT-002 | P0 | An incomplete provider result shall always cause abstention from unattended acceptance. |
| MAT-003 | P0 | A single search candidate shall not receive a synthetic runner-up margin or be treated as evidence of uniqueness. |
| MAT-004 | P0 | Non-finite scores, invalid thresholds, and malformed candidate data shall fail closed. |
| MAT-005 | P1 | Matching shall report separate confidence for recording identity, release-group identity, and exact-release identity. |
| MAT-006 | P1 | Exact-release confidence shall rely on edition evidence such as barcode, label/catalog number, Disc ID/TOC, matrix/runout, medium, country, receipt, or explicit provider ID. |
| MAT-007 | P1 | Acoustic fingerprints shall contribute only recording-level evidence. |
| MAT-008 | P1 | Track assignment shall consider title, duration, medium, numeric and printed position, medium boundaries, track count, hidden tracks, data tracks, and pregaps. |
| MAT-009 | P1 | Existing embedded MusicBrainz and other supported provider IDs shall be consumed before fuzzy text search. |
| MAT-010 | P1 | The review interface shall show field-level differences, contradictory evidence, candidate-set completeness, and reasons for confidence or abstention. |
| MAT-011 | P2 | Manual match decisions shall be saved as provenance-backed locks that can later be reviewed or revoked. |
| MAT-012 | P2 | Fuzzy exact-release autoacceptance shall require zero false accepts in at least 30,000 independent hard-negative evaluation cases stratified by release group and edition. |
| MAT-013 | P2 | Matching evaluation shall report acceptance coverage and calibrated error bounds, with abstention preferred over false acceptance. |

## 5. Metadata providers and licensing

Research basis: [Metadata authorities and provider policy](./media-library-research.md#4-metadata-authorities-and-provider-policy).

| ID | Priority | Requirement |
|---|---:|---|
| PRO-001 | P1 | Provider interfaces shall report candidate completeness, requested and resolved counts, partial failures, and retriable errors. |
| PRO-002 | P1 | Providers shall support direct-ID lookup in addition to text search where the source permits it. |
| PRO-003 | P1 | Enrichment shall use a persistent, resumable, rate-limited queue with cached raw responses. |
| PRO-004 | P1 | MusicBrainz shall be authoritative for its entity IDs and relationship model, but search relevance shall not be treated as exact-edition probability. |
| PRO-005 | P1 | Metadata fields and raw snapshots shall retain their source-specific license classification. |
| PRO-006 | P2 | Discogs ingestion shall preserve all formats, descriptions, labels, catalog numbers, identifiers, credits, genres, and styles without lossy first-value selection. |
| PRO-007 | P2 | Discogs general genres and specific styles shall remain separate concepts. |
| PRO-008 | P2 | Durable Discogs bulk ingestion shall prefer licensed CC0 database dumps; separately governed API content shall retain its distinct restrictions and attribution. |
| PRO-009 | P2 | AcoustID and Chromaprint shall generate recording candidates only and shall not assert release, edition, mastering, or byte identity. |
| PRO-010 | P2 | RateYourMusic/Sonemic data shall enter only through an official API, explicit partnership, lawful user-provided export, or manual entry. |
| PRO-011 | P2 | `rsbts` shall not crawl or scrape RateYourMusic. |
| PRO-012 | P2 | Community ratings, genres, descriptors, and personal lists shall be stored as sourced curation claims rather than identity facts. |

## 6. Tags and media formats

Research basis: [Tags and file formats](./media-library-research.md#6-tags-and-file-formats).

| ID | Priority | Requirement |
|---|---:|---|
| TAG-001 | P2 | Media type shall be determined from content and codec properties, not filename extension alone. |
| TAG-002 | P2 | Container, audio codec, and tag dialect shall be modeled separately. |
| TAG-003 | P2 | Supported-format claims shall be published as a versioned capability matrix covering read, write, multivalue, artwork, preservation, and validation behavior. |
| TAG-004 | P2 | Initial full-support targets shall include FLAC, MP3, Ogg Vorbis/Opus/Speex, MP4 AAC/ALAC, standalone AAC, WAV/BWF, AIFF, WavPack, APE, and Musepack. |
| TAG-005 | P2 | Unsupported files shall remain catalogable as opaque assets and shall never be mislabeled or rewritten through an incompatible dialect. |
| TAG-006 | P2 | Repeated artists, album artists, genres, identifiers, and roles shall remain multivalued. |
| TAG-007 | P2 | Unknown native tags, attachments, padding, and noncanonical metadata shall be preserved unless a selected profile explicitly removes them. |
| TAG-008 | P2 | Tag output shall support archival/native-rich, Picard/Navidrome, ID3v2.3 legacy, and portable-player profiles. |
| TAG-009 | P2 | Canonical tag metadata shall include display and multivalue artist credits, dates, totals, provider IDs, labels, catalog numbers, territory, medium, barcode, classical fields, and applicable loudness data. |
| TAG-010 | P2 | Tag writing shall use sibling temporary output, durability sync, reread validation, audio-essence validation, and no-clobber publication. |
| TAG-011 | P2 | A failed tag validation shall preserve the original file and sufficient evidence for diagnosis and recovery. |
| TAG-012 | P2 | Provider refresh shall not modify file tags until the user approves a tag-projection plan. |
| TAG-013 | P2 | Each advertised container, codec, and tag-dialect tuple shall have golden round-trip tests proving its stated preservation properties. |

## 7. Artwork

Research basis: [Artwork management](./media-library-research.md#7-artwork-management).

| ID | Priority | Requirement |
|---|---:|---|
| ART-001 | P2 | Artwork shall be associated with an exact release whenever that provenance is known. |
| ART-002 | P2 | Release-group artwork fallback shall be explicitly labeled as potentially inexact. |
| ART-003 | P2 | Original artwork shall be retained content-addressably without destructive normalization. |
| ART-004 | P2 | Artwork metadata shall retain role, source, provider release ID, MIME, decoded dimensions, digest, approval state, and applicable rights information. |
| ART-005 | P2 | The system shall support front, back, booklet, disc, obi, spine, and extensible additional artwork roles. |
| ART-006 | P2 | Image input shall be fully decoded under bounded pixel, byte, and resource limits rather than accepted from magic bytes alone. |
| ART-007 | P2 | Player derivatives shall be reproducible, sRGB, size-bounded, and shall not be automatically cropped, upscaled, or generatively modified. |
| ART-008 | P2 | Artwork creation, replacement, removal, audit, rollback, and recovery shall use the same ownership and journal rules as audio files. |
| ART-009 | P2 | External cover files and embedded front covers shall be projections derived from the retained original. |

## 8. Naming, paths, and library roots

Research basis: [Naming, paths, and roots](./media-library-research.md#8-naming-paths-and-roots).

| ID | Priority | Requirement |
|---|---:|---|
| NAM-001 | P1 | Catalog path identity shall use library root UUID plus relative path rather than an absolute path. |
| NAM-002 | P1 | Roots shall expose state and capabilities including online/offline, read-only, case behavior, normalization, locking, atomic rename, and no-replace support. |
| NAM-003 | P1 | Default directory names shall distinguish editions using stable release facts rather than import order. |
| NAM-004 | P1 | Compilation tracks shall support track artist in the filename without fragmenting album identity. |
| NAM-005 | P1 | Name collision resolution shall be deterministic and append a stable short identifier only when the edition signature collides. |
| NAM-006 | P1 | Stored/displayed names shall use NFC; search and collision keys shall use compatibility normalization plus full case folding without changing display text. |
| NAM-007 | P1 | Path rendering shall handle reserved names, trailing periods, illegal characters, component limits, Unicode grapheme boundaries, and case/normalization collisions. |
| NAM-008 | P2 | Naming shall support portable, native-filesystem, and archival profiles. |
| NAM-009 | P2 | Renaming shall be a previewable, journaled projection rather than an automatic side effect of metadata refresh. |
| NAM-010 | P1 | A root whose capabilities cannot satisfy a requested safety guarantee shall reject that operation or require an explicitly weaker non-destructive mode. |

## 9. Scale and operations

Research basis: [Million-item scale research](./media-library-research.md#9-million-item-scale-research).

| ID | Priority | Requirement |
|---|---:|---|
| OPS-001 | P0 | Ordinary read operations and current-schema dry runs shall not copy the entire database. |
| OPS-002 | P3 | Normal commands shall use bounded memory independent of total catalog size. |
| OPS-003 | P3 | Query and report output shall be streamed and support explicit limits or keyset pagination. |
| OPS-004 | P3 | Full database integrity checking shall be explicit or scheduled rather than performed on every normal open. |
| OPS-005 | P3 | At one million tracks, reference p95 targets shall be open below 250 ms, browse page below 200 ms, statistics below one second, and dry-run RSS below 128 MiB on the published benchmark environment. |
| OPS-006 | P3 | Deep collection operations may be linear in collection size but shall be resumable, cancellable, bounded in memory, and visibly report progress. |
| OPS-007 | P3 | Provider enrichment shall resume after interruption without repeating successful requests. |
| OPS-008 | P3 | Database indexes and journal mode shall be selected from measured representative workloads. |
| OPS-009 | P1 | The redundant explicit path index shall be removed through a compatible migration. |
| OPS-010 | P3 | Benchmark methodology, dataset shape, hardware, filesystem, cache state, and percentile calculation shall be published with performance claims. |

## 10. CLI and public API

Research basis: [Current readiness and verdict](./media-library-research.md#1-current-readiness-and-verdict)
and [Rust repository and release engineering](./media-library-research.md#12-rust-repository-and-release-engineering).

| ID | Priority | Requirement |
|---|---:|---|
| CLI-001 | P0 | Preview, approval, and execution shall remain distinct phases. |
| CLI-002 | P0 | Invalid or empty mutating queries shall fail before database open, migration, or recovery. |
| CLI-003 | P1 | Matching, audit, provider refresh, tag projection, path projection, and removal shall offer machine-readable JSON or JSONL results. |
| CLI-004 | P2 | Long-running work shall expose durable plan IDs, progress, resumability, cancellation, and actionable failure state. |
| API-001 | P1 | Public domain values with invariants shall use validated newtypes rather than unvalidated strings or floats. |
| API-002 | P1 | Public fields shall be private where construction invariants or future evolution require control. |
| API-003 | P1 | Extensible public enums shall be non-exhaustive before 1.0. |
| API-004 | P1 | Dependency-specific errors shall not become accidental permanent public API contracts. |
| API-005 | P1 | Public behavior and compatibility commitments shall be documented before the 1.0 boundary. |

## 11. Engineering and release quality

Research basis: [Rust repository and release engineering](./media-library-research.md#12-rust-repository-and-release-engineering).

| ID | Priority | Requirement |
|---|---:|---|
| ENG-001 | P0 | Safety tests shall inject failure after every filesystem mutation, durability sync, and database transition. |
| ENG-002 | P0 | Tests shall cover concurrent writers, source replacement, destination replacement, symlink races, permission failure, ENOSPC, interruption, rollback, and repeated recovery. |
| ENG-003 | P0 | A second writer shall be proven to stop before performing filesystem I/O. |
| ENG-004 | P2 | Every advertised format shall have golden read, native-write, multivalue, artwork, unknown-preservation, and audio-essence round-trip fixtures. |
| ENG-005 | P2 | Parsers, path rendering, matching, and journal state transitions shall receive property and fuzz testing. |
| ENG-006 | P2 | CI shall run full tests on the minimum supported Rust version and supported Linux, macOS, and Windows targets. |
| ENG-007 | P2 | CI actions and executable dependencies shall be pinned to reviewed immutable revisions. |
| ENG-008 | P2 | Releases shall include checksums, SBOM, provenance/attestation, changelog, and reproducible validation instructions. |
| ENG-009 | P2 | Public API compatibility shall be checked automatically before release. |
| ENG-010 | P2 | Dependency policy shall check vulnerabilities, licenses, sources, and unmaintained crates. |
| ENG-011 | P2 | The safety core shall have coverage and mutation-testing thresholds that detect removed validation and recovery branches. |
| ENG-012 | P2 | Automated provider tests shall use mocks, recorded lawful fixtures, or local datasets rather than live public-service calls. |

## 12. Release-blocking acceptance criteria

Research basis: [Prioritized roadmap and release gates](./media-library-research.md#13-prioritized-roadmap-and-release-gates).

The following gates summarize the contract and take precedence over schedule:

1. **Safety:** no failpoint or race test may lose or overwrite unowned bytes;
   recovery is idempotent at every tested boundary.
2. **Ownership:** destructive work is permitted only for persistently verified
   assets while the exclusive collection lease is held.
3. **Accuracy:** incomplete matching always abstains, and fuzzy exact-edition
   acceptance remains manual until MAT-012 passes.
4. **Projection integrity:** every advertised tag, artwork, or path projection
   preserves unknown data and audio essence or aborts without replacing the
   original.
5. **Scale:** routine million-item operations use bounded memory and meet the
   published OPS-005 reference targets.
6. **Recovery:** backup restore, journal recovery, and exported manifests are
   exercised rather than merely generated.

## 13. Explicit non-requirements and prohibitions

Research basis: [Provider policy](./media-library-research.md#4-metadata-authorities-and-provider-policy),
[Matching](./media-library-research.md#3-matching-and-catalog-accuracy), and
[Artwork](./media-library-research.md#7-artwork-management).

Unless a later requirements revision explicitly changes these boundaries,
`rsbts` shall not:

- Scrape RateYourMusic or use crawling as an unofficial API.
- Infer an exact release, edition, pressing, or mastering from an acoustic
  fingerprint alone.
- Automatically deduplicate merely similar audio.
- Treat a path occupant as owned solely because the path appears in SQLite.
- Silently rewrite file tags or paths after a provider refresh.
- Automatically crop, upscale, or generatively modify archival artwork.
- Overwrite a newcomer to make rollback or recovery convenient.
- Claim support for a format combination without its capability-matrix tests.
