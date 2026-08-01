-- Safety foundation: durable operation journal, generic provider IDs, exact sizes.

ALTER TABLE albums ADD COLUMN metadata_provider TEXT;
ALTER TABLE albums ADD COLUMN external_release_id TEXT;

ALTER TABLE items ADD COLUMN file_size INTEGER;
ALTER TABLE items ADD COLUMN metadata_provider TEXT;
ALTER TABLE items ADD COLUMN external_track_id TEXT;
ALTER TABLE items ADD COLUMN external_release_id TEXT;

UPDATE albums
SET metadata_provider = 'musicbrainz', external_release_id = mb_albumid
WHERE mb_albumid IS NOT NULL;

UPDATE items
SET metadata_provider = 'musicbrainz',
    external_track_id = mb_trackid,
    external_release_id = mb_albumid
WHERE mb_trackid IS NOT NULL OR mb_albumid IS NOT NULL;

CREATE TABLE operation_journal (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    error TEXT
);

CREATE TABLE operation_files (
    operation_id TEXT NOT NULL REFERENCES operation_journal(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    source_path TEXT NOT NULL,
    staged_path TEXT NOT NULL,
    destination_path TEXT NOT NULL,
    content_hash TEXT,
    role TEXT NOT NULL DEFAULT 'track',
    state TEXT NOT NULL,
    PRIMARY KEY (operation_id, ordinal)
);

CREATE INDEX idx_operation_journal_state ON operation_journal(state);
CREATE INDEX idx_items_external_release
    ON items(metadata_provider, external_release_id);
CREATE INDEX idx_albums_external_release
    ON albums(metadata_provider, external_release_id);
