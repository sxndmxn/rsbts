-- Durable plans, projection metadata, fixity history, and archival exports.

ALTER TABLE operation_journal ADD COLUMN plan_id TEXT;
ALTER TABLE operation_journal ADD COLUMN decision_json TEXT CHECK (decision_json IS NULL OR json_valid(decision_json));
ALTER TABLE operation_files ADD COLUMN root_id TEXT REFERENCES library_roots(id);
ALTER TABLE operation_files ADD COLUMN source_relative_path TEXT;
ALTER TABLE operation_files ADD COLUMN staged_relative_path TEXT;
ALTER TABLE operation_files ADD COLUMN destination_relative_path TEXT;

CREATE TABLE durable_plans (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('planned', 'approved', 'running', 'paused', 'complete', 'failed', 'cancelled')),
    request_json TEXT NOT NULL CHECK (json_valid(request_json)),
    preview_json TEXT NOT NULL CHECK (json_valid(preview_json)),
    progress_current INTEGER NOT NULL DEFAULT 0 CHECK (progress_current >= 0),
    progress_total INTEGER CHECK (progress_total IS NULL OR progress_total >= 0),
    resume_cursor TEXT,
    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0, 1)),
    error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    completed_at TEXT
);

CREATE TABLE plan_events (
    plan_id TEXT NOT NULL REFERENCES durable_plans(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    event_type TEXT NOT NULL,
    detail_json TEXT NOT NULL CHECK (json_valid(detail_json)),
    created_at TEXT NOT NULL,
    PRIMARY KEY (plan_id, sequence)
);

CREATE TABLE fixity_runs (
    id TEXT PRIMARY KEY,
    plan_id TEXT REFERENCES durable_plans(id),
    mode TEXT NOT NULL CHECK (mode IN ('quick', 'deep', 'manifest-verify', 'restore-verify')),
    state TEXT NOT NULL CHECK (state IN ('running', 'paused', 'complete', 'failed', 'cancelled')),
    cursor_asset_id TEXT,
    checked_count INTEGER NOT NULL DEFAULT 0 CHECK (checked_count >= 0),
    failure_count INTEGER NOT NULL DEFAULT 0 CHECK (failure_count >= 0),
    started_at TEXT NOT NULL,
    completed_at TEXT
);

CREATE TABLE fixity_results (
    run_id TEXT NOT NULL REFERENCES fixity_runs(id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('ok', 'missing', 'modified', 'replaced', 'unverified', 'corrupt', 'offline', 'policy-divergent', 'unreadable')),
    observed_size INTEGER,
    observed_blake3 TEXT,
    observed_sha256 TEXT,
    observed_audio_essence_hash TEXT,
    detail TEXT,
    checked_at TEXT NOT NULL,
    PRIMARY KEY (run_id, asset_id)
);

CREATE TABLE preservation_manifests (
    id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL REFERENCES library_roots(id),
    format TEXT NOT NULL CHECK (format IN ('sha256', 'bagit')),
    manifest_path TEXT NOT NULL,
    manifest_sha256 TEXT NOT NULL,
    asset_count INTEGER NOT NULL CHECK (asset_count >= 0),
    byte_count INTEGER NOT NULL CHECK (byte_count >= 0),
    created_at TEXT NOT NULL,
    verified_at TEXT,
    verification_state TEXT NOT NULL CHECK (verification_state IN ('unverified', 'verified', 'failed'))
);

CREATE TABLE backup_restore_runs (
    id TEXT PRIMARY KEY,
    manifest_id TEXT REFERENCES preservation_manifests(id),
    source_path TEXT NOT NULL,
    restore_path TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('running', 'complete', 'failed')),
    detail TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT
);

CREATE TABLE artwork_metadata (
    asset_id TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
    exact_release_id TEXT REFERENCES releases(id),
    release_group_id TEXT REFERENCES release_groups(id),
    potentially_inexact INTEGER NOT NULL DEFAULT 0 CHECK (potentially_inexact IN (0, 1)),
    role TEXT NOT NULL,
    source_provider TEXT,
    source_reference TEXT,
    provider_release_id TEXT,
    mime TEXT NOT NULL,
    width INTEGER NOT NULL CHECK (width > 0),
    height INTEGER NOT NULL CHECK (height > 0),
    approval_state TEXT NOT NULL CHECK (approval_state IN ('pending', 'approved', 'rejected')),
    rights TEXT,
    original_asset_id TEXT REFERENCES assets(id),
    transform_json TEXT CHECK (transform_json IS NULL OR json_valid(transform_json))
);

CREATE TABLE projection_plans (
    id TEXT PRIMARY KEY REFERENCES durable_plans(id) ON DELETE CASCADE,
    projection_type TEXT NOT NULL CHECK (projection_type IN ('tags', 'paths', 'artwork')),
    profile TEXT NOT NULL,
    policy_version INTEGER NOT NULL CHECK (policy_version > 0),
    approved_at TEXT
);

CREATE TABLE asset_projection_steps (
    plan_id TEXT NOT NULL REFERENCES projection_plans(id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL REFERENCES assets(id),
    before_json TEXT NOT NULL CHECK (json_valid(before_json)),
    after_json TEXT NOT NULL CHECK (json_valid(after_json)),
    state TEXT NOT NULL CHECK (state IN ('planned', 'staged', 'validated', 'published', 'failed', 'rolled-back')),
    evidence_json TEXT NOT NULL CHECK (json_valid(evidence_json)),
    PRIMARY KEY (plan_id, asset_id)
);

CREATE TABLE dedup_decisions (
    id TEXT PRIMARY KEY,
    retained_asset_id TEXT NOT NULL REFERENCES assets(id),
    duplicate_asset_id TEXT NOT NULL REFERENCES assets(id),
    authorization_algorithm TEXT NOT NULL CHECK (authorization_algorithm IN ('blake3+sha256')),
    digest TEXT NOT NULL,
    approved_at TEXT NOT NULL,
    CHECK (retained_asset_id != duplicate_asset_id)
);

CREATE TABLE recording_assets (
    recording_id TEXT NOT NULL REFERENCES recordings(id) ON DELETE CASCADE,
    release_track_id TEXT REFERENCES release_tracks(id) ON DELETE SET NULL,
    asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE RESTRICT,
    relationship TEXT NOT NULL DEFAULT 'audio',
    segment_start REAL,
    segment_end REAL,
    PRIMARY KEY (recording_id, asset_id, relationship)
);

CREATE INDEX idx_durable_plans_state ON durable_plans(state, updated_at, id);
CREATE INDEX idx_fixity_runs_state ON fixity_runs(state, started_at, id);
CREATE INDEX idx_fixity_results_state ON fixity_results(state, asset_id);
CREATE INDEX idx_artwork_exact_release ON artwork_metadata(exact_release_id, role);
CREATE INDEX idx_operation_files_asset ON operation_files(asset_id, operation_id);
CREATE INDEX idx_recording_assets_track ON recording_assets(release_track_id, asset_id);
