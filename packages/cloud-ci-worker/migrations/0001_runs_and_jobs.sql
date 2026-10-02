-- RunCoordinator's D1 projection (ADR 0004: "D1 rows are its projection").
-- Only RunCoordinator ever writes these tables; everything else reads them.

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    repo_id INTEGER NOT NULL,
    sha TEXT NOT NULL,
    run_key TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    status TEXT NOT NULL,
    -- JSON array of job names, NULL until the first non-empty `expect_jobs`
    -- on BeginRun sets it (byo-ci.md: "The first non-empty `expect_jobs`
    -- wins; a later different value is rejected").
    expect_jobs TEXT,
    trigger TEXT NOT NULL,
    external_url TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    UNIQUE (repo_id, sha, run_key, attempt)
);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs (id),
    job_name TEXT NOT NULL,
    shard_total INTEGER NOT NULL,
    runner_label TEXT NOT NULL DEFAULT '',
    -- JSON array of check names.
    check_names TEXT NOT NULL DEFAULT '[]',
    UNIQUE (run_id, job_name)
);
