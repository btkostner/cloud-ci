-- docs/design/parallelization.md's "### Merge barrier (RunCoordinator)" /
-- "### Failed-shard retry semantics": `RunCoordinator`'s own SQLite-backed
-- `shard_state` table (one row per `(job_name, idx, attempt)`, see
-- `coordinator::mod`'s `ensure_schema`) is the authoritative, DO-local
-- record for one run's shard-group merge barrier. This migration adds its
-- D1 projection, same authoritative-DO/projected-D1 split as every other
-- table in this crate (`nodes`, migration 0010, is the closest precedent:
-- identity key `(run_id, node_id)` in D1 vs. `node_id` alone DO-local,
-- since one `RunCoordinator` instance *is* one run). Forward-only, per
-- AGENTS.md.
--
-- Scope decision (this round is the barrier *state machine* only — see
-- `coordinator::logic`'s "Shard groups / merge barrier" section for the
-- exact boundary): `job_group` is deliberately NOT projected to D1 here.
-- Nothing outside `RunCoordinator` reads it yet — no dashboard view, no
-- cross-run query — same "a table with no caller" reasoning migration
-- 0011 already used to exclude `report_summaries`/`test_failures`. A
-- future round adding a shard-group dashboard view can add that
-- projection then; this one keeps `job_group` DO-local-only, matching
-- parallelization.md's own wording ("`RunCoordinator`'s SQLite-backed
-- state ... holds, per shard group" — it never lists `job_group` among
-- the "### D1 tables" section's tables either).
CREATE TABLE shard_states (
    run_id      TEXT NOT NULL REFERENCES runs (id),
    job_name    TEXT NOT NULL,
    idx         INTEGER NOT NULL,
    attempt     INTEGER NOT NULL DEFAULT 1,
    status      TEXT NOT NULL, -- passed | failed (terminal only, see logic::ShardTerminalStatus)
    report_key  TEXT,
    duration_ms INTEGER,
    finished_at INTEGER,
    PRIMARY KEY (run_id, job_name, idx, attempt)
);
