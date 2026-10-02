-- RunCoordinator's D1 projection (ADR 0004: "D1 rows are its projection") of
-- the node identity/idempotency rows docs/design/dynamic-pipelines.md's
-- "### Execution model" describes (`startNode`/completion/`ack` —
-- see coordinator::mod's module docs' "Nodes" section for this round's exact
-- scope: no Dynamic Workflow integration, no real container starting, no
-- redelivery timer, just the idempotency state machine). Keyed by
-- `(run_id, node_id)`, matching the design doc's `D1 nodes` row exactly —
-- D1 is shared across runs, unlike the DO's own `node` table, whose primary
-- key is `node_id` alone since one `RunCoordinator` instance *is* one run
-- (`coordinator::ensure_schema`, same "run_id implicit" pattern as
-- `job`/`job_shard`).
CREATE TABLE nodes (
    run_id TEXT NOT NULL REFERENCES runs (id),
    node_id TEXT NOT NULL,
    spec_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    check_name TEXT,
    result TEXT,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    acked INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, node_id)
);
