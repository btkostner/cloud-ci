-- Extends RunCoordinator's D1 projection (ADR 0004: "D1 rows are its
-- projection") with the BYO-CI ingest bookkeeping docs/design/byo-ci.md's
-- "Data model" section describes. Only RunCoordinator ever writes these
-- tables; everything else reads them. Forward-only, per AGENTS.md: this adds
-- tables and columns, never alters or drops anything 0001 created.
--
-- Trimmed from byo-ci.md's full column set for this round's single-part-only
-- upload scope (see packages/cloud-ci-worker/src/coordinator/mod.rs module
-- docs for what's deferred and why):
--   - `uploads.received_parts` (a bitset of parts received, for resuming a
--     multipart upload) is omitted: with no multipart support, an upload
--     either has its one part or it doesn't, so `uploads.state` alone
--     answers "which parts do you have".
--   - No run-level completion columns (webhook correlation, timeout alarms)
--     are added here; `job_shards` and `jobs.state`/`jobs.conclusion` below
--     are everything `CompleteShard`'s job-level completion needs this round.

ALTER TABLE jobs ADD COLUMN state TEXT NOT NULL DEFAULT 'running';
ALTER TABLE jobs ADD COLUMN conclusion TEXT;

CREATE TABLE job_shards (
    job_id TEXT NOT NULL REFERENCES jobs (id),
    shard_index INTEGER NOT NULL,
    -- pending | uploaded | missing
    state TEXT NOT NULL DEFAULT 'pending',
    conclusion TEXT,
    external_url TEXT NOT NULL DEFAULT '',
    completed_at INTEGER,
    PRIMARY KEY (job_id, shard_index)
);

CREATE TABLE uploads (
    id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL REFERENCES jobs (id),
    shard_index INTEGER NOT NULL,
    -- report | artifact | site
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    -- Plain metadata column, not part of the dedupe key below (byo-ci.md's
    -- Idempotency: "`scope` is a plain column on the row, not part of the
    -- key").
    scope TEXT NOT NULL DEFAULT '',
    sha256 TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    content_type TEXT NOT NULL DEFAULT '',
    -- pending | complete
    state TEXT NOT NULL DEFAULT 'pending',
    -- Immutable, content-addressed R2 location, set once and never
    -- rewritten (byo-ci.md's Idempotency section).
    r2_key TEXT NOT NULL,
    -- RunCoordinator's own per-run monotonic counter, assigned at accept
    -- time (byo-ci.md's Idempotency section).
    accepted_seq INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (job_id, shard_index, kind, name, sha256)
);

CREATE TABLE reports (
    id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL REFERENCES jobs (id),
    shard_index INTEGER NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT '',
    content_sha256 TEXT NOT NULL,
    -- NULL for an inline-data report; set for an upload-backed one.
    upload_id TEXT REFERENCES uploads (id),
    accepted_seq INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    -- The row with the highest `accepted_seq` for a (job_id, shard_index,
    -- kind, name) slot is canonical (byo-ci.md's Idempotency section).
    is_canonical INTEGER NOT NULL DEFAULT 1,
    -- 0 when no `cloud-ci-reports` parser exists for `kind`, or parsing the
    -- bytes failed; the raw bytes are still stored and this row still
    -- exists (byo-ci.md's Failure modes: "report attached, unparsed").
    parsed INTEGER NOT NULL DEFAULT 0,
    -- JSON: {"passed":n,"failed":n,"skipped":n,"errored":n,"failed_tests":[{"name":...,"message":...}]}.
    -- NULL when `parsed = 0`.
    summary TEXT,
    UNIQUE (job_id, shard_index, kind, name, content_sha256)
);
