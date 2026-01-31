# rsbts [work in progress]

A music library manager with MusicBrainz integration. Import albums, fetch metadata, and organize your collection.

## Features

- Import albums from filesystem directories
- Automatic metadata lookup via MusicBrainz API
- Cover art fetching from Cover Art Archive
- SQLite database storage
- Support for MP3 and FLAC files

## Installation

```bash
cargo install --path .
```

## Usage

### Import an album

```bash
rsbts import /path/to/album/directory
```

The importer reads ID3/Vorbis tags to identify the artist and album, then queries MusicBrainz to fetch canonical metadata.

### List tracks

```bash
rsbts ls
```

### Show library statistics

```bash
rsbts stats
```

Example output:
```
Tracks: 36
Albums: 5
Artists: 4
Total time: 7:12:34
```

## Configuration

Copy `config.example.toml` to `~/.config/rsbts/config.toml`:

```toml
[library]
path = "/path/to/music/library"

[database]
path = "~/.local/share/rsbts/library.db"
```

## License

MIT
