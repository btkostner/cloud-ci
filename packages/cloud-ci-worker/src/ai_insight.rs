//! Pure decision logic backing the **failure summaries** feature of
//! `docs/design/ai.md`: "### Failure summaries: inputs", "### Token
//! budgeting", and "### Prompting and output contract". No `worker`/
//! Durable-Object/Queue/`AI`-binding dependency, so it is unit-testable
//! with plain `cargo test` — same "foundation ahead of caller" pattern as
//! `cloud-ci-core`'s `rightsizing` module.
//!
//! # Placement: `cloud-ci-worker`, not `cloud-ci-core`
//!
//! `cloud-ci-core` is shared with `cloud-ci-cli` (BYO CI) — see
//! `cloud-ci-core`'s own module docs: "the same binary and the same split
//! algorithm ... used in both places". AI insights are exclusively a
//! `cloud-ci-worker` feature (ai.md: "cloud-ci uses Workers AI, through the
//! `AI` binding in `cloud-ci-worker`" — `cloud-ci-cli` never calls it, and
//! ai.md's own "Prompt templates live in `cloud-ci-worker` as versioned
//! constants" already anchors the rest of this feature there). Unlike
//! `rightsizing` (genuinely executor-agnostic, usable by a future
//! non-default `Executor`), there is no analogous reason for `cloud-ci-cli`
//! to ever link this module, so it lives alongside its eventual caller
//! (`coordinator`'s `AnalysisRequested` consumer, once built) rather than
//! in the cross-crate-shared `cloud-ci-core`. This mirrors
//! `coordinator::logic`'s own placement reasoning one level up: pure logic
//! submodule of the crate that will actually call it, not a new shared
//! crate for logic with exactly one caller.
//!
//! This module reuses `sha2::Sha256`, already a `cloud-ci-worker`
//! dependency (`ingest_token.rs`'s HMAC signing, `coordinator::logic`'s
//! `test_id`, `coordinator::mod`'s `hex_sha256`/`do_name`) — no new hashing
//! crate added. Message normalization and library-frame classification are
//! hand-rolled character/substring scans rather than a `regex` dependency:
//! `cloud-ci-worker` has no regex crate today, the patterns involved
//! (digit runs, `0x`-hex, UUID's fixed 8-4-4-4-12 grouping, a short list of
//! duration-unit suffixes, a short list of path-prefix substrings) are all
//! simple enough to scan directly, and a hand-rolled scanner avoids a new
//! dependency plus a regex engine's binary-size cost in a wasm target.
//!
//! # Scope boundary — what this round builds and what it does not
//!
//! This round is **only** the pure math described in the three doc
//! sections named above: fingerprinting, systemic-failure detection, token
//! budgeting (the fixed table, the `ceil(utf8_bytes / 3.2)` estimator, and
//! the priority-list reallocation rule), structural truncation, and
//! output-schema validation (including the evidence-ref-drop /
//! confidence-downgrade rule). It deliberately does **not** build, and
//! nothing in the running Worker calls this module yet, because none of
//! these exist in this codebase:
//!
//! - The `cloud-ci-analysis` Queue or its consumer (`max_batch_size = 1`,
//!   retry/DLQ settings from the Pipeline section) — grepping the worker
//!   crate for `cloud-ci-analysis`/`AnalysisRequested` finds nothing.
//! - The context builder: R2 log reads, D1 run-history reads, and the
//!   GitHub PR-diff fetch (`GET /repos/{owner}/{repo}/pulls/{n}` with the
//!   diff media type) that would supply this module's functions with real
//!   failing-test/log-tail/diff bytes.
//! - `exclude_paths` filtering (the pipeline diagram's `RED[Redact +
//!   exclude_paths + fingerprint]` step's path-exclusion half) is still
//!   not applied anywhere in this crate — see `ai_queue.rs`'s module doc
//!   comment's "Honest gaps" list. Content-based **redaction** (`RED`'s
//!   other half) is no longer an open gap: [`crate::ai_redact::redact`]
//!   now runs in [`crate::ai_queue::assemble_failure_context`], strictly
//!   before this module's [`failure_fingerprint`] is ever called — this
//!   module's own functions still take plain, already-redacted strings
//!   and still do no redaction themselves (that responsibility belongs
//!   to the caller, not to this crate's pure-math layer), but the
//!   caller that was missing is now real.
//! - The `env.AI.run()` Workers AI binding call, `AI_GATEWAY_ID` wiring, or
//!   the `ai_usage_daily`/`ai_insight` D1 tables (budget-cap checks,
//!   fingerprint cache, stored prompt/response) — no migration for either
//!   table exists in `packages/cloud-ci-worker/migrations/` yet.
//! - `PROMPT_SUMMARY_V1` or any other prompt template constant.
//!
//! A future round wires the Queue consumer, context builder, redaction,
//! `AI` binding call, and D1 tables, and calls this round's pure
//! functions — [`failure_fingerprint`], [`plan_summarization`],
//! [`scale_budget_for_model`], [`allot_budget`], [`truncate_log_tail`],
//! [`truncate_stack_trace`], [`truncate_diff`], [`validate_schema_lengths`],
//! [`validate_output`] — with real data. Until then this module is linked
//! into `cloud-ci-worker` (see `lib.rs`) but called from nowhere running,
//! same as `cloud-ci-core::rightsizing` today.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Failure summaries: inputs — fingerprinting
// (docs/design/ai.md "### Failure summaries: inputs")
// ---------------------------------------------------------------------------

/// Default library/stdlib path-prefix substrings excluded from "top 5
/// non-library frames" (ai.md: "the first 30 stack frames with
/// `node_modules`/stdlib frames collapsed"). Deliberately a plain `&[&str]`
/// substring list, not a hardcoded single-language rule — ai.md frames
/// code and logs as language-agnostic input, and callers may pass their
/// own list via [`top_non_library_frames`]'s `library_patterns` parameter;
/// this constant is only the default for a caller with no stronger
/// opinion. Covers the ecosystems cloud-ci already parses reports for
/// (JUnit/Vitest/Playwright, i.e. JS/TS, Python, and JVM/Rust via generic
/// toolchain paths), not an exhaustive list.
pub const DEFAULT_LIBRARY_PATTERNS: &[&str] = &[
    "node_modules/",
    "<anonymous>",
    "internal/",
    "site-packages/",
    "dist-packages/",
    "/usr/lib/python",
    "/usr/local/lib/python",
    ".venv/",
    "/rustc/",
    ".cargo/registry/",
    ".cargo/git/",
    "vendor/",
    "/gems/",
];

/// Returns the first `n` frames of `frames` whose string does not contain
/// any of `library_patterns` as a substring, preserving `frames`' own
/// order ("top 5 non-library frames" — top meaning closest to the fault,
/// i.e. the start of the stack as the report format already orders it,
/// not re-sorted here).
pub fn top_non_library_frames(frames: &[&str], library_patterns: &[&str], n: usize) -> Vec<String> {
    frames
        .iter()
        .filter(|frame| !library_patterns.iter().any(|pat| frame.contains(pat)))
        .take(n)
        .map(|s| (*s).to_string())
        .collect()
}

/// `0x1f` (ASCII Unit Separator) joins fingerprint fields unambiguously,
/// matching `coordinator::logic::test_id`'s existing convention for the
/// exact same reason: a printable delimiter could itself appear in a
/// message or frame, an unprintable control byte cannot in practice.
const FIELD_SEPARATOR: u8 = 0x1f;

/// `sha256(test_id || normalize(message) || top 5 non-library frames)`
/// (ai.md's "Failure fingerprint"), hex-encoded. `frames` should already be
/// ordered top-of-stack-first; this function selects the top 5
/// non-library frames from it (per [`top_non_library_frames`]) rather than
/// taking all of `frames` verbatim, so two failures whose only difference
/// is deep library-frame noise still fingerprint identically.
pub fn failure_fingerprint(
    test_id: &str,
    message: &str,
    frames: &[&str],
    library_patterns: &[&str],
) -> String {
    let normalized = normalize(message);
    let top_frames = top_non_library_frames(frames, library_patterns, 5);

    let mut hasher = Sha256::new();
    hasher.update(test_id.as_bytes());
    hasher.update([FIELD_SEPARATOR]);
    hasher.update(normalized.as_bytes());
    for frame in &top_frames {
        hasher.update([FIELD_SEPARATOR]);
        hasher.update(frame.as_bytes());
    }
    let digest = hasher.finalize();

    let mut out = String::with_capacity(64);
    for byte in digest {
        // `write!` into a `String` cannot fail; a manual hex table avoids
        // pulling in `std::fmt::Write` purely for this, matching
        // `coordinator::mod::hex_sha256`'s own approach.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Replaces numbers, hex addresses, UUIDs, temp paths, and durations with
/// placeholders (ai.md's `normalize` description, verbatim). Operates per
/// whitespace-delimited token: each of a message's "words" is classified
/// independently and reassembled with single-space separators. This is a
/// deliberate simplification — it does not catch a number embedded mid-word
/// without any separator from a different category, nor does it preserve a
/// message's original whitespace exactly — but it is sufficient for the
/// test/log assertion-message shapes ai.md actually describes (e.g.
/// `"expected 42 but got 17 after 350ms (id 4b1a...-...)"`), and keeps the
/// scanner simple and dependency-free (see module docs on not pulling in a
/// `regex` crate).
///
/// Classification order per token, first match wins: temp path, then
/// UUID, then duration, then a final pass that replaces `0x`-prefixed or
/// bare long hex runs with `<hex>` and any other digit run with `<num>`.
/// Order matters because a UUID is itself all hex/digits (it must be
/// recognized whole before the generic hex/number pass would shred it into
/// several placeholders), and a duration (`"350ms"`) must be recognized
/// before its leading digits are treated as a bare `<num>`.
pub fn normalize(message: &str) -> String {
    message
        .split_whitespace()
        .map(normalize_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Temp-path prefixes recognized by [`normalize`]. Covers the common
/// Linux/macOS/Windows temp-directory conventions; a token containing any
/// of these as a substring is replaced with `<tmppath>` in full (the whole
/// token, not just the matched prefix) since the rest of the path — a
/// per-run random suffix — carries no stable information either.
const TEMP_PATH_PATTERNS: &[&str] = &[
    "/tmp/",
    "/var/folders/",
    "/private/var/folders/",
    "\\AppData\\Local\\Temp\\",
    "\\Temp\\",
    "/data/local/tmp/",
];

/// Duration unit suffixes [`normalize`] recognizes, longest-prefix-first
/// so `"ms"` is tried before `"s"` would otherwise match its trailing
/// character first. Covers the units cloud-ci's own log/report output
/// uses (nanoseconds through hours); a bare `"m"` is deliberately excluded
/// since it collides with common non-duration single-letter suffixes
/// (sizes like `"4m"` meaning nothing in this codebase, but `"4mi"` for
/// MiB would already fail the "trailing chars are exactly a unit" check
/// below) — a duration must spell out a recognized unit AND have no
/// trailing letters after it.
const DURATION_UNITS: &[&str] = &["ns", "us", "\u{b5}s", "ms", "s", "h"];

fn normalize_token(token: &str) -> String {
    if TEMP_PATH_PATTERNS.iter().any(|pat| token.contains(pat)) {
        return "<tmppath>".to_string();
    }
    if is_uuid_token(token) {
        return "<uuid>".to_string();
    }
    if is_duration_token(token) {
        return "<duration>".to_string();
    }
    replace_hex_and_numbers(token)
}

/// A UUID token: 5 hyphen-separated hex groups of exactly 8-4-4-4-12
/// characters (RFC 4122 textual form), after stripping any non-hex
/// leading/trailing punctuation the token might carry (e.g. a trailing
/// comma or wrapping parenthesis in a log message).
fn is_uuid_token(token: &str) -> bool {
    let core = token.trim_matches(|c: char| !c.is_ascii_hexdigit() && c != '-');
    let groups: Vec<&str> = core.split('-').collect();
    let expected_lens = [8usize, 4, 4, 4, 12];
    groups.len() == 5
        && groups
            .iter()
            .zip(expected_lens)
            .all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A duration token: one or more `<digits-with-optional-dot><unit>`
/// groups concatenated with no separator (`"350ms"`, `"2h30s"`), after
/// stripping non-alphanumeric trailing punctuation. Requires the *entire*
/// stripped core to be consumed by such groups — a bare number that
/// happens to end in a letter sequence that is not actually a unit (or
/// that leaves a remainder) is not a duration.
fn is_duration_token(token: &str) -> bool {
    let core = token.trim_matches(|c: char| c.is_ascii_punctuation() && c != '.');
    if core.is_empty() {
        return false;
    }
    let bytes = core.as_bytes();
    let mut i = 0usize;
    let mut matched_any_group = false;
    while i < bytes.len() {
        let digit_start = i;
        let mut saw_digit = false;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            if bytes[i].is_ascii_digit() {
                saw_digit = true;
            }
            i += 1;
        }
        if !saw_digit || i == digit_start {
            return false;
        }
        let Some(unit) = DURATION_UNITS
            .iter()
            .find(|unit| core[i..].starts_with(**unit))
        else {
            return false;
        };
        i += unit.len();
        matched_any_group = true;
    }
    matched_any_group
}

/// Final pass over a token that is neither a temp path, UUID, nor
/// duration: replaces `0x`-prefixed hex runs, and bare hex-looking runs of
/// 8+ characters containing at least one `a`-`f`/`A`-`F` letter (long
/// enough, and alphabetic enough, to be a hash/address rather than a
/// plain decimal number — e.g. a short git sha `"4b1a9c2"`), with
/// `<hex>`; any other run of ASCII digits (optionally containing a single
/// `.`, for floats/durations-without-unit like plain seconds counts) with
/// `<num>`. Non-digit, non-hex characters pass through unchanged.
fn replace_hex_and_numbers(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let mut out = String::with_capacity(token.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '0'
            && chars
                .get(i + 1)
                .map(|c| *c == 'x' || *c == 'X')
                .unwrap_or(false)
            && chars
                .get(i + 2)
                .map(|c| c.is_ascii_hexdigit())
                .unwrap_or(false)
        {
            let mut j = i + 2;
            while j < chars.len() && chars[j].is_ascii_hexdigit() {
                j += 1;
            }
            out.push_str("<hex>");
            i = j;
            continue;
        }
        if chars[i].is_ascii_hexdigit() {
            let start = i;
            let mut j = i;
            let mut has_alpha = false;
            while j < chars.len() && chars[j].is_ascii_hexdigit() {
                if chars[j].is_ascii_alphabetic() {
                    has_alpha = true;
                }
                j += 1;
            }
            if has_alpha && j - start >= 8 {
                out.push_str("<hex>");
            } else {
                // Plain digit run (no hex letters): a number, possibly
                // with one embedded '.', possibly immediately followed by
                // trailing hex letters that didn't reach the length-8
                // threshold above — re-scan just the digit/'.' prefix.
                let mut k = start;
                while k < chars.len() && (chars[k].is_ascii_digit() || chars[k] == '.') {
                    k += 1;
                }
                if k > start {
                    out.push_str("<num>");
                    j = k;
                } else {
                    out.push(chars[start]);
                    j = start + 1;
                }
            }
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Failure summaries: inputs — systemic-failure detection
// (docs/design/ai.md "### Failure summaries: inputs", last sentence)
// ---------------------------------------------------------------------------

/// One failing job's log-tail signature, as input to
/// [`plan_summarization`]'s "more than 3 failing jobs that share a
/// log-tail signature" check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailingJob {
    pub job_name: String,
    pub log_tail_signature: String,
}

/// What [`plan_summarization`] decided to summarize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizationPlan {
    /// Not systemic: one summary per distinct fingerprint, in first-seen
    /// order (ai.md: "Shards that fail with the same fingerprint are
    /// summarized once").
    PerFingerprint(Vec<String>),
    /// Systemic: "one summary over the 3 most frequent fingerprints, not
    /// one per test." Ties in frequency are broken by first-seen order,
    /// for a deterministic result given the same input order.
    Systemic(Vec<String>),
}

/// Implements ai.md's systemic-failure rule: "A run with more than 20
/// failing tests, or more than 3 failing jobs that share a log-tail
/// signature, is treated as a systemic failure. That case gets one summary
/// over the 3 most frequent fingerprints, not one per test."
///
/// `failure_fingerprints` is one entry per failing test (duplicates
/// expected and meaningful — frequency is exactly what ranks the "3 most
/// frequent" systemic case). `failing_jobs` is one entry per failing job;
/// jobs are deduplicated by `job_name` before counting shared-signature
/// groups, so one job listed twice never inflates the "jobs sharing a
/// signature" count by itself.
pub fn plan_summarization(
    failure_fingerprints: &[String],
    failing_jobs: &[FailingJob],
) -> SummarizationPlan {
    let systemic_by_failure_count = failure_fingerprints.len() > 20;
    let systemic_by_shared_signature = max_jobs_sharing_signature(failing_jobs) > 3;

    if systemic_by_failure_count || systemic_by_shared_signature {
        SummarizationPlan::Systemic(top_n_frequent(failure_fingerprints, 3))
    } else {
        SummarizationPlan::PerFingerprint(dedupe_preserve_order(failure_fingerprints))
    }
}

fn max_jobs_sharing_signature(failing_jobs: &[FailingJob]) -> usize {
    let deduped = dedupe_jobs_by_name(failing_jobs);
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for job in &deduped {
        match counts
            .iter_mut()
            .find(|(sig, _)| *sig == job.log_tail_signature)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((job.log_tail_signature.as_str(), 1)),
        }
    }
    counts
        .into_iter()
        .map(|(_, count)| count)
        .max()
        .unwrap_or(0)
}

fn dedupe_jobs_by_name(failing_jobs: &[FailingJob]) -> Vec<&FailingJob> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for job in failing_jobs {
        if !seen.contains(&job.job_name.as_str()) {
            seen.push(&job.job_name);
            out.push(job);
        }
    }
    out
}

fn dedupe_preserve_order(fingerprints: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for fp in fingerprints {
        if !out.contains(fp) {
            out.push(fp.clone());
        }
    }
    out
}

fn top_n_frequent(fingerprints: &[String], n: usize) -> Vec<String> {
    let mut counted: Vec<(String, usize, usize)> = Vec::new(); // (fp, count, first_seen_index)
    for (idx, fp) in fingerprints.iter().enumerate() {
        match counted.iter_mut().find(|(existing, _, _)| existing == fp) {
            Some((_, count, _)) => *count += 1,
            None => counted.push((fp.clone(), 1, idx)),
        }
    }
    counted.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    counted.into_iter().take(n).map(|(fp, _, _)| fp).collect()
}

// ---------------------------------------------------------------------------
// Token budgeting (docs/design/ai.md "### Token budgeting")
// ---------------------------------------------------------------------------

/// `gpt-oss-120b`'s context window, the default model (ai.md: "128,000
/// tokens" / "The default input budget is about 19% of `gpt-oss-120b`'s
/// 128,000-token context window"). The budget table below is this
/// model's default allotment; [`scale_budget_for_model`] scales it down
/// for a smaller-window model.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;

/// One input/output token budget table, ai.md's "Token budgeting" table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget {
    pub system_prompt: u32,
    pub run_metadata: u32,
    pub failing_tests: u32,
    pub log_tail: u32,
    pub diff: u32,
    pub output: u32,
}

impl TokenBudget {
    /// `max_input_tokens`: the sum of every input slot (excludes `output`,
    /// which is a separate `max_tokens` cap on the model's response).
    pub fn max_input_tokens(&self) -> u32 {
        self.system_prompt + self.run_metadata + self.failing_tests + self.log_tail + self.diff
    }
}

/// ai.md's table exactly: system prompt 1,200; run metadata + history 600;
/// failing tests 8,000; log tail 7,000; diff 7,000 (input total 23,800);
/// output 1,200.
pub const DEFAULT_BUDGET: TokenBudget = TokenBudget {
    system_prompt: 1_200,
    run_metadata: 600,
    failing_tests: 8_000,
    log_tail: 7_000,
    diff: 7_000,
    output: 1_200,
};

/// `ceil(utf8_bytes / 3.2)` (ai.md's token estimator), computed with exact
/// integer arithmetic rather than floating point: `bytes / 3.2 == bytes *
/// 5 / 16` (since `3.2 == 16/5`), so `ceil(bytes / 3.2) == (bytes * 5 +
/// 15) / 16` using integer division — deterministic across platforms, no
/// float-rounding edge cases.
pub fn estimate_tokens(byte_len: usize) -> u32 {
    ((byte_len as u64) * 5).div_ceil(16) as u32
}

/// Scales [`DEFAULT_BUDGET`] proportionally for a model with a smaller
/// context window than [`DEFAULT_CONTEXT_WINDOW`] (ai.md: "Before each
/// call, the builder checks the selected model's context window against a
/// static table ... If a configured model has a smaller window, the
/// budgets scale down proportionally"). A model with a window at or above
/// the default gets the default table unchanged — the table was never
/// meant to *grow* with a bigger window (ai.md frames the default
/// 23,800-token input budget as a deliberate 19%-of-128,000 ceiling for
/// cost/latency reasons, not a floor to scale up from).
///
/// Each slot (including `output`) is scaled by the same integer ratio
/// `model_context_window / DEFAULT_CONTEXT_WINDOW`, computed with 64-bit
/// intermediates and floored — flooring, not rounding, keeps the scaled
/// total from ever exceeding the model's actual proportional share.
pub fn scale_budget_for_model(model_context_window: u32) -> TokenBudget {
    if model_context_window >= DEFAULT_CONTEXT_WINDOW {
        return DEFAULT_BUDGET;
    }
    let scale = |v: u32| -> u32 {
        ((v as u64) * (model_context_window as u64) / (DEFAULT_CONTEXT_WINDOW as u64)) as u32
    };
    TokenBudget {
        system_prompt: scale(DEFAULT_BUDGET.system_prompt),
        run_metadata: scale(DEFAULT_BUDGET.run_metadata),
        failing_tests: scale(DEFAULT_BUDGET.failing_tests),
        log_tail: scale(DEFAULT_BUDGET.log_tail),
        diff: scale(DEFAULT_BUDGET.diff),
        output: scale(DEFAULT_BUDGET.output),
    }
}

/// The actual (estimated) token size of each reallocatable input class,
/// as input to [`allot_budget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassSizes {
    pub run_metadata: u32,
    pub failing_tests: u32,
    pub log_tail: u32,
    pub diff: u32,
}

/// [`allot_budget`]'s result: each class's final token allotment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinalAllotment {
    pub system_prompt: u32,
    pub run_metadata: u32,
    pub failing_tests: u32,
    pub log_tail: u32,
    pub diff: u32,
    pub output: u32,
}

/// Implements "unused budget flows down the priority list" against a
/// [`TokenBudget`] table (typically [`DEFAULT_BUDGET`] or
/// [`scale_budget_for_model`]'s output) and each class's actual size.
///
/// The priority list is exactly the table's own row order for the
/// reallocatable classes: run metadata + history, failing tests, log
/// tail, diff (ai.md's table order). `system_prompt` and `output` are not
/// part of this reallocation — `system_prompt` is a fixed template with
/// no variable content to under-use, and `output` is the model's response
/// cap, not a selected input; both pass through unchanged.
///
/// For each class in priority order: its final allotment is
/// `min(need, cap + carry)`, where `carry` is whatever earlier classes in
/// the list left unused. Any remainder (cap and carry not fully consumed
/// by `need`) carries forward to the next class. A class whose `need`
/// exceeds its `cap + carry` gets the full `cap + carry` (truncation is
/// this function's caller's job, via [`truncate_log_tail`] /
/// [`truncate_stack_trace`] / [`truncate_diff`] against this returned
/// allotment) and leaves no carry for the next class.
pub fn allot_budget(table: &TokenBudget, actual: &ClassSizes) -> FinalAllotment {
    let mut carry = 0u32;
    let take = |cap: u32, need: u32, carry: &mut u32| -> u32 {
        let available = cap + *carry;
        if need <= available {
            *carry = available - need;
            need
        } else {
            *carry = 0;
            available
        }
    };
    let run_metadata = take(table.run_metadata, actual.run_metadata, &mut carry);
    let failing_tests = take(table.failing_tests, actual.failing_tests, &mut carry);
    let log_tail = take(table.log_tail, actual.log_tail, &mut carry);
    let diff = take(table.diff, actual.diff, &mut carry);
    FinalAllotment {
        system_prompt: table.system_prompt,
        run_metadata,
        failing_tests,
        log_tail,
        diff,
        output: table.output,
    }
}

// ---------------------------------------------------------------------------
// Structural truncation (docs/design/ai.md "### Token budgeting", last
// paragraph: "Truncation is structural, never mid-line...")
// ---------------------------------------------------------------------------

/// Log tails keep their end (ai.md: "Log tails keep their end"). Keeps
/// whole lines only, working backward from the last line until adding the
/// next (earlier) line would exceed `token_budget`, then restores original
/// order. A single line whose own estimated cost exceeds `token_budget` on
/// its own is dropped rather than included partially — truncation here is
/// always whole-line, never mid-line, even at the cost of an empty result
/// for a pathologically long single line.
pub fn truncate_log_tail(lines: &[&str], token_budget: u32) -> Vec<String> {
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0u32;
    for line in lines.iter().rev() {
        let cost = estimate_tokens(line.len() + 1);
        if used + cost > token_budget {
            break;
        }
        used += cost;
        kept.push(line);
    }
    kept.reverse();
    kept.into_iter().map(|s| s.to_string()).collect()
}

/// Stack traces keep their head and tail (ai.md: "Stack traces keep their
/// head and tail"). If every frame fits, returns them all unchanged.
/// Otherwise alternately grows a head run (from the start) and a tail run
/// (from the end), each frame added only if it still fits in
/// `token_budget`, until no more frames fit from either end; the frames
/// dropped in the middle are replaced by one `"... [N frames omitted]
/// ..."` marker (structural, whole-frame truncation — never mid-frame).
pub fn truncate_stack_trace(frames: &[&str], token_budget: u32) -> Vec<String> {
    if frames.is_empty() {
        return Vec::new();
    }
    let total: u32 = frames.iter().map(|f| estimate_tokens(f.len() + 1)).sum();
    if total <= token_budget {
        return frames.iter().map(|s| s.to_string()).collect();
    }

    let mut used = 0u32;
    let mut head_end = 0usize;
    let mut tail_start = frames.len();
    let mut take_head = true;
    while head_end < tail_start {
        let idx = if take_head { head_end } else { tail_start - 1 };
        let cost = estimate_tokens(frames[idx].len() + 1);
        if used + cost > token_budget {
            break;
        }
        used += cost;
        if take_head {
            head_end += 1;
        } else {
            tail_start -= 1;
        }
        take_head = !take_head;
    }

    let mut out: Vec<String> = frames[..head_end].iter().map(|s| s.to_string()).collect();
    if tail_start > head_end {
        out.push(format!(
            "... [{} frames omitted] ...",
            tail_start - head_end
        ));
    }
    out.extend(frames[tail_start..].iter().map(|s| s.to_string()));
    out
}

/// One diff hunk, as input to [`truncate_diff`]. `lines` is the hunk's own
/// text verbatim (including its `@@ ... @@` header line); `added`/
/// `removed` are its `+N`/`-M` line counts, needed for the omitted-hunk
/// placeholder's exact wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    pub lines: Vec<String>,
}

/// Diffs drop whole hunks, replacing each dropped hunk with
/// `"[hunk omitted: path, +N/-M]"` (ai.md, verbatim format). Hunks are
/// considered in `hunks`' own order (the caller ranks them first, per
/// ai.md's "Hunks are ranked: (1) files that appear in stack frames, (2)
/// files named in the log tail, (3) test files, (4) everything else" —
/// out of this function's scope, which only decides keep-vs-omit once an
/// order is already given) — a hunk is kept in full if it fits within
/// whatever budget remains after every earlier hunk; once the budget is
/// exhausted, every remaining hunk is omitted (never partially included,
/// matching "drop whole hunks").
pub fn truncate_diff(hunks: &[DiffHunk], token_budget: u32) -> Vec<String> {
    let mut used = 0u32;
    let mut out = Vec::new();
    for hunk in hunks {
        let cost: u32 = hunk
            .lines
            .iter()
            .map(|l| estimate_tokens(l.len() + 1))
            .sum();
        if used + cost <= token_budget {
            used += cost;
            out.extend(hunk.lines.iter().cloned());
        } else {
            out.push(format!(
                "[hunk omitted: {}, +{}/-{}]",
                hunk.path, hunk.added, hunk.removed
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Prompting and output contract
// (docs/design/ai.md "### Prompting and output contract")
// ---------------------------------------------------------------------------

/// `evidence[].kind`'s four documented values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceKind {
    Stack,
    Log,
    Diff,
    History,
}

/// One `evidence` array entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    #[serde(rename = "ref")]
    pub reference: String,
}

/// `confidence`'s three documented values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// `category`'s eight documented values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    TestAssertion,
    Build,
    Dependency,
    Infra,
    Timeout,
    Oom,
    Flaky,
    Unknown,
}

/// The failure-summary JSON schema from ai.md's "### Prompting and output
/// contract", directly `serde`-(de)serializable to/from the shape the
/// model is asked to return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureSummary {
    pub headline: String,
    pub likely_cause: String,
    pub evidence: Vec<Evidence>,
    pub next_step: String,
    pub confidence: Confidence,
    pub category: Category,
}

pub const MAX_HEADLINE_CHARS: usize = 140;
pub const MAX_LIKELY_CAUSE_CHARS: usize = 600;
pub const MAX_NEXT_STEP_CHARS: usize = 300;

/// Why a parsed [`FailureSummary`] fails schema validation — ai.md: "If
/// the output fails schema validation, the call is retried once with the
/// validation error appended." (The retry itself is the future caller's
/// job; this function only detects the condition.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaValidationError {
    HeadlineTooLong,
    LikelyCauseTooLong,
    NextStepTooLong,
}

/// Validates the three char-length caps the schema comment documents
/// (`headline <= 140`, `likely_cause <= 600`, `next_step <= 300`), counted
/// in Unicode scalar values (`chars().count()`), not bytes — a length cap
/// described as "chars" in a doc meant for display-width budgeting should
/// not penalize multi-byte UTF-8 text more than single-byte text.
pub fn validate_schema_lengths(summary: &FailureSummary) -> Result<(), SchemaValidationError> {
    if summary.headline.chars().count() > MAX_HEADLINE_CHARS {
        return Err(SchemaValidationError::HeadlineTooLong);
    }
    if summary.likely_cause.chars().count() > MAX_LIKELY_CAUSE_CHARS {
        return Err(SchemaValidationError::LikelyCauseTooLong);
    }
    if summary.next_step.chars().count() > MAX_NEXT_STEP_CHARS {
        return Err(SchemaValidationError::NextStepTooLong);
    }
    Ok(())
}

/// Implements ai.md's evidence-ref validation rule verbatim: "Each
/// `evidence.ref` must name a path or line that occurs in the input.
/// References that don't are dropped, and if every reference is dropped,
/// `confidence` is forced to `low`."
///
/// `input_segments` is every piece of text actually sent to the model
/// (failing-test output, log tail, diff, run metadata/history — whatever
/// was assembled and truncated by this module's other functions) — a
/// `ref` is valid if it occurs as a substring of *any* segment. A
/// `FailureSummary` whose `evidence` was already empty before this call is
/// left alone (not itself treated as "every reference dropped" — there
/// was nothing to drop); only a summary that *had* evidence and loses all
/// of it gets downgraded.
pub fn validate_output(mut summary: FailureSummary, input_segments: &[&str]) -> FailureSummary {
    let had_evidence = !summary.evidence.is_empty();
    summary.evidence.retain(|e| {
        input_segments
            .iter()
            .any(|seg| seg.contains(e.reference.as_str()))
    });
    if had_evidence && summary.evidence.is_empty() {
        summary.confidence = Confidence::Low;
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- normalize / fingerprinting --------------------------------------

    #[test]
    fn normalize_replaces_uuid() {
        assert_eq!(
            normalize("request 4b1a9c2e-1234-4abc-9def-0123456789ab failed"),
            "request <uuid> failed"
        );
    }

    #[test]
    fn normalize_replaces_hex_address() {
        assert_eq!(
            normalize("segfault at 0x7ffeabcdef12 in handler"),
            "segfault at <hex> in handler"
        );
    }

    #[test]
    fn normalize_replaces_temp_path() {
        assert_eq!(
            normalize("wrote output to /tmp/cloud-ci-83f2a1/out.log"),
            "wrote output to <tmppath>"
        );
    }

    #[test]
    fn normalize_replaces_duration() {
        assert_eq!(
            normalize("timed out after 350ms waiting"),
            "timed out after <duration> waiting"
        );
        assert_eq!(normalize("slept 2h30s"), "slept <duration>");
    }

    #[test]
    fn normalize_replaces_plain_number() {
        assert_eq!(
            normalize("expected 42 but got 17"),
            "expected <num> but got <num>"
        );
    }

    #[test]
    fn fingerprint_is_deterministic_for_same_inputs() {
        let a = failure_fingerprint(
            "test_abc",
            "assertion failed",
            &["at src/user.ts:44"],
            DEFAULT_LIBRARY_PATTERNS,
        );
        let b = failure_fingerprint(
            "test_abc",
            "assertion failed",
            &["at src/user.ts:44"],
            DEFAULT_LIBRARY_PATTERNS,
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // hex sha256
    }

    #[test]
    fn fingerprint_differs_for_different_test_id_message_or_frames() {
        let base = failure_fingerprint(
            "test_abc",
            "assertion failed",
            &["at src/user.ts:44"],
            DEFAULT_LIBRARY_PATTERNS,
        );
        let diff_test_id = failure_fingerprint(
            "test_xyz",
            "assertion failed",
            &["at src/user.ts:44"],
            DEFAULT_LIBRARY_PATTERNS,
        );
        let diff_message = failure_fingerprint(
            "test_abc",
            "different failure",
            &["at src/user.ts:44"],
            DEFAULT_LIBRARY_PATTERNS,
        );
        let diff_frames = failure_fingerprint(
            "test_abc",
            "assertion failed",
            &["at src/other.ts:10"],
            DEFAULT_LIBRARY_PATTERNS,
        );
        assert_ne!(base, diff_test_id);
        assert_ne!(base, diff_message);
        assert_ne!(base, diff_frames);
    }

    #[test]
    fn fingerprint_same_across_normalized_substrings() {
        // Same test_id/frames; messages differ only in a UUID, a hex
        // address, a temp path, and a duration — normalize() should
        // collapse both to the same placeholders, so the fingerprints
        // match even though the raw messages don't.
        let a = failure_fingerprint(
            "test_abc",
            "request 4b1a9c2e-1234-4abc-9def-0123456789ab failed at 0x7ffeabcdef12, wrote /tmp/run-aaa/out.log after 350ms",
            &["at src/user.ts:44"],
            DEFAULT_LIBRARY_PATTERNS,
        );
        let b = failure_fingerprint(
            "test_abc",
            "request 11111111-2222-4333-8444-555555555555 failed at 0xdeadbeef, wrote /tmp/run-zzz-different/out.log after 999ms",
            &["at src/user.ts:44"],
            DEFAULT_LIBRARY_PATTERNS,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn top_non_library_frames_excludes_node_modules_and_caps_at_n() {
        let frames = [
            "at a (src/x.ts:1)",
            "at b (node_modules/foo/index.js:1)",
            "at c (src/y.ts:2)",
            "at d (src/z.ts:3)",
            "at e (src/w.ts:4)",
            "at f (src/v.ts:5)",
            "at g (src/u.ts:6)",
        ];
        let top = top_non_library_frames(&frames, DEFAULT_LIBRARY_PATTERNS, 5);
        assert_eq!(top.len(), 5);
        assert!(top.iter().all(|f| !f.contains("node_modules")));
        assert_eq!(top[0], "at a (src/x.ts:1)");
    }

    // -- systemic-failure detection ---------------------------------------

    #[test]
    fn plan_summarization_per_fingerprint_below_threshold() {
        let fps = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let jobs = vec![FailingJob {
            job_name: "job1".into(),
            log_tail_signature: "sig1".into(),
        }];
        let plan = plan_summarization(&fps, &jobs);
        assert_eq!(
            plan,
            SummarizationPlan::PerFingerprint(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn plan_summarization_systemic_at_21_failures_not_at_20() {
        let fps_20: Vec<String> = (0..20).map(|i| format!("fp{i}")).collect();
        let jobs: Vec<FailingJob> = Vec::new();
        assert!(matches!(
            plan_summarization(&fps_20, &jobs),
            SummarizationPlan::PerFingerprint(_)
        ));

        let fps_21: Vec<String> = (0..21).map(|i| format!("fp{i}")).collect();
        assert!(matches!(
            plan_summarization(&fps_21, &jobs),
            SummarizationPlan::Systemic(_)
        ));
    }

    #[test]
    fn plan_summarization_systemic_at_4_shared_jobs_not_at_3() {
        let fps = vec!["a".to_string()];
        let jobs_3: Vec<FailingJob> = (0..3)
            .map(|i| FailingJob {
                job_name: format!("job{i}"),
                log_tail_signature: "shared".into(),
            })
            .collect();
        assert!(matches!(
            plan_summarization(&fps, &jobs_3),
            SummarizationPlan::PerFingerprint(_)
        ));

        let jobs_4: Vec<FailingJob> = (0..4)
            .map(|i| FailingJob {
                job_name: format!("job{i}"),
                log_tail_signature: "shared".into(),
            })
            .collect();
        assert!(matches!(
            plan_summarization(&fps, &jobs_4),
            SummarizationPlan::Systemic(_)
        ));
    }

    #[test]
    fn plan_summarization_systemic_picks_3_most_frequent() {
        let fps = vec![
            "common".to_string(),
            "common".to_string(),
            "common".to_string(),
            "second".to_string(),
            "second".to_string(),
            "third".to_string(),
            "third".to_string(),
            "rare".to_string(),
        ];
        let fps_padded: Vec<String> = fps
            .into_iter()
            .chain((0..13).map(|i| format!("filler{i}")))
            .collect();
        let jobs: Vec<FailingJob> = Vec::new();
        let plan = plan_summarization(&fps_padded, &jobs);
        assert_eq!(
            plan,
            SummarizationPlan::Systemic(vec![
                "common".to_string(),
                "second".to_string(),
                "third".to_string()
            ])
        );
    }

    // -- token budgeting ----------------------------------------------------

    #[test]
    fn estimate_tokens_matches_ceil_bytes_over_3_2() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1); // ceil(1/3.2) = 1
        assert_eq!(estimate_tokens(3), 1); // ceil(3/3.2) = 1
        assert_eq!(estimate_tokens(4), 2); // ceil(4/3.2) = 2 (1.25 -> 2)
        assert_eq!(estimate_tokens(32), 10); // ceil(32/3.2) = 10 exactly
        assert_eq!(estimate_tokens(33), 11);
    }

    #[test]
    fn default_budget_matches_doc_table() {
        assert_eq!(DEFAULT_BUDGET.system_prompt, 1_200);
        assert_eq!(DEFAULT_BUDGET.run_metadata, 600);
        assert_eq!(DEFAULT_BUDGET.failing_tests, 8_000);
        assert_eq!(DEFAULT_BUDGET.log_tail, 7_000);
        assert_eq!(DEFAULT_BUDGET.diff, 7_000);
        assert_eq!(DEFAULT_BUDGET.max_input_tokens(), 23_800);
        assert_eq!(DEFAULT_BUDGET.output, 1_200);
    }

    #[test]
    fn scale_budget_unchanged_for_default_or_larger_window() {
        assert_eq!(
            scale_budget_for_model(DEFAULT_CONTEXT_WINDOW),
            DEFAULT_BUDGET
        );
        assert_eq!(scale_budget_for_model(200_000), DEFAULT_BUDGET);
    }

    #[test]
    fn scale_budget_scales_proportionally_for_smaller_window() {
        // qwen2.5-coder-32b-instruct: 32,768 tokens (ai.md).
        let scaled = scale_budget_for_model(32_768);
        let ratio = 32_768.0 / 128_000.0;
        assert_eq!(scaled.system_prompt, ((1_200.0 * ratio) as u32));
        assert_eq!(scaled.run_metadata, ((600.0 * ratio) as u32));
        assert_eq!(scaled.failing_tests, ((8_000.0 * ratio) as u32));
        assert_eq!(scaled.log_tail, ((7_000.0 * ratio) as u32));
        assert_eq!(scaled.diff, ((7_000.0 * ratio) as u32));
        assert_eq!(scaled.output, ((1_200.0 * ratio) as u32));
        // Sanity: strictly smaller than the default table.
        assert!(scaled.max_input_tokens() < DEFAULT_BUDGET.max_input_tokens());
    }

    #[test]
    fn allot_budget_unused_flows_down_the_priority_list() {
        // run_metadata needs far less than its 600 cap; the surplus should
        // flow to failing_tests, which needs more than its own 8,000 cap.
        let actual = ClassSizes {
            run_metadata: 100,
            failing_tests: 8_300,
            log_tail: 7_000,
            diff: 7_000,
        };
        let allotment = allot_budget(&DEFAULT_BUDGET, &actual);
        assert_eq!(allotment.run_metadata, 100);
        // 500 unused from run_metadata flows down: 8,000 + 500 = 8,500 >= 8,300 need.
        assert_eq!(allotment.failing_tests, 8_300);
        // No surplus left for log_tail/diff; both get exactly their need since it matches cap.
        assert_eq!(allotment.log_tail, 7_000);
        assert_eq!(allotment.diff, 7_000);
        assert_eq!(allotment.system_prompt, DEFAULT_BUDGET.system_prompt);
        assert_eq!(allotment.output, DEFAULT_BUDGET.output);
    }

    #[test]
    fn allot_budget_caps_need_exceeding_cap_plus_carry() {
        let actual = ClassSizes {
            run_metadata: 0,
            failing_tests: 100_000,
            log_tail: 0,
            diff: 0,
        };
        let allotment = allot_budget(&DEFAULT_BUDGET, &actual);
        // 600 (unused run_metadata) + 8,000 cap = 8,600, capped there.
        assert_eq!(allotment.failing_tests, 8_600);
        assert_eq!(allotment.log_tail, 0);
        assert_eq!(allotment.diff, 0);
    }

    // -- structural truncation ----------------------------------------------

    #[test]
    fn truncate_log_tail_keeps_the_end_not_the_start() {
        let lines = ["line one", "line two", "line three", "line four"];
        // Budget enough for only the last 2 short lines.
        let budget =
            estimate_tokens("line three".len() + 1) + estimate_tokens("line four".len() + 1);
        let kept = truncate_log_tail(&lines, budget);
        assert_eq!(
            kept,
            vec!["line three".to_string(), "line four".to_string()]
        );
    }

    #[test]
    fn truncate_stack_trace_keeps_head_and_tail() {
        let frames: Vec<String> = (0..20).map(|i| format!("frame {i}")).collect();
        let frame_refs: Vec<&str> = frames.iter().map(|s| s.as_str()).collect();
        let per_frame = estimate_tokens("frame 0".len() + 1);
        let budget = per_frame * 4; // room for ~4 frames total (head + tail)
        let kept = truncate_stack_trace(&frame_refs, budget);
        assert_eq!(kept[0], "frame 0");
        assert_eq!(kept[kept.len() - 1], "frame 19");
        assert!(kept.iter().any(|l| l.contains("omitted")));
    }

    #[test]
    fn truncate_stack_trace_returns_all_frames_when_budget_sufficient() {
        let frames = ["a", "b", "c"];
        let kept = truncate_stack_trace(&frames, 1_000);
        assert_eq!(
            kept,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn truncate_diff_omits_dropped_hunks_with_exact_placeholder_format() {
        let hunks = vec![
            DiffHunk {
                path: "src/user.ts".into(),
                added: 3,
                removed: 1,
                lines: vec!["@@ -1,1 +1,3 @@".into(), "+line".into()],
            },
            DiffHunk {
                path: "src/big.ts".into(),
                added: 400,
                removed: 200,
                lines: vec!["x".repeat(10_000)],
            },
        ];
        let budget =
            estimate_tokens("@@ -1,1 +1,3 @@".len() + 1) + estimate_tokens("+line".len() + 1);
        let out = truncate_diff(&hunks, budget);
        assert_eq!(
            out,
            vec![
                "@@ -1,1 +1,3 @@".to_string(),
                "+line".to_string(),
                "[hunk omitted: src/big.ts, +400/-200]".to_string(),
            ]
        );
    }

    // -- output-schema validation --------------------------------------------

    fn sample_summary() -> FailureSummary {
        FailureSummary {
            headline: "createUser now awaits hash()".into(),
            likely_cause: "mock returns a plain string".into(),
            evidence: vec![
                Evidence {
                    kind: EvidenceKind::Stack,
                    reference: "src/user.ts:44".into(),
                },
                Evidence {
                    kind: EvidenceKind::Diff,
                    reference: "src/user.ts".into(),
                },
            ],
            next_step: "make the mock return Promise.resolve(\"x\")".into(),
            confidence: Confidence::High,
            category: Category::TestAssertion,
        }
    }

    #[test]
    fn validate_output_passes_through_when_every_ref_occurs_in_input() {
        let summary = sample_summary();
        let inputs = [
            "stack trace mentions src/user.ts:44 here",
            "diff touches src/user.ts",
        ];
        let validated = validate_output(summary.clone(), &inputs);
        assert_eq!(validated, summary);
    }

    #[test]
    fn validate_output_drops_only_the_dangling_ref() {
        let mut summary = sample_summary();
        summary.evidence.push(Evidence {
            kind: EvidenceKind::Log,
            reference: "nonexistent/path.ts:99".into(),
        });
        let inputs = [
            "stack trace mentions src/user.ts:44 here",
            "diff touches src/user.ts",
        ];
        let validated = validate_output(summary, &inputs);
        assert_eq!(validated.evidence.len(), 2);
        assert!(
            validated
                .evidence
                .iter()
                .all(|e| e.reference != "nonexistent/path.ts:99")
        );
        // Confidence unaffected: not every reference was dropped.
        assert_eq!(validated.confidence, Confidence::High);
    }

    #[test]
    fn validate_output_forces_confidence_low_when_every_ref_dropped() {
        let summary = sample_summary();
        let inputs = ["nothing in here matches any evidence ref at all"];
        let validated = validate_output(summary, &inputs);
        assert!(validated.evidence.is_empty());
        assert_eq!(validated.confidence, Confidence::Low);
    }

    #[test]
    fn validate_output_leaves_already_empty_evidence_untouched() {
        let mut summary = sample_summary();
        summary.evidence.clear();
        summary.confidence = Confidence::Medium;
        let validated = validate_output(summary, &["anything"]);
        assert!(validated.evidence.is_empty());
        assert_eq!(validated.confidence, Confidence::Medium);
    }

    #[test]
    fn validate_schema_lengths_accepts_within_caps_and_rejects_over() {
        let ok = sample_summary();
        assert_eq!(validate_schema_lengths(&ok), Ok(()));

        let mut too_long_headline = sample_summary();
        too_long_headline.headline = "x".repeat(141);
        assert_eq!(
            validate_schema_lengths(&too_long_headline),
            Err(SchemaValidationError::HeadlineTooLong)
        );

        let mut too_long_cause = sample_summary();
        too_long_cause.likely_cause = "x".repeat(601);
        assert_eq!(
            validate_schema_lengths(&too_long_cause),
            Err(SchemaValidationError::LikelyCauseTooLong)
        );

        let mut too_long_next_step = sample_summary();
        too_long_next_step.next_step = "x".repeat(301);
        assert_eq!(
            validate_schema_lengths(&too_long_next_step),
            Err(SchemaValidationError::NextStepTooLong)
        );
    }

    #[test]
    fn failure_summary_schema_round_trips_through_serde_json() -> Result<(), serde_json::Error> {
        let summary = sample_summary();
        let json = serde_json::to_string(&summary)?;
        let parsed: FailureSummary = serde_json::from_str(&json)?;
        assert_eq!(parsed, summary);
        assert!(json.contains("\"kind\":\"stack\""));
        assert!(json.contains("\"ref\":\"src/user.ts:44\""));
        assert!(json.contains("\"confidence\":\"high\""));
        assert!(json.contains("\"category\":\"test_assertion\""));
        Ok(())
    }
}
