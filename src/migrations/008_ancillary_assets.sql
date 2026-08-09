-- Typed ancillary-asset metadata without flattening files into musical entities.

CREATE TABLE ancillary_metadata (
    asset_id TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN (
        'cue-sheet', 'rip-log', 'checksum', 'lyrics', 'pdf', 'scan',
        'booklet', 'data-file', 'other'
    )),
    media_type TEXT NOT NULL,
    original_filename TEXT NOT NULL,
    imported_at TEXT NOT NULL
);

CREATE INDEX idx_ancillary_kind ON ancillary_metadata(kind, asset_id);
