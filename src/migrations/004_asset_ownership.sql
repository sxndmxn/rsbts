-- Persistent roots, managed assets, ancillary ownership, and durable operation history.

DROP INDEX IF EXISTS idx_items_path;

ALTER TABLE operation_journal ADD COLUMN completed_at TEXT;
ALTER TABLE operation_files ADD COLUMN sha256 TEXT;
ALTER TABLE operation_files ADD COLUMN asset_id TEXT;

CREATE TABLE library_roots (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('online', 'offline', 'read-only', 'degraded', 'legacy')),
    capabilities_json TEXT NOT NULL CHECK (json_valid(capabilities_json)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE assets (
    id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL REFERENCES library_roots(id),
    relative_path TEXT NOT NULL,
    absolute_path TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL,
    managed INTEGER NOT NULL CHECK (managed IN (0, 1)),
    verification_state TEXT NOT NULL
        CHECK (verification_state IN ('unverified', 'verified', 'modified', 'missing', 'corrupt')),
    byte_size INTEGER,
    blake3 TEXT,
    sha256 TEXT,
    audio_essence_hash TEXT,
    mtime TEXT,
    entry_identity TEXT,
    media_json TEXT CHECK (media_json IS NULL OR json_valid(media_json)),
    projection_state TEXT NOT NULL DEFAULT 'current'
        CHECK (projection_state IN ('current', 'diverged', 'pending', 'failed')),
    first_seen_at TEXT NOT NULL,
    last_verified_at TEXT,
    UNIQUE (root_id, relative_path)
);

CREATE TABLE item_assets (
    item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE RESTRICT,
    relationship TEXT NOT NULL DEFAULT 'audio',
    segment_start REAL,
    segment_end REAL,
    PRIMARY KEY (item_id, asset_id, relationship)
);

CREATE TABLE album_assets (
    album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE RESTRICT,
    relationship TEXT NOT NULL,
    PRIMARY KEY (album_id, asset_id, relationship)
);

CREATE INDEX idx_assets_blake3 ON assets(blake3) WHERE blake3 IS NOT NULL;
CREATE INDEX idx_assets_sha256 ON assets(sha256) WHERE sha256 IS NOT NULL;
CREATE INDEX idx_assets_verification ON assets(verification_state, id);
CREATE INDEX idx_items_album_id ON items(album_id);
CREATE INDEX idx_items_browse ON items(artist, album, disc, track, id);
CREATE INDEX idx_albums_browse ON albums(albumartist, year, album, id);

-- The all-zero root is a typed migration sentinel, not an assertion about the
-- filesystem root. Legacy absolute paths remain intact until a configured root
-- claims and verifies each asset.
INSERT INTO library_roots
    (id, path, state, capabilities_json, created_at, updated_at)
VALUES
    ('00000000-0000-0000-0000-000000000000', '', 'legacy', '{}', datetime('now'), datetime('now'));

INSERT INTO assets
    (id, root_id, relative_path, absolute_path, role, managed,
    verification_state, byte_size, mtime, first_seen_at)
SELECT
    printf('00000000-0000-0000-0000-%012x', id),
    '00000000-0000-0000-0000-000000000000',
    path,
    path,
    'audio',
    1,
    'unverified',
    file_size,
    mtime,
    added
FROM items;

INSERT INTO item_assets (item_id, asset_id, relationship)
SELECT id, printf('00000000-0000-0000-0000-%012x', id), 'audio'
FROM items;

INSERT INTO assets
    (id, root_id, relative_path, absolute_path, role, managed,
     verification_state, first_seen_at)
SELECT
    printf('10000000-0000-0000-0000-%012x', id),
    '00000000-0000-0000-0000-000000000000',
    artpath,
    artpath,
    'artwork',
    1,
    'unverified',
    added
FROM albums
WHERE artpath IS NOT NULL
ON CONFLICT(absolute_path) DO NOTHING;

INSERT INTO album_assets (album_id, asset_id, relationship)
SELECT albums.id, assets.id, 'front'
FROM albums
JOIN assets ON assets.absolute_path = albums.artpath
WHERE albums.artpath IS NOT NULL;
