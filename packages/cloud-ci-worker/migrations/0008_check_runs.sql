-- RunCoordinator's D1 projection (ADR 0004: "D1 rows are its projection") of
-- the Check Runs it creates for StartJob.check_names/`cloud-ci upload --check`
-- (docs/design/byo-ci.md's "Checks and scopes", docs/design/pr-comment.md's
-- "Check Runs" table). Storage design: `check_runs` is keyed by
-- `(run_id, check_name)`, matching the doc's "first time a job names it"
-- rule, which is per-run, not per-job — several jobs naming the same check
-- share one row. The DO's own SQLite (`check_run` table, see
-- `coordinator::ensure_schema`) is authoritative and is what idempotency
-- decisions are made against; this table is a read-only projection for
-- dashboard/query use, same as `runs`/`jobs`/`job_shards`.
--
-- `github_check_run_id` is `NOT NULL`: a row is only inserted after the
-- `POST /check-runs` call that creates it on GitHub actually succeeds (see
-- `coordinator::mod`'s Check Run creation path), so there is never a row
-- with a check name but no corresponding GitHub check run — the insert
-- always has a real id in hand by construction.
CREATE TABLE check_runs (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs (id),
    check_name TEXT NOT NULL,
    github_check_run_id INTEGER NOT NULL,
    status TEXT NOT NULL,
    conclusion TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (run_id, check_name)
);
