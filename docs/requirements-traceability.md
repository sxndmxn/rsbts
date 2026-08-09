# Requirements traceability

This matrix is the implementation index for
[the normative requirements](./media-library-requirements.md). “Implemented”
means the behavior is present in rsbts 0.3 and covered by the cited automated
evidence; it is not a waiver of the release-blocking gates. The research note
remains rationale, not executable evidence.

The complete verification command is `cargo test --locked --all-targets`.
Supply-chain, compatibility, coverage, mutation, fuzz, cross-platform, and
million-track gates are defined in `.github/workflows/rust.yml`; the release
artifact gates are in `.github/workflows/release.yml`.

| Requirement | Status | Implementation | Verification |
|---|---|---|---|
| SAF-001 | Implemented | `src/lease.rs`, lease acquisition in `src/db.rs` | lease contention and pre-I/O writer tests |
| SAF-002 | Implemented | `src/asset.rs`, migration 004, `src/db.rs` | asset migration, verification, and ownership tests |
| SAF-003 | Implemented | `src/asset.rs`, migration 004, `src/db.rs` | asset migration, verification, and ownership tests |
| SAF-004 | Implemented | `src/asset.rs`, migration 004, `src/db.rs` | asset migration, verification, and ownership tests |
| SAF-005 | Implemented | `src/asset.rs`, migration 004, `src/db.rs` | asset migration, verification, and ownership tests |
| SAF-006 | Implemented | `src/fsops.rs`, `src/import.rs`, `src/remove.rs` | symlink/dangling/newcomer/no-clobber race tests |
| SAF-007 | Implemented | `src/fsops.rs`, `src/import.rs`, `src/remove.rs` | symlink/dangling/newcomer/no-clobber race tests |
| SAF-008 | Implemented | `src/fsops.rs`, `src/import.rs`, `src/remove.rs` | symlink/dangling/newcomer/no-clobber race tests |
| SAF-009 | Implemented | `src/fsops.rs`, `src/import.rs`, `src/remove.rs` | symlink/dangling/newcomer/no-clobber race tests |
| SAF-010 | Implemented | typed journals across executors and migrations 004–008 | exhaustive failpoint and repeated-recovery tests |
| SAF-011 | Implemented | typed journals across executors and migrations 004–008 | exhaustive failpoint and repeated-recovery tests |
| SAF-012 | Implemented | typed journals across executors and migrations 004–008 | exhaustive failpoint and repeated-recovery tests |
| SAF-013 | Implemented | `src/remove.rs` quarantine and explicit purge plans | removal, retention, purge, and purge-recovery tests |
| SAF-014 | Implemented | read-only CLI preflight and shared planners in `src/cli.rs` | CLI dry-run filesystem/database snapshot tests |
| SAF-015 | Implemented | read-only CLI preflight and shared planners in `src/cli.rs` | CLI dry-run filesystem/database snapshot tests |
| SAF-016 | Implemented | execution-boundary revalidation in import/remove/projection executors | source/destination replacement and rollback tests |
| SAF-017 | Implemented | execution-boundary revalidation in import/remove/projection executors | source/destination replacement and rollback tests |
| SAF-018 | Implemented | execution-boundary revalidation in import/remove/projection executors | source/destination replacement and rollback tests |
| AUD-001 | Implemented | `src/db.rs`, `src/asset.rs`, `src/ancillary.rs` | bounded quick-audit and asset-state tests |
| AUD-002 | Implemented | `src/db.rs`, `src/asset.rs`, `src/ancillary.rs` | bounded quick-audit and asset-state tests |
| AUD-003 | Implemented | `src/db.rs`, `src/asset.rs`, `src/ancillary.rs` | bounded quick-audit and asset-state tests |
| AUD-004 | Implemented | `src/db.rs`, `src/asset.rs`, `src/ancillary.rs` | bounded quick-audit and asset-state tests |
| AUD-005 | Implemented | `src/db.rs`, `src/asset.rs`, `src/ancillary.rs` | bounded quick-audit and asset-state tests |
| AUD-006 | Implemented | `src/db.rs`, `src/asset.rs`, `src/ancillary.rs` | bounded quick-audit and asset-state tests |
| AUD-007 | Implemented | `src/db.rs`, `src/asset.rs`, `src/ancillary.rs` | bounded quick-audit and asset-state tests |
| AUD-008 | Implemented | `src/fixity.rs`, durable plans/events | paged resume/cancel/progress CLI and unit tests |
| AUD-009 | Implemented | `src/preservation.rs` | SHA-256, BagIt, verification, and restore tests |
| AUD-010 | Implemented | `src/preservation.rs` | SHA-256, BagIt, verification, and restore tests |
| AUD-011 | Implemented | fixity schedules/history in `src/fixity.rs` and migration 007 | schedule, due-run, result-history tests |
| CAT-001 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-002 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-003 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-004 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-005 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-006 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-007 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-008 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-009 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-010 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-011 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-012 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-013 | Implemented | `src/catalog.rs` and migration 005 | entity, claim, snapshot, resolution, lock, and refresh tests |
| CAT-014 | Implemented | `src/ancillary.rs` and migration 008 | bounded scan/import/recovery tests for related assets |
| CAT-015 | Implemented | normalized entity/link/segment schema in migration 005 | multidisc, compilation, role, and segment model tests |
| MAT-001 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-002 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-003 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-004 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-005 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-006 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-007 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-008 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-009 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-010 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-011 | Implemented | `src/import.rs`, `src/provider.rs`, `src/provider_policy.rs` | matching gates, direct-ID, structural-track, lock, and abstention tests |
| MAT-012 | Implemented | `src/matching_eval.rs` | 30,000 hard-negative attestation and calibrated-report tests |
| MAT-013 | Implemented | `src/matching_eval.rs` | 30,000 hard-negative attestation and calibrated-report tests |
| PRO-001 | Implemented | `src/provider.rs`, `src/musicbrainz.rs`, `src/operations.rs`, `src/catalog.rs` | mock provider completeness/direct-ID/queue/snapshot tests |
| PRO-002 | Implemented | `src/provider.rs`, `src/musicbrainz.rs`, `src/operations.rs`, `src/catalog.rs` | mock provider completeness/direct-ID/queue/snapshot tests |
| PRO-003 | Implemented | `src/provider.rs`, `src/musicbrainz.rs`, `src/operations.rs`, `src/catalog.rs` | mock provider completeness/direct-ID/queue/snapshot tests |
| PRO-004 | Implemented | `src/provider.rs`, `src/musicbrainz.rs`, `src/operations.rs`, `src/catalog.rs` | mock provider completeness/direct-ID/queue/snapshot tests |
| PRO-005 | Implemented | `src/provider.rs`, `src/musicbrainz.rs`, `src/operations.rs`, `src/catalog.rs` | mock provider completeness/direct-ID/queue/snapshot tests |
| PRO-006 | Implemented | Discogs lossless/import-license types in `src/provider_policy.rs` | Discogs shape, genre/style, and license-policy tests |
| PRO-007 | Implemented | Discogs lossless/import-license types in `src/provider_policy.rs` | Discogs shape, genre/style, and license-policy tests |
| PRO-008 | Implemented | Discogs lossless/import-license types in `src/provider_policy.rs` | Discogs shape, genre/style, and license-policy tests |
| PRO-009 | Implemented | typed AcoustID recording evidence in `src/provider_policy.rs` | scope-enforcement and deserialization tests |
| PRO-010 | Implemented | curation provenance and lawful-ingest policy in `src/provider_policy.rs` | source-policy, rating-boundary, and prohibition tests |
| PRO-011 | Implemented | curation provenance and lawful-ingest policy in `src/provider_policy.rs` | source-policy, rating-boundary, and prohibition tests |
| PRO-012 | Implemented | curation provenance and lawful-ingest policy in `src/provider_policy.rs` | source-policy, rating-boundary, and prohibition tests |
| TAG-001 | Implemented | `src/media.rs` and capability matrix v2 | content-probe, opaque fallback, and 13-tuple matrix tests |
| TAG-002 | Implemented | `src/media.rs` and capability matrix v2 | content-probe, opaque fallback, and 13-tuple matrix tests |
| TAG-003 | Implemented | `src/media.rs` and capability matrix v2 | content-probe, opaque fallback, and 13-tuple matrix tests |
| TAG-004 | Implemented | `src/media.rs` and capability matrix v2 | content-probe, opaque fallback, and 13-tuple matrix tests |
| TAG-005 | Implemented | `src/media.rs` and capability matrix v2 | content-probe, opaque fallback, and 13-tuple matrix tests |
| TAG-006 | Implemented | `src/tags.rs` and `docs/tag-capabilities.md` | four-profile golden multivalue/artwork/unknown-data tests |
| TAG-007 | Implemented | `src/tags.rs` and `docs/tag-capabilities.md` | four-profile golden multivalue/artwork/unknown-data tests |
| TAG-008 | Implemented | `src/tags.rs` and `docs/tag-capabilities.md` | four-profile golden multivalue/artwork/unknown-data tests |
| TAG-009 | Implemented | `src/tags.rs` and `docs/tag-capabilities.md` | four-profile golden multivalue/artwork/unknown-data tests |
| TAG-010 | Implemented | `src/tag_projection.rs` | temp/sync/reread/essence/no-clobber failpoint tests |
| TAG-011 | Implemented | `src/tag_projection.rs` | temp/sync/reread/essence/no-clobber failpoint tests |
| TAG-012 | Implemented | `src/tag_projection.rs` | temp/sync/reread/essence/no-clobber failpoint tests |
| TAG-013 | Implemented | real fixtures in `tests/fixtures/formats` | `every_advertised_tuple_has_a_golden_native_round_trip` |
| ART-001 | Implemented | `src/artwork.rs`, provider artwork provenance | bounded decode, roles, content-addressing, deterministic ICC-sRGB tests |
| ART-002 | Implemented | `src/artwork.rs`, provider artwork provenance | bounded decode, roles, content-addressing, deterministic ICC-sRGB tests |
| ART-003 | Implemented | `src/artwork.rs`, provider artwork provenance | bounded decode, roles, content-addressing, deterministic ICC-sRGB tests |
| ART-004 | Implemented | `src/artwork.rs`, provider artwork provenance | bounded decode, roles, content-addressing, deterministic ICC-sRGB tests |
| ART-005 | Implemented | `src/artwork.rs`, provider artwork provenance | bounded decode, roles, content-addressing, deterministic ICC-sRGB tests |
| ART-006 | Implemented | `src/artwork.rs`, provider artwork provenance | bounded decode, roles, content-addressing, deterministic ICC-sRGB tests |
| ART-007 | Implemented | `src/artwork.rs`, provider artwork provenance | bounded decode, roles, content-addressing, deterministic ICC-sRGB tests |
| ART-008 | Implemented | `src/artwork_projection.rs` | embedded/external replace/remove and exhaustive recovery-boundary tests |
| ART-009 | Implemented | `src/artwork_projection.rs` | embedded/external replace/remove and exhaustive recovery-boundary tests |
| NAM-001 | Implemented | `src/roots.rs`, migration 004 | root registration, capability, state, and relative-identity tests |
| NAM-002 | Implemented | `src/roots.rs`, migration 004 | root registration, capability, state, and relative-identity tests |
| NAM-003 | Implemented | `src/naming.rs`, `src/pathformat.rs` | Unicode, reserved-name, grapheme, collision, edition-profile property tests |
| NAM-004 | Implemented | `src/naming.rs`, `src/pathformat.rs` | Unicode, reserved-name, grapheme, collision, edition-profile property tests |
| NAM-005 | Implemented | `src/naming.rs`, `src/pathformat.rs` | Unicode, reserved-name, grapheme, collision, edition-profile property tests |
| NAM-006 | Implemented | `src/naming.rs`, `src/pathformat.rs` | Unicode, reserved-name, grapheme, collision, edition-profile property tests |
| NAM-007 | Implemented | `src/naming.rs`, `src/pathformat.rs` | Unicode, reserved-name, grapheme, collision, edition-profile property tests |
| NAM-008 | Implemented | `src/naming.rs`, `src/pathformat.rs` | Unicode, reserved-name, grapheme, collision, edition-profile property tests |
| NAM-009 | Implemented | `src/path_projection.rs` | preview/approve/execute and exhaustive recovery tests |
| NAM-010 | Implemented | root capability gates and `src/fsops.rs` platform backend | unsupported-platform rejection and capability tests |
| OPS-001 | Implemented | snapshot/read-only/current-schema opens in `src/db.rs` and CLI | dry-run no-copy/no-recovery tests |
| OPS-002 | Implemented | bounded query/keyset/fixity/integrity APIs in `src/db.rs`, `src/query.rs`, `src/fixity.rs` | limit, pagination, cancellation, and explicit-integrity tests |
| OPS-003 | Implemented | bounded query/keyset/fixity/integrity APIs in `src/db.rs`, `src/query.rs`, `src/fixity.rs` | limit, pagination, cancellation, and explicit-integrity tests |
| OPS-004 | Implemented | bounded query/keyset/fixity/integrity APIs in `src/db.rs`, `src/query.rs`, `src/fixity.rs` | limit, pagination, cancellation, and explicit-integrity tests |
| OPS-005 | Implemented | `benchmarks/` release benchmark | million-track threshold-enforcing benchmark |
| OPS-006 | Implemented | durable plan cursors/events/jobs in `src/fixity.rs` and `src/operations.rs` | resume/cancel and provider retry tests |
| OPS-007 | Implemented | durable plan cursors/events/jobs in `src/fixity.rs` and `src/operations.rs` | resume/cancel and provider retry tests |
| OPS-008 | Implemented | measured indexes, WAL, aggregate schema in migrations 005/009 | migration and million-track workload tests |
| OPS-009 | Implemented | measured indexes, WAL, aggregate schema in migrations 005/009 | migration and million-track workload tests |
| OPS-010 | Implemented | `docs/performance.md` and `benchmarks/` | published 30-sample p95 methodology and reference result |
| CLI-001 | Implemented | command orchestration in `src/cli.rs`/`src/main.rs` | plan/approve/run integration workflows |
| CLI-002 | Implemented | mutating-query preflight in `src/cli.rs` | empty/malformed query no-database tests |
| CLI-003 | Implemented | global `--output text|json|jsonl` and bounded emitters | parseable machine-output integration tests |
| CLI-004 | Implemented | durable fixity/provider plan commands and actionable states | paged fixity plan/approve/run/results CLI test |
| API-001 | Implemented | validated ID, confidence, date, rating, plan/root/schedule/fingerprint newtypes | constructor and adversarial deserialization tests |
| API-002 | Implemented | private invariant-bearing fields plus checked accessors | compile/tests exercising controlled construction |
| API-003 | Implemented | `#[non_exhaustive]` public extensible enums/errors | strict Clippy/docs build and public-API audit |
| API-004 | Implemented | rsbts-owned string error payloads in `src/lib.rs` | dependency-error conversion tests and semver check |
| API-005 | Implemented | `docs/api-compatibility.md`, changelog, semver CI | pinned `cargo-semver-checks` gate |
| ENG-001 | Implemented | failpoint framework and all journaled executors | mutation/sync/commit, writer-contention, race, ENOSPC, permission tests |
| ENG-002 | Implemented | failpoint framework and all journaled executors | mutation/sync/commit, writer-contention, race, ENOSPC, permission tests |
| ENG-003 | Implemented | failpoint framework and all journaled executors | mutation/sync/commit, writer-contention, race, ENOSPC, permission tests |
| ENG-004 | Implemented | 13 real format fixtures and matrix-v2 golden test | all profiles and preservation assertions |
| ENG-005 | Implemented | proptest suites and `fuzz/` targets | parser/path/matching/journal property tests and pinned fuzz smoke |
| ENG-006 | Implemented | `.github/workflows/rust.yml` OS matrix and Rust 1.89 job | Exact Rust 1.89 local suite plus Linux/macOS/Windows CI matrix |
| ENG-007 | Implemented | SHA-pinned GitHub Actions and pinned tool versions | actionlint validation and workflow review |
| ENG-008 | Implemented | `.github/workflows/release.yml`, `CHANGELOG.md`, `RELEASING.md` | checksums, CycloneDX, attestations, reproducible archives |
| ENG-009 | Implemented | pinned `cargo-semver-checks` quality gate | baseline public-API compatibility run |
| ENG-010 | Implemented | `deny.toml` and pinned cargo-deny job | advisory/license/source/ban check |
| ENG-011 | Implemented | 75% safety-core line gate and selected mutation gate | 80.58% local line result; 21 caught and 1 compiler-unviable selected mutations |
| ENG-012 | Implemented | mock/fixture-only provider tests | test suite contains no live public-service dependency |
