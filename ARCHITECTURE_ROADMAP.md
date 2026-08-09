# rsbts architecture critique and roadmap

Status: proposed architecture, based on `codex/plugin-free-beets-core` at
`077c46e` and Beets 2.11.0 at `26ab6b26361e8c9d77cdf04ba9cf5ca64bbbc722`.

> Relationship to the current product: this is a non-normative compatibility
> roadmap and historical critique. The implemented 0.4 contract is
> [docs/media-library-requirements.md](docs/media-library-requirements.md), with
> its research basis beside it in
> [docs/media-library-research.md](docs/media-library-research.md). Where this
> roadmap proposes a different path model, database ownership model, plugin
> scope, or safety tradeoff, the numbered requirements and release gates take
> precedence until they are explicitly revised.

## Product contract

rsbts is a Rust implementation of the Beets 2.11 command-line application and
its bundled plugins for Linux and macOS. It ships one user-facing executable,
`rsbts`. It does not load third-party Python plugins and does not promise a Rust
dynamic-plugin ABI before 1.0.

Compatibility means more than accepting similar commands. For all behavior in
the checked-in compatibility manifest, rsbts must match Beets' command parsing,
configuration merge rules, query and template semantics, catalog changes, tag
changes, file changes, output, diagnostics, and exit status. The manifest must
be generated from the pinned Beets source and must include every core command,
option, fixed field, bundled plugin, plugin extension surface, configuration
key, event, and import stage. A feature is not complete merely because code for
it exists; its differential tests must pass.

The Unix contract is intentionally stronger than Beets' behavior:

- stdout is data; diagnostics, prompts, and progress go to stderr;
- every record-producing command has a documented machine-readable stream;
- output is incremental and a closed pipe terminates successfully without a
  panic or traceback;
- native filenames are preserved losslessly, including non-UTF-8 names on Unix;
- noninteractive use never waits on an unavailable terminal;
- catalog and media files remain user-owned, documented, and recoverable with
  standard tools;
- commands are quiet on success unless their primary purpose is to produce
  data.

These deviations must be listed in `compat/unix-divergences.toml`, tested, and
shown in release notes. Unrecorded incompatibility is a bug.

## Findings that block a 1.0 claim

### The current architecture is a prototype, not a Beets-compatible core

The single library package publicly exports storage, CLI-adjacent workflows,
providers, migrations, filesystem operations, query parsing, templates, and
media adapters. The largest two modules combine unrelated policy and mechanism:
`src/import.rs` contains scanning, grouping, matching, planning, terminal
progress, database coordination, hashing, and descriptor-relative filesystem
operations; `src/db.rs` contains the repository, schema migration, operation
journal, crash recovery, audit, and filesystem repair. Configuration imports
import policy, while every workflow imports the concrete database and query
compiler. The resulting graph cannot enforce inward dependencies.

The public error type leaks `rusqlite`, `std::io`, and `lofty` errors alongside
stringly typed domain failures. The crate globally permits lossy numeric casts.
Passing Clippy therefore does not establish the invariants the code claims.

### Ordinary reads perform deep database checks

`Library::open` invokes `run_migrations`, which executes SQLite
`integrity_check` and a foreign-key scan before and after every open, even when
no migration is pending. This contradicts the README's statement that deep
checks only run during `audit` and dominates normal query startup on a large
catalog. Normal open must inspect only application/schema identity and the
current migration number. Full integrity checks belong to an explicit audit or
to the migration path when a migration will actually run.

### Reads materialize everything and then issue N+1 queries

`Library::query_items` first collects every fixed row into a `Vec`, then loads
extended metadata separately for every item. Each hydration performs multiple
statements. A 100,000-item `ls` therefore cannot stream and issues hundreds of
thousands of avoidable queries. Repositories need projection-aware streaming
queries and batched relation loading. A list command should not read flexible
fields or external identifiers it will not render.

### The Unix pipe contract is broken

`rsbts ls | head -n1` currently panics on `EPIPE` and exits 101 because CLI
output uses `println!`. All application output must use injected, buffered
`std::io::Write` values. `BrokenPipe` while writing primary output is normal
successful termination. Prompts and progress must be injected through a
separate interaction port and disabled when stderr is not a terminal.

### Paths are lossy by design

The database converts `Path` to UTF-8 `TEXT`; scanning and tag-write staging
also reject non-UTF-8 filenames. POSIX path components are byte strings except
for slash and NUL. This is both a Unix violation and a Beets incompatibility:
Beets stores paths as BLOBs and uses filesystem encoding at the presentation
boundary. Introduce a `NativePath` value, store path bytes losslessly on Unix,
and convert only in renderers. JSON output must include a normal display string
when lossless UTF-8 is possible and a documented byte representation otherwise.

### Schema, configuration, query, template, and tag parity are distant

The Beets 2.11 catalog has 97 fixed item columns and 43 fixed album columns,
plus typed flexible attributes. rsbts has 23 and 9, with additional JSON tables
that are not Beets-compatible. Beets' media layer exposes 82 tag fields; rsbts
reads and verifies about eight canonical fields and writes those plus track and
disc totals. Beets has 13 default template functions and leaves unknown symbols
intact; rsbts has six and rejects unknown symbols. Beets accepts multiple query
arguments and supports field-specific query types; rsbts accepts a single query
string, which changes shell quoting and exact-match behavior.

Beets uses layered YAML with dynamic plugin sections. rsbts uses a small, strict
TOML schema and its migration relies on the deprecated `serde_yaml` package.
These are compatibility subsystems, not incremental additions to the current
DTOs.

### There is no bundled-plugin kernel

Beets 2.11's bundled plugins extend at least commands, fixed/flexible field
types, query prefixes and named queries, template functions and fields, import
stages, metadata candidate sources, and typed lifecycle events. The source
snapshot contains about 79 public bundled plugin modules/packages; 50 define
commands, 34 subscribe to events, and 13 participate in import stages. rsbts'
`MetadataProvider` trait covers only one of these surfaces. Porting plugins
before defining the kernel would hard-code more cross-layer calls into the
monolith.

### Async is broader than the problem

The multi-thread Tokio runtime wraps the whole application although SQLite and
most filesystem work are synchronous. Provider adapters duplicate rate-limit,
retry, and response-limit policy, and `ProviderSet` awaits providers
sequentially while hiding warnings in a mutex side channel. Keep local
application use cases synchronous. Put async behind network/server adapters,
start a runtime only for commands that need it, bound provider concurrency, and
merge candidates deterministically.

### Tests prove internal consistency, not parity

The current 93 tests are mostly inline unit tests. Four process-level tests cover
the executable. There is no pinned-Beets differential harness, compatibility
inventory, HTTP behavior mock suite, property test, fuzz target, fault-injection
matrix, benchmark gate, coverage report, dependency policy, or architecture
check. A green build says the implementation agrees with itself; it does not say
it behaves like Beets.

## Target workspace

Use a virtual Cargo workspace and enforce this inward dependency graph with an
`xtask architecture` check over `cargo metadata`:

```text
rsbts-domain       pure IDs, values, catalog entities, NativePath
rsbts-query        query AST, parser, semantics
rsbts-template     template AST, parser, evaluator
rsbts-matching     pure candidate scoring and assignment
rsbts-plugin-api   capability interfaces, descriptors, events
        \             |             /
              rsbts-application
       synchronous use cases and ports; no concrete I/O
        /       |        |        |        \
rsbts-sqlite rsbts-fs rsbts-media rsbts-config rsbts-http
        \       |        |        |        /
              bundled plugins
                     |
                 rsbts-cli
              composition root only
```

The pure crates may depend on small general-purpose libraries, but never on
SQLite, HTTP, Tokio, terminal UI, media formats, or OS-specific filesystem
calls. `rsbts-application` depends only on the pure crates and declares ports.
Adapters depend inward and implement those ports. `rsbts-cli` selects adapters,
plugins, renderers, and interaction policy; it contains no business rules.

Do not create a package merely to reduce file length. Create one when it
enforces a dependency boundary, owns optional/system dependencies, needs an
independent compatibility/version contract, or must be fault-tested in
isolation. The bundled plugins meet those tests and should be individual
`publish = false` packages under `plugins/`; generated manifests and workspace
membership make the large set manageable. Shared plugin utilities belong in a
small plugin-support package, not in an unbounded grab bag.

## Application ports and transaction model

The application layer should own use cases such as `ListItems`, `Import`,
`Modify`, `WriteTags`, `MoveManagedFiles`, `Remove`, `Audit`, and `Recover`.
Their dependencies are capabilities, not concrete libraries:

- `CatalogRead`, `CatalogWrite`, and `UnitOfWork`, with projections and streams;
- `TagReader` and `TagWriter` using a complete, typed media-field map;
- `FileTransaction`, exposing safe high-level operations rather than raw paths;
- `MetadataSource`, returning candidates, diagnostics, and retry information in
  one value;
- `Clock` and `IdGenerator` for deterministic plans and tests;
- `Interaction` for confirmation and progress;
- `RecordWriter` for human, JSONL, JSON, TSV, path, and NUL-delimited output;
- `PluginRegistry` and a typed event dispatcher.

Copy, move, hard-link, symlink, tag replacement, deletion, quarantine, and
artwork changes must share one file-operation state machine:

```text
plan -> prepare/revalidate -> journal -> execute -> catalog commit
     -> finalize -> complete
                    \ failure -> compensate/recover
```

The `rsbts-fs` adapter should retain the current descriptor-relative,
no-follow Unix work built on Rustix. It should become the only package allowed
to perform managed-file mutation. Switching libraries is not a safety proof;
race and crash-injection tests are.

## Plugin kernel

Avoid a single `Plugin` god trait. A descriptor registers any combination of
small capabilities:

- command provider;
- metadata source;
- item/album field schema;
- query prefix or named query;
- template function or computed field;
- ordered import hook;
- typed event subscriber;
- media-field extension where Beets requires one.

The binary uses a generated, explicit registry; there is no runtime filesystem
discovery. Activation follows Beets configuration order. Duplicate command,
field, query-prefix, or template names are deterministic configuration errors
unless Beets 2.11 defines overriding behavior. Events are typed structures and
subscribers receive only documented capabilities, not the global application.
Network, external-process, and server plugins get separate capability sets and
resource limits.

## Catalog, configuration, and native paths

The canonical catalog should remain the Beets 2.11 schema, including fixed and
flexible fields and BLOB relative paths. Add namespaced `rsbts_*` tables and
indexes for operation journals and derived state inside the same SQLite file so
catalog and journal commits can be atomic. Beets must be able to reopen the file
after rsbts changes it. This reverses the current one-way takeover design, but it
best satisfies 1:1 behavior, Unix data ownership, and direct artifact opening.
If an unavoidable extension cannot be represented in the shared file, it must
have a lossless documented export before release.

Do not enable WAL by reflex. A single CLI writer and a portable one-file catalog
may favor the rollback journal; WAL improves reader/writer concurrency but its
WAL file is persistent state and requires checkpoint policy. Benchmark both
with realistic failure tests and record the choice in an ADR.

Configuration must implement Beets' layered YAML merge, includes, `BEETSDIR`,
path resolution, dynamic plugin sections, and legacy YAML boolean behavior.
Wrap the parser behind `rsbts-config`. A pure-Rust parser such as `noyalib` is a
candidate because it offers YAML 1.1 behavior and resource budgets, but its
pre-1.0 version must be exactly pinned and accepted only after differential
testing against a real-config corpus. Preserve unknown plugin sections until
the responsible plugin consumes them.

On Unix, `NativePath` stores `OsStr` bytes and SQLite uses BLOBs. Path templates
operate on metadata text, then the filesystem adapter joins native components
without lossy conversion. Renderers own escaping. On macOS, tests must cover
non-normalized Unicode names; on Linux they must cover arbitrary non-NUL bytes.

## CLI and stream contract

Keep Beets-compatible human behavior as the default, including multiple query
arguments. Add explicit composable modes without reusing Beets' template-format
option:

- `--jsonl` for the default streaming record contract;
- `--json` only when bounded materialization is acceptable;
- `--tsv`, `--path`, and `--null` where their record models make sense;
- `--plain`, `--quiet`, `--verbose`, `--color`, and `NO_COLOR` behavior;
- `-` for documented stdin/stdout file operands.

Selection-producing commands emit stable item IDs plus a catalog revision.
Mutating commands may accept JSONL selections on stdin and reject stale
revisions before planning. If stdin supplies data, confirmation uses `/dev/tty`
when explicitly requested or requires `--yes`; it never competes with the data
stream. Human progress is TTY-aware and stays on stderr. Define and test the exit
taxonomy: success, partial/skipped work, validation/runtime failure, CLI syntax
failure, and successful broken-pipe termination.

## Verification system

`compat/beets-2.11.toml` is generated from the pinned source and reviewed in the
repository. Every entry records `missing`, `partial`, `parity`, or an approved
Unix divergence and links to tests. Generation fails if the upstream inventory
changes unexpectedly.

The differential runner executes pinned Python Beets and rsbts against separate
copies of the same fixture. It compares:

- stdout bytes, stderr bytes, and exit status;
- normalized SQLite rows and types;
- file names, bytes, permissions, tags, and artwork;
- external HTTP/process requests through deterministic fakes;
- recovery results after injected failures at every durable transition.

Normalize only values documented as nondeterministic, such as temporary roots,
timestamps, and request IDs. Keep translated upstream tests with attribution.
Add unit tests for pure semantics, adapter integration tests, end-to-end CLI
tests, `proptest` laws for query/template/path/config parsing, fuzz targets for
untrusted parsers and tag input, and race/crash tests for file transactions.

CI must run format, strict Clippy without global cast exceptions, tests,
rustdoc, package checks, coverage, dependency/advisory/license policy, unused
dependency checks, the architecture graph, the compatibility manifest, and
Linux/macOS integration suites. Semver checks apply to intentionally public
crates. Coverage is diagnostic; parity-manifest coverage and fault tests are the
release gates.

## Performance contract

Measure before optimizing. Keep a versioned synthetic catalog and a scrubbed
realistic fixture. Bench both library functions and fresh processes. Report
median and tail latency, peak RSS, allocations where practical, filesystem
operations, SQLite statements, and network requests.

The current prototype is around 34x faster for repeated `version`, 23x for an
empty `stats`, and 9.5x for `stats` on a synthetic 100,000-item catalog. It is
about 5.8x *slower* for an exact query returning 100 rows because startup checks
dominate, while the equivalent raw SQL is faster. Listing all 100,000 rows is
about 3x faster, below the proposed 5x target. These numbers are directional,
not a release claim: rsbts currently performs far less compatibility work.

For 1.0, require at least 5x lower median wall time than pinned Beets 2.11 for
the agreed startup, exact query, full-list, stats, update scan, and import-scan
workloads after parity tests pass. Also set absolute memory and p95 regression
budgets so a ratio cannot hide a slow implementation. Provider/network latency
is measured separately from deterministic local processing.

## Strangler roadmap

### Phase 0: freeze the contract

Deliver the generated Beets inventory, Unix divergence manifest, pinned Python
test environment, differential runner skeleton, benchmark fixtures, and ADRs
for schema ownership, plugin model, async boundaries, path representation, and
SQLite journal mode. Rewrite the README so it no longer promises a plugin-free,
non-drop-in product. Stop adding plugin behavior to the current modules.

Exit: every requested 1.0 behavior is inventory-addressable; the two binaries
can run against copied fixtures; intentional incompatibilities are explicit.

### Phase 1: remove immediate architectural hazards

Introduce fallible output/diagnostic writers and correct broken-pipe handling.
Move full integrity and foreign-key scans to migration/audit paths. Add
fresh-process benchmarks and statement-count tests for representative queries.
Replace global lossy-cast allowances with local checked conversions.

Exit: pipes do not panic, normal open is O(schema inspection), and benchmark
results are reproducible in CI without changing user-visible commands.

### Phase 2: extract the workspace without behavior changes

Extract domain, query, template, matching, application ports, SQLite, filesystem,
media, HTTP, config, and CLI packages in that order. Consolidate file mutation
behind the state machine. Add the cargo-metadata dependency check. Keep adapter
facades temporarily so each extraction is reviewable and bisectable.

Exit: forbidden dependency edges fail CI; application and pure crates contain no
concrete I/O dependencies; all existing tests and safety invariants still pass.

### Phase 3: establish catalog and language compatibility

Implement direct Beets schema access, complete fixed/flexible types, lossless
paths, layered YAML, query parsing, templates, and the complete media-field map.
Remove the lossy one-way migration as the primary workflow; retain a validated
backup/export tool. Batch and stream repository reads.

Exit: Beets can reopen and accurately read a catalog changed by rsbts; the
field/config/query/template/tag differential matrices pass; a 100,000-row list
is streaming and has a bounded statement count.

### Phase 4: make core commands composable

Move all core commands onto application use cases and renderers. Add structured
output, NUL/path modes, stdin selection with revisions, TTY interaction policy,
shell completions, and man pages. Finish the unified transaction/recovery model
for import, write, move, remove, and artwork.

Exit: core command parity passes in human mode, Unix stream contracts pass in
machine mode, and fault injection proves recovery at every state transition.

### Phase 5: land the plugin kernel and import pipeline

Implement capability registration, typed events, ordered import stages, the
explicit registry, conflict checks, shared HTTP policy, bounded concurrent
metadata search, and deterministic merging. Make the default MusicBrainz path a
registered built-in capability rather than a special-case dependency.

Exit: synthetic plugins exercise every extension surface; ordering and failure
isolation match Beets; no plugin can reach an undeclared adapter capability.

### Phase 6: port bundled plugins in risk waves

Port pure transforms/query/template plugins first, then tag/filesystem/artwork
plugins, external-process plugins, network metadata plugins, and finally
integration/server plugins. Each plugin package owns its configuration schema,
dependencies, fixtures, and compatibility entries. A plugin moves to `parity`
only when its differential suite passes; compiled presence is not completion.

Exit: all generated bundled-plugin entries are parity or an approved documented
divergence, including error and unavailable-dependency behavior.

### Phase 7: release hardening

Profile the parity-complete implementation, remove measured bottlenecks, enforce
the 5x local-performance matrix, run long crash/race tests, audit dependencies,
stabilize the public API, and ship preview releases on Linux and macOS before
1.0.

Exit: all compatibility and Unix manifests are green, no critical advisories or
unresolved recovery cases remain, the performance contract passes, and install,
upgrade, backup, recovery, and export are documented and rehearsed.

## Immediate next changes

The first implementation slice should be deliberately small: add an output
abstraction and broken-pipe test, make migration integrity checks conditional on
an actual migration, add a statement-count regression test for a small query,
and establish the fresh-process benchmark command. Do not begin workspace
splitting until those tests capture the current external behavior and the
startup regression is removed.

## Research basis

- Local `roadmap-sh-rust` material on modules/crates, CLI applications, errors,
  testing, property testing, profiling, SQLite, async, dependency management,
  and Serde.
- Local Unix-philosophy notes based on McIlroy and Raymond: modularity,
  composition, separation of policy and mechanism, transparency, simple data,
  silence, and measured optimization.
- [Beets 2.11 source](https://github.com/beetbox/beets/tree/v2.11.0), especially
  its [plugin API](https://github.com/beetbox/beets/blob/v2.11.0/beets/plugins.py),
  [library models](https://github.com/beetbox/beets/blob/v2.11.0/beets/library/models.py),
  [query implementation](https://github.com/beetbox/beets/blob/v2.11.0/beets/dbcore/query.py),
  and [configuration reference](https://beets.readthedocs.io/en/stable/reference/config.html).
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/),
  [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html),
  and [Tokio's synchronous bridging guidance](https://tokio.rs/tokio/topics/bridging).
- [Command Line Interface Guidelines](https://clig.dev/), Rust's
  [`std::io`](https://doc.rust-lang.org/std/io/) and
  [`BrokenPipe`](https://doc.rust-lang.org/stable/core/io/enum.ErrorKind.html),
  and Unix [`OsStrExt`](https://doc.rust-lang.org/std/os/unix/ffi/trait.OsStrExt.html).
- [SQLite WAL documentation](https://www.sqlite.org/wal.html), used to avoid
  treating WAL as an unconditional optimization.
