-- Initial schema for rsbts music library database

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
