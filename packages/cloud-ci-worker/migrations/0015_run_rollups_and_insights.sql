-- docs/design/analytics.md § "D1 rollup tables": the `*/15` rollup cron's
-- (`rollup::run`, `lib.rs`'s `#[event(scheduled)]`) two output tables.
-- Forward-only, per AGENTS.md: this migration only adds tables, never
-- alters/drops anything 0001-0014 created.
--
-- Scope decision: analytics.md's "D1 tables" list also names
-- `sizing_decisions`, owned by the (separate, not-yet-built) nightly
-- rightsizing cron — not added here; adding it now would be a table with
-- no writer, which this crate's migrations avoid elsewhere (migration
-- 0011's doc comment on `report_summaries`/`test_failures` is the same
-- reasoning).
--
-- Honesty note (see `rollup.rs`'s module doc comment for the full
-- explanation): this round's cron populates `run_id`/`repo_id`/
-- `head_sha`/`started_at`/`status` for real, read from the existing
-- `runs` table. `duration_ms`/`queue_ms`/`critical_path_ms`/
-- `cache_hit_rate`/`cost_usd_estimate` are left NULL on every row this
-- round writes — none of `step`/`sample`/`cache` Analytics Engine events
-- are wired yet (only `test` is, per `coordinator::mod::write_test_events`),
-- and those are exactly the event kinds analytics.md's schema says those
-- columns are derived from. Nullable, not NOT NULL DEFAULT 0, so a
-- dashboard reading this table can distinguish "not yet computed" from
-- "measured as zero".
CREATE TABLE run_rollups (
    run_id              TEXT PRIMARY KEY REFERENCES runs (id),
    repo_id             INTEGER NOT NULL,
    head_sha            TEXT NOT NULL,
    started_at          INTEGER NOT NULL,
    duration_ms         INTEGER,
    queue_ms            INTEGER,
    critical_path_ms    INTEGER,
    cache_hit_rate      REAL,
    cost_usd_estimate   REAL,
    status              TEXT NOT NULL,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX idx_run_rollups_repo ON run_rollups (repo_id, started_at);

-- docs/design/analytics.md's "Derived insights" table lists `slow_test`/
-- `flaky_test`/`duration_regression`/`queue_regression`/`cache_degraded`/
-- `duration_improvement`/`sizing_improvement` — every one of those needs a
-- baseline (a 7/30-day window, a prior branch average, a prior instance
-- size) this round's `test`-only data cannot support, or depends on
-- `step`/`sample`/`cache` events not yet wired. `rollup.rs` writes only a
-- new, additive `run_test_failures` kind (count of failed tests in one
-- run, from the `test`-kind Analytics Engine rows alone) as a minimal,
-- honest starting slice — see that module's doc comment.
CREATE TABLE insights (
    repo_id       INTEGER NOT NULL,
    kind          TEXT NOT NULL,
    subject_id    TEXT NOT NULL,
    severity      TEXT NOT NULL,
    detail_json   TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (repo_id, kind, subject_id, created_at)
);
CREATE INDEX idx_insights_repo_created ON insights (repo_id, created_at);
