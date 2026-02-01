use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::{Album, AudioFormat, Item, Result};

pub struct Database {
    conn: Connection,
}

pub struct Stats {
    pub tracks: u64,
    pub albums: u64,
    pub artists: u64,
    pub total_length: f64,
    pub total_size: u64,
}

impl Database {
    /// Open a database connection at the given path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Ok(Self { conn })
    }

    /// Run database migrations to create/update schema.
    ///
    /// # Errors
    /// Returns an error if migrations fail.
    pub fn migrate(&self) -> Result<()> {
        crate::migrations::run_migrations(&self.conn)
    }

    /// Get the current migration version.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn migration_version(&self) -> Result<u32> {
        crate::migrations::current_version(&self.conn)
    }

    /// Insert an album and return its ID.
    ///
    /// # Errors
    /// Returns an error if the insert fails.
    pub fn insert_album(&self, album: &Album) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO albums (album, albumartist, year, artpath, mb_albumid, added)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                album.album,
                album.albumartist,
                album.year,
                album.artpath.as_ref().map(|p| p.to_string_lossy().to_string()),
                album.mb_albumid,
                album.added.to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Insert an item and return its ID.
    ///
    /// # Errors
    /// Returns an error if the insert fails.
    pub fn insert_item(&self, item: &Item) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO items (album_id, path, title, artist, album, albumartist, genre, year,
                               track, disc, format, bitrate, length, mb_trackid, mb_albumid, added, mtime)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                item.album_id,
                item.path.to_string_lossy().to_string(),
                item.title,
                item.artist,
                item.album,
                item.albumartist,
                item.genre,
                item.year,
                item.track,
                item.disc,
                item.format.as_str(),
                item.bitrate,
                item.length,
                item.mb_trackid,
                item.mb_albumid,
                item.added.to_rfc3339(),
                item.mtime.to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Update an existing item.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub fn update_item(&self, id: i64, item: &Item) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET title=?1, artist=?2, album=?3, albumartist=?4, genre=?5,
             year=?6, track=?7, disc=?8, bitrate=?9, length=?10, mtime=?11 WHERE id=?12",
            params![
                item.title,
                item.artist,
                item.album,
                item.albumartist,
                item.genre,
                item.year,
                item.track,
                item.disc,
                item.bitrate,
                item.length,
                item.mtime.to_rfc3339(),
                id,
            ],
        )?;
        Ok(())
    }

    /// Remove an item from the database.
    ///
    /// # Errors
    /// Returns an error if the delete fails.
    pub fn remove_item(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM items WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Allowed fields for `modify_item` to prevent SQL injection.
    const ALLOWED_ITEM_FIELDS: &[&'static str] = &[
        "title",
        "artist",
        "album",
        "albumartist",
        "genre",
        "year",
        "track",
        "disc",
        "format",
        "bitrate",
        "length",
        "mb_trackid",
        "mb_albumid",
    ];

    /// Modify an item's fields.
    ///
    /// # Errors
    /// Returns an error if the update fails or field is invalid.
    pub fn modify_item(&self, id: i64, fields: &[String]) -> Result<()> {
        for field in fields {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };

            if !Self::ALLOWED_ITEM_FIELDS.contains(&key) {
                return Err(crate::Error::Query(format!("Invalid field: {key}")));
            }

            // Use match for safe SQL generation - each field maps to explicit SQL
            let sql = match key {
                "title" => "UPDATE items SET title = ?1 WHERE id = ?2",
                "artist" => "UPDATE items SET artist = ?1 WHERE id = ?2",
                "album" => "UPDATE items SET album = ?1 WHERE id = ?2",
                "albumartist" => "UPDATE items SET albumartist = ?1 WHERE id = ?2",
                "genre" => "UPDATE items SET genre = ?1 WHERE id = ?2",
                "year" => "UPDATE items SET year = ?1 WHERE id = ?2",
                "track" => "UPDATE items SET track = ?1 WHERE id = ?2",
                "disc" => "UPDATE items SET disc = ?1 WHERE id = ?2",
                "format" => "UPDATE items SET format = ?1 WHERE id = ?2",
                "bitrate" => "UPDATE items SET bitrate = ?1 WHERE id = ?2",
                "length" => "UPDATE items SET length = ?1 WHERE id = ?2",
                "mb_trackid" => "UPDATE items SET mb_trackid = ?1 WHERE id = ?2",
                "mb_albumid" => "UPDATE items SET mb_albumid = ?1 WHERE id = ?2",
                _ => continue, // Should never reach here due to whitelist check above
            };
            self.conn.execute(sql, params![value, id])?;
        }
        Ok(())
    }

    /// Query items matching the given query string.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn query_items(&self, query: Option<&str>) -> Result<Vec<Item>> {
        let sql = query.map_or_else(
            || "SELECT * FROM items ORDER BY artist, album, disc, track".into(),
            |q| {
                if q.contains(':') {
                    crate::query::to_sql(q).unwrap_or_else(|_| {
                        "SELECT * FROM items ORDER BY artist, album, disc, track".into()
                    })
                } else {
                    format!(
                        "SELECT i.* FROM items i JOIN items_fts f ON i.id = f.rowid WHERE items_fts MATCH '{}'",
                        q.replace('\'', "''")
                    )
                }
            },
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let items = stmt
            .query_map([], row_to_item)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Query albums matching the given query string.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn query_albums(&self, query: Option<&str>) -> Result<Vec<Album>> {
        match query {
            None => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT * FROM albums ORDER BY albumartist, year, album")?;
                let albums = stmt
                    .query_map([], row_to_album)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(albums)
            }
            Some(q) => {
                let pattern = format!("%{q}%");
                let mut stmt = self.conn.prepare(
                    "SELECT * FROM albums WHERE album LIKE ?1 OR albumartist LIKE ?1 \
                     ORDER BY albumartist, year, album",
                )?;
                let albums = stmt
                    .query_map([&pattern], row_to_album)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(albums)
            }
        }
    }

    /// Get library statistics.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn stats(&self) -> Result<Stats> {
        let tracks: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))?;
        let albums: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM albums", [], |row| row.get(0))?;
        let artists: u64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT artist) FROM items",
            [],
            |row| row.get(0),
        )?;
        let total_length: f64 = self
            .conn
            .query_row("SELECT COALESCE(SUM(length), 0) FROM items", [], |row| {
                row.get(0)
            })?;

        let total_size: u64 = self.conn.query_row(
            "SELECT COALESCE(SUM(bitrate * length / 8), 0) FROM items",
            [],
            |row| row.get::<_, f64>(0).map(|v| v.max(0.0) as u64),
        )?;

        Ok(Stats {
            tracks,
            albums,
            artists,
            total_length,
            total_size,
        })
    }

    /// Check if an item with the given path exists.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn item_exists(&self, path: &Path) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM items WHERE path = ?1",
            [path.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }
}

/// Trait for converting database rows to domain types.
trait FromRow: Sized {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self>;
}

impl FromRow for Item {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let format_str: String = row.get("format")?;
        let path_str: String = row.get("path")?;
        let added_str: String = row.get("added")?;
        let mtime_str: String = row.get("mtime")?;
        let albumartist: Option<String> = row.get("albumartist")?;

        Ok(Self {
            id: row.get("id")?,
            album_id: row.get("album_id")?,
            path: path_str.into(),
            title: row.get("title")?,
            artist: row.get("artist")?,
            album: row.get("album")?,
            albumartist,
            genre: row.get("genre")?,
            year: row.get("year")?,
            track: row.get("track")?,
            disc: row.get("disc")?,
            format: AudioFormat::from_extension(&format_str),
            bitrate: row.get("bitrate")?,
            length: row.get("length")?,
            mb_trackid: row.get("mb_trackid")?,
            mb_albumid: row.get("mb_albumid")?,
            added: parse_datetime(&added_str),
            mtime: parse_datetime(&mtime_str),
        })
    }
}

impl FromRow for Album {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let artpath_str: Option<String> = row.get("artpath")?;
        let added_str: String = row.get("added")?;

        Ok(Self {
            id: row.get("id")?,
            album: row.get("album")?,
            albumartist: row.get("albumartist")?,
            year: row.get("year")?,
            artpath: artpath_str.map(Into::into),
            mb_albumid: row.get("mb_albumid")?,
            added: parse_datetime(&added_str),
        })
    }
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Item::from_row(row)
}

fn row_to_album(row: &rusqlite::Row<'_>) -> rusqlite::Result<Album> {
    Album::from_row(row)
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(DateTime::UNIX_EPOCH)
}
