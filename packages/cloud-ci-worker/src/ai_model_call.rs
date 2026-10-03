//! The real `env.AI.run()` call this crate's AI pipeline stopped short of
//! in the prior round (see `src/ai_queue.rs`'s module doc comment — that
//! round wrote `ai_insight` rows with `status = 'pending_model_call'` and
//! went no further). This module is the pure half of closing that gap:
//! the versioned prompt template (`PROMPT_SUMMARY_V1`), the chat
//! `messages` builder from a stored `ai_insight.context_json` row, the
//! retry-with-appended-error message builder, and response parsing +
//! validation (reusing [`crate::ai_insight::validate_schema_lengths`]/
//! [`crate::ai_insight::validate_output`] verbatim, never duplicating
//! them). The Workers-runtime-only half — the D1 scan for
//! `pending_model_call` rows, the real `env.AI.run()` call, and the
//! status-transition writes — lives in `src/lib.rs`'s
//! `ai_model_call_pass` function, called from the `#[event(scheduled)]`
//! handler (see that module's own doc comment for why a cron pass, not a
//! Queue consumer, drives this round — the prior round's Queue message is
//! already consumed by the time a row reaches `pending_model_call`; there
//! is no second message to re-trigger on).
//!
//! # The `context_json` this module builds messages from is pre-redacted
//!
//! Every `ai_insight.context_json` row this module reads
//! ([`StoredInsightContext`]) was built by `ai_queue::assemble_failure_context`,
//! which redacts every failing-test string through
//! [`crate::ai_redact::redact`] *before* writing anything to D1 (see that
//! function's own doc comment and `ai_redact`'s module doc comment). This
//! module therefore never needs its own redaction pass, and never calls
//! [`crate::ai_redact::redact`] itself: by the time a row reaches
//! `pending_model_call`, redaction has already happened, unconditionally,
//! as part of writing the row — there is no "redact later" flag or
//! second gate here, because there is nothing left to redact.
//!
//! # `env.AI.run()`'s real call shape (confirmed against the pinned crate)
//!
//! `worker-0.8.7`'s `src/ai.rs` (the crates.io source, not patched by this
//! repo's `[patch.crates-io]` fork — that patch only touches Container
//! startup fields per `docs/adr/0011-patching-third-party-crates.md`, not
//! `ai.rs`) defines:
//!
//! ```ignore
//! impl Ai {
//!     pub async fn run<T: Serialize, U: DeserializeOwned>(
//!         &self, model: impl AsRef<str>, input: T,
//!     ) -> Result<U> { ... }
//! }
//! ```
//!
//! Exactly two arguments after `self` (model, input) — confirmed by
//! reading the method body: it calls `self.0.run(model.as_ref(),
//! serde_wasm_bindgen::to_value(&input)?)` on the underlying
//! `worker_sys::Ai` binding, a 2-argument JS call. ai.md's "### AI
//! Gateway" section's own `[unverified]` note ("Whether `run` accepts the
//! third `{ gateway }` options argument is `[unverified]`") is now
//! resolved: **it does not**. This round therefore calls `env.AI.run()`
//! with no gateway options at all — `AI_GATEWAY_ID` wiring stays exactly
//! the "separate, later round" follow-up ai.md's own text already
//! anticipates for the no-3-argument-`run` branch, not a gap this round
//! silently introduces.
//!
//! `Env::ai(&self, binding: &str) -> Result<Ai>` (`worker-0.8.7`'s
//! `src/env.rs`) is the accessor; this crate calls it as `env.ai("AI")?`
//! against the `[ai]` `binding = "AI"` entry `wrangler.toml` now declares.
//!
//! The model/input/output shapes themselves are confirmed against
//! `@cf/openai/gpt-oss-120b`'s own Cloudflare docs page (checked
//! 2026-10-02, https://developers.cloudflare.com/workers-ai/models/gpt-oss-120b/)
//! and its raw `sync-input.json`/a sibling model's `sync-output.json`
//! schema (the `gpt-oss-120b` page's own `sync-output.json` is a bare
//! `{"type":"object"}` with no `properties`, but
//! `llama-3.3-70b-instruct-fp8-fast`'s sibling schema — same Workers AI
//! text-generation output convention, and the shape ai.md's own "Cost
//! controls" section already assumes with its "`usage` field the model
//! returns (`prompt_tokens`, `completion_tokens`)" language — spells out
//! `{"response": string, "usage": {"prompt_tokens", "completion_tokens",
//! "total_tokens"}, "tool_calls": [...]}`; [`ChatResponse`]/[`ChatUsage`]
//! below mirror that exactly, `tool_calls` omitted since this module
//! never sends `tools`):
//!
//! - **Input** (the `Messages` variant of the model's input schema):
//!   `{"messages": [{"role", "content"}, ...], "response_format": {"type":
//!   "json_object"}, "temperature": 0.2, "max_tokens": <int>}` — no
//!   `prompt` field (the `Messages`/`Prompt` input shapes are
//!   mutually exclusive `oneOf` branches in the model's own
//!   `sync-input.json`).
//! - **Output**: `{"response": "<model's raw text>", "usage": {
//!   "prompt_tokens", "completion_tokens", "total_tokens"}}`.
//!
//! # Retry-with-appended-error mechanism
//!
//! ai.md: "If the output fails schema validation, the call is retried
//! once with the validation error appended." This module implements
//! "appended" as two additional chat turns on top of the original
//! `[system, user]` pair: an `assistant` turn carrying the model's
//! invalid raw response verbatim, then a `user` turn describing exactly
//! which schema rule it broke and asking for a corrected JSON object —
//! not a mutation of the original user message. This is the practical
//! shape for `env.AI.run()`'s real call signature confirmed above: `run`
//! takes one `input` value per call, and `messages` is itself an array,
//! so "append the error" naturally means "send a longer `messages` array
//! on the second call", preserving the full conversation (including what
//! the model actually said) rather than silently discarding its first
//! attempt. [`build_retry_messages`] builds exactly this array; the
//! caller (`src/lib.rs`) is responsible for the two separate
//! `env.AI.run()` calls themselves — this module never calls it, staying
//! `worker`-free so `cargo test` can exercise it directly.

use serde::{Deserialize, Serialize};

use crate::ai_insight::{self, FailureSummary, SchemaValidationError};
use crate::ai_queue::FailingTestEntry;

/// Deploy-time default for the summary model (ai.md's "### Model
/// selection" table: "Summaries, perf, autofix | `@cf/openai/gpt-oss-120b`").
/// Overridable per-deployment via the `AI_MODEL_SUMMARY` `wrangler.toml`
/// var (empty string, this round's placeholder, means "use this
/// default") — ai.md's "Deploy-time defaults are set in wrangler `vars`"
/// sentence, applied to the one model this round actually calls.
/// Per-repo overrides (`ai.model_summary` in `.cloud-ci/settings.yml`)
/// are not wired this round — same "Settings integration" gap
/// `ai_queue.rs`'s module docs already name for the budget cap, now
/// inherited by model selection too, since both need the same
/// not-yet-built settings-fetch path.
pub const DEFAULT_SUMMARY_MODEL: &str = "@cf/openai/gpt-oss-120b";

/// `prompt_version` stored on every `ai_insight` row this round's model
/// call writes (replacing the prior round's `"unversioned"` placeholder —
/// see `src/lib.rs`'s `handle_analysis_requested`). ai.md: "Prompt
/// templates live in `cloud-ci-worker` as versioned constants
/// (`PROMPT_SUMMARY_V1`, and so on). `prompt_version` is part of the
/// cache key and of the stored insight, so a template change never
/// serves stale cached output." Bumping this string is the whole
/// contract for "a template change" — nothing else needs to change for a
/// future `PROMPT_SUMMARY_V2` to invalidate old cache entries.
pub const PROMPT_VERSION: &str = "summary_v1";

/// The system prompt, instructing the model to return JSON matching
/// [`FailureSummary`]'s exact field names/types (ai.md's "### Prompting
/// and output contract" schema, reproduced here field-for-field,
/// including the `serde(rename_all)` wire values for `evidence[].kind`/
/// `confidence`/`category` — `EvidenceKind`/`Confidence` are
/// `rename_all = "lowercase"`, `Category` is `rename_all = "snake_case"`,
/// both already enforced by `ai_insight.rs`'s own `Deserialize` impls, so
/// this prompt's enum value lists below are not an independent copy that
/// could drift from the type — they are the literal wire strings those
/// `serde` attributes produce).
pub const PROMPT_SUMMARY_V1: &str = r#"You explain why one CI test failed, for display in a GitHub pull request comment. You are given one failing test's id, a normalized failure message (numbers/hashes/UUIDs/paths already replaced with placeholders), its stack frames, and its captured stdout/stderr tail.

Respond with exactly one JSON object and nothing else — no markdown code fences, no commentary before or after it — matching this schema:

{
  "headline": "string, at most 140 characters: a one-line summary of what failed",
  "likely_cause": "string, at most 600 characters: your best explanation of why it failed, citing specific files/lines/frames from the input when you can",
  "evidence": [{"kind": "stack" | "log" | "diff" | "history", "ref": "a path or line that appears verbatim, character-for-character, in the input you were given, e.g. src/user.ts:44"}],
  "next_step": "string, at most 300 characters: one concrete suggested next step",
  "confidence": "low" | "medium" | "high",
  "category": "test_assertion" | "build" | "dependency" | "infra" | "timeout" | "oom" | "flaky" | "unknown"
}

Rules: every `evidence[].ref` must be an exact substring of the input you were given — never invent a file path, line number, or fact that is not present in the input. If you are not confident in your explanation, set `confidence` to "low". If you cannot find any useful evidence, return an empty `evidence` array rather than inventing one."#;

/// The stored shape of `ai_insight.context_json` written by the prior
/// round's `handle_analysis_requested` (`src/lib.rs`): `{"entry": ...,
/// "systemic": ..., "selected_fingerprints": ..., "model_context_window":
/// ..., "failing_tests_token_estimate": ..., "failing_tests_token_allotment":
/// ...}`. Field names/types mirror that call site's `serde_json::json!`
/// literal exactly — this struct exists so this module can deserialize
/// that JSON back into typed data rather than re-parsing it ad hoc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredInsightContext {
    pub entry: FailingTestEntry,
    pub systemic: bool,
    pub selected_fingerprints: Vec<String>,
    pub model_context_window: u32,
    pub failing_tests_token_estimate: u32,
    pub failing_tests_token_allotment: u32,
}

/// One chat message — `env.AI.run()`'s `messages[]` input shape
/// (`{"role", "content"}`, confirmed above).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// `response_format`'s `"json_object"` mode (ai.md: "requests JSON
/// through `response_format`"; the model's own `sync-input.json` lists
/// `"json_object"`/`"json_schema"` as the two `type` values — this module
/// uses the simpler `"json_object"` mode, not `"json_schema"`, since
/// `FailureSummary`'s validation is already done in Rust by
/// [`ai_insight::validate_schema_lengths`]/[`ai_insight::validate_output`]
/// rather than delegated to the model provider's own schema enforcement,
/// which ai.md itself flags as `[unverified]` per alternative model).
#[derive(Debug, Clone, Serialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub format_type: String,
}

/// `env.AI.run()`'s full input value for this round's call — the `T` in
/// `Ai::run<T: Serialize, U: DeserializeOwned>`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub response_format: ResponseFormat,
}

/// ai.md's "`temperature: 0.2`", verbatim.
pub const SUMMARY_TEMPERATURE: f64 = 0.2;

/// Builds the full request for one `env.AI.run()` call from an already
/// -assembled `messages` array — shared by both the first attempt
/// ([`build_summary_messages`]) and the retry
/// ([`build_retry_messages`]). `max_tokens` is
/// [`ai_insight::scale_budget_for_model`]'s `output` slot for `context`'s
/// own `model_context_window` — the same scaling the context builder
/// already applied to the *input* budget, now applied to the *output*
/// cap too, so a smaller-context-window model (an operator override)
/// never gets asked for more output tokens than its own budget table
/// allows.
pub fn build_chat_request(
    messages: Vec<ChatMessage>,
    context: &StoredInsightContext,
) -> ChatRequest {
    let budget = ai_insight::scale_budget_for_model(context.model_context_window);
    ChatRequest {
        messages,
        temperature: SUMMARY_TEMPERATURE,
        max_tokens: budget.output,
        response_format: ResponseFormat {
            format_type: "json_object".to_string(),
        },
    }
}

/// `env.AI.run()`'s usage sub-object (confirmed shape above):
/// `prompt_tokens`/`completion_tokens`/`total_tokens`, each defaulting to
/// `0` server-side per the sibling model's own schema description
/// ("default: 0") — `#[serde(default)]` mirrors that here rather than
/// failing to deserialize a response that omits the field entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ChatUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

/// `env.AI.run()`'s full output value — the `U` in `Ai::run<T, U>`.
/// `usage` is `Option` because ai.md itself flags "the `usage` field the
/// model returns" as `[unverified: field presence per model]`; a
/// response that omits it entirely still deserializes (as `None`) rather
/// than failing the whole call.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub response: String,
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

/// Builds every input segment [`ai_insight::validate_output`] checks
/// `evidence[].ref` substrings against — every piece of text this
/// round's prompt actually sends the model (ai.md: "`input_segments` is
/// every piece of text actually sent to the model"). Mirrors
/// [`user_prompt`]'s own field selection so a `ref` the model copied
/// verbatim from what it read always validates.
pub fn input_segments(context: &StoredInsightContext) -> Vec<String> {
    let mut segments = vec![
        context.entry.test_id.clone(),
        context.entry.normalized_message.clone(),
        context.entry.output_tail.clone(),
    ];
    segments.extend(context.entry.frames.iter().cloned());
    segments
}

/// The user message's content: a plain-text rendering of
/// [`StoredInsightContext`]'s one [`FailingTestEntry`] plus the systemic
/// -failure note ai.md's pipeline already decided
/// ([`crate::ai_insight::plan_summarization`], upstream of this module).
pub fn user_prompt(context: &StoredInsightContext) -> String {
    let frames = if context.entry.frames.is_empty() {
        "(no stack frames available)".to_string()
    } else {
        context.entry.frames.join("\n")
    };
    let output_tail = if context.entry.output_tail.is_empty() {
        "(no captured output)".to_string()
    } else {
        context.entry.output_tail.clone()
    };
    let systemic_note = if context.systemic {
        format!(
            "This run was classified as a systemic failure: this is one of the {} most frequent \
             failure fingerprints among many failing tests, not the only failure in the run.\n\n",
            context.selected_fingerprints.len(),
        )
    } else {
        String::new()
    };
    format!(
        "{systemic_note}Failing test: {}\n\nNormalized failure message:\n{}\n\n\
         Stack frames (closest to the fault first):\n{frames}\n\n\
         Captured output (stdout/stderr tail):\n{output_tail}",
        context.entry.test_id, context.entry.normalized_message,
    )
}

/// The first attempt's full `[system, user]` message array.
pub fn build_summary_messages(context: &StoredInsightContext) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(PROMPT_SUMMARY_V1),
        ChatMessage::user(user_prompt(context)),
    ]
}

/// Human-readable description of [`SchemaValidationError`], used in the
/// retry message's appended error text. Not `Display` on the
/// `ai_insight` type itself — that type's crate boundary is "pure
/// decision logic", and this string is specifically prompt-facing
/// wording, not a general error message.
pub fn describe_schema_error(error: SchemaValidationError) -> &'static str {
    match error {
        SchemaValidationError::HeadlineTooLong => {
            "\"headline\" was longer than 140 characters. Shorten it."
        }
        SchemaValidationError::LikelyCauseTooLong => {
            "\"likely_cause\" was longer than 600 characters. Shorten it."
        }
        SchemaValidationError::NextStepTooLong => {
            "\"next_step\" was longer than 300 characters. Shorten it."
        }
    }
}

/// Builds the retry's full message array (module docs' "Retry-with
/// -appended-error mechanism"): `base` (the original `[system, user]`
/// pair) plus an `assistant` turn carrying the invalid raw response
/// verbatim, plus a `user` turn naming exactly which rule it broke and
/// asking for a corrected JSON object. `base` is taken by reference and
/// copied, not consumed — the caller still has the original pair
/// available for logging/storage without rebuilding it.
pub fn build_retry_messages(
    base: &[ChatMessage],
    previous_raw_response: &str,
    error_description: &str,
) -> Vec<ChatMessage> {
    let mut messages = base.to_vec();
    messages.push(ChatMessage::assistant(previous_raw_response));
    messages.push(ChatMessage::user(format!(
        "That response failed validation: {error_description} Return a corrected JSON object \
         only, with no markdown fences and no commentary, that fixes this and otherwise \
         follows the schema exactly as given.",
    )));
    messages
}

/// Prompt-facing description of a [`ModelResponseError`], covering both
/// variants [`describe_schema_error`] alone does not (an
/// [`ModelResponseError::InvalidJson`] has no [`SchemaValidationError`]
/// to describe) — the single function [`build_retry_messages`]'s caller
/// needs regardless of which variant the first attempt failed with.
pub fn describe_model_response_error(error: &ModelResponseError) -> String {
    match error {
        ModelResponseError::InvalidJson(detail) => {
            format!("the response was not valid JSON matching the schema ({detail}).")
        }
        ModelResponseError::Schema(schema_err) => describe_schema_error(*schema_err).to_string(),
    }
}

/// Why a raw model response could not become a validated
/// [`FailureSummary`] — either it was not valid JSON for the
/// [`FailureSummary`] shape at all, or it parsed but failed
/// [`ai_insight::validate_schema_lengths`]'s length caps. Evidence-ref
/// dropping ([`ai_insight::validate_output`]) is never a failure here —
/// per that function's own contract, it always returns a (possibly
/// evidence-stripped, possibly confidence-downgraded) summary rather
/// than an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelResponseError {
    /// The response was not valid JSON for the `FailureSummary` shape at
    /// all (missing/mistyped field, extra non-JSON text the model added
    /// despite the prompt's instruction not to, etc.). Carries
    /// `serde_json`'s error message for logging.
    InvalidJson(String),
    Schema(SchemaValidationError),
}

/// Strips a leading/trailing Markdown code fence (`` ```json ... ``` `` or
/// `` ``` ... ``` ``) from `raw`, if present, before JSON parsing — a
/// defensive step against gpt-oss-120b (or an operator-configured
/// alternative model) wrapping its JSON in a fence despite
/// [`PROMPT_SUMMARY_V1`]'s explicit "no markdown code fences" instruction;
/// `response_format: {"type": "json_object"}` is a request, not a
/// guarantee (ai.md: "whether each alternative model enforces the JSON
/// schema is `[unverified]`"). Returns `raw` unchanged if it does not
/// look like a fenced block.
pub fn strip_json_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let after_open = after_open
        .strip_prefix("json")
        .unwrap_or(after_open)
        .trim_start_matches(['\n', '\r']);
    match after_open.rfind("```") {
        Some(close_idx) => after_open[..close_idx].trim(),
        None => trimmed,
    }
}

/// Parses `raw` (one `env.AI.run()` response's `response` string) as a
/// [`FailureSummary`] and validates it: [`strip_json_fence`] defensively
/// unwraps an accidental code fence, `serde_json` parses the schema
/// shape, [`ai_insight::validate_schema_lengths`] checks the three
/// char-length caps, and on success
/// [`ai_insight::validate_output`] applies the evidence-ref-drop /
/// confidence-downgrade rule against `input_segments` before returning
/// the final, validated summary. Any failure before that last step
/// returns [`ModelResponseError`] instead — the caller's signal to
/// retry once (ai.md's rule).
pub fn parse_and_validate(
    raw: &str,
    input_segments: &[&str],
) -> Result<FailureSummary, ModelResponseError> {
    let cleaned = strip_json_fence(raw);
    let summary: FailureSummary = serde_json::from_str(cleaned)
        .map_err(|e| ModelResponseError::InvalidJson(e.to_string()))?;
    ai_insight::validate_schema_lengths(&summary).map_err(ModelResponseError::Schema)?;
    Ok(ai_insight::validate_output(summary, input_segments))
}

/// gpt-oss-120b's neurons-per-million-token rates (ai.md's "### Model
/// selection" table: "31,818 / 68,182 neurons per M" input/output,
/// checked 2026-09-30, https://developers.cloudflare.com/workers-ai/platform/pricing/).
const GPT_OSS_120B_NEURONS_PER_M_INPUT: u64 = 31_818;
const GPT_OSS_120B_NEURONS_PER_M_OUTPUT: u64 = 68_182;

/// Converts one `env.AI.run()` call's real `usage` into neurons, for
/// `ai_usage_daily.neuron_count` accounting (module docs' "Budget/usage
/// accounting" section). Returns `None` for any `model` other than
/// [`DEFAULT_SUMMARY_MODEL`] — this crate carries no capabilities table
/// mapping an operator-configured `AI_MODEL_SUMMARY` override to its own
/// neurons-per-token rate (ai.md's "Model selection" section describes
/// such a table as a future "static capabilities table (context window,
/// JSON mode, price per M tokens)" the worker does not have yet; see
/// this module's own doc comment's honest gap note), so usage for a
/// non-default model is left unaccounted rather than guessed at the
/// default model's rate.
pub fn neurons_for_usage(model: &str, usage: ChatUsage) -> Option<i64> {
    if model != DEFAULT_SUMMARY_MODEL {
        return None;
    }
    let input_neurons =
        (usage.prompt_tokens as u64 * GPT_OSS_120B_NEURONS_PER_M_INPUT).div_ceil(1_000_000);
    let output_neurons =
        (usage.completion_tokens as u64 * GPT_OSS_120B_NEURONS_PER_M_OUTPUT).div_ceil(1_000_000);
    Some((input_neurons + output_neurons) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_insight::{Category, Confidence};

    fn sample_context(systemic: bool, frames: Vec<&str>) -> StoredInsightContext {
        StoredInsightContext {
            entry: FailingTestEntry {
                test_id: "UserService > creates user".to_string(),
                fingerprint: "abc123".to_string(),
                normalized_message: "expected <num> but got <num>".to_string(),
                frames: frames.into_iter().map(str::to_string).collect(),
                output_tail: "TypeError: Cannot read properties of undefined".to_string(),
            },
            systemic,
            selected_fingerprints: vec!["abc123".to_string()],
            model_context_window: ai_insight::DEFAULT_CONTEXT_WINDOW,
            failing_tests_token_estimate: 100,
            failing_tests_token_allotment: 8_000,
        }
    }

    #[test]
    fn build_summary_messages_has_system_then_user() {
        let context = sample_context(false, vec!["src/user.ts:44"]);
        let messages = build_summary_messages(&context);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, PROMPT_SUMMARY_V1);
        assert_eq!(messages[1].role, "user");
        assert!(messages[1].content.contains("UserService > creates user"));
        assert!(messages[1].content.contains("src/user.ts:44"));
        assert!(!messages[1].content.contains("systemic"));
    }

    #[test]
    fn user_prompt_notes_systemic_failures() {
        let context = sample_context(true, vec!["src/user.ts:44"]);
        let prompt = user_prompt(&context);
        assert!(prompt.contains("systemic failure"));
    }

    #[test]
    fn user_prompt_handles_empty_frames_and_output() {
        let mut context = sample_context(false, vec![]);
        context.entry.output_tail = String::new();
        let prompt = user_prompt(&context);
        assert!(prompt.contains("no stack frames available"));
        assert!(prompt.contains("no captured output"));
    }

    #[test]
    fn build_chat_request_uses_default_temperature_and_json_mode() {
        let context = sample_context(false, vec![]);
        let request = build_chat_request(build_summary_messages(&context), &context);
        assert_eq!(request.temperature, 0.2);
        assert_eq!(request.response_format.format_type, "json_object");
        assert_eq!(request.max_tokens, ai_insight::DEFAULT_BUDGET.output);
    }

    #[test]
    fn build_chat_request_scales_max_tokens_for_smaller_model_window() {
        let mut context = sample_context(false, vec![]);
        context.model_context_window = 24_000; // half of DEFAULT_CONTEXT_WINDOW's 128,000
        let request = build_chat_request(build_summary_messages(&context), &context);
        assert!(request.max_tokens < ai_insight::DEFAULT_BUDGET.output);
    }

    #[test]
    fn input_segments_includes_every_entry_field() {
        let context = sample_context(false, vec!["frame one", "frame two"]);
        let segments = input_segments(&context);
        assert!(segments.contains(&"UserService > creates user".to_string()));
        assert!(segments.contains(&"expected <num> but got <num>".to_string()));
        assert!(segments.contains(&"frame one".to_string()));
        assert!(segments.contains(&"frame two".to_string()));
        assert!(segments.contains(&"TypeError: Cannot read properties of undefined".to_string()));
    }

    #[test]
    fn build_retry_messages_appends_assistant_and_error_turns() {
        let context = sample_context(false, vec![]);
        let base = build_summary_messages(&context);
        let description = describe_schema_error(SchemaValidationError::HeadlineTooLong);
        let retry = build_retry_messages(&base, "{\"headline\": \"x\"}", description);
        assert_eq!(retry.len(), 4);
        assert_eq!(retry[0].role, "system");
        assert_eq!(retry[1].role, "user");
        assert_eq!(retry[2].role, "assistant");
        assert_eq!(retry[2].content, "{\"headline\": \"x\"}");
        assert_eq!(retry[3].role, "user");
        assert!(retry[3].content.contains("140 characters"));
        // The original base messages are untouched (not consumed).
        assert_eq!(base.len(), 2);
    }

    #[test]
    fn describe_model_response_error_covers_both_variants() {
        let schema_err = describe_model_response_error(&ModelResponseError::Schema(
            SchemaValidationError::NextStepTooLong,
        ));
        assert!(schema_err.contains("300 characters"));
        let json_err =
            describe_model_response_error(&ModelResponseError::InvalidJson("eof".to_string()));
        assert!(json_err.contains("not valid JSON"));
        assert!(json_err.contains("eof"));
    }

    fn valid_summary_json(evidence_ref: &str) -> String {
        serde_json::json!({
            "headline": "createUser now awaits hash()",
            "likely_cause": "hash() changed to async but the mock still returns a string",
            "evidence": [{"kind": "stack", "ref": evidence_ref}],
            "next_step": "make the mock return a Promise",
            "confidence": "high",
            "category": "test_assertion",
        })
        .to_string()
    }

    #[test]
    fn parse_and_validate_accepts_well_formed_response_with_valid_evidence()
    -> Result<(), ModelResponseError> {
        let raw = valid_summary_json("src/user.ts:44");
        let segments = ["some text containing src/user.ts:44 inline"];
        let summary = parse_and_validate(&raw, &segments)?;
        assert_eq!(summary.confidence, Confidence::High);
        assert_eq!(summary.category, Category::TestAssertion);
        assert_eq!(summary.evidence.len(), 1);
        Ok(())
    }

    #[test]
    fn parse_and_validate_drops_unmatched_evidence_and_downgrades_confidence()
    -> Result<(), ModelResponseError> {
        let raw = valid_summary_json("src/nonexistent.ts:1");
        let segments = ["totally unrelated input text"];
        let summary = parse_and_validate(&raw, &segments)?;
        assert!(summary.evidence.is_empty());
        assert_eq!(summary.confidence, Confidence::Low);
        Ok(())
    }

    #[test]
    fn parse_and_validate_strips_markdown_fence() -> Result<(), ModelResponseError> {
        let fenced = format!("```json\n{}\n```", valid_summary_json("src/user.ts:44"));
        let segments = ["src/user.ts:44"];
        let summary = parse_and_validate(&fenced, &segments)?;
        assert_eq!(summary.headline, "createUser now awaits hash()");
        Ok(())
    }

    #[test]
    fn parse_and_validate_rejects_invalid_json() {
        assert!(matches!(
            parse_and_validate("not json at all", &[]),
            Err(ModelResponseError::InvalidJson(_))
        ));
    }

    #[test]
    fn parse_and_validate_rejects_schema_violation() {
        let raw = serde_json::json!({
            "headline": "x".repeat(200),
            "likely_cause": "ok",
            "evidence": [],
            "next_step": "ok",
            "confidence": "low",
            "category": "unknown",
        })
        .to_string();
        assert_eq!(
            parse_and_validate(&raw, &[]),
            Err(ModelResponseError::Schema(
                SchemaValidationError::HeadlineTooLong
            ))
        );
    }

    #[test]
    fn describe_schema_error_is_distinct_per_variant() {
        assert_ne!(
            describe_schema_error(SchemaValidationError::HeadlineTooLong),
            describe_schema_error(SchemaValidationError::LikelyCauseTooLong)
        );
        assert_ne!(
            describe_schema_error(SchemaValidationError::LikelyCauseTooLong),
            describe_schema_error(SchemaValidationError::NextStepTooLong)
        );
    }

    #[test]
    fn neurons_for_usage_computes_default_model_rate() {
        let usage = ChatUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
        };
        assert_eq!(
            neurons_for_usage(DEFAULT_SUMMARY_MODEL, usage),
            Some(31_818 + 68_182)
        );
    }

    #[test]
    fn neurons_for_usage_rounds_up_fractional_neurons() {
        let usage = ChatUsage {
            prompt_tokens: 1,
            completion_tokens: 0,
            total_tokens: 1,
        };
        // ceil(31_818 / 1_000_000) == 1, never 0 for nonzero usage.
        assert_eq!(neurons_for_usage(DEFAULT_SUMMARY_MODEL, usage), Some(1));
    }

    #[test]
    fn neurons_for_usage_unknown_for_non_default_model() {
        let usage = ChatUsage {
            prompt_tokens: 1_000,
            completion_tokens: 1_000,
            total_tokens: 2_000,
        };
        assert_eq!(
            neurons_for_usage("@cf/qwen/qwen2.5-coder-32b-instruct", usage),
            None
        );
    }

    #[test]
    fn stored_insight_context_round_trips_through_serde() -> Result<(), serde_json::Error> {
        let context = sample_context(true, vec!["frame a", "frame b"]);
        let json = serde_json::to_string(&context)?;
        let round_tripped: StoredInsightContext = serde_json::from_str(&json)?;
        assert_eq!(context, round_tripped);
        Ok(())
    }

    #[test]
    fn strip_json_fence_unwraps_json_tagged_fence() {
        assert_eq!(strip_json_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn strip_json_fence_unwraps_untagged_fence() {
        assert_eq!(strip_json_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn strip_json_fence_passes_through_unfenced_input() {
        assert_eq!(strip_json_fence("{\"a\":1}"), "{\"a\":1}");
    }
}
