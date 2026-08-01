# rsbts

A safe, plan-first music library manager and reusable Rust library. rsbts scans
local audio, searches a metadata provider, explains its match confidence, and
organizes approved albums without silently overwriting files.

## Safety model

- Imports are previewed and approved per album. A collision rejects the whole
  album; an existing, managed destination with identical content is a no-op.
- Copy, move, link, cover-art, and file-deleting removal operations use a
  persistent SQLite journal. Interrupted work is recovered when the CLI opens.
- Move sources are deleted only after the album and tracks commit to SQLite.
- Removal is planned and confirmed as one complete set. With `--delete`, files
  are first quarantined by rename, then database rows commit, then files are
  unlinked.
- Schema upgrades run transactionally and create a timestamped, integrity-checked
  database backup before migrating an existing library.
- Missing files remain represented in the database and are reported by audit.
- Query values are bound SQLite parameters; malformed fields fail closed.

## Install

```bash
cargo install --path . --locked
```

Copy `config.example.toml` to `~/.config/rsbts/config.toml` if the defaults do
not fit. Relative paths in an explicit config are resolved from that config's
directory. Loading configuration itself creates nothing.

## Import

```bash
rsbts import --dry-run /path/to/album
rsbts import /path/to/album
rsbts import --copy /path/to/album
rsbts import --move /path/to/album
rsbts import --link /path/to/album
rsbts import --yes /path/to/albums
```

Interactive imports show candidate-level artist, album, track, provider, total,
and runner-up scores before asking for a decision. `--yes` is deliberately
strict: it accepts only the top result when tags are non-placeholder, track
counts match, artist similarity is at least 95%, album similarity is at least
92%, every track has either 90% title similarity or matching number/disc plus a
duration within three seconds, the composite score is at least 92%, and the
runner-up margin is at least five points. Failed gates are printed and that
album is skipped.

Without a terminal, mutating imports require `--yes`; `--dry-run` never mutates.
Failures are isolated per album so later albums can still be processed.

## Query and manage

```bash
rsbts ls
rsbts ls "black sabbath"
rsbts ls "artist:beatles year:1960..1969 year+"
rsbts ls --album "paranoid"
rsbts stats
rsbts update "artist:beatles"
rsbts modify "album:=Paranoid" genre=Metal year=1970
rsbts rm --dry-run "artist:beatles"
rsbts rm --yes "artist:beatles"
rsbts rm --delete --yes "artist:beatles"
```

Field filters support `field:value`, exact `field:=value`, glob
`field::pattern`, ranges such as `year:1960..1969`, negation with `^`, relative
added dates such as `added:-7d`, and ascending/descending sort suffixes `+`/`-`.

Exit status is `0` for success, `2` when some work was skipped or failed while
the rest continued, and `1` for a fatal error.

## Library API

The public API exposes provider-neutral metadata DTOs and an async
`MetadataProvider` trait, typed `Query` values, explicit `ImportPlan` /
`ApprovedAlbumPlan` and `RemovalPlan` types, journaled executors, library audit,
and explicit `Library::recover_pending()` recovery. Library consumers choose
when recovery runs; the CLI runs it automatically.

## Supported audio

MP3, FLAC, Ogg Vorbis, Opus, M4A/AAC, ALAC, WAV, and AIFF.

## License

MIT
