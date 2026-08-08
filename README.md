# rsbts

A safe, plan-first Rust implementation progressing toward Beets 2.11 core and
bundled-plugin compatibility on Linux and macOS. The current 0.3 implementation
catalogs albums and singles, searches its built-in MusicBrainz and Discogs
providers, explains match confidence, and organizes approved music without
silently overwriting files. Its compatibility status and intentional Unix
improvements are tracked under `compat/`; it is not yet a drop-in replacement.

## Safety model

- Imports are previewed and approved per album. A collision rejects the whole
  album; an existing, managed destination with identical content is a no-op.
- On Unix, library directories are opened component-by-component without
  following symlinks. Creation, staging, and no-clobber finalization stay
  relative to pinned directory handles and abort if the library root or a
  destination parent changes during execution.
- Copy, move, link, cover-art, explicit tag-write, managed-file move, and
  file-deleting removal operations use a persistent SQLite journal.
  Interrupted work is reconciled when the CLI opens for a non-dry-run command;
  if ownership cannot be proven, recovery stops and reports the paths requiring
  manual attention.
- Move sources are deleted only after the album and tracks commit to SQLite;
  recovery also requires the journaled file identity and content hash to match.
- Removal is planned and confirmed as one complete set. With `--delete`, files
  are first moved into a same-directory, no-clobber quarantine, then database
  rows commit, then quarantined files are unlinked. A file replaced after the
  plan is shown is preserved, even when its bytes are identical.
- Schema upgrades run transactionally and create a timestamped, integrity-checked
  database backup before migrating an existing library.
- Missing files remain represented in the database and are reported by audit.
- Query values are bound SQLite parameters; malformed fields fail closed.
- Paths must round-trip through UTF-8 before they can enter SQLite or the
  operation journal; unrepresentable paths are reported and skipped.
- Dry runs use an in-memory database snapshot and skip journal recovery, so they
  do not create or modify the configured database or library.

## Install

```bash
cargo install rsbts --locked
```

rsbts requires Rust 1.89 or newer. Library API documentation is available on
[docs.rs](https://docs.rs/rsbts).

To install the current source checkout instead:

```bash
cargo install --path . --locked
```

## Quick start

The built-in defaults use MusicBrainz, `~/Music` for organized files, and
`~/.local/share/rsbts/library.db` for the catalog. Configuration loading is
side-effect free, so it is safe to inspect a new installation before importing
anything:

```bash
rsbts --version
rsbts stats
rsbts audit
```

Create a configuration only when you want different paths or behavior:

```bash
mkdir -p ~/.config/rsbts
cp config.example.toml ~/.config/rsbts/config.toml
```

For a first album, start with a preview. A dry run reads tags, searches
MusicBrainz, displays match scores and the planned destination, but does not
create or modify the database or library:

```bash
rsbts import --dry-run ~/Music/Incoming/album
rsbts import ~/Music/Incoming/album
rsbts ls
rsbts stats
rsbts audit
```

The interactive import lets you choose a provider candidate, keep the existing
file tags, or skip the album or single. Tied results can all display excellent
scores while still failing the runner-up-margin gate. This is intentional:
`--yes` skips ambiguous matches instead of guessing and exits with status `2`
when work was skipped. Imports update only the catalog and file placement;
audio tags change only when `rsbts write` is explicitly run.

Relative paths in an explicit config are resolved from that config's directory.
An explicit missing config or an unknown TOML key is an error; loading
configuration itself creates nothing.

Path templates describe the destination stem. rsbts appends the source audio
extension, preserving periods that are part of a title. Variables are
`$albumartist`, `$artist`, `$album`, `$genre`, `$year`, `$track`, `$title`, and
`$disc`. Functions are `%upper{value}`, `%lower{value}`, `%title{value}`,
`%left{length,value}`, `%right{length,value}`, and
`%if{condition,true-value,false-value}`. Unknown fields, malformed functions,
absolute paths, parent traversal, and control characters are rejected during
configuration or planning.

## Import

```bash
rsbts import --dry-run /path/to/album
rsbts import /path/to/album
rsbts import --copy /path/to/album
rsbts import --move /path/to/album
rsbts import --link /path/to/album
rsbts import --in-place /path/to/track.flac
rsbts import --yes /path/to/albums
```

Interactive imports show candidate-level artist, album, track, provider, total,
and runner-up scores before asking for a decision. `--yes` is deliberately
strict: it accepts only the top result when tags are non-placeholder, track
counts match, artist similarity is at least 95%, album similarity is at least
92%, every track has either 90% title similarity or matching number/disc plus a
duration within three seconds, and the configured composite and
runner-up-margin thresholds pass (92% and five points by default). Failed gates
are printed and that album is skipped.

Without a terminal, mutating imports require `--yes`; `--dry-run` never mutates.
Failures are isolated per album so later albums can still be processed.
Directly named files and files without usable album tags are treated as
singletons and matched with provider track searches. Album grouping is scoped
to source directories, with adjacent disc folders grouped under their parent,
so unrelated releases with identical tags do not merge globally.

## Metadata providers

MusicBrainz and Discogs are compiled into rsbts; there are no plugins. The
default is `providers.enabled = ["musicbrainz"]`. To add Discogs, list it in
`providers.enabled` and export a personal token before running rsbts:

```bash
export RSBTS_DISCOGS_TOKEN='your-token'
```

Provider failures are isolated when another enabled provider succeeds. Search
limits, request pacing, response-size limits, retries, explicit ID lookup, and
provider-qualified cover-art lookup are built in.

## Migrate from Beets

Migration reads the Beets SQLite database and optional YAML configuration but
never modifies them. Preview first, then create a new rsbts database:

```bash
rsbts migrate beets \
  --beets-library ~/.config/beets/library.db \
  --beets-config ~/.config/beets/config.yaml \
  --output-database ~/.local/share/rsbts/library.db \
  --output-config ~/.config/rsbts/config.toml \
  --dry-run

rsbts migrate beets \
  --beets-library ~/.config/beets/library.db \
  --beets-config ~/.config/beets/config.yaml \
  --output-database ~/.local/share/rsbts/library.db \
  --output-config ~/.config/rsbts/config.toml \
  --yes
```

Use `--music-directory` if the Beets database stores relative paths and the
directory cannot be derived from its config. The destination database and
optional config must not already exist. Migration preserves albums, singletons,
missing-file rows, partial dates, multi-value metadata, MusicBrainz/Discogs IDs,
and fixed or flexible custom fields as typed values where SQLite retains their
type. Beets plugin configuration and incompatible path-template behavior are
reported rather than emulated.

## Query and manage

```bash
rsbts ls
rsbts ls "black sabbath"
rsbts ls "artist:beatles year:1960..1969 year+"
rsbts ls --album "paranoid"
rsbts stats
rsbts audit
rsbts update "artist:beatles"
rsbts modify "album:=Paranoid" genre=Metal year=1970
rsbts write --dry-run "album:=Paranoid"
rsbts write --yes "album:=Paranoid"
rsbts move --dry-run "artist:beatles"
rsbts move --yes "artist:beatles"
rsbts rm --dry-run "artist:beatles"
rsbts rm --yes "artist:beatles"
rsbts rm --delete --yes "artist:beatles"
```

Field filters support literal substring `field:value`, exact `field:=value`,
regular expressions with `field::pattern`, globs with `field:~pattern` (`*`,
`?`, and `[]`), ranges such as `year:1960..1969`, negation with `^`, relative
added dates such as `added:-7d`, and ascending/descending sort suffixes `+`/`-`.
Separate OR groups with a standalone comma, for example
`artist:=Sabbath , artist:=Ozzy`. Migrated custom fields are queried as
`flex.field_name:value`.
Double-quote values containing spaces, for example
`artist:"Black Sabbath" album:="Master of Reality"`; malformed quotes are
rejected.

`modify` validates the entire request before changing anything and commits all
matched rows together. Set an optional field to an empty value (for example,
`genre=` or `year=`) to clear it. Album summaries are reconciled when album,
album-artist, artist, or year fields change. Empty `modify` and `rm` query
strings are rejected, preventing an unset shell variable from selecting the
entire library. Explicitly empty `ls` and `update` queries are rejected too;
omit the query argument when selecting all items is intentional.

Changing canonical title/artist/track fields clears the affected provider track
ID; changing album/album-artist/year clears the affected provider release ID.
Changing artist also clears the release ID when that item has no explicit album
artist. The same selective invalidation applies when `update` detects changed
file tags. Singletons remain outside album rows when updated or modified.

`write` previews and confirms a complete selection before rewriting audio tags
from catalog metadata. Each file is copied to a sibling temporary file, tagged,
re-read for verification, finalized without overwriting, and journaled for
recovery. `move` separately previews and confirms reorganization of already
managed files using the current path template. Both commands require either an
explicit query or `--all`; neither runs implicitly during import or modify.

`audit` prints every missing file, unknown file size, orphaned item, invalid
timestamp, SQLite integrity or foreign-key problem, and full-text search index
inconsistency. It exits with status `2` when it finds an issue. Database-wide
checks run for explicit audit and actual schema migrations, not current-schema
command startup; filesystem checks run only for explicit audit.

Exit status is `0` for success, `2` when some work was skipped or failed while
the rest continued, and `1` for validation or fatal runtime errors. Clap uses
status `2` when the command-line arguments themselves are syntactically invalid.
A downstream command closing stdout early is normal pipeline termination and
also exits `0`; primary output stays on stdout while warnings and diagnostics
use stderr.

## Library API

The public API exposes provider-neutral metadata DTOs, partial dates, typed
flexible metadata, multi-provider IDs, and an async `MetadataProvider` trait.
It also exposes typed `Query` values; explicit import, removal, move, and tag
write plans; journaled executors; library audit; Beets migration; and explicit
`Library::recover_pending()` recovery. Library consumers choose when recovery
runs; the CLI runs it automatically before every command except a dry run.
`Library::open_snapshot()` provides a non-mutating database view.

## Development checks

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo package --locked
```

To exercise the installed CLI without touching a real collection, point an
explicit config at a temporary library and database, then import disposable,
tagged audio. Cover at least this user journey: empty `stats` and `audit`, an
import dry run, an interactive or explicitly confirmed import, filtered `ls`,
`modify`, `update`, removal dry run, confirmed removal, and a final clean
`audit`.

## Supported audio

MP3, FLAC, Ogg Vorbis, Opus, M4A/AAC, ALAC, WAV, and AIFF.

The first stable target is macOS and Linux. The deliberately excluded scope is
plugins, archive extraction, artwork embedding, transcoding, web/player
integrations, and Windows support.

## License

MIT
