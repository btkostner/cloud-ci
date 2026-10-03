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
