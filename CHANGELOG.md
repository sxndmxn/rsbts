# Changelog

All notable changes are documented here. The project follows semantic
versioning, with pre-1.0 minor releases allowed to contain documented API
breaks.

## [Unreleased]

## [0.3.0] - 2026-08-09

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
- Cross-platform/MSRV CI, fuzz, coverage, mutation, dependency-policy, SBOM,
  checksums, provenance, and multi-platform release automation.

### Changed

- The public API is hardened around validated newtypes, private invariant
  fields, non-exhaustive enums, and rsbts-owned error contracts.
- Deep audit is now a separately approved, resumable fixity plan rather than a
  synchronous scan.
- Removal defaults to retained quarantine; permanent deletion is an explicit
  purge plan.

### Compatibility

- This is an intentional pre-1.0 minor-version API break from 0.2. See
  [API compatibility](docs/api-compatibility.md).

[Unreleased]: https://github.com/sxndmxn/rsbts/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/sxndmxn/rsbts/compare/v0.2.0...v0.3.0
