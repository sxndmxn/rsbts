//! Read-only migration from a Beets `SQLite` library and YAML configuration.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use num_traits::ToPrimitive;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};

use crate::config::Config;
use crate::import::Action;
use crate::{
    Album, AudioFormat, Error, ExtendedMetadata, ExternalId, FlexibleValue, Item, PartialDate,
    Result,
};

type SqlRow = BTreeMap<String, Value>;

#[derive(Debug, Clone, Default)]
pub struct BeetsMigrationReport {
    pub albums: usize,
    pub album_tracks: usize,
    pub singletons: usize,
    pub external_ids: usize,
    pub flexible_fields: usize,
    pub missing_files: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BeetsMigration {
    pub groups: Vec<(Option<Album>, Vec<Item>)>,
    pub translated_config: Config,
    pub report: BeetsMigrationReport,
}

impl BeetsMigration {
    pub fn read(
        library_path: &Path,
        config_path: Option<&Path>,
        music_directory: Option<&Path>,
        output_database: PathBuf,
    ) -> Result<Self> {
        let yaml = config_path.map(read_yaml).transpose()?;
        let music_directory = music_directory
            .map(Path::to_path_buf)
            .or_else(|| yaml.as_ref().and_then(configured_music_directory))
            .map(expand_home)
            .transpose()?;
        let connection = Connection::open_with_flags(
            library_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        verify_source(&connection)?;
        let album_rows = read_table(&connection, "albums")?;
        let item_rows = read_table(&connection, "items")?;
        let album_attributes = read_attributes(&connection, "album_attributes")?;
        let item_attributes = read_attributes(&connection, "item_attributes")?;

        let mut report = BeetsMigrationReport::default();
        let mut albums = HashMap::new();
        for row in album_rows {
            let id = integer(&row, "id")
                .ok_or_else(|| Error::Import("Beets album row is missing an integer ID".into()))?;
            let album = migrate_album(&row, album_attributes.get(&id), music_directory.as_deref())?;
            report.external_ids += album.extended.external_ids.len();
            report.flexible_fields += album.extended.flexible_fields.len();
            albums.insert(id, album);
        }

        let mut grouped: HashMap<i64, Vec<Item>> = HashMap::new();
        let mut singletons = Vec::new();
        for row in item_rows {
            let id = integer(&row, "id")
                .ok_or_else(|| Error::Import("Beets item row is missing an integer ID".into()))?;
            let album_id = integer(&row, "album_id");
            let mut item =
                migrate_item(&row, item_attributes.get(&id), music_directory.as_deref())?;
            item.singleton = album_id.is_none();
            if !item.path.exists() {
                report.missing_files += 1;
            }
            report.external_ids += item.extended.external_ids.len();
            report.flexible_fields += item.extended.flexible_fields.len();
            if let Some(album_id) = album_id {
                grouped.entry(album_id).or_default().push(item);
            } else {
                singletons.push(item);
            }
        }

        let mut groups = Vec::new();
        let mut album_ids = albums.keys().copied().collect::<Vec<_>>();
        album_ids.sort_unstable();
        for album_id in album_ids {
            let album = albums.remove(&album_id).ok_or_else(|| {
                Error::Import(format!(
                    "Beets album disappeared during migration: {album_id}"
                ))
            })?;
            let items = grouped.remove(&album_id).unwrap_or_default();
            report.album_tracks += items.len();
            report.albums += 1;
            groups.push((Some(album), items));
        }
        if !grouped.is_empty() {
            return Err(Error::Import(format!(
                "Beets items reference {} missing album row(s)",
                grouped.len()
            )));
        }
        report.singletons = singletons.len();
        for singleton in singletons {
            groups.push((None, vec![singleton]));
        }

        let mut translated_config = Config::default();
        if let Some(directory) = music_directory {
            translated_config.library.directory = directory;
        }
        translated_config.library.database = output_database;
        if let Some(yaml) = &yaml {
            translate_yaml(yaml, &mut translated_config, &mut report.warnings);
        }
        translated_config.validate()?;
        Ok(Self {
            groups,
            translated_config,
            report,
        })
    }

    pub fn validate_in_memory(&self) -> Result<()> {
        let mut library = crate::db::Library::open_in_memory()?;
        library.import_migrated_groups(&self.groups)?;
        let expected = self.report.album_tracks + self.report.singletons;
        if usize::try_from(library.stats()?.tracks).ok() != Some(expected) {
            return Err(Error::Recovery(
                "migrated track count does not match the Beets source".into(),
            ));
        }
        Ok(())
    }
}

pub fn write_config_create_new(config: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(config)
        .map_err(|error| Error::Config(format!("cannot serialize migrated config: {error}")))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn verify_source(connection: &Connection) -> Result<()> {
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::Recovery(format!(
            "Beets database integrity check failed: {integrity}"
        )));
    }
    for table in ["items", "albums"] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(Error::Import(format!(
                "source is not a Beets library; missing table {table}"
            )));
        }
    }
    Ok(())
}

fn read_table(connection: &Connection, table: &str) -> Result<Vec<SqlRow>> {
    let sql = format!("SELECT * FROM {table} ORDER BY id");
    let mut statement = connection.prepare(&sql)?;
    let names = statement
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let rows = statement.query_map([], |row| {
        let mut values = BTreeMap::new();
        for (index, name) in names.iter().enumerate() {
            values.insert(name.clone(), row.get::<_, Value>(index)?);
        }
        Ok(values)
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn read_attributes(
    connection: &Connection,
    table: &str,
) -> Result<HashMap<i64, BTreeMap<String, FlexibleValue>>> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(HashMap::new());
    }
    let sql = format!("SELECT entity_id, key, value FROM {table} ORDER BY entity_id, key");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Value>(2)?,
        ))
    })?;
    let mut output: HashMap<i64, BTreeMap<String, FlexibleValue>> = HashMap::new();
    for row in rows {
        let (id, key, value) = row?;
        if valid_field_name(&key) {
            if let Some(value) = flexible_value(&value) {
                output.entry(id).or_default().insert(key, value);
            }
        }
    }
    Ok(output)
}

fn migrate_album(
    row: &SqlRow,
    attributes: Option<&BTreeMap<String, FlexibleValue>>,
    music_directory: Option<&Path>,
) -> Result<Album> {
    let mut extended = extended_metadata(row, attributes)?;
    add_provider_ids(row, &mut extended, false);
    Ok(Album {
        id: None,
        album: required_text(row, "album", "Unknown Album"),
        albumartist: required_text(row, "albumartist", "Unknown Artist"),
        year: positive_i32(row, "year"),
        artpath: row
            .get("artpath")
            .filter(|value| match value {
                Value::Text(value) => !value.is_empty(),
                Value::Blob(value) => !value.is_empty(),
                Value::Null => false,
                Value::Integer(_) | Value::Real(_) => true,
            })
            .map(|value| resolve_path(value, music_directory))
            .transpose()?,
        external_id: preferred_release_id(&extended.external_ids),
        added: timestamp(row, "added")?,
        extended,
    })
}

fn migrate_item(
    row: &SqlRow,
    attributes: Option<&BTreeMap<String, FlexibleValue>>,
    music_directory: Option<&Path>,
) -> Result<Item> {
    let path = row
        .get("path")
        .ok_or_else(|| Error::Import("Beets item has no path".into()))
        .and_then(|value| resolve_path(value, music_directory))?;
    let mut extended = extended_metadata(row, attributes)?;
    add_provider_ids(row, &mut extended, true);
    let metadata = std::fs::metadata(&path).ok();
    let genre = text(row, "genre")
        .or_else(|| extended.genres.first().cloned())
        .filter(|value| !value.is_empty());
    let track_external_id = preferred_track_id(&extended.external_ids);
    let release_external_id = preferred_release_id(&extended.external_ids);
    Ok(Item {
        id: None,
        album_id: None,
        path,
        title: required_text(row, "title", "Untitled"),
        artist: required_text(row, "artist", "Unknown Artist"),
        album: required_text(row, "album", "Unknown Album"),
        albumartist: text(row, "albumartist").filter(|value| !value.is_empty()),
        genre,
        year: positive_i32(row, "year"),
        track: positive_u32(row, "track"),
        disc: positive_u32(row, "disc"),
        format: AudioFormat::from_storage(&text(row, "format").unwrap_or_default()),
        bitrate: integer(row, "bitrate")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0),
        length: real(row, "length").unwrap_or(0.0),
        file_size: metadata.as_ref().map(std::fs::Metadata::len),
        track_external_id,
        release_external_id,
        added: timestamp(row, "added")?,
        mtime: timestamp(row, "mtime")?,
        singleton: false,
        extended,
    })
}

fn extended_metadata(
    row: &SqlRow,
    attributes: Option<&BTreeMap<String, FlexibleValue>>,
) -> Result<ExtendedMetadata> {
    let mut flexible_fields = attributes.cloned().unwrap_or_default();
    let native = native_fields();
    for (key, value) in row {
        if !native.contains(key.as_str()) && valid_field_name(key) {
            if let Some(value) = flexible_value(value) {
                flexible_fields.insert(key.clone(), value);
            }
        }
    }
    Ok(ExtendedMetadata {
        date: partial_date(row, "year", "month", "day")?,
        original_date: partial_date(row, "original_year", "original_month", "original_day")?,
        track_total: positive_u32(row, "tracktotal"),
        disc_total: positive_u32(row, "disctotal"),
        compilation: integer(row, "comp").map(|value| value != 0),
        label: text(row, "label").filter(|value| !value.is_empty()),
        catalog_number: text(row, "catalognum").filter(|value| !value.is_empty()),
        country: text(row, "country").filter(|value| !value.is_empty()),
        media: text(row, "media").filter(|value| !value.is_empty()),
        language: text(row, "language").filter(|value| !value.is_empty()),
        artists: list(row, "artists"),
        album_artists: list(row, "albumartists"),
        genres: list(row, "genres"),
        composers: list(row, "composers"),
        external_ids: Vec::new(),
        flexible_fields,
    })
}

fn add_provider_ids(row: &SqlRow, metadata: &mut ExtendedMetadata, item: bool) {
    let mut add = |provider: &str, kind: &str, field: &str| {
        if let Some(value) = text_or_integer(row, field).filter(|value| !value.is_empty()) {
            let id = ExternalId {
                provider: provider.into(),
                kind: kind.into(),
                value,
            };
            if !metadata.external_ids.contains(&id) {
                metadata.external_ids.push(id);
            }
        }
    };
    add("musicbrainz", "release", "mb_albumid");
    add("musicbrainz", "release_group", "mb_releasegroupid");
    add("musicbrainz", "artist", "mb_albumartistid");
    add("discogs", "release", "discogs_albumid");
    add("discogs", "artist", "discogs_artistid");
    add("discogs", "label", "discogs_labelid");
    if item {
        add("musicbrainz", "recording", "mb_trackid");
        add("musicbrainz", "release_track", "mb_releasetrackid");
        add("musicbrainz", "work", "mb_workid");
        add("musicbrainz", "artist", "mb_artistid");
        add("acoustid", "track", "acoustid_id");
        add("isrc", "recording", "isrc");
    }
}

fn preferred_release_id(ids: &[ExternalId]) -> Option<ExternalId> {
    ids.iter()
        .find(|id| id.provider == "musicbrainz" && id.kind == "release")
        .or_else(|| ids.iter().find(|id| id.kind == "release"))
        .cloned()
}

fn preferred_track_id(ids: &[ExternalId]) -> Option<ExternalId> {
    ids.iter()
        .find(|id| id.kind == "recording" || id.kind == "release_track")
        .cloned()
}

fn native_fields() -> HashSet<&'static str> {
    [
        "id",
        "album_id",
        "path",
        "title",
        "artist",
        "album",
        "albumartist",
        "genre",
        "genres",
        "year",
        "month",
        "day",
        "original_year",
        "original_month",
        "original_day",
        "track",
        "tracktotal",
        "disc",
        "disctotal",
        "format",
        "bitrate",
        "length",
        "added",
        "mtime",
        "artpath",
        "comp",
        "label",
        "catalognum",
        "country",
        "media",
        "language",
        "artists",
        "albumartists",
        "composers",
        "mb_albumid",
        "mb_trackid",
        "mb_releasetrackid",
        "mb_releasegroupid",
        "mb_workid",
        "discogs_albumid",
    ]
    .into_iter()
    .collect()
}

fn resolve_path(value: &Value, music_directory: Option<&Path>) -> Result<PathBuf> {
    let bytes = match value {
        Value::Text(value) => value.as_bytes().to_vec(),
        Value::Blob(value) => value.clone(),
        _ => {
            return Err(Error::Import(
                "Beets path has an invalid SQLite type".into(),
            ))
        }
    };
    let value = String::from_utf8(bytes)
        .map_err(|_error| Error::Import("Beets path is not valid UTF-8".into()))?;
    let path = PathBuf::from(value.replace('/', std::path::MAIN_SEPARATOR_STR));
    if path.is_absolute() {
        Ok(path)
    } else {
        music_directory
            .map(|directory| directory.join(path))
            .ok_or_else(|| {
                Error::Import(
                    "Beets stores relative paths; provide --music-directory or --beets-config"
                        .into(),
                )
            })
    }
}

fn read_yaml(path: &Path) -> Result<serde_yaml::Value> {
    let content = std::fs::read_to_string(path)?;
    serde_yaml::from_str(&content)
        .map_err(|error| Error::Config(format!("invalid Beets YAML: {error}")))
}

fn configured_music_directory(value: &serde_yaml::Value) -> Option<PathBuf> {
    value
        .get("directory")
        .and_then(serde_yaml::Value::as_str)
        .map(PathBuf::from)
}

fn translate_yaml(yaml: &serde_yaml::Value, config: &mut Config, warnings: &mut Vec<String>) {
    if let Some(import) = yaml.get("import") {
        config.import.action = if import
            .get("move")
            .and_then(serde_yaml::Value::as_bool)
            .unwrap_or(false)
        {
            Action::Move
        } else if import
            .get("link")
            .and_then(serde_yaml::Value::as_bool)
            .unwrap_or(false)
        {
            Action::Link
        } else if import.get("copy").and_then(serde_yaml::Value::as_bool) == Some(false) {
            Action::InPlace
        } else {
            Action::Copy
        };
    }
    if let Some(format) = yaml
        .get("paths")
        .and_then(|paths| paths.get("default"))
        .and_then(serde_yaml::Value::as_str)
    {
        if crate::pathformat::validate_template(format).is_ok() {
            config.paths.format = format.to_string();
        } else {
            warnings.push(
                "Beets paths.default is not losslessly compatible; retained the rsbts default"
                    .into(),
            );
        }
    }
    if yaml.get("plugins").is_some() {
        warnings.push("Beets plugins are intentionally not migrated".into());
    }
}

fn expand_home(path: PathBuf) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        dirs::home_dir().ok_or_else(|| Error::Config("cannot resolve home directory".into()))
    } else if let Some(value) = text.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(value))
            .ok_or_else(|| Error::Config("cannot resolve home directory".into()))
    } else {
        Ok(path)
    }
}

fn timestamp(row: &SqlRow, field: &str) -> Result<DateTime<Utc>> {
    let seconds = real(row, field).unwrap_or(0.0);
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(Error::Import(format!("Beets {field} timestamp is invalid")));
    }
    let whole = seconds
        .floor()
        .to_i64()
        .ok_or_else(|| Error::Import(format!("Beets {field} timestamp is out of range")))?;
    let nanos = ((seconds - seconds.floor()) * 1_000_000_000.0)
        .to_u32()
        .ok_or_else(|| Error::Import(format!("Beets {field} timestamp is out of range")))?;
    DateTime::from_timestamp(whole, nanos)
        .ok_or_else(|| Error::Import(format!("Beets {field} timestamp is out of range")))
}

fn required_text(row: &SqlRow, field: &str, fallback: &str) -> String {
    let value = text(row, field).unwrap_or_default();
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn text(row: &SqlRow, field: &str) -> Option<String> {
    match row.get(field)? {
        Value::Text(value) => Some(value.clone()),
        Value::Blob(value) => String::from_utf8(value.clone()).ok(),
        _ => None,
    }
}

fn text_or_integer(row: &SqlRow, field: &str) -> Option<String> {
    text(row, field).or_else(|| integer(row, field).map(|value| value.to_string()))
}

fn integer(row: &SqlRow, field: &str) -> Option<i64> {
    match row.get(field)? {
        Value::Integer(value) => Some(*value),
        Value::Real(value) => value.to_i64(),
        _ => None,
    }
}

fn real(row: &SqlRow, field: &str) -> Option<f64> {
    match row.get(field)? {
        Value::Real(value) => Some(*value),
        Value::Integer(value) => value.to_f64(),
        _ => None,
    }
}

fn positive_i32(row: &SqlRow, field: &str) -> Option<i32> {
    integer(row, field)
        .filter(|value| *value > 0)
        .and_then(|value| i32::try_from(value).ok())
}

fn positive_u32(row: &SqlRow, field: &str) -> Option<u32> {
    integer(row, field)
        .filter(|value| *value > 0)
        .and_then(|value| u32::try_from(value).ok())
}

fn partial_date(
    row: &SqlRow,
    year_field: &str,
    month_field: &str,
    day_field: &str,
) -> Result<PartialDate> {
    let year = positive_i32(row, year_field);
    let month = integer(row, month_field)
        .filter(|value| *value > 0)
        .map(|value| {
            u8::try_from(value).map_err(|_error| {
                Error::Import(format!("Beets date field {month_field} is out of range"))
            })
        })
        .transpose()?;
    let day = integer(row, day_field)
        .filter(|value| *value > 0)
        .map(|value| {
            u8::try_from(value).map_err(|_error| {
                Error::Import(format!("Beets date field {day_field} is out of range"))
            })
        })
        .transpose()?;
    if month.is_some_and(|value| value > 12) || day.is_some_and(|value| value > 31) {
        return Err(Error::Import(format!(
            "Beets date fields {year_field}/{month_field}/{day_field} are invalid"
        )));
    }
    if let (Some(year), Some(month), Some(day)) = (year, month, day) {
        if chrono::NaiveDate::from_ymd_opt(year, u32::from(month), u32::from(day)).is_none() {
            return Err(Error::Import(format!(
                "Beets date fields {year_field}/{month_field}/{day_field} are invalid"
            )));
        }
    }
    Ok(PartialDate { year, month, day })
}

fn list(row: &SqlRow, field: &str) -> Vec<String> {
    let Some(value) = text(row, field) else {
        return Vec::new();
    };
    let delimiter = if value.contains("\\␀") {
        "\\␀"
    } else {
        "; "
    };
    value
        .split(delimiter)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn flexible_value(value: &Value) -> Option<FlexibleValue> {
    match value {
        Value::Integer(value) => Some(FlexibleValue::Integer(*value)),
        Value::Real(value) if value.is_finite() => Some(FlexibleValue::Float(*value)),
        Value::Text(value) => Some(FlexibleValue::String(value.clone())),
        Value::Blob(value) => String::from_utf8(value.clone())
            .ok()
            .map(FlexibleValue::String),
        Value::Null | Value::Real(_) => None,
    }
}

fn valid_field_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn migrates_albums_singletons_ids_missing_files_and_custom_fields() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let music_directory = temporary.path().join("music");
        let album_directory = music_directory.join("Album");
        std::fs::create_dir_all(&album_directory)?;
        std::fs::write(album_directory.join("track.flac"), b"audio")?;
        let source = temporary.path().join("beets.db");
        let connection = Connection::open(&source)?;
        connection.execute_batch(
            "CREATE TABLE albums (
                id INTEGER PRIMARY KEY, album TEXT, albumartist TEXT, year INTEGER,
                added REAL, mb_albumid TEXT, custom_number INTEGER
             );
             CREATE TABLE items (
                id INTEGER PRIMARY KEY, album_id INTEGER, path BLOB, title TEXT,
                artist TEXT, album TEXT, albumartist TEXT, year INTEGER,
                track INTEGER, disc INTEGER, format TEXT, bitrate INTEGER,
                length REAL, added REAL, mtime REAL, mb_trackid TEXT,
                custom_flag INTEGER
             );
             CREATE TABLE album_attributes (entity_id INTEGER, key TEXT, value TEXT);
             CREATE TABLE item_attributes (entity_id INTEGER, key TEXT, value TEXT);",
        )?;
        connection.execute(
            "INSERT INTO albums
             (id, album, albumartist, year, added, mb_albumid, custom_number)
             VALUES (1, 'Migrated Album', 'Migrated Artist', 1970, 10.0, 'release-id', 42)",
            [],
        )?;
        connection.execute(
            "INSERT INTO items
             (id, album_id, path, title, artist, album, albumartist, year, track, disc,
              format, bitrate, length, added, mtime, mb_trackid, custom_flag)
             VALUES (1, 1, ?1, 'Album Track', 'Migrated Artist', 'Migrated Album',
                     'Migrated Artist', 1970, 1, 1, 'FLAC', 1000, 3.0, 10.0, 11.0,
                     'recording-id', 1)",
            params![b"Album/track.flac".to_vec()],
        )?;
        let missing_path = music_directory.join("missing-single.wav");
        connection.execute(
            "INSERT INTO items
             (id, album_id, path, title, artist, album, albumartist, year, track, disc,
              format, bitrate, length, added, mtime, mb_trackid, custom_flag)
             VALUES (2, NULL, ?1, 'Single', 'Solo Artist', '', NULL, 2024, NULL, NULL,
                     'WAV', 0, 1.0, 12.0, 13.0, NULL, 0)",
            params![missing_path.to_string_lossy()],
        )?;
        connection.execute(
            "INSERT INTO album_attributes VALUES (1, 'edition_note', 'first pressing')",
            [],
        )?;
        connection.execute(
            "INSERT INTO item_attributes VALUES (1, 'mood', 'heavy')",
            [],
        )?;
        drop(connection);

        let config_path = temporary.path().join("config.yaml");
        std::fs::write(
            &config_path,
            format!(
                "directory: \"{}\"\nplugins: [fetchart]\nimport:\n  copy: false\n",
                music_directory.display()
            ),
        )?;
        let output_database = temporary.path().join("rsbts.db");

        let migration =
            BeetsMigration::read(&source, Some(&config_path), None, output_database.clone())?;

        migration.validate_in_memory()?;
        let mut migrated_library = crate::db::Library::open_in_memory()?;
        migrated_library.import_migrated_groups(&migration.groups)?;
        assert_eq!(
            migrated_library
                .query_items(&crate::query::Query::parse("flex.mood:=heavy")?)?
                .len(),
            1
        );
        assert_eq!(
            migrated_library
                .query_items(&crate::query::Query::parse(
                    "title::^Single$ , flex.mood:=heavy"
                )?)?
                .len(),
            2
        );
        assert_eq!(
            migrated_library
                .query_items(&crate::query::Query::parse("title:~Album*")?)?
                .len(),
            1
        );
        let album_track = migrated_library
            .query_items(&crate::query::Query::parse("flex.mood:=heavy")?)?
            .remove(0);
        migrated_library.modify_item(
            album_track
                .id
                .ok_or_else(|| Error::Import("migrated item has no ID".into()))?,
            &["title=Retitled".into()],
        )?;
        let retitled = migrated_library
            .query_items(&crate::query::Query::parse("title:=Retitled")?)?
            .remove(0);
        assert!(retitled.track_external_id.is_none());
        assert!(retitled
            .extended
            .external_ids
            .iter()
            .all(|id| id.kind != "recording"));
        let singleton = migrated_library
            .query_items(&crate::query::Query::parse("title:=Single")?)?
            .remove(0);
        migrated_library.modify_item(
            singleton
                .id
                .ok_or_else(|| Error::Import("migrated singleton has no ID".into()))?,
            &["artist=Renamed Solo Artist".into()],
        )?;
        let singleton = migrated_library
            .query_items(&crate::query::Query::parse("title:=Single")?)?
            .remove(0);
        assert!(singleton.singleton);
        assert!(singleton.album_id.is_none());
        assert_eq!(migration.report.albums, 1);
        assert_eq!(migration.report.album_tracks, 1);
        assert_eq!(migration.report.singletons, 1);
        assert_eq!(migration.report.missing_files, 1);
        assert_eq!(migration.translated_config.import.action, Action::InPlace);
        assert_eq!(
            migration.translated_config.library.database,
            output_database
        );
        assert!(migration
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("plugins")));

        let (album, items) = &migration.groups[0];
        let album = album
            .as_ref()
            .ok_or_else(|| Error::Import("album fixture migrated as a singleton".into()))?;
        assert_eq!(
            album.external_id.as_ref().map(|id| id.value.as_str()),
            Some("release-id")
        );
        assert_eq!(
            album.extended.flexible_fields.get("custom_number"),
            Some(&FlexibleValue::Integer(42))
        );
        assert_eq!(
            album.extended.flexible_fields.get("edition_note"),
            Some(&FlexibleValue::String("first pressing".into()))
        );
        assert_eq!(items[0].path, album_directory.join("track.flac"));
        assert_eq!(
            items[0].extended.flexible_fields.get("custom_flag"),
            Some(&FlexibleValue::Integer(1))
        );
        assert_eq!(
            items[0].extended.flexible_fields.get("mood"),
            Some(&FlexibleValue::String("heavy".into()))
        );
        assert!(migration.groups[1].0.is_none());
        assert!(migration.groups[1].1[0].singleton);
        assert_eq!(migration.groups[1].1[0].path, missing_path);
        Ok(())
    }
}
