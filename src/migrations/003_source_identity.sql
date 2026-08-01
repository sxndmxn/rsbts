-- Preserve move sources and removal quarantines unless recovery can prove ownership.

ALTER TABLE operation_files ADD COLUMN source_identity TEXT;
ALTER TABLE operation_files ADD COLUMN owned_identity TEXT;
