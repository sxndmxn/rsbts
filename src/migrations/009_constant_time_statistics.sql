CREATE TABLE library_statistics (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    tracks INTEGER NOT NULL CHECK (tracks >= 0),
    albums INTEGER NOT NULL CHECK (albums >= 0),
    artists INTEGER NOT NULL CHECK (artists >= 0),
    total_length REAL NOT NULL,
    total_size INTEGER NOT NULL CHECK (total_size >= 0),
    unknown_sizes INTEGER NOT NULL CHECK (unknown_sizes >= 0)
);

CREATE TABLE statistics_album_members (
    album_id INTEGER PRIMARY KEY,
    member_count INTEGER NOT NULL CHECK (member_count >= 0)
);

CREATE TABLE statistics_artist_members (
    artist TEXT PRIMARY KEY,
    member_count INTEGER NOT NULL CHECK (member_count >= 0)
);

INSERT INTO statistics_album_members (album_id, member_count)
SELECT album_id, COUNT(*) FROM items GROUP BY album_id;

INSERT INTO statistics_artist_members (artist, member_count)
SELECT artist, COUNT(*) FROM items GROUP BY artist;

INSERT INTO library_statistics
    (singleton, tracks, albums, artists, total_length, total_size, unknown_sizes)
SELECT 1,
       COUNT(*),
       COUNT(DISTINCT album_id),
       COUNT(DISTINCT artist),
       COALESCE(SUM(length), 0.0),
       COALESCE(SUM(file_size), 0),
       COALESCE(SUM(file_size IS NULL), 0)
FROM items;

CREATE TRIGGER statistics_items_insert
AFTER INSERT ON items
BEGIN
    INSERT INTO statistics_album_members (album_id, member_count)
    VALUES (NEW.album_id, 1)
    ON CONFLICT(album_id) DO UPDATE SET member_count = member_count + 1;

    INSERT INTO statistics_artist_members (artist, member_count)
    VALUES (NEW.artist, 1)
    ON CONFLICT(artist) DO UPDATE SET member_count = member_count + 1;

    UPDATE library_statistics
    SET tracks = tracks + 1,
        albums = albums + (
            SELECT member_count = 1
            FROM statistics_album_members WHERE album_id = NEW.album_id
        ),
        artists = artists + (
            SELECT member_count = 1
            FROM statistics_artist_members WHERE artist = NEW.artist
        ),
        total_length = total_length + NEW.length,
        total_size = total_size + COALESCE(NEW.file_size, 0),
        unknown_sizes = unknown_sizes + (NEW.file_size IS NULL)
    WHERE singleton = 1;
END;

CREATE TRIGGER statistics_items_delete
AFTER DELETE ON items
BEGIN
    UPDATE statistics_album_members
    SET member_count = member_count - 1 WHERE album_id = OLD.album_id;
    UPDATE statistics_artist_members
    SET member_count = member_count - 1 WHERE artist = OLD.artist;

    UPDATE library_statistics
    SET tracks = tracks - 1,
        albums = albums - COALESCE((
            SELECT member_count = 0
            FROM statistics_album_members WHERE album_id = OLD.album_id
        ), 0),
        artists = artists - COALESCE((
            SELECT member_count = 0
            FROM statistics_artist_members WHERE artist = OLD.artist
        ), 0),
        total_length = total_length - OLD.length,
        total_size = total_size - COALESCE(OLD.file_size, 0),
        unknown_sizes = unknown_sizes - (OLD.file_size IS NULL)
    WHERE singleton = 1;

    DELETE FROM statistics_album_members
    WHERE album_id = OLD.album_id AND member_count = 0;
    DELETE FROM statistics_artist_members
    WHERE artist = OLD.artist AND member_count = 0;
END;

CREATE TRIGGER statistics_items_album_update
AFTER UPDATE OF album_id ON items
WHEN NEW.album_id IS NOT OLD.album_id
BEGIN
    UPDATE statistics_album_members
    SET member_count = member_count - 1 WHERE album_id = OLD.album_id;
    INSERT INTO statistics_album_members (album_id, member_count)
    VALUES (NEW.album_id, 1)
    ON CONFLICT(album_id) DO UPDATE SET member_count = member_count + 1;
    UPDATE library_statistics
    SET albums = albums
        - COALESCE((
            SELECT member_count = 0
            FROM statistics_album_members WHERE album_id = OLD.album_id
        ), 0)
        + (
            SELECT member_count = 1
            FROM statistics_album_members WHERE album_id = NEW.album_id
        )
    WHERE singleton = 1;
    DELETE FROM statistics_album_members
    WHERE album_id = OLD.album_id AND member_count = 0;
END;

CREATE TRIGGER statistics_items_artist_update
AFTER UPDATE OF artist ON items
WHEN NEW.artist IS NOT OLD.artist
BEGIN
    UPDATE statistics_artist_members
    SET member_count = member_count - 1 WHERE artist = OLD.artist;
    INSERT INTO statistics_artist_members (artist, member_count)
    VALUES (NEW.artist, 1)
    ON CONFLICT(artist) DO UPDATE SET member_count = member_count + 1;
    UPDATE library_statistics
    SET artists = artists
        - COALESCE((
            SELECT member_count = 0
            FROM statistics_artist_members WHERE artist = OLD.artist
        ), 0)
        + (
            SELECT member_count = 1
            FROM statistics_artist_members WHERE artist = NEW.artist
        )
    WHERE singleton = 1;
    DELETE FROM statistics_artist_members
    WHERE artist = OLD.artist AND member_count = 0;
END;

CREATE TRIGGER statistics_items_measure_update
AFTER UPDATE OF length, file_size ON items
WHEN NEW.length IS NOT OLD.length OR NEW.file_size IS NOT OLD.file_size
BEGIN
    UPDATE library_statistics
    SET total_length = total_length - OLD.length + NEW.length,
        total_size = total_size - COALESCE(OLD.file_size, 0) + COALESCE(NEW.file_size, 0),
        unknown_sizes = unknown_sizes
            - (OLD.file_size IS NULL)
            + (NEW.file_size IS NULL)
    WHERE singleton = 1;
END;
