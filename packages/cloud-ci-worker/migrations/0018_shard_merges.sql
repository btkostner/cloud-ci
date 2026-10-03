-- docs/design/parallelization.md's "### Merge strategies per report type": the `cloud-ci-merge`
-- Queue consumer's own record of each merge it performed, for a future dashboard/full-report
-- page to find the merged artifact (see `src/shard_merge.rs`'s module doc comment for the full
-- dispatch/consumer story this table is the write side of). Forward-only, per AGENTS.md: this
-- migration only adds a table, never alters/drops anything 0001-0017 created.
--
-- Scope decision: unlike `shard_states`/`reports`, this is NOT a DO-authoritative-then-projected
-- table — the Queue consumer (`src/lib.rs`'s `handle_shard_merge_requested`) is a plain Worker
-- handler with direct D1 access, not a `RunCoordinator` RPC, so it writes here directly. One row
-- per `(run_id, job_name, report_kind, report_name)` the consumer successfully merged;
-- `included_idxs`/`source_r2_keys` are JSON arrays recording exactly which shards' reports went
-- into this merge (parallelization.md's `if_any_passed` default means that set can be a strict
-- subset of the job's full shard count). No row is written for an `Unsupported`-kind report
-- (`shard_merge::MergeableKind`) — see that module's doc comment: logged and skipped, not a
-- recorded no-op, since there is no merged artifact to point a dashboard at.
CREATE TABLE shard_merges (
    id              TEXT PRIMARY KEY,
    run_id          TEXT NOT NULL REFERENCES runs (id),
    job_id          TEXT NOT NULL REFERENCES jobs (id),
    job_name        TEXT NOT NULL,
    report_kind     TEXT NOT NULL, -- 'junit' | 'lcov' (shard_merge::JUNIT_KIND/LCOV_KIND)
    report_name     TEXT NOT NULL,
    included_idxs   TEXT NOT NULL, -- JSON array of shard indices merged
    content_sha256  TEXT NOT NULL,
    r2_key          TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    UNIQUE (run_id, job_name, report_kind, report_name, content_sha256)
);
CREATE INDEX idx_shard_merges_run ON shard_merges (run_id, job_name);
