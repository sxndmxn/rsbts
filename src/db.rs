use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
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
        self.conn.execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS albums (
                id INTEGER PRIMARY KEY,
                album TEXT NOT NULL,
                albumartist TEXT NOT NULL,
                year INTEGER,
                artpath TEXT,
                mb_albumid TEXT,
                added TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS items (
                id INTEGER PRIMARY KEY,
                album_id INTEGER REFERENCES albums(id),
                path TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL,
                artist TEXT NOT NULL,
                album TEXT NOT NULL,
                albumartist TEXT,
                genre TEXT,
                year INTEGER,
                track INTEGER,
                disc INTEGER,
                format TEXT NOT NULL,
                bitrate INTEGER NOT NULL,
                length REAL NOT NULL,
                mb_trackid TEXT,
                mb_albumid TEXT,
                added TEXT NOT NULL,
                mtime TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_items_artist ON items(artist);
            CREATE INDEX IF NOT EXISTS idx_items_album ON items(album);
            CREATE INDEX IF NOT EXISTS idx_items_year ON items(year);
            CREATE INDEX IF NOT EXISTS idx_items_genre ON items(genre);
            CREATE INDEX IF NOT EXISTS idx_items_path ON items(path);

            CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
                title, artist, album, albumartist, genre,
                content='items',
                content_rowid='id'
            );

            CREATE TRIGGER IF NOT EXISTS items_ai AFTER INSERT ON items BEGIN
                INSERT INTO items_fts(rowid, title, artist, album, albumartist, genre)
                VALUES (new.id, new.title, new.artist, new.album, new.albumartist, new.genre);
            END;

            CREATE TRIGGER IF NOT EXISTS items_ad AFTER DELETE ON items BEGIN
                INSERT INTO items_fts(items_fts, rowid, title, artist, album, albumartist, genre)
                VALUES ('delete', old.id, old.title, old.artist, old.album, old.albumartist, old.genre);
            END;

            CREATE TRIGGER IF NOT EXISTS items_au AFTER UPDATE ON items BEGIN
                INSERT INTO items_fts(items_fts, rowid, title, artist, album, albumartist, genre)
                VALUES ('delete', old.id, old.title, old.artist, old.album, old.albumartist, old.genre);
                INSERT INTO items_fts(rowid, title, artist, album, albumartist, genre)
                VALUES (new.id, new.title, new.artist, new.album, new.albumartist, new.genre);
            END;
            ",
        )?;
        Ok(())
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

    /// Modify an item's fields.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub fn modify_item(&self, id: i64, fields: &[String]) -> Result<()> {
        for field in fields {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            let sql = format!("UPDATE items SET {key} = ?1 WHERE id = ?2");
            self.conn.execute(&sql, params![value, id])?;
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
        let sql = query.map_or_else(
            || "SELECT * FROM albums ORDER BY albumartist, year, album".into(),
            |q| {
                let escaped = q.replace('\'', "''");
                format!(
                    "SELECT * FROM albums WHERE album LIKE '%{escaped}%' OR albumartist LIKE '%{escaped}%' ORDER BY albumartist, year, album"
                )
            },
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let albums = stmt
            .query_map([], row_to_album)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(albums)
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

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let format_str: String = row.get("format")?;
    let path_str: String = row.get("path")?;
    let added_str: String = row.get("added")?;
    let mtime_str: String = row.get("mtime")?;
    let artpath_str: Option<String> = row.get("albumartist")?;

    Ok(Item {
        id: row.get("id")?,
        album_id: row.get("album_id")?,
        path: path_str.into(),
        title: row.get("title")?,
        artist: row.get("artist")?,
        album: row.get("album")?,
        albumartist: artpath_str,
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

fn row_to_album(row: &rusqlite::Row<'_>) -> rusqlite::Result<Album> {
    let artpath_str: Option<String> = row.get("artpath")?;
    let added_str: String = row.get("added")?;

    Ok(Album {
        id: row.get("id")?,
        album: row.get("album")?,
        albumartist: row.get("albumartist")?,
        year: row.get("year")?,
        artpath: artpath_str.map(Into::into),
        mb_albumid: row.get("mb_albumid")?,
        added: parse_datetime(&added_str),
    })
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).map_or_else(
        |_| Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap(),
        |dt| dt.with_timezone(&Utc),
    )
}
