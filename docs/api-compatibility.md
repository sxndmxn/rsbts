# Public API compatibility

`rsbts` 0.4 is a pre-1.0 Rust library and CLI. Minor releases may make
intentional breaking API changes, but patch releases preserve the documented
public API and machine-output contracts. Every intentional break requires a
minor version bump, a changelog entry, and a migration note.

## Stable commitments in 0.4

- Validated identifiers and bounded numeric domain values cannot be
  deserialized around their constructors. Construction-invariant fields are
  private and exposed through checked constructors and read-only accessors.
- Extensible public enums are `#[non_exhaustive]`; callers must include a
  wildcard arm.
- Public errors describe rsbts domains and store dependency failures as text;
  third-party error types are not compatibility contracts.
- SQLite schema upgrades are forward-only, transactional, backed up before
  migration, and refuse a database newer than the running binary.
- JSON and JSONL output is UTF-8, emits one complete JSON value per document or
  line, and uses stable kebab-case state names. New optional object fields may
  be added in a minor release. Existing fields are not silently repurposed.
- Durable plan IDs, asset IDs, root IDs, provider/entity IDs, and content
  digests are opaque. Consumers must not infer meaning from their spelling.

Provider response and configuration structs are boundary data, not trusted
canonical domain objects. They are validated before matching or mutation;
malformed, non-finite, empty, or contradictory values fail closed.

## Automated compatibility check

Pull requests run pinned `cargo-semver-checks` against the target branch. A
release is also reproduced with the exact locked dependency graph and Rust
1.89 minimum toolchain. The release process in [RELEASING.md](../RELEASING.md)
requires reviewing the generated compatibility report before tagging.

The 1.0 boundary will promote the patch-level API promise to normal Rust
semantic versioning. It will not weaken the ownership, plan/approval/execution,
no-clobber, or recovery contracts.
