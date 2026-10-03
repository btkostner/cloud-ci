-- Nullable: a run with no frozen sha has no retroactive way to get one
-- -- an explicit "no snapshot" condition, never a live-HEAD substitute.
ALTER TABLE runs ADD COLUMN settings_sha TEXT;
