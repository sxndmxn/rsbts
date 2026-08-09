-- Plugin-free core replacement metadata: singletons, typed flexible fields, and multi-provider IDs.

ALTER TABLE items ADD COLUMN singleton INTEGER NOT NULL DEFAULT 0
    CHECK (singleton IN (0, 1));

CREATE TABLE entity_metadata (
    entity_type TEXT NOT NULL CHECK (entity_type IN ('item', 'album')),
    entity_id INTEGER NOT NULL,
    field TEXT NOT NULL,
    ordinal INTEGER NOT NULL DEFAULT 0,
    value_type TEXT NOT NULL CHECK (
        value_type IN ('string', 'integer', 'float', 'boolean', 'date', 'string_list')
    ),
    value_json TEXT NOT NULL,
    PRIMARY KEY (entity_type, entity_id, field, ordinal)
);

CREATE INDEX idx_entity_metadata_lookup
    ON entity_metadata(entity_type, field, entity_id);

CREATE TABLE library_external_ids (
    entity_type TEXT NOT NULL CHECK (entity_type IN ('item', 'album')),
    entity_id INTEGER NOT NULL,
    provider TEXT NOT NULL,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (entity_type, entity_id, provider, kind, value)
);

CREATE INDEX idx_library_external_ids_lookup
    ON library_external_ids(provider, kind, value);

INSERT OR IGNORE INTO library_external_ids(entity_type, entity_id, provider, kind, value)
SELECT 'album', id, metadata_provider, 'release', external_release_id
FROM albums
WHERE metadata_provider IS NOT NULL AND external_release_id IS NOT NULL;

INSERT OR IGNORE INTO library_external_ids(entity_type, entity_id, provider, kind, value)
SELECT 'item', id, metadata_provider, 'recording', external_track_id
FROM items
WHERE metadata_provider IS NOT NULL AND external_track_id IS NOT NULL;

INSERT OR IGNORE INTO library_external_ids(entity_type, entity_id, provider, kind, value)
SELECT 'item', id, metadata_provider, 'release', external_release_id
FROM items
WHERE metadata_provider IS NOT NULL AND external_release_id IS NOT NULL;
