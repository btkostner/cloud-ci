-- Consumer idempotency for `ai_insight` (docs/design/ai-analysis-outbox.md,
-- "The consumer is not idempotent today"). Queue delivery is at-least-once,
-- and the consumer used to INSERT a fresh-ulid row per entry on every
-- delivery, so a redelivery or a retry after a partial insert created
-- duplicate rows and duplicate model calls.
--
-- Idempotency key: (run_id, kind, fingerprint, prompt_version,
-- COALESCE(diff_hash, '')). `diff_hash` is NULL for every row written so
-- far, and SQLite treats NULLs as distinct in a UNIQUE index, so the
-- column is coalesced to '' inside the index expression.
--
-- Forward-only and safe on a live deployment that may already hold
-- duplicates: step 1 deletes duplicates with a deterministic rule, step 2
-- adds the index, so the index creation cannot fail on existing data.
-- Keep rule per key: a row that already left 'pending_model_call' (it holds
-- a model result and spent neurons) beats a pending one; ties go to the
-- oldest `created_at`, then the smallest `id`. Nothing references
-- `ai_insight.id`, so deleting rows is safe.
DELETE FROM ai_insight
WHERE EXISTS (
    SELECT 1 FROM ai_insight AS keep
    WHERE keep.run_id = ai_insight.run_id
      AND keep.kind = ai_insight.kind
      AND keep.fingerprint = ai_insight.fingerprint
      AND keep.prompt_version = ai_insight.prompt_version
      AND COALESCE(keep.diff_hash, '') = COALESCE(ai_insight.diff_hash, '')
      AND keep.id <> ai_insight.id
      AND (
            CASE WHEN keep.status = 'pending_model_call' THEN 1 ELSE 0 END,
            keep.created_at,
            keep.id
          ) < (
            CASE WHEN ai_insight.status = 'pending_model_call' THEN 1 ELSE 0 END,
            ai_insight.created_at,
            ai_insight.id
          )
);

CREATE UNIQUE INDEX idx_ai_insight_idempotency ON ai_insight (
    run_id, kind, fingerprint, prompt_version, COALESCE(diff_hash, '')
);

-- Model-call claim lease: `ai_model_call_pass` sets this (epoch ms) with a
-- conditional UPDATE before calling the model, so two concurrent passes
-- cannot both call the model for one row. NULL = unclaimed.
ALTER TABLE ai_insight ADD COLUMN claimed_at INTEGER;
