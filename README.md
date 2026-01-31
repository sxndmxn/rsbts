# rsbts [work in progress]

A music library manager with MusicBrainz integration. Import albums, fetch metadata, and query your collection.

## Features

- Import music from filesystem paths
- Automatic metadata lookup via MusicBrainz API
- Cover art fetching from Cover Art Archive
- Full-text search with SQLite FTS5
- Supports MP3, FLAC, OGG, Opus, M4A, AAC, WAV, AIFF

## Installation

```bash
cargo install --path .
```

## Usage

### Import music

```bash
rsbts import /path/to/album
rsbts import -C /path/to/files   # copy files to library
rsbts import -M /path/to/files   # move files to library
```

Reads ID3/Vorbis tags, queries MusicBrainz for canonical metadata, and stores tracks in the database.

### List tracks

```bash
rsbts ls                    # all tracks
rsbts ls "black sabbath"    # search tracks
rsbts ls --album            # list albums
rsbts ls --album "paranoid" # search albums
```

### Show statistics

```bash
rsbts stats
```

```
Tracks: 36
Albums: 5
Artists: 4
Total time: 7:12:34
Total size: 1.2 GB
```

### Update tags

```bash
rsbts update              # re-read all tags from files
rsbts update "artist:x"   # update specific items
```

### Remove items

```bash
rsbts rm "query"          # remove from database
rsbts rm -d "query"       # also delete files from disk
```

### Modify metadata

```bash
rsbts modify "query" genre=Rock year=1970
```

## Configuration

Copy `config.example.toml` to `~/.config/rsbts/config.toml`:

```toml
[library]
directory = "~/Music"
database = "~/.local/share/rsbts/library.db"

[paths]
format = "$albumartist/$album/$track - $title"

[import]
action = "copy"      # copy, move, or link
fetch_art = true

[musicbrainz]
search_limit = 5
```

## License

MIT
