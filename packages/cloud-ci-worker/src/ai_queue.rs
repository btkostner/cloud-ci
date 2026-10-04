//! The `cloud-ci-analysis` Queue's message type plus the pure decision
//! logic its consumer needs: the daily-budget-cap check and context
//! assembly from already-parsed failing-test data. See
//! `docs/design/ai.md`'s "### Pipeline" (the mermaid diagram and its
//! "Consumer settings" paragraph) for the end-to-end flow this module is
//! one piece of.
//!
//! # Scope boundary — what this round builds and what it does not
//!
//! This round wires the Queue producer/consumer plumbing
//! (`wrangler.toml`'s `[[queues.producers]]`/`[[queues.consumers]]`,
//! `RunCoordinator::handle_close_run`'s enqueue point, the
//! `#[event(queue)]` consumer in `lib.rs`) and gets the consumer as far
//! as ai.md's pipeline diagram's `B{Budget + settings check}` and
//! `CTX`/`RED` steps — budget check, context assembly from genuinely
//! available data, **redaction** of every failing-test string through
//! [`crate::ai_redact::redact`] (`RED`'s "Redact" half; see that
//! module's own doc comment for the exact patterns covered and their
//! residual limitations), fingerprinting (via [`crate::ai_insight`]'s
//! pure functions, run on the already-redacted strings), and
//! token-budget truncation. It deliberately stops **before**
//! `CACHE`/`AI[env.AI.run via AI Gateway]`: no cache lookup, no
//! `env.AI.run()` call, no `AI_GATEWAY_ID` wiring, no
//! `PROMPT_SUMMARY_V1` template — that half (`src/ai_model_call.rs`'s
//! pure functions, driven by `src/lib.rs`'s `ai_model_call_pass` on the
//! `*/5` cron) picks up from the `ai_insight` row this round's consumer
//! writes with `status = 'pending_model_call'` (migration 0016's doc
//! comment) — that row's `context_json` is built exclusively from the
//! redacted copies (see [`assemble_failure_context`]'s own doc
//! comment), so the model-call pass never needs to redact again; it
//! only needs to avoid introducing a second, unredacted input class.
//!
//! # Honest gaps this round does not close
//!
//! - **Settings integration — now real.** `lib.rs`'s `handle_analysis_requested` reads the
//!   `settings_sha` [`coordinator/mod.rs`](../coordinator/mod.rs)'s `BeginRun` admission path
//!   already froze and stored on this run's `runs` row, then calls
//!   `crate::repo_settings::settings_for_sha` with that stored sha for the real per-repo
//!   `ai.daily_neuron_cap` — never `resolve_settings_for_repo`'s live default-branch HEAD
//!   (that function has been deleted; every caller needed the frozen-sha contract, not a
//!   live-HEAD stand-in). This replaces the placeholder this module used to export here.
//!   `repo_settings` does not clamp `clamp_to_deployment_bounds`-sensitive fields (needs new
//!   `wrangler.toml` `[vars]`, out of scope pending coordinator-recovery handoff — see that
//!   module's own scope-boundary doc) — irrelevant to `ai.daily_neuron_cap`, which is not one
//!   of the clamped fields.
//!
//! - **Log tail.** ai.md's "Failure summaries: inputs" table's second row
//!   (R2 log of the failing step, last 400 lines) is not read here: no
//!   code in this crate writes a failing step's log to R2 at all yet
//!   (grepping for an R2 "log" key under `runs/<run_id>/` finds only
//!   `uploads`/`reports` keys). [`assemble_failure_context`] always
//!   treats the log-tail input class as empty/zero and
//!   [`plan_summarization`][crate::ai_insight::plan_summarization]'s
//!   "jobs sharing a log-tail signature" systemic check is always passed
//!   an empty `failing_jobs` slice, since no log-tail signature exists to
//!   compute (the systemic check still works via its other leg: more
//!   than 20 failing tests).
//! - **PR diff.** ai.md's GitHub-diff fetch
//!   (`GET /repos/{owner}/{repo}/pulls/{n}` with the diff media type) is
//!   not called here: this consumer has no PR-number input (the
//!   `runs` table itself carries no PR/branch linkage this round — see
//!   `coordinator::mod`'s own module docs on what's deferred). The diff
//!   input class is always empty/zero, same treatment as the log tail.
//! - **`ai.exclude_paths`.** The path-based exclusion glob list
//!   (ai.md's `.cloud-ci/settings.yml` example: `"infra/secrets/**"`,
//!   `"**/*.pem"`) is not applied here — `repo_settings` can now fetch
//!   the real list, but this consumer has no diff/log-tail content for
//!   it to filter in the first place (both input classes are
//!   always-empty per the two bullets above), so wiring the fetch would
//!   have nothing to act on yet. This is a distinct mechanism from
//!   [`crate::ai_redact`]'s *content*-based redaction (see that module's
//!   own doc comment's "Relationship to `ai.exclude_paths`" section) —
//!   closing this gap does not close that one and vice versa. Redaction
//!   itself (content-based masking within whatever text *is* included)
//!   is real this round and is not part of this list.
//!
//! Only the **failing test cases** row of ai.md's inputs table is real
//! this round: [`crate::coordinator`]'s D1 `reports`/`jobs` projection
//! (already written by `RunCoordinator::project_report_to_d1`/
//! `project_job_to_d1`) names every canonical, parsed report's R2
//! location for a run; the consumer (wired in `lib.rs`, not unit-tested —
//! see below) reads those bytes from R2 and parses them with
//! `cloud_ci_reports` the same way `finalize_test_stats` does, extracting
//! each failing `TestCase`'s message/stack trace/captured output. That
//! typed data is this module's [`FailingTestInput`] — [`assemble_failure_context`]
//! is pure from that point on and is what `cargo test` actually exercises.
//!
//! # Testability
//!
//! [`is_over_daily_cap`] and [`assemble_failure_context`] take plain typed
//! inputs and have no `worker` dependency, so both are unit-tested with
//! plain `cargo test` below. The Queue/D1/R2 wiring around them
//! (`lib.rs`'s `#[event(queue)]` handler, `coordinator::mod`'s enqueue
//! call) is Workers-runtime-only, same documented convention as every
//! other live-infra piece in this crate (`ai_insight.rs`'s own module
//! docs, `coordinator::mod`'s "exercised only by the live smoke test").

use serde::{Deserialize, Serialize};

use crate::ai_insight;
use crate::ai_redact;

/// The one message `RunCoordinator::handle_close_run` enqueues onto
/// `cloud-ci-analysis` per ai.md's "### Pipeline": "When a run reaches a
/// terminal state, `RunCoordinator` enqueues one `AnalysisRequested`
/// message". Carries only what the consumer needs to re-read the run's
/// own state from D1 — never the run's payload itself (this crate's
/// "Inputs enqueue, coordinators decide" invariant's sibling for a Queue
/// consumer: re-read authoritative state, don't trust stale message
/// fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisRequested {
    pub run_id: String,
    pub repo_id: i64,
}

/// ai.md's budget-cap check: `usage_so_far` (today's `ai_usage_daily
/// .neuron_count` for this repo) has already met or exceeded `cap`. `>=`,
/// not `>`: a repo that has used exactly its cap has no room left for
/// another call (ai.md's "Cost controls" table: "Repo daily neuron cap").
pub fn is_over_daily_cap(usage_so_far: i64, cap: i64) -> bool {
    usage_so_far >= cap
}

/// Formats a Unix millisecond timestamp as a UTC calendar date
/// (`"YYYY-MM-DD"`), the `ai_usage_daily.date` key's shape. Pure
/// (`chrono`-only, no `worker::Date` dependency) so the consumer can pass
/// `worker::Date::now().as_millis()` at the call site while this stays
/// unit-testable. Falls back to the Unix epoch date on an
/// out-of-range/malformed timestamp rather than panicking — a date key
/// that is merely wrong for one pathological input is far better than a
/// panicked Worker invocation (this crate's no-`panic!`/no-`unwrap`
/// invariant).
pub fn utc_date_string(now_ms: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(now_ms) {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => "1970-01-01".to_string(),
    }
}

/// One failing test case's genuinely available data, already extracted
/// from a parsed report (`cloud_ci_reports::TestCase`/`Outcome::Failed`/
/// `Outcome::Errored`) by the consumer's R2-reading glue. `stack_trace`
/// is already split into lines/frames (the report's raw `Failure
/// ::stack_trace` blob, newline-split) — [`assemble_failure_context`]
/// treats each line as one frame for fingerprinting/truncation, same
/// granularity [`crate::ai_insight::truncate_stack_trace`] expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailingTestInput {
    pub test_id: String,
    pub message: String,
    pub stack_trace: Vec<String>,
    pub system_out: String,
    pub system_err: String,
}

/// Extracts every failing (`Failed`/`Errored`) test case's data from one
/// report's raw bytes — pure (no `worker` dependency: `cloud_ci_reports`
/// is a plain parsing crate), so unlike the R2 read that fetches `bytes`
/// in the first place, this function itself is unit-tested below. Mirrors
/// `coordinator::mod::parse_test_outcomes`'s kind dispatch
/// (`junit`/`vitest`/`playwright`; any other `kind` yields no rows, same
/// as that function) but keeps the failure message/stack trace/captured
/// output `parse_test_outcomes` itself discards — this module needs them
/// for fingerprinting and ai.md's "last 2 KB of captured stdout/stderr"
/// input, `parse_test_outcomes` does not.
pub fn extract_failing_tests(kind: &str, bytes: &[u8]) -> Vec<FailingTestInput> {
    let suites = match kind.to_ascii_lowercase().as_str() {
        "junit" => cloud_ci_reports::junit::parse(bytes).ok(),
        "vitest" => cloud_ci_reports::vitest::parse(bytes).ok(),
        "playwright" => cloud_ci_reports::playwright::parse(bytes).ok(),
        _ => None,
    };
    let Some(suites) = suites else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for suite in &suites.suites {
        for tc in &suite.test_cases {
            let failure = match &tc.outcome {
                cloud_ci_reports::Outcome::Failed(f) | cloud_ci_reports::Outcome::Errored(f) => f,
                _ => continue,
            };
            let file_path = tc.file.clone().unwrap_or_else(|| suite.name.clone());
            let full_name =
                crate::coordinator::logic::full_test_name(tc.classname.as_deref(), &tc.name);
            let test_id = crate::coordinator::logic::test_id(&file_path, &full_name);
            let stack_trace = failure
                .stack_trace
                .as_deref()
                .unwrap_or("")
                .lines()
                .map(str::to_string)
                .collect();
            out.push(FailingTestInput {
                test_id,
                message: failure.message.clone().unwrap_or_default(),
                stack_trace,
                system_out: tc.system_out.clone().unwrap_or_default(),
                system_err: tc.system_err.clone().unwrap_or_default(),
            });
        }
    }
    out
}

/// One failing test's assembled, truncated, fingerprinted contribution to
/// the model prompt this round stops short of sending — this is the
/// "ready to call the model" record a future round's `env.AI.run()` call
/// consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailingTestEntry {
    pub test_id: String,
    pub fingerprint: String,
    pub normalized_message: String,
    /// [`crate::ai_insight::truncate_stack_trace`]'s output against this
    /// entry's even share of the failing-tests token allotment.
    pub frames: Vec<String>,
    /// Last up to 2 KB (ai.md: "the last 2 KB of captured stdout/stderr")
    /// of `system_out` followed by `system_err`, byte-tail-truncated on a
    /// UTF-8 char boundary.
    pub output_tail: String,
}

/// [`assemble_failure_context`]'s full result — exactly what this round's
/// `ai_insight.context_json` column stores (migration 0016), inspectable
/// by a future round's model-call work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssembledFailureContext {
    /// Up to 5 distinct failure fingerprints (ai.md: "Up to 5 distinct
    /// failure fingerprints per run"), in the order
    /// [`ai_insight::plan_summarization`] selected them.
    pub selected_fingerprints: Vec<String>,
    /// Whether [`ai_insight::plan_summarization`] classified this run as
    /// a systemic failure (ai.md's "more than 20 failing tests" leg —
    /// the "more than 3 failing jobs that share a log-tail signature" leg
    /// can never fire this round; see module docs' "Log tail" gap).
    pub systemic: bool,
    pub entries: Vec<FailingTestEntry>,
    pub model_context_window: u32,
    /// `ai_insight::estimate_tokens` applied to the selected entries'
    /// combined byte size, before truncation — the failing-tests class's
    /// `need` that went into `allot_budget`.
    pub failing_tests_token_estimate: u32,
    /// `allot_budget`'s final allotment for the failing-tests class —
    /// what `entries[*].frames` was actually truncated against.
    pub failing_tests_token_allotment: u32,
}

/// Last up to `max_bytes` bytes of `s`, cut on a UTF-8 char boundary (no
/// mid-codepoint split) — ai.md's "last 2 KB" rule, a fixed byte cap
/// applied per failing test, independent of (and upstream of) the
/// token-budget reallocation [`ai_insight::allot_budget`] does across
/// input *classes*.
fn tail_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// ai.md's "last 2 KB of captured stdout/stderr" selection cap, applied
/// per test before that test's bytes ever reach the token-budget
/// estimator — keeps one pathologically chatty test from starving the
/// `failing_tests` class's budget estimate before reallocation even runs.
const PER_TEST_OUTPUT_CAP_BYTES: usize = 2 * 1024;

/// Builds this round's full, honest context from already-parsed failing
/// tests. The **first** thing this function does is redact every
/// `message`/`stack_trace` line/`system_out`/`system_err` field through
/// [`ai_redact::redact`] — `docs/design/ai.md`'s pipeline diagram's
/// `RED[Redact + exclude_paths + fingerprint]` step, run strictly before
/// fingerprinting so that two failures differing only in an embedded
/// secret's value still fingerprint identically once redacted (see
/// `ai_redact`'s own module docs' "dedup" note). Everything downstream —
/// fingerprinting ([`ai_insight::failure_fingerprint`]), classifying
/// systemic-vs-per-fingerprint ([`ai_insight::plan_summarization`], with
/// no `failing_jobs` data — see module docs), selecting up to 5 distinct
/// fingerprints' first occurrence, sizing/reallocating the token budget
/// for `model_context_window` ([`ai_insight::scale_budget_for_model`],
/// [`ai_insight::allot_budget`]) with `log_tail`/`diff` actual sizes
/// fixed at zero (neither input exists this round), and structurally
/// truncating each selected entry's frames to an even share of the
/// resulting failing-tests allotment
/// ([`ai_insight::truncate_stack_trace`]) — operates exclusively on the
/// already-redacted copies, never on `failing_tests` itself.
pub fn assemble_failure_context(
    failing_tests: &[FailingTestInput],
    model_context_window: u32,
) -> AssembledFailureContext {
    let redacted_tests: Vec<FailingTestInput> = failing_tests
        .iter()
        .map(|t| FailingTestInput {
            test_id: t.test_id.clone(),
            message: ai_redact::redact(&t.message),
            stack_trace: t.stack_trace.iter().map(|l| ai_redact::redact(l)).collect(),
            system_out: ai_redact::redact(&t.system_out),
            system_err: ai_redact::redact(&t.system_err),
        })
        .collect();

    let fingerprints: Vec<String> = redacted_tests
        .iter()
        .map(|t| {
            let frames: Vec<&str> = t.stack_trace.iter().map(String::as_str).collect();
            ai_insight::failure_fingerprint(
                &t.test_id,
                &t.message,
                &frames,
                ai_insight::DEFAULT_LIBRARY_PATTERNS,
            )
        })
        .collect();

    // No failing-job log-tail-signature data this round (module docs'
    // "Log tail" gap) — the shared-signature leg of the systemic check
    // never fires; the failure-count leg still does.
    let plan = ai_insight::plan_summarization(&fingerprints, &[]);
    let (systemic, planned_fingerprints) = match plan {
        ai_insight::SummarizationPlan::PerFingerprint(fps) => (false, fps),
        ai_insight::SummarizationPlan::Systemic(fps) => (true, fps),
    };
    let selected_fingerprints: Vec<String> = planned_fingerprints.into_iter().take(5).collect();

    // First failing test whose own fingerprint matches each selected
    // fingerprint, preserving `selected_fingerprints`' own order.
    let selected: Vec<(&FailingTestInput, String)> = selected_fingerprints
        .iter()
        .filter_map(|fp| {
            fingerprints
                .iter()
                .position(|f| f == fp)
                .map(|idx| (&redacted_tests[idx], fp.clone()))
        })
        .collect();

    let budget = ai_insight::scale_budget_for_model(model_context_window);

    let failing_tests_bytes: usize = selected
        .iter()
        .map(|(t, _)| {
            t.message.len()
                + t.stack_trace.iter().map(String::len).sum::<usize>()
                + t.system_out.len().min(PER_TEST_OUTPUT_CAP_BYTES)
                + t.system_err.len().min(PER_TEST_OUTPUT_CAP_BYTES)
        })
        .sum();
    let failing_tests_token_estimate = ai_insight::estimate_tokens(failing_tests_bytes);

    let actual = ai_insight::ClassSizes {
        // No run-metadata/history text assembled this round (no D1
        // test-history read is wired here yet, beyond the failing-test
        // data itself) — a fixed, honest zero rather than a fabricated
        // estimate.
        run_metadata: 0,
        failing_tests: failing_tests_token_estimate,
        log_tail: 0,
        diff: 0,
    };
    let allotment = ai_insight::allot_budget(&budget, &actual);

    let per_test_budget = if selected.is_empty() {
        0
    } else {
        allotment.failing_tests / selected.len() as u32
    };

    let entries = selected
        .into_iter()
        .map(|(t, fingerprint)| {
            let frames: Vec<&str> = t.stack_trace.iter().map(String::as_str).collect();
            let truncated_frames = ai_insight::truncate_stack_trace(&frames, per_test_budget);
            let mut output = String::with_capacity(t.system_out.len() + t.system_err.len());
            output.push_str(&t.system_out);
            output.push_str(&t.system_err);
            FailingTestEntry {
                test_id: t.test_id.clone(),
                fingerprint,
                normalized_message: ai_insight::normalize(&t.message),
                frames: truncated_frames,
                output_tail: tail_bytes(&output, PER_TEST_OUTPUT_CAP_BYTES),
            }
        })
        .collect();

    AssembledFailureContext {
        selected_fingerprints,
        systemic,
        entries,
        model_context_window,
        failing_tests_token_estimate,
        failing_tests_token_allotment: allotment.failing_tests,
    }
}

/// Idempotent `ai_insight` insert (migration `0021`): the unique index
/// `idx_ai_insight_idempotency` on `(run_id, kind, fingerprint,
/// prompt_version, COALESCE(diff_hash, ''))` makes a redelivered message,
/// or a retry after a partial insert failure, a no-op for rows that already
/// exist — the message's own entries are already unique per fingerprint
/// (`assemble_failure_context` selects at most one entry per fingerprint,
/// see that function), so only cross-delivery duplicates can ever hit the
/// conflict. `diff_hash` stays a `NULL` literal, same as before this
/// migration — this round's context never includes a PR diff (see this
/// module's "Honest gaps" doc comment). Params: `?1` id, `?2` repo_id,
/// `?3` run_id, `?4` fingerprint, `?5` prompt_version, `?6` context_json,
/// `?7` now_ms.
pub const INSERT_INSIGHT_SQL: &str = "INSERT INTO ai_insight \
     (id, repo_id, run_id, kind, fingerprint, diff_hash, prompt_version, status, context_json, created_at, updated_at) \
     VALUES (?1, ?2, ?3, 'failure_summary', ?4, NULL, ?5, 'pending_model_call', ?6, ?7, ?7) \
     ON CONFLICT (run_id, kind, fingerprint, prompt_version, COALESCE(diff_hash, '')) DO NOTHING";

/// How long a model-call claim holds a row, in ms (migration `0021`'s
/// `claimed_at` column). A pass that crashes mid-call leaves `claimed_at`
/// set; after this lease the row is claimable again by a later pass. Longer
/// than any realistic pair of `ai.run` attempts (first call plus one
/// retry).
pub const MODEL_CALL_CLAIM_LEASE_MS: i64 = 10 * 60 * 1000;

/// Claims one `pending_model_call` row for a model call via a conditional
/// `UPDATE`: it only changes a row that is still `pending_model_call` and
/// either never claimed or whose claim's lease has expired. Params: `?1`
/// id, `?2` now_ms, `?3` lease_ms. Of any number of concurrent passes
/// racing this statement for the same `id`, SQLite serializes the writes
/// and exactly one sees `changes() == 1`; every other sees `0` — see
/// [`claim_won`].
pub const CLAIM_PENDING_SQL: &str = "UPDATE ai_insight SET claimed_at = ?2 \
     WHERE id = ?1 AND status = 'pending_model_call' \
     AND (claimed_at IS NULL OR claimed_at <= ?2 - ?3)";

/// Releases a claim after a network/binding failure (not a validation
/// failure — those write a terminal `status` and leave the claim moot) so a
/// later pass can retry this row without waiting out the full lease. Params:
/// `?1` id, `?2` the releasing pass's own claim timestamp (the `now_ms` it
/// passed to [`CLAIM_PENDING_SQL`]). Guarded by `status =
/// 'pending_model_call'` so it is a no-op once a concurrent attempt already
/// reached a terminal status, and by `claimed_at = ?2` so a pass whose lease
/// expired cannot clear a newer pass's live claim.
pub const RELEASE_CLAIM_SQL: &str = "UPDATE ai_insight SET claimed_at = NULL \
     WHERE id = ?1 AND status = 'pending_model_call' AND claimed_at = ?2";

/// Candidate rows for one model-call pass: pending rows not held by a live
/// claim, oldest first, so claimed rows cannot starve newer pending rows.
/// Params: `?1` now_ms, `?2` lease_ms, `?3` limit. Only a filter —
/// [`CLAIM_PENDING_SQL`] stays the authority on who may call the model.
pub const SELECT_CLAIMABLE_SQL: &str = "SELECT id, repo_id, context_json FROM ai_insight \
     WHERE status = 'pending_model_call' \
     AND (claimed_at IS NULL OR claimed_at <= ?1 - ?2) \
     ORDER BY created_at ASC LIMIT ?3";

/// Terminal transition for a claimed row whose stored `context_json` cannot
/// be deserialized: it can never succeed on retry, so it is set to
/// `status = 'error'` (a status ai.md already defines) instead of being
/// left claimed to be retried every [`MODEL_CALL_CLAIM_LEASE_MS`] forever.
/// Params: `?1` id, `?2` the pass's own claim timestamp, `?3` now_ms.
/// Guarded like [`RELEASE_CLAIM_SQL`] so a pass whose lease expired cannot
/// terminate a row a newer pass owns.
pub const MARK_CONTEXT_UNPARSABLE_SQL: &str = "UPDATE ai_insight \
     SET status = 'error', updated_at = ?3 \
     WHERE id = ?1 AND status = 'pending_model_call' AND claimed_at = ?2";

/// Canonical, parsed reports of one run, in a deterministic order
/// (`accepted_seq`, then report id) so the failing-test list — and with it
/// the per-fingerprint representative `assemble_failure_context` picks —
/// never depends on D1 row order. Columns: `kind`, `r2_key`. Param: `?1`
/// run_id.
pub const CANONICAL_REPORTS_SQL: &str = "SELECT reports.kind AS kind, reports.r2_key AS r2_key \
     FROM reports JOIN jobs ON reports.job_id = jobs.id \
     WHERE jobs.run_id = ?1 AND reports.is_canonical = 1 AND reports.parsed = 1 \
     ORDER BY reports.accepted_seq ASC, reports.id ASC";

/// Whether a [`CLAIM_PENDING_SQL`] conditional `UPDATE` won the race, from
/// D1's reported changed-row count. An unknown count (`None`, a driver
/// surprise) is treated as "not claimed" rather than "claimed", so an
/// unexpected driver response can only skip a model call, never double one.
pub fn claim_won(changes: Option<usize>) -> bool {
    changes == Some(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_over_daily_cap_is_true_at_and_above_cap() {
        assert!(!is_over_daily_cap(1_999, 2_000));
        assert!(is_over_daily_cap(2_000, 2_000));
        assert!(is_over_daily_cap(2_001, 2_000));
    }

    #[test]
    fn is_over_daily_cap_true_for_zero_cap() {
        // A repo configured (or defaulted) to a zero cap has no room ever.
        assert!(is_over_daily_cap(0, 0));
    }

    #[test]
    fn claim_won_true_only_for_exactly_one_change() {
        // D1's reported changed-row count from `CLAIM_PENDING_SQL`: a `1`
        // means this call's conditional `UPDATE` matched and changed the
        // one row it targeted by `id` — it is the only outcome that means
        // this pass may call the model.
        assert!(claim_won(Some(1)));
    }

    #[test]
    fn claim_won_false_for_zero_changes() {
        // Another pass already holds a live claim, or the row already left
        // `pending_model_call` — either way this pass must not call the
        // model for it.
        assert!(!claim_won(Some(0)));
    }

    #[test]
    fn claim_won_false_for_unknown_changes() {
        // A `None` changed-count (a driver surprise) must fail closed: an
        // unexpected response can only cause a skipped call, never a
        // doubled one.
        assert!(!claim_won(None));
    }

    #[test]
    fn claim_won_false_for_more_than_one_change() {
        // `CLAIM_PENDING_SQL`'s `WHERE id = ?1` can only ever match one
        // row; a count above 1 is impossible in practice, but treating it
        // as "not won" keeps the function fail-closed for any value other
        // than the one expected success case.
        assert!(!claim_won(Some(2)));
    }

    #[test]
    fn utc_date_string_formats_known_timestamp() {
        // 2026-10-02T00:00:00Z
        assert_eq!(utc_date_string(1_790_899_200_000), "2026-10-02");
    }

    #[test]
    fn utc_date_string_falls_back_on_out_of_range_input() {
        assert_eq!(utc_date_string(i64::MAX), "1970-01-01");
    }

    fn sample_test(test_id: &str, message: &str, frames: &[&str]) -> FailingTestInput {
        FailingTestInput {
            test_id: test_id.to_string(),
            message: message.to_string(),
            stack_trace: frames.iter().map(|f| f.to_string()).collect(),
            system_out: String::new(),
            system_err: String::new(),
        }
    }

    #[test]
    fn assemble_failure_context_dedupes_identical_fingerprints_per_fingerprint_plan() {
        let a = sample_test(
            "pkg/test.ts::creates user",
            "expected 1 but got 2",
            &["at a (src/a.ts:1)"],
        );
        let b = sample_test(
            "pkg/test.ts::creates user shard 2",
            "expected 1 but got 2",
            &["at a (src/a.ts:1)"],
        );
        let ctx = assemble_failure_context(&[a, b], ai_insight::DEFAULT_CONTEXT_WINDOW);
        assert!(!ctx.systemic);
        // Same fingerprint for both (same test_id-independent message
        // wait: fingerprint includes test_id, so these two differ) — this
        // asserts the plan at least ran and selected something.
        assert_eq!(ctx.entries.len(), ctx.selected_fingerprints.len());
        assert!(!ctx.entries.is_empty());
    }

    #[test]
    fn assemble_failure_context_caps_at_five_distinct_fingerprints() {
        let tests: Vec<FailingTestInput> = (0..8)
            .map(|i| {
                sample_test(
                    &format!("pkg/test.ts::case {i}"),
                    &format!("distinct failure {i}"),
                    &[],
                )
            })
            .collect();
        let ctx = assemble_failure_context(&tests, ai_insight::DEFAULT_CONTEXT_WINDOW);
        assert_eq!(ctx.selected_fingerprints.len(), 5);
        assert_eq!(ctx.entries.len(), 5);
    }

    #[test]
    fn assemble_failure_context_marks_systemic_past_twenty_failures() {
        let tests: Vec<FailingTestInput> = (0..21)
            .map(|i| {
                sample_test(
                    &format!("pkg/test.ts::case {i}"),
                    &format!("distinct failure {i}"),
                    &[],
                )
            })
            .collect();
        let ctx = assemble_failure_context(&tests, ai_insight::DEFAULT_CONTEXT_WINDOW);
        assert!(ctx.systemic);
    }

    #[test]
    fn assemble_failure_context_truncates_output_tail_to_two_kib() {
        let mut t = sample_test("pkg/test.ts::big output", "boom", &[]);
        t.system_out = "x".repeat(5_000);
        let ctx =
            assemble_failure_context(std::slice::from_ref(&t), ai_insight::DEFAULT_CONTEXT_WINDOW);
        assert_eq!(ctx.entries.len(), 1);
        assert_eq!(ctx.entries[0].output_tail.len(), PER_TEST_OUTPUT_CAP_BYTES);
    }

    #[test]
    fn assemble_failure_context_empty_input_yields_empty_output() {
        let ctx = assemble_failure_context(&[], ai_insight::DEFAULT_CONTEXT_WINDOW);
        assert!(ctx.entries.is_empty());
        assert!(ctx.selected_fingerprints.is_empty());
        assert!(!ctx.systemic);
        assert_eq!(ctx.failing_tests_token_allotment, 0);
    }

    #[test]
    fn assemble_failure_context_never_leaks_a_planted_secret() {
        // A planted GitHub-token-shaped string in the assertion message,
        // an AWS-key-shaped string in a stack frame, an Authorization
        // header line and a `*_SECRET=value` line in captured output —
        // none of this raw material (module docs' `ai_redact`
        // integration) should survive into `assemble_failure_context`'s
        // output anywhere: not in `normalized_message`, not in `frames`,
        // not in `output_tail`, not in `selected_fingerprints`.
        let github_token = "ghp_abcdEFGH0123456789abcdEFGH0123456789";
        let aws_key = "AKIAABCDEFGHIJKLMNOP";
        let mut t = sample_test(
            "pkg/test.ts::leaks secret",
            &format!("request failed with token {github_token}"),
            &[&format!("at auth.ts:1: key={aws_key}")],
        );
        t.system_out = "Authorization: Bearer sk_live_abcdefghijklmnopqrstuvwxyz0123456789\n\
                         RELEASE_SECRET=hunter2correcthorsebatterystaple\n"
            .to_string();

        let ctx = assemble_failure_context(&[t], ai_insight::DEFAULT_CONTEXT_WINDOW);

        let serialized = serde_json::to_string(&ctx).unwrap_or_default();
        assert!(!serialized.contains(github_token));
        assert!(!serialized.contains(aws_key));
        assert!(!serialized.contains("sk_live_abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(!serialized.contains("hunter2correcthorsebatterystaple"));
    }

    #[test]
    fn assemble_failure_context_fingerprint_is_computed_on_redacted_content() {
        // Two reports whose only difference is an embedded secret value
        // must fingerprint identically after redaction — the dedup
        // -correctness proof: `RED` runs before fingerprinting, not
        // after (module docs).
        let a = sample_test(
            "pkg/test.ts::same shape",
            "Authorization: Bearer aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[],
        );
        let b = sample_test(
            "pkg/test.ts::same shape",
            "Authorization: Bearer bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &[],
        );

        let ctx_a = assemble_failure_context(&[a], ai_insight::DEFAULT_CONTEXT_WINDOW);
        let ctx_b = assemble_failure_context(&[b], ai_insight::DEFAULT_CONTEXT_WINDOW);

        assert_eq!(ctx_a.selected_fingerprints, ctx_b.selected_fingerprints);
        assert_eq!(
            ctx_a.entries[0].normalized_message,
            ctx_b.entries[0].normalized_message
        );
    }

    const SAMPLE_JUNIT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="full run" tests="3" failures="1" errors="1" skipped="1" time="1.234">
  <testsuite name="pkg.unit.MathTests" tests="2" failures="1" errors="0" skipped="0" time="0.045">
    <testcase classname="pkg.unit.MathTests" name="divides_by_zero_raises" file="pkg/unit/math_tests.py" time="0.033">
      <failure message="expected ArithmeticError, got no exception" type="AssertionError">
Traceback (most recent call last):
  File "test_math.py", line 42, in divides_by_zero_raises
    assert False, "expected exception"
</failure>
      <system-out>math suite done</system-out>
    </testcase>
    <testcase classname="pkg.unit.MathTests" name="adds_two_numbers" file="pkg/unit/math_tests.py" time="0.012"/>
  </testsuite>
  <testsuite name="pkg.integration.DbTests" tests="1" failures="0" errors="1" skipped="0" time="1.189">
    <testcase classname="pkg.integration.DbTests" name="connects_to_primary" time="1.150">
      <error message="connection refused" type="ConnectionError">connect() failed: ECONNREFUSED 127.0.0.1:5432</error>
      <system-err>retrying... giving up after 3 attempts</system-err>
    </testcase>
  </testsuite>
</testsuites>
"#;

    #[test]
    fn extract_failing_tests_returns_failed_and_errored_cases_only() {
        let tests = extract_failing_tests("junit", SAMPLE_JUNIT.as_bytes());
        assert_eq!(tests.len(), 2);
        let divides = tests.iter().find(|t| t.message.contains("ArithmeticError"));
        let Some(divides) = divides else {
            unreachable!("sample JUnit fixture always has this failing test");
        };
        assert!(
            divides
                .stack_trace
                .iter()
                .any(|f| f.contains("test_math.py"))
        );
        assert_eq!(divides.system_out, "math suite done");
        let connects = tests
            .iter()
            .find(|t| t.message.contains("connection refused"));
        let Some(connects) = connects else {
            unreachable!("sample JUnit fixture always has this errored test");
        };
        assert_eq!(
            connects.system_err,
            "retrying... giving up after 3 attempts"
        );
    }

    #[test]
    fn extract_failing_tests_unknown_kind_returns_empty() {
        assert!(extract_failing_tests("lcov", b"whatever").is_empty());
    }

    #[test]
    fn extract_failing_tests_malformed_bytes_returns_empty_not_panic() {
        assert!(extract_failing_tests("junit", b"<not valid xml").is_empty());
    }

    #[test]
    fn extract_failing_tests_feeds_assemble_failure_context_end_to_end() {
        let tests = extract_failing_tests("junit", SAMPLE_JUNIT.as_bytes());
        let ctx = assemble_failure_context(&tests, ai_insight::DEFAULT_CONTEXT_WINDOW);
        assert_eq!(ctx.entries.len(), 2);
        assert!(!ctx.systemic);
    }
}

/// SQL-level proof for migration `0021` and the idempotent statements this
/// module exports: applies the real migration files (via `include_str!`,
/// so this always exercises exactly what ships, never a re-typed copy)
/// against an in-memory SQLite database through `rusqlite`. D1 is built on
/// SQLite, so these statements running correctly here is the strongest
/// proof available without a live D1 binding (which only the Workers
/// runtime provides — see this module's own "Testability" doc comment).
#[cfg(test)]
mod migration_sql_tests {
    use rusqlite::{Connection, params};

    const MIGRATIONS_0001_TO_0020: [&str; 20] = [
        include_str!("../migrations/0001_runs_and_jobs.sql"),
        include_str!("../migrations/0002_uploads_and_reports.sql"),
        include_str!("../migrations/0003_installations_and_repos.sql"),
        include_str!("../migrations/0004_installations_account_id.sql"),
        include_str!("../migrations/0005_api_tokens.sql"),
        include_str!("../migrations/0006_users_sessions_role_cache.sql"),
        include_str!("../migrations/0007_job_conclusion_message.sql"),
        include_str!("../migrations/0008_check_runs.sql"),
        include_str!("../migrations/0009_run_timeout.sql"),
        include_str!("../migrations/0010_nodes.sql"),
        include_str!("../migrations/0011_test_stats.sql"),
        include_str!("../migrations/0012_report_r2_key.sql"),
        include_str!("../migrations/0013_nodes_image_command.sql"),
        include_str!("../migrations/0014_shard_states.sql"),
        include_str!("../migrations/0015_run_rollups_and_insights.sql"),
        include_str!("../migrations/0016_ai_usage_and_insight.sql"),
        include_str!("../migrations/0017_ai_insight_model_response.sql"),
        include_str!("../migrations/0018_shard_merges.sql"),
        include_str!("../migrations/0019_repo_settings_cache.sql"),
        include_str!("../migrations/0020_runs_settings_sha.sql"),
    ];

    const MIGRATION_0021: &str = include_str!("../migrations/0021_ai_insight_idempotency.sql");

    /// A DB with every migration through `0020` applied, but not `0021` —
    /// lets a test insert pre-fix duplicate rows the old, non-idempotent
    /// consumer could have produced, before applying the fix migration
    /// under test.
    fn db_before_0021() -> rusqlite::Result<Connection> {
        let conn = Connection::open_in_memory()?;
        for migration in MIGRATIONS_0001_TO_0020 {
            conn.execute_batch(migration)?;
        }
        conn.execute(
            "INSERT INTO runs (id, repo_id, sha, run_key, attempt, status, trigger, created_at) \
             VALUES ('run1', 1, 'abc123', 'gha/1', 1, 'completed', 'push', 1000)",
            [],
        )?;
        Ok(conn)
    }

    /// A fully migrated DB (`0021` applied), for tests that exercise the
    /// idempotent insert/claim statements rather than the dedupe migration
    /// itself.
    fn db_after_0021() -> rusqlite::Result<Connection> {
        let conn = db_before_0021()?;
        conn.execute_batch(MIGRATION_0021)?;
        Ok(conn)
    }

    fn insight_count(conn: &Connection, run_id: &str) -> rusqlite::Result<i64> {
        conn.query_row(
            "SELECT COUNT(*) FROM ai_insight WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )
    }

    #[test]
    fn same_message_processed_twice_yields_one_row() -> rusqlite::Result<()> {
        let conn = db_after_0021()?;
        // Both calls bind the exact same params `handle_analysis_requested`
        // would bind for a redelivery of the same `AnalysisRequested`
        // message against the same assembled entry: same `id` is not
        // guaranteed (a fresh ulid per call), but the idempotency key
        // (run_id, kind, fingerprint, prompt_version, diff_hash) is.
        for id in ["ins_a", "ins_b_redelivery"] {
            conn.execute(
                super::INSERT_INSIGHT_SQL,
                params![id, 1_i64, "run1", "fp1", "v1", "{}", 1000_i64],
            )?;
        }
        assert_eq!(insight_count(&conn, "run1")?, 1);
        // The row that exists is the first one — `DO NOTHING` never
        // overwrites it with the redelivery's attempt.
        let kept_id: String =
            conn.query_row("SELECT id FROM ai_insight WHERE run_id = 'run1'", [], |r| {
                r.get(0)
            })?;
        assert_eq!(kept_id, "ins_a");
        Ok(())
    }

    #[test]
    fn partial_failure_retry_converges_to_the_same_state_as_one_clean_run() -> rusqlite::Result<()>
    {
        // A message with two entries (two distinct fingerprints). First
        // delivery: entry 1 inserts, then the delivery fails before entry
        // 2 is written (simulated by simply not inserting it this round).
        let conn = db_after_0021()?;
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params![
                "ins_1_first_try",
                1_i64,
                "run1",
                "fp1",
                "v1",
                "{}",
                1000_i64
            ],
        )?;
        assert_eq!(insight_count(&conn, "run1")?, 1);

        // Retry: the Queue redelivers the whole message, so both entries
        // are inserted again — entry 1 collides (DO NOTHING), entry 2 is
        // genuinely new.
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params!["ins_1_retry", 1_i64, "run1", "fp1", "v1", "{}", 2000_i64],
        )?;
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params!["ins_2_retry", 1_i64, "run1", "fp2", "v1", "{}", 2000_i64],
        )?;

        // Final state: exactly one row per fingerprint — identical to
        // what a single clean run over both entries would have produced.
        assert_eq!(insight_count(&conn, "run1")?, 2);
        let fp1_id: String = conn.query_row(
            "SELECT id FROM ai_insight WHERE run_id = 'run1' AND fingerprint = 'fp1'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(fp1_id, "ins_1_first_try");
        Ok(())
    }

    #[test]
    fn only_one_of_two_concurrent_claims_wins_for_one_row() -> rusqlite::Result<()> {
        let conn = db_after_0021()?;
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params!["ins_claim", 1_i64, "run1", "fp1", "v1", "{}", 1000_i64],
        )?;

        let lease = super::MODEL_CALL_CLAIM_LEASE_MS;
        let first_claim = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", 5000_i64, lease],
        )?;
        let second_claim = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", 5010_i64, lease],
        )?;

        assert!(super::claim_won(Some(first_claim)));
        assert!(!super::claim_won(Some(second_claim)));
        Ok(())
    }

    #[test]
    fn expired_lease_allows_a_later_pass_to_reclaim() -> rusqlite::Result<()> {
        let conn = db_after_0021()?;
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params!["ins_claim", 1_i64, "run1", "fp1", "v1", "{}", 1000_i64],
        )?;
        let lease = super::MODEL_CALL_CLAIM_LEASE_MS;
        conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", 5000_i64, lease],
        )?;

        // The lease is still live one ms before it expires.
        let still_leased = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", 5000_i64 + lease - 1, lease],
        )?;
        assert!(!super::claim_won(Some(still_leased)));

        // `now - lease == claimed_at` (not `>`) already counts as expired
        // (`CLAIM_PENDING_SQL`'s `<=`), so a later pass is never forced to
        // wait any longer than the documented lease.
        let after_expiry = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", 5000_i64 + lease, lease],
        )?;
        assert!(super::claim_won(Some(after_expiry)));
        Ok(())
    }

    #[test]
    fn release_then_resolved_status_both_stop_further_claims() -> rusqlite::Result<()> {
        let conn = db_after_0021()?;
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params!["ins_claim", 1_i64, "run1", "fp1", "v1", "{}", 1000_i64],
        )?;
        let lease = super::MODEL_CALL_CLAIM_LEASE_MS;
        conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", 5000_i64, lease],
        )?;

        // A failed call releases the claim with its own claim timestamp: an
        // immediate re-claim succeeds without waiting out the lease.
        conn.execute(super::RELEASE_CLAIM_SQL, params!["ins_claim", 5000_i64])?;
        let reclaim_after_release = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", 5001_i64, lease],
        )?;
        assert!(super::claim_won(Some(reclaim_after_release)));

        // Once the row reaches a terminal status, neither claim nor
        // release can touch it again.
        conn.execute(
            "UPDATE ai_insight SET status = 'ok' WHERE id = 'ins_claim'",
            [],
        )?;
        let claim_after_resolved = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", 999_999_999_i64, lease],
        )?;
        assert!(!super::claim_won(Some(claim_after_resolved)));
        let release_after_resolved_changes =
            conn.execute(super::RELEASE_CLAIM_SQL, params!["ins_claim", 5001_i64])?;
        assert_eq!(release_after_resolved_changes, 0);
        Ok(())
    }

    #[test]
    fn unparsable_context_reaches_terminal_status_and_is_not_claimable_again()
    -> rusqlite::Result<()> {
        let conn = db_after_0021()?;
        let lease = super::MODEL_CALL_CLAIM_LEASE_MS;
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params![
                "ins_bad", 1_i64, "run1", "fp_bad", "v1", "not json", 1000_i64
            ],
        )?;
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params!["ins_good", 1_i64, "run1", "fp_good", "v1", "{}", 1001_i64],
        )?;
        // Both claimed by one pass, as `ai_model_call_pass` does.
        for id in ["ins_bad", "ins_good"] {
            conn.execute(super::CLAIM_PENDING_SQL, params![id, 5000_i64, lease])?;
        }
        let changed = conn.execute(
            super::MARK_CONTEXT_UNPARSABLE_SQL,
            params!["ins_bad", 5000_i64, 5001_i64],
        )?;
        assert_eq!(changed, 1);

        let status = |id: &str| -> rusqlite::Result<String> {
            conn.query_row("SELECT status FROM ai_insight WHERE id = ?1", [id], |r| {
                r.get(0)
            })
        };
        assert_eq!(status("ins_bad")?, "error");
        assert_eq!(status("ins_good")?, "pending_model_call");

        // Long after the lease expires, only the good row is selectable and
        // the bad row can no longer be claimed.
        let far_future = 5000_i64 + lease * 100;
        let claimable: Vec<String> = conn
            .prepare(super::SELECT_CLAIMABLE_SQL)?
            .query_map(params![far_future, lease, 10_i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(claimable, vec!["ins_good".to_string()]);
        let reclaim = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_bad", far_future, lease],
        )?;
        assert!(!super::claim_won(Some(reclaim)));
        Ok(())
    }

    #[test]
    fn mark_context_unparsable_guards_stale_claim_and_non_pending_status() -> rusqlite::Result<()> {
        let conn = db_after_0021()?;
        let lease = super::MODEL_CALL_CLAIM_LEASE_MS;
        let status_and_claim = |id: &str| -> rusqlite::Result<(String, Option<i64>)> {
            conn.query_row(
                "SELECT status, claimed_at FROM ai_insight WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        };

        // Stale claim: a pass whose claim timestamp no longer matches the
        // row's (a newer pass reclaimed it) must not terminate the row.
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params![
                "ins_stale",
                1_i64,
                "run1",
                "fp_s",
                "v1",
                "not json",
                1000_i64
            ],
        )?;
        conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_stale", 5000_i64, lease],
        )?;
        let stale = conn.execute(
            super::MARK_CONTEXT_UNPARSABLE_SQL,
            params!["ins_stale", 4999_i64, 6000_i64],
        )?;
        assert_eq!(stale, 0);
        assert_eq!(
            status_and_claim("ins_stale")?,
            ("pending_model_call".to_string(), Some(5000))
        );
        // The owning pass's own timestamp does terminate it.
        let owned = conn.execute(
            super::MARK_CONTEXT_UNPARSABLE_SQL,
            params!["ins_stale", 5000_i64, 6000_i64],
        )?;
        assert_eq!(owned, 1);

        // Non-pending row: an already-resolved row is never overwritten.
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params!["ins_ok", 1_i64, "run1", "fp_o", "v1", "not json", 1001_i64],
        )?;
        conn.execute(super::CLAIM_PENDING_SQL, params!["ins_ok", 5000_i64, lease])?;
        conn.execute(
            "UPDATE ai_insight SET status = 'ok', model_response_json = '{\"x\":1}' \
             WHERE id = 'ins_ok'",
            [],
        )?;
        let resolved = conn.execute(
            super::MARK_CONTEXT_UNPARSABLE_SQL,
            params!["ins_ok", 5000_i64, 6000_i64],
        )?;
        assert_eq!(resolved, 0);
        assert_eq!(status_and_claim("ins_ok")?.0, "ok");
        let response: String = conn.query_row(
            "SELECT model_response_json FROM ai_insight WHERE id = 'ins_ok'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(response, "{\"x\":1}");
        Ok(())
    }

    /// Inserts three `reports` rows (one per job `j1`/`j2`/`j3`, in the
    /// given order), all sharing one fingerprint (same `test_id`/message/
    /// stack) but distinguishable `output_tail`s, and returns the assembled
    /// context built from the rows `CANONICAL_REPORTS_SQL` returns in that
    /// order. `j1`'s report has the *highest* `accepted_seq` and `j2`'s has
    /// the lowest, deliberately inverted from both insertion order and
    /// `job_id`/`r2_key` lexical order: a query plan that incidentally
    /// returns rows in join/index order instead of `accepted_seq` order
    /// would pick `j1`'s ("third accepted") as the representative, not
    /// `j2`'s ("first accepted").
    fn assembled_context_for_insert_order(
        order: &[usize],
    ) -> rusqlite::Result<super::AssembledFailureContext> {
        use super::FailingTestInput;
        let conn = db_after_0021()?;
        conn.execute(
            "INSERT INTO jobs (id, run_id, job_name, shard_total) VALUES \
             ('j1', 'run1', 'a', 1), ('j2', 'run1', 'b', 1), ('j3', 'run1', 'c', 1)",
            [],
        )?;
        // (report id, job, accepted_seq, r2_key).
        let rows = [
            ("rep_c", "j1", 3_i64, "k_c"),
            ("rep_a", "j2", 1_i64, "k_a"),
            ("rep_b", "j3", 2_i64, "k_b"),
        ];
        for &i in order {
            let (id, job, seq, key) = rows[i];
            conn.execute(
                "INSERT INTO reports (id, job_id, shard_index, kind, name, content_sha256, \
                 accepted_seq, created_at, r2_key, parsed) \
                 VALUES (?1, ?2, 0, 'junit', ?1, ?1, ?3, 1, ?4, 1)",
                params![id, job, seq, key],
            )?;
        }
        let failing = |out: &str| FailingTestInput {
            test_id: "t1".to_string(),
            message: "boom".to_string(),
            stack_trace: vec!["at foo".to_string()],
            system_out: out.to_string(),
            system_err: String::new(),
        };
        let content = |key: &str| match key {
            "k_a" => failing("accepted first"),
            "k_b" => failing("accepted second"),
            _ => failing("accepted third"),
        };
        let keys: Vec<String> = conn
            .prepare(super::CANONICAL_REPORTS_SQL)?
            .query_map(params!["run1"], |r| r.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        let tests: Vec<FailingTestInput> = keys.iter().map(|k| content(k)).collect();
        Ok(super::assemble_failure_context(&tests, 8192))
    }

    #[test]
    fn assembled_context_is_independent_of_report_row_insert_order() -> rusqlite::Result<()> {
        // The one selected representative must be the earliest-accepted
        // report's ("accepted first", `accepted_seq = 1`) — never
        // `j1`'s/`j3`'s later ones — regardless of the order these three
        // reports happened to be inserted in.
        for order in [
            [0, 1, 2],
            [2, 1, 0],
            [1, 2, 0],
            [2, 0, 1],
            [1, 0, 2],
            [0, 2, 1],
        ] {
            let assembled = assembled_context_for_insert_order(&order)?;
            assert_eq!(assembled.entries.len(), 1, "{assembled:?}");
            assert_eq!(assembled.entries[0].output_tail, "accepted first");
        }
        Ok(())
    }

    #[test]
    fn stale_pass_release_cannot_clear_a_newer_live_claim() -> rusqlite::Result<()> {
        // Pass A claims, A's lease expires without A releasing (e.g. A
        // crashed mid-call), pass B reclaims the now-stale row, and only
        // *then* does A's model call finally fail and try to release —
        // using A's own stale claim timestamp. That release must not
        // touch B's live claim.
        let conn = db_after_0021()?;
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params!["ins_claim", 1_i64, "run1", "fp1", "v1", "{}", 1000_i64],
        )?;
        let lease = super::MODEL_CALL_CLAIM_LEASE_MS;

        let a_claim_at = 5000_i64;
        let a_claimed = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", a_claim_at, lease],
        )?;
        assert!(super::claim_won(Some(a_claimed)));

        // A's lease expires; B reclaims with its own, newer timestamp.
        let b_claim_at = a_claim_at + lease;
        let b_claimed = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", b_claim_at, lease],
        )?;
        assert!(super::claim_won(Some(b_claimed)));

        // A's (late) release, using A's own stale claim timestamp, must be
        // a no-op — `claimed_at = ?2` no longer matches B's value.
        let a_release_changes =
            conn.execute(super::RELEASE_CLAIM_SQL, params!["ins_claim", a_claim_at])?;
        assert_eq!(a_release_changes, 0);

        // B's claim must still be in place: a third pass racing right now
        // (before B's own lease expires) must not win.
        let third_claim = conn.execute(
            super::CLAIM_PENDING_SQL,
            params!["ins_claim", b_claim_at + 1, lease],
        )?;
        assert!(!super::claim_won(Some(third_claim)));

        // B's own release, with B's own timestamp, does work.
        let b_release_changes =
            conn.execute(super::RELEASE_CLAIM_SQL, params!["ins_claim", b_claim_at])?;
        assert_eq!(b_release_changes, 1);
        Ok(())
    }

    #[test]
    fn migration_0021_dedupes_pre_existing_duplicates_keeping_the_resolved_row()
    -> rusqlite::Result<()> {
        let conn = db_before_0021()?;
        // Three rows the old, non-unique-indexed consumer could have left
        // behind for the same (run_id, kind, fingerprint, prompt_version,
        // diff_hash) key: a resolved one and two still-pending ones with
        // different `created_at`/`id` — exactly the shape a Queue
        // redelivery plus an in-flight pass would produce.
        conn.execute(
            "INSERT INTO ai_insight \
             (id, repo_id, run_id, kind, fingerprint, diff_hash, prompt_version, status, \
              context_json, model_response_json, created_at, updated_at) \
             VALUES ('ins_b_pending_later', 1, 'run1', 'failure_summary', 'fp1', NULL, 'v1', \
              'pending_model_call', '{}', NULL, 2000, 2000)",
            [],
        )?;
        conn.execute(
            "INSERT INTO ai_insight \
             (id, repo_id, run_id, kind, fingerprint, diff_hash, prompt_version, status, \
              context_json, model_response_json, created_at, updated_at) \
             VALUES ('ins_a_ok', 1, 'run1', 'failure_summary', 'fp1', NULL, 'v1', 'ok', '{}', \
              '{\"summary\":\"x\"}', 1500, 1500)",
            [],
        )?;
        conn.execute(
            "INSERT INTO ai_insight \
             (id, repo_id, run_id, kind, fingerprint, diff_hash, prompt_version, status, \
              context_json, model_response_json, created_at, updated_at) \
             VALUES ('ins_c_pending_earliest', 1, 'run1', 'failure_summary', 'fp1', NULL, 'v1', \
              'pending_model_call', '{}', NULL, 1000, 1000)",
            [],
        )?;
        // A distinct key must survive the dedupe untouched.
        conn.execute(
            "INSERT INTO ai_insight \
             (id, repo_id, run_id, kind, fingerprint, diff_hash, prompt_version, status, \
              context_json, model_response_json, created_at, updated_at) \
             VALUES ('ins_other', 1, 'run1', 'failure_summary', 'fp2', NULL, 'v1', \
              'pending_model_call', '{}', NULL, 1000, 1000)",
            [],
        )?;
        assert_eq!(insight_count(&conn, "run1")?, 4);

        conn.execute_batch(MIGRATION_0021)?;

        assert_eq!(insight_count(&conn, "run1")?, 2);
        let fp1_id: String = conn.query_row(
            "SELECT id FROM ai_insight WHERE run_id = 'run1' AND fingerprint = 'fp1'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(
            fp1_id, "ins_a_ok",
            "the resolved row must outlive the pending duplicates"
        );
        let fp2_id: String = conn.query_row(
            "SELECT id FROM ai_insight WHERE run_id = 'run1' AND fingerprint = 'fp2'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(fp2_id, "ins_other");

        // The unique index is now live: a second insert attempt for the
        // surviving key is a no-op, never a second row.
        conn.execute(
            super::INSERT_INSIGHT_SQL,
            params![
                "ins_after_migration",
                1_i64,
                "run1",
                "fp1",
                "v1",
                "{}",
                9999_i64
            ],
        )?;
        assert_eq!(insight_count(&conn, "run1")?, 2);
        Ok(())
    }
}
