# Repository guidance

## Project

`rsbts` is a Rust CLI and reusable library for planning, matching, importing,
querying, updating, and removing a local music collection. It treats file
ownership, database consistency, and recoverability as product requirements.
Rust 1.89 or newer is required; keep dependency resolution locked.

## Repository map

- `src/main.rs`: command-line argument definitions and process exit behavior.
- `src/cli.rs`: CLI preflight validation, prompting, and command orchestration.
- `src/config.rs`: defaults and strict TOML loading.
- `src/import.rs` and `src/remove.rs`: plans, approvals, and journaled executors.
- `src/db.rs` and `src/migrations.rs`: SQLite access, recovery, audit, and schema
  upgrades.
- `src/query.rs`: typed query parsing and bound SQL compilation.
- `src/tags.rs`, `src/provider.rs`, and `src/musicbrainz.rs`: audio metadata and
  provider integration.
- `src/pathformat.rs`: safe destination-template parsing and rendering.
- `tests/cli.rs`: disposable end-to-end CLI coverage.

## Safety invariants

- Never silently overwrite a destination or weaken collision checks.
- On Unix, keep library creation, staging, and finalization descriptor-relative
  and no-follow; reject a changed library root or destination parent.
- Keep preview, approval, and execution separate. Revalidate source identity
  and destination state at the execution boundary.
- Journal file-creating, file-moving, and file-deleting operations so an
  interrupted command can be reconciled safely.
- Commit imported rows before deleting move sources. Preserve a source or
  destination whenever ownership or identity cannot be proven.
- Keep removal atomic at the selected-set level. Quarantine files before the
  database commit and never clobber quarantine paths.
- A dry run must not create or modify the configured database or library and
  must not perform journal recovery.
- Missing files stay cataloged until the user explicitly removes them; `audit`
  reports inconsistencies.
- Reject malformed or empty mutating queries before opening the database. Bind
  query values instead of interpolating SQL.
- Reject paths that cannot round-trip through UTF-8 before they reach SQLite or
  the operation journal.
- Fail closed on malformed config, non-finite scores, invalid templates,
  changed files, and uncertain recovery state.

## Working practices

- Check `git status` before editing and preserve unrelated user changes.
- Add or update tests with behavior changes, especially for races, recovery,
  dry-run guarantees, malformed input, and process exit codes.
- Use temporary directories and an explicit temporary config for manual CLI
  testing. Never aim tests at a developer's real music directory or database.
- Mock or isolate provider behavior in automated tests. Live MusicBrainz calls
  belong only in intentional manual smoke tests.
- Keep user-visible control characters escaped and error messages actionable.
- Update `README.md` when commands, configuration, safety behavior, or exit
  semantics change.

## Verification

Run targeted tests while iterating, then run the complete release checks before
pushing:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo package --locked
```

Cargo refuses to package a dirty worktree by default. Use
`cargo package --locked --allow-dirty` to validate intended uncommitted content,
then rerun the command above after committing.

For user-facing CLI changes, also exercise a disposable workflow covering
preview, import, query, audit, update or modification as applicable, removal
preview, and confirmed cleanup.
