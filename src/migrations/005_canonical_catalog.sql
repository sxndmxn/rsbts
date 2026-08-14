-- Normalized musical identity, immutable claims, provider snapshots, and jobs.

CREATE TABLE release_groups (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    primary_type TEXT,
    secondary_types_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(secondary_types_json)),
    disambiguation TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE releases (
    id TEXT PRIMARY KEY,
    release_group_id TEXT REFERENCES release_groups(id),
    title TEXT NOT NULL,
    disambiguation TEXT,
    status TEXT,
    packaging TEXT,
    barcode TEXT,
    country TEXT,
    release_date TEXT,
    original_release_date TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE media (
    id TEXT PRIMARY KEY,
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position > 0),
    title TEXT,
    format TEXT,
    track_count INTEGER CHECK (track_count IS NULL OR track_count >= 0),
    disc_id TEXT,
    data_track_count INTEGER NOT NULL DEFAULT 0 CHECK (data_track_count >= 0),
    pregap_ms INTEGER CHECK (pregap_ms IS NULL OR pregap_ms >= 0),
    UNIQUE (release_id, position)
);

CREATE TABLE recordings (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    disambiguation TEXT,
    duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
    video INTEGER NOT NULL DEFAULT 0 CHECK (video IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE works (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    work_type TEXT,
    language TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE release_tracks (
    id TEXT PRIMARY KEY,
    medium_id TEXT NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    recording_id TEXT REFERENCES recordings(id),
    position INTEGER NOT NULL CHECK (position > 0),
    printed_position TEXT,
    title TEXT NOT NULL,
    length_ms INTEGER CHECK (length_ms IS NULL OR length_ms >= 0),
    is_data_track INTEGER NOT NULL DEFAULT 0 CHECK (is_data_track IN (0, 1)),
    is_hidden INTEGER NOT NULL DEFAULT 0 CHECK (is_hidden IN (0, 1)),
    pregap_ms INTEGER CHECK (pregap_ms IS NULL OR pregap_ms >= 0),
    UNIQUE (medium_id, position)
);

CREATE TABLE artists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    sort_name TEXT,
    disambiguation TEXT,
    artist_type TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE artist_credits (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL
);

CREATE TABLE artist_credit_names (
    credit_id TEXT NOT NULL REFERENCES artist_credits(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    artist_id TEXT NOT NULL REFERENCES artists(id),
    credited_name TEXT NOT NULL,
    join_phrase TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (credit_id, position)
);

CREATE TABLE entity_artist_credits (
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    credit_id TEXT NOT NULL REFERENCES artist_credits(id),
    relationship TEXT NOT NULL,
    PRIMARY KEY (entity_type, entity_id, relationship)
);

CREATE TABLE labels (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    sort_name TEXT,
    disambiguation TEXT,
    label_type TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE release_labels (
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE CASCADE,
    label_id TEXT REFERENCES labels(id),
    position INTEGER NOT NULL DEFAULT 0,
    catalog_number TEXT,
    PRIMARY KEY (release_id, position)
);

CREATE TABLE release_events (
    release_id TEXT NOT NULL REFERENCES releases(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    date TEXT,
    country TEXT,
    region TEXT,
    PRIMARY KEY (release_id, position)
);

CREATE TABLE credits (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    artist_id TEXT REFERENCES artists(id),
    credited_name TEXT,
    role TEXT NOT NULL,
    detail TEXT,
    position INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE recording_works (
    recording_id TEXT NOT NULL REFERENCES recordings(id) ON DELETE CASCADE,
    work_id TEXT NOT NULL REFERENCES works(id) ON DELETE CASCADE,
    relationship TEXT NOT NULL DEFAULT 'performance',
    PRIMARY KEY (recording_id, work_id, relationship)
);

CREATE TABLE external_ids (
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    external_id TEXT NOT NULL,
    data_license TEXT NOT NULL,
    source_url TEXT,
    PRIMARY KEY (entity_type, entity_id, provider, external_id),
    UNIQUE (provider, entity_type, external_id)
);

CREATE TABLE provider_snapshots (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    request_key TEXT NOT NULL,
    entity_type TEXT,
    entity_id TEXT,
    retrieved_at TEXT NOT NULL,
    data_license TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    compression TEXT NOT NULL DEFAULT 'none',
    payload BLOB NOT NULL,
    complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
    UNIQUE (provider, request_key, content_sha256)
);

CREATE TABLE metadata_claims (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    field TEXT NOT NULL,
    value_state TEXT NOT NULL
        CHECK (value_state IN ('known', 'unknown', 'absent', 'not-applicable', 'conflict')),
    value_json TEXT CHECK (value_json IS NULL OR json_valid(value_json)),
    source_kind TEXT NOT NULL,
    source_provider TEXT,
    source_reference TEXT,
    retrieved_at TEXT NOT NULL,
    confidence REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    data_license TEXT NOT NULL,
    locked INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0, 1)),
    superseded_by TEXT REFERENCES metadata_claims(id)
);

CREATE TABLE canonical_values (
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    field TEXT NOT NULL,
    value_state TEXT NOT NULL
        CHECK (value_state IN ('known', 'unknown', 'absent', 'not-applicable', 'conflict')),
    value_json TEXT CHECK (value_json IS NULL OR json_valid(value_json)),
    winning_claim_id TEXT REFERENCES metadata_claims(id),
    policy_version INTEGER NOT NULL,
    resolved_at TEXT NOT NULL,
    PRIMARY KEY (entity_type, entity_id, field)
);

CREATE TABLE manual_match_locks (
    file_asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    identity_level TEXT NOT NULL
        CHECK (identity_level IN ('recording', 'release-group', 'release')),
    entity_id TEXT NOT NULL,
    evidence_json TEXT NOT NULL CHECK (json_valid(evidence_json)),
    created_at TEXT NOT NULL,
    revoked_at TEXT,
    PRIMARY KEY (file_asset_id, identity_level)
);

CREATE TABLE asset_relationships (
    parent_asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    child_asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    relationship TEXT NOT NULL,
    position INTEGER,
    PRIMARY KEY (parent_asset_id, child_asset_id, relationship)
);

CREATE TABLE provider_jobs (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    operation TEXT NOT NULL,
    request_json TEXT NOT NULL CHECK (json_valid(request_json)),
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'retry', 'complete', 'failed', 'cancelled')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    available_at TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT,
    last_error TEXT,
    response_snapshot_id TEXT REFERENCES provider_snapshots(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_releases_group ON releases(release_group_id);
CREATE INDEX idx_media_release ON media(release_id, position);
CREATE INDEX idx_release_tracks_recording ON release_tracks(recording_id);
CREATE INDEX idx_claims_resolution
    ON metadata_claims(entity_type, entity_id, field, locked DESC, confidence DESC, retrieved_at DESC);
CREATE INDEX idx_snapshots_entity ON provider_snapshots(entity_type, entity_id, retrieved_at);
CREATE INDEX idx_provider_jobs_ready ON provider_jobs(state, available_at, id);

ALTER TABLE albums ADD COLUMN canonical_release_id TEXT REFERENCES releases(id);
ALTER TABLE items ADD COLUMN release_track_id TEXT REFERENCES release_tracks(id);
ALTER TABLE items ADD COLUMN recording_id TEXT REFERENCES recordings(id);

-- Legacy rows become explicitly partial normalized entities. This preserves
-- known titles and positions without inventing exact-edition semantics.
INSERT INTO release_groups (id, title, created_at, updated_at)
SELECT printf('20000000-0000-0000-0000-%012x', id), album, added, added
FROM albums;

INSERT INTO releases
    (id, release_group_id, title, release_date, original_release_date, created_at, updated_at)
SELECT
    printf('30000000-0000-0000-0000-%012x', id),
    printf('20000000-0000-0000-0000-%012x', id),
    album,
    CASE WHEN year IS NULL THEN NULL ELSE printf('%04d', year) END,
    NULL,
    added,
    added
FROM albums;

INSERT INTO media (id, release_id, position, track_count)
SELECT
    printf('40000000-0000-0000-0000-%012x', albums.id),
    printf('30000000-0000-0000-0000-%012x', albums.id),
    1,
    COUNT(items.id)
FROM albums LEFT JOIN items ON items.album_id = albums.id
GROUP BY albums.id;

INSERT INTO recordings (id, title, duration_ms, created_at, updated_at)
SELECT
    printf('50000000-0000-0000-0000-%012x', id),
    title,
    CAST(round(length * 1000.0) AS INTEGER),
    added,
    added
FROM items;

INSERT INTO release_tracks
    (id, medium_id, recording_id, position, printed_position, title, length_ms)
SELECT
    printf('60000000-0000-0000-0000-%012x', items.id),
    printf('40000000-0000-0000-0000-%012x', items.album_id),
    printf('50000000-0000-0000-0000-%012x', items.id),
    COALESCE(items.track, items.id),
    CASE WHEN items.track IS NULL THEN NULL ELSE CAST(items.track AS TEXT) END,
    items.title,
    CAST(round(items.length * 1000.0) AS INTEGER)
FROM items;

UPDATE albums
SET canonical_release_id = printf('30000000-0000-0000-0000-%012x', id);

UPDATE items
SET release_track_id = printf('60000000-0000-0000-0000-%012x', id),
    recording_id = printf('50000000-0000-0000-0000-%012x', id);

INSERT OR IGNORE INTO external_ids
    (entity_type, entity_id, provider, external_id, data_license)
SELECT
    'release',
    canonical_release_id,
    metadata_provider,
    external_release_id,
    CASE WHEN metadata_provider = 'musicbrainz' THEN 'CC0-1.0' ELSE 'source-specific' END
FROM albums
WHERE metadata_provider IS NOT NULL AND external_release_id IS NOT NULL;

-- Version 2's external_track_id stores recording IDs for the built-in
-- MusicBrainz provider; migrate it honestly as a recording identifier.
INSERT OR IGNORE INTO external_ids
    (entity_type, entity_id, provider, external_id, data_license)
SELECT
    'recording',
    recording_id,
    metadata_provider,
    external_track_id,
    CASE WHEN metadata_provider = 'musicbrainz' THEN 'CC0-1.0' ELSE 'source-specific' END
FROM items
WHERE metadata_provider IS NOT NULL AND external_track_id IS NOT NULL;
