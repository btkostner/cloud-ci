-- docs/design/ai.md § "### Pipeline": the Analysis consumer's budget-cap
-- gate (`B{Budget + settings check<br/>D1 ai_usage_daily}`) and
-- fingerprint cache (`CACHE{D1 ai_insight<br/>same fingerprint?}`).
-- Forward-only, per AGENTS.md: this migration only adds tables, never
-- alters/drops anything 0001-0015 created.
--
-- Scope decision (see `src/ai_queue.rs`'s module doc comment for the full
-- honesty note): this round's consumer only reaches as far as "budget
-- check passed, context built, fingerprinted, truncated-to-budget, ready
-- for the model call" — it never calls `env.AI.run()`. `ai_insight` rows
-- this round are therefore always `status = 'pending_model_call'`; no row
-- this round ever reaches `status IN ('ok', 'skipped_budget',
-- 'invalid_output')`, the other states ai.md's "Cost controls"/"Prompting
-- and output contract" sections describe for a later round's real model
-- call to write.
CREATE TABLE ai_usage_daily (
    repo_id       INTEGER NOT NULL,
    date          TEXT NOT NULL, -- UTC calendar date, "YYYY-MM-DD"
    neuron_count  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (repo_id, date)
);

-- `fingerprint` is ai.md's "Failure fingerprint" (`ai_insight.rs`'s
-- `failure_fingerprint`); `prompt_version` is part of the cache key per
-- ai.md's "Prompting and output contract": "`prompt_version` is part of
-- the cache key and of the stored insight, so a template change never
-- serves stale cached output." `diff_hash` is nullable: this round's
-- context never includes a PR diff (no GitHub PR-diff fetch wired yet —
-- see `src/ai_queue.rs` module docs), so every row this round writes has
-- `diff_hash = NULL`; a future round's cache lookup that does have a diff
-- hash is not satisfied by a NULL-diff_hash row with the same
-- fingerprint, by ordinary SQL NULL-comparison semantics.
CREATE TABLE ai_insight (
    id              TEXT PRIMARY KEY,
    repo_id         INTEGER NOT NULL,
    run_id          TEXT NOT NULL REFERENCES runs (id),
    kind            TEXT NOT NULL, -- 'failure_summary' this round; 'flaky_hint'/'perf_suggestion' are future kinds
    fingerprint     TEXT NOT NULL,
    diff_hash       TEXT,
    prompt_version  TEXT NOT NULL,
    status          TEXT NOT NULL, -- 'pending_model_call' this round; see module docs above
    context_json    TEXT NOT NULL, -- the assembled, truncated context this round built, inspectable by the next round's model-call work
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_ai_insight_fingerprint ON ai_insight (repo_id, kind, fingerprint, diff_hash);
CREATE INDEX idx_ai_insight_run ON ai_insight (run_id);
