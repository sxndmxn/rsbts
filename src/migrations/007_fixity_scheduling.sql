-- Persistent fixity schedules and schedule-to-run provenance.

CREATE TABLE fixity_schedules (
    id TEXT PRIMARY KEY,
    mode TEXT NOT NULL CHECK (mode IN ('quick', 'deep')),
    interval_seconds INTEGER NOT NULL CHECK (interval_seconds > 0),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    next_run_at TEXT NOT NULL,
    last_plan_id TEXT REFERENCES durable_plans(id),
    last_completed_at TEXT,
    last_failure_count INTEGER CHECK (last_failure_count IS NULL OR last_failure_count >= 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

ALTER TABLE fixity_runs ADD COLUMN schedule_id TEXT REFERENCES fixity_schedules(id);

CREATE INDEX idx_fixity_schedules_due
    ON fixity_schedules(enabled, next_run_at, id);
CREATE INDEX idx_fixity_runs_schedule
    ON fixity_runs(schedule_id, started_at, id);
