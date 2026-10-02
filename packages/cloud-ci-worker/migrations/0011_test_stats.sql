-- docs/design/analytics.md § "D1 rollup tables": the rolling per-test
-- aggregate `parallelization.md`'s `timing` split strategy reads, plus the
-- idempotency gate for the `RunCoordinator`-finalization write path that
-- populates it (`coordinator::mod`'s `finalize_test_stats`). Forward-only,
-- per AGENTS.md.
--
-- Scope decision (docs/design/analytics.md names five D1Test tables:
-- `report_uploads`, `report_summaries`, `test_stats_applications`,
-- `test_failures`, `test_stats`): this migration adds only `test_stats` and
-- `test_stats_applications`. `report_uploads`/`report_summaries` are NOT
-- added as parallel tables — this crate's existing `reports` table
-- (migration 0002) already plays `report_uploads`' role for this round's
-- purposes: it is a 1:1, never-updated-after-insert row per accepted
-- report content (`UNIQUE (job_id, shard_index, kind, name,
-- content_sha256)`), already tracks `is_canonical` (the "latest accepted
-- content wins" rule `report_uploads`/`report_summaries` exist to serve)
-- and `parsed` (whether a `cloud-ci-reports` parser produced a summary).
-- What `reports` does not have — total/passed/failed/skipped/duration_ms
-- broken into columns, and a direct `r2_key` to the full parsed object —
-- is not needed by this round's finalization, which re-parses full test
-- outcomes from raw report bytes directly (see migration 0012's `r2_key`
-- column) rather than reading a pre-aggregated `report_uploads` row.
-- `report_summaries` (the live partial-aggregate dashboard view) has no
-- reader built yet in this round's scope boundary — adding it now would be
-- a table with no caller, which this crate's forward-only migrations avoid
-- elsewhere. A future round that needs `report_summaries`' specific
-- recompute-wholesale semantics can add it then; nothing here blocks that.
--
-- `test_failures` is also not added this round: the brief's scope is
-- "make `test_stats` a real, populated table", and `test_failures` has no
-- reader yet either (same reasoning as `report_summaries`, above).
--
-- Default-branch scoping: analytics.md and parallelization.md both say
-- `test_stats` is "updated in place on every default-branch run" /
-- "scoped to default-branch runs" for the `timing` split strategy's
-- correctness story (a PR branch's flaky/transient failures would
-- otherwise pollute `recent_outcomes`/`flakiness_score`). This repo's
-- `run` row (`coordinator::mod`'s `ensure_schema`) does not track branch
-- name at all today — `BeginRun` has no branch field, and no caller
-- resolves "is this the default branch" anywhere in this crate. Adding
-- that (a new `BeginRun` field, GitHub default-branch resolution, wiring
-- through every caller) is a genuinely separate round's scope, not an
-- incidental add-on to this one. This round's choice: `finalize_test_stats`
-- is correctness-complete and branch-agnostic for whichever runs call it;
-- default-branch *filtering* (deciding which runs' finalization actually
-- fires, or skipping non-default-branch runs) is deferred to a future
-- caller. Until that lands, every run (including PR branches) updates
-- `test_stats` — a known, documented gap in the flakiness signal's
-- cleanliness, not a silent omission.
CREATE TABLE test_stats (
    repo_id          INTEGER NOT NULL,
    test_id          TEXT NOT NULL,
    file_path        TEXT NOT NULL,
    test_name        TEXT NOT NULL,
    runs             INTEGER NOT NULL DEFAULT 0,
    -- exponential moving average, alpha = 0.2 (coordinator::logic::ewma)
    duration_ewma_ms REAL NOT NULL,
    last_duration_ms INTEGER NOT NULL,
    -- pass | fail | skip
    last_status      TEXT NOT NULL,
    -- last 20 outcomes, one char each ('P'/'F'/'S'), newest last
    recent_outcomes  TEXT NOT NULL,
    -- flips / (len(recent_outcomes) - 1); best-effort, see finalize_test_stats
    flakiness_score  REAL NOT NULL DEFAULT 0,
    last_run_id      TEXT NOT NULL,
    last_sha         TEXT NOT NULL,
    updated_at       INTEGER NOT NULL,
    PRIMARY KEY (repo_id, test_id)
);
CREATE INDEX idx_test_stats_flaky ON test_stats (repo_id, flakiness_score);

-- Gate for applying one run's finalized groups to test_stats exactly once
-- (analytics.md's "Idempotency"). Plain INSERTTs with no `OR IGNORE`: a
-- redelivered finalization batch hits this PRIMARY KEY and the whole D1
-- `batch()` call rolls back, which `finalize_test_stats` treats as
-- "already applied", not an error to retry.
CREATE TABLE test_stats_applications (
    run_id        TEXT NOT NULL,
    job_name      TEXT NOT NULL,
    report_type   TEXT NOT NULL,
    report_name   TEXT NOT NULL,
    scope         TEXT NOT NULL,
    applied_at    INTEGER NOT NULL,
    PRIMARY KEY (run_id, job_name, report_type, report_name, scope)
);
