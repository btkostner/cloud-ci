//! PR comment rendering: `PrReport` context, the built-in default
//! template, and byte-budget truncation
//! ([docs/design/pr-comment.md](../../../docs/design/pr-comment.md)'s
//! "Template rendering", "Comment layout (mockup)", "Scopes (monorepo)",
//! and "Size budget and truncation" sections).
//!
//! **Scope of this round**, same posture as `template_spike.rs`,
//! `github_checks.rs`, and `oidc.rs` before their own live-wiring rounds:
//! this module defines the real `PrReport` context and renders it into
//! markdown matching the documented mockup, with the documented byte
//! budget enforced. It is **not** wired into any webhook, Durable
//! Object, or `RunCoordinator` — pr-comment.md's "Design" section
//! (`PullRequestState`, debounce/coalescing alarms, `notify_dirty`,
//! slash commands, checkbox actions) describes a large, separate live
//! system that does not exist yet. Nothing in `lib.rs` constructs a
//! `PrReport` from real run data this round; every `PrReport` in this
//! module's tests is built by hand, standalone.
//!
//! Pure Rust, no Workers-runtime API use: `minijinja` already proved it
//! runs on this Worker's `wasm32` target in `template_spike.rs`, so this
//! module (and this round) is fully `cargo test`-provable with no live
//! `wrangler dev` smoke run needed.
//!
//! # `PrReport` field provenance and modeling choices
//!
//! pr-comment.md's "Template rendering" section names the top-level
//! fields literally: `runs`, `pipelines`, `checks`, `scopes`, `tests`,
//! `failures`, `coverage`, `perf`, `reports`, `deployments`, `links`,
//! `actions`, plus `all_passed`. This module's [`PrReport`] carries all
//! of them, but several needed a documented choice because "exact field
//! shapes are finalized alongside the `PrReport` proto" per that same
//! section:
//!
//! - **`tests`/`failures`/`coverage` live inside [`Scope`], not as flat
//!   top-level lists.** The "Scopes (monorepo)" section is explicit that
//!   "the context's `scopes` field groups `tests`, `failures`,
//!   `coverage`, and `reports` by scope", and the mockup only ever shows
//!   these grouped per scope (`#### web — 2 failed`, then its own tests/
//!   coverage lines, then `api`'s own collapsed block). A genuinely
//!   top-level, ungrouped `tests`/`failures`/`coverage` would duplicate
//!   that data for no renderer benefit, so this module nests them under
//!   [`Scope`] and treats the doc's top-level field *names* as satisfied
//!   by that nesting (a single-scope repo still gets one implicit
//!   `Scope`, per that section's last sentence).
//! - **`reports`/`deployments`/`flaky` are flat, not scope-grouped**,
//!   even though "Aggregation" says reports are "grouped by scope": the
//!   mockup itself renders one combined `#### Reports` and
//!   `#### Previews` section, not one per scope. This module keeps the
//!   doc's literal per-scope grouping out of [`Scope`] (nothing forces a
//!   future custom template to keep reports flat — it would need the
//!   per-scope association pr-comment.md describes, which is a gap this
//!   round's built-in template doesn't need to close) and flattens for
//!   its own built-in rendering, matching the mockup exactly. `flaky`
//!   is not named in the doc's top-level field list at all (only
//!   discussed in "Aggregation" and rendered in the mockup's `#### Flaky`
//!   section); this module adds it as a top-level field for the same
//!   reason `reports`/`deployments` are top-level despite being, in
//!   principle, scope-relative.
//! - **Coverage's baseline (`vs `abc1234` on `main``) is a field on
//!   [`CoverageReport`]`, not a separate comparison table.** The
//!   "Aggregation" section's coverage paragraph describes a head report
//!   matched to a base report by stable identity, falling back to
//!   `(approximate base)` or `no base report`; this module models only
//!   what the mockup actually renders (a sha/branch pair) via
//!   [`Baseline`], leaving `(approximate base)`/`no base report` labels
//!   as a future caller's job (not exercised by any mockup text).
//! - **`runs`/`pipelines`/`checks`/`perf` are modeled but not rendered**
//!   by the built-in template. pr-comment.md says outright that "the
//!   built-in template does not render [perf]" and never shows pipeline/
//!   check/run lists in the mockup at all (the header's job counts are
//!   the only run-derived thing shown). This module keeps minimal,
//!   honestly-incomplete shapes for these four ([`RunSummary`],
//!   [`PipelineSummary`], [`CheckSummary`], [`PerfSummary`]) so the
//!   field names exist on `PrReport` per the doc, but does not invent
//!   rendering for them — a future template that wants them has the
//!   data, the built-in one does not use it.
//! - **The header's job counts are a precomputed [`JobCounts`]**, not
//!   derived in the template from `runs`/`checks` by iterating job
//!   state. This follows the exact precedent `all_passed` itself sets
//!   (a precomputed derived value the template just reads) and avoids
//!   inventing an unspecified per-job-status shape inside `runs` only to
//!   immediately reduce it back to three counters.
//! - **List fields are caller-ordered, not re-sorted here.** "Failures:
//!   ... ordered by (required check first, first-failure-on-PR first,
//!   job DAG order, test name)" and "Flaky: ... keep top 10 by flake
//!   rate" are both about *what order the data arrives in*, not a
//!   renderer-side sort. [`render_pr_report`] and [`truncate_markdown`]
//!   both only ever take prefixes of these lists — building a `PrReport`
//!   with pre-sorted `failures`/`flaky` is the caller's job, exactly
//!   like `cloud_ci_reports`'s parsers hand back `Vec<TestCase>` in
//!   document order rather than re-sorting themselves.
//!
//! # Why this isn't built on `cloud_ci_reports::TestSuites`
//!
//! `cloud_ci_reports::{TestSuites, TestCase, Outcome}` is the parsed
//! shape of one JUnit/Vitest/Playwright report file — it exists to
//! answer "what did this one file say happened", with `Option<Attempt>`
//! retry history and a `Failure` carrying a raw `stack_trace`. A
//! `PrReport`'s `Scope::tests`/`Scope::failures` answer a different
//! question: "what should the sticky comment say about this scope,
//! merged across every shard and every report file, trimmed to render-
//! sized text" — [`TestTotals`] is a `TestSuite`'s four counters
//! collapsed to the ones the mockup actually shows (no `time` on every
//! case, no `system_out`), and [`Failure`] here is the post-merge,
//! post-ordering, post-size-capping (message/stack already trimmed to
//! the doc's per-failure byte caps) presentation of a cross-shard
//! failure cluster, not any single `<testcase>` element. Reusing
//! `cloud_ci_reports`'s types directly would either lose that
//! aggregation step or force a parallel "comment view" wrapper around
//! them anyway, so this module defines its own small, comment-shaped
//! types and leaves the merge-from-`TestSuites`-into-`PrReport` step
//! (D1 aggregation, not pure data mapping) to the real "Aggregation"
//! round this one explicitly excludes.
//!
//! # Size budget and truncation
//!
//! pr-comment.md's "Size budget and truncation" section's own citation:
//! GitHub's REST API returns `body is too long (maximum is 65536
//! characters)` on an oversized issue-comment body (observed error,
//! sourced in that section to
//! <https://github.com/orgs/community/discussions/41331> and
//! <https://github.com/renovatebot/renovate/issues/15850>, not documented
//! in GitHub's REST docs themselves), and whether that limit counts
//! characters or bytes is `[unverified]` per a GitHub docs issue
//! (<https://github.com/github/docs/issues/35252>) the same section
//! cites — so the doc budgets in **UTF-8 bytes** ("the byte length is at
//! least the character length, so a byte cap is safe under either
//! interpretation") and sets the PR comment body's hard cap at **60,000
//! bytes** (the doc's own table, not the raw 65,536 — 60,000 is this
//! round's `MAX_BODY_BYTES`).
//!
//! The doc's renderer is section-based: render fully, then "walk the
//! output looking for complete markdown blocks ... in template-declared
//! priority order, so truncation only ever removes whole blocks and
//! can't leave an open fence or an unclosed `<details>`". This module
//! implements "template-declared priority" literally: [`DEFAULT_TEMPLATE`]
//! wraps each candidate block in a hidden HTML-comment marker
//! (`<!--cc:f idx=.. line=..-->...<!--/cc:f-->` for a failure,
//! `<!--cc:c-->...<!--/cc:c-->` for a scope's coverage block, `<!--cc:r-->`
//! for the Reports section, `<!--cc:fl idx=..-->` per flaky entry,
//! `<!--cc:t-->` per scope's tests-summary line), and [`truncate_markdown`]
//! parses those markers back out of the *rendered markdown* (never the
//! template) to apply the doc's priority table: failures first (first 10
//! kept full, next 40 collapsed to the one-line form embedded as the
//! marker's own `line=` base64 attribute — computed by the template at
//! render time from the same `Failure` the full block came from, then
//! read back by the truncator as plain text already present in the
//! rendered output, not re-templated), then whole-section drops in the
//! doc's own fallback order (flaky, then reports collapsed to one link,
//! then coverage, then forcing every remaining failure to its one-line
//! form). The header (marker line, title, base/links line) is never
//! touched, so the `[Full report](...)` link in it — present on every
//! non-placeholder render — satisfies the Goals section's "the full
//! untruncated report is always one click away" without the truncator
//! needing to add anything itself. If the body is still over budget
//! after every drop (the doc's "should not happen" final guard), this
//! module falls back to the minimal header-only form and reports
//! `overflow: true` on [`RenderedReport`] for the (not-yet-existing)
//! caller to log `comment_render_overflow`.
//!
//! Not implemented this round, and explicitly out of this task's scope:
//! the doc's "Untrusted content escaping" section (MiniJinja
//! `escape_formatter` hook, `<!--`/`-->` neutralization, `@`-mention
//! defusing) and repo-override template loading (`pr_comment.template`
//! in settings.yml) — neither is named in this round's three numbered
//! scope items (context struct, built-in template, byte budget).

use base64::Engine;
use minijinja::Environment;
use serde::Serialize;

#[derive(Debug, PartialEq, Eq)]
pub struct PrCommentError(String);

impl std::fmt::Display for PrCommentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PrCommentError {}

// --- Context data model -------------------------------------------------

/// `links` (doc's literal top-level field name): the Dashboard link
/// (always present) and the Full report link (absent only on the very
/// first placeholder render, before any run/scope data exists to link a
/// full report page for).
#[derive(Debug, Clone, Serialize)]
pub struct Links {
    pub dashboard_url: String,
    pub full_report_url: Option<String>,
}

/// Precomputed header job counts — see module docs' "The header's job
/// counts are a precomputed `JobCounts`" bullet.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct JobCounts {
    pub passed: u32,
    pub failed: u32,
    pub running: u32,
}

/// `runs` (doc's top-level field name): present on [`PrReport`] per the
/// doc's field list, not rendered by the built-in template (see module
/// docs). Minimal shape: enough to identify a run, nothing the built-in
/// template needs beyond [`JobCounts`].
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub run_key: String,
    pub external: bool,
}

/// `pipelines` (doc's top-level field name): not rendered by the
/// built-in template (see module docs).
#[derive(Debug, Clone, Serialize)]
pub struct PipelineSummary {
    pub name: String,
}

/// `checks` (doc's top-level field name): not rendered by the built-in
/// template (see module docs).
#[derive(Debug, Clone, Serialize)]
pub struct CheckSummary {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub url: Option<String>,
}

/// `perf` (doc's top-level field name): "job wall-time deltas and
/// benchmark report deltas, plus the run's critical path" — modeled
/// minimally since the built-in template never renders it (Aggregation:
/// "the built-in template does not render it").
#[derive(Debug, Clone, Serialize)]
pub struct JobDelta {
    pub job: String,
    pub delta_seconds: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerfSummary {
    pub job_deltas: Vec<JobDelta>,
    pub critical_path: Vec<String>,
}

/// A shard identifier (`Shard 3/8` in the mockup). A plain `(u32, u32)`
/// tuple serializes to a JSON array, which MiniJinja can only index as
/// `f.shard.0`/`.1` via bracket syntax, not attribute syntax — a named
/// struct keeps the template readable (`f.shard.index`/`.total`).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Shard {
    pub index: u32,
    pub total: u32,
}

/// One failed test cluster, post-merge and post-ordering (see module
/// docs' "Why this isn't built on `cloud_ci_reports::TestSuites`").
/// `message`/`stack`/`ai_summary` are expected already trimmed to the
/// doc's per-failure byte caps (message 1,000 bytes, stack 2,000 bytes,
/// AI summary 800 bytes) by the caller that builds the `PrReport` — this
/// module's truncator operates on whole failures, not on trimming an
/// individual failure's fields further.
#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub job: String,
    pub external: bool,
    pub test: String,
    pub ai_summary: Option<String>,
    pub message: String,
    pub stack: Option<String>,
    pub shard: Option<Shard>,
    pub attempt: u32,
    pub first_failure_on_pr: bool,
    pub log_url: Option<String>,
    pub history_url: Option<String>,
}

/// A scope's merged test totals. `duration` is pre-formatted
/// (`"11m 04s total across shards"`) rather than a raw `Duration`,
/// since the mockup's exact wording ("total across shards") is a
/// presentation detail this module doesn't need to own twice.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TestTotals {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub flaky: u32,
    pub duration: Option<String>,
    /// Aggregation: "While shards are still running, partial totals are
    /// shown and labelled `partial`."
    pub partial: bool,
}

/// `vs `abc1234` on `main`` — see module docs' baseline bullet.
#[derive(Debug, Clone, Serialize)]
pub struct Baseline {
    pub sha: String,
    pub branch: String,
}

/// One named, scoped coverage report (`unit` in the mockup).
#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    pub report_name: String,
    pub lines_pct: f64,
    pub lines_delta: f64,
    pub branches_pct: Option<f64>,
    pub branches_delta: Option<f64>,
    pub baseline: Option<Baseline>,
}

/// One monorepo scope ("Scopes (monorepo)"): groups `tests`,
/// `failures`, and `coverage`. A single-package repo gets one implicit
/// `Scope` (that section's last sentence) — callers name it however
/// they like (e.g. the repo name), since there's no "unscoped" marker
/// value this module needs to special-case.
#[derive(Debug, Clone, Serialize)]
pub struct Scope {
    pub name: String,
    pub tests: TestTotals,
    pub failures: Vec<Failure>,
    pub coverage: Vec<CoverageReport>,
}

/// One entry in the `#### Reports` section.
#[derive(Debug, Clone, Serialize)]
pub struct ReportLink {
    pub label: String,
    pub url: String,
}

/// `deployment` report shape from
/// [byo-ci.md](../../../docs/design/byo-ci.md)'s "Deployments
/// (previews)" section: `name`, `preview_url`, optional `inspect_url`,
/// optional `scope`. `inspect_url` is carried for a future template that
/// wants it; the built-in template links `name` to `preview_url` only,
/// matching the mockup's Previews section.
#[derive(Debug, Clone, Serialize)]
pub struct Deployment {
    pub name: String,
    pub preview_url: String,
    pub inspect_url: Option<String>,
    pub scope: Option<String>,
}

/// One `#### Flaky` bullet.
#[derive(Debug, Clone, Serialize)]
pub struct FlakyEntry {
    pub job: String,
    pub test: String,
    /// Pre-formatted detail text (`"failed attempt 1, passed attempt 2.
    /// Flake rate on \`main\` over 30 days: 4.1%."` in the mockup) — the
    /// exact composition of this sentence isn't specified field-by-field
    /// anywhere in pr-comment.md, so this module takes it as already
    /// composed, the same way `TestTotals::duration` is pre-formatted.
    pub detail: String,
}

/// One checkbox action descriptor ("Checkbox actions").
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub id: String,
    pub label: String,
}

/// "Stale-sha handling": the one-line `Previous head` footer kept after
/// a force-push.
#[derive(Debug, Clone, Serialize)]
pub struct PreviousHead {
    pub sha: String,
    pub failed: u32,
    pub passed: u32,
    pub superseded: bool,
}

/// The full `PrReport` template context (pr-comment.md's "Template
/// rendering" section's field list; see module docs for every
/// underspecified-shape decision this struct makes).
#[derive(Debug, Clone, Serialize)]
pub struct PrReport {
    pub repo_id: u64,
    pub pr_number: u64,
    pub head_sha: String,
    pub base_branch: String,
    pub base_sha: String,
    pub links: Links,
    /// True only for the very first render of a head sha, before any
    /// job has reported anything ("The comment is posted the instant
    /// the first run ... is created, before any job has reported
    /// anything"). Kept as an explicit flag rather than inferred from
    /// zero `job_counts`, since "zero of everything" is also a valid
    /// (if unusual) terminal state this module shouldn't conflate with
    /// the placeholder.
    pub is_placeholder: bool,
    pub job_counts: JobCounts,
    pub all_passed: bool,
    /// Pre-formatted (`"18:42:10 UTC"`). `None` on the placeholder
    /// render (the mockup's first example has no `updated` clause).
    pub updated_at: Option<String>,
    pub scopes: Vec<Scope>,
    pub reports: Vec<ReportLink>,
    pub deployments: Vec<Deployment>,
    pub flaky: Vec<FlakyEntry>,
    pub actions: Vec<Action>,
    pub previous_head: Option<PreviousHead>,
    pub runs: Vec<RunSummary>,
    pub pipelines: Vec<PipelineSummary>,
    pub checks: Vec<CheckSummary>,
    pub perf: Option<PerfSummary>,
}

// --- Rendering -----------------------------------------------------------

/// `GET`-free thousands separator (`4812` -> `"4,812"`) matching the
/// mockup's `4,812 passed`. Pure digit grouping, no locale.
fn format_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i != 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The built-in default template (pr-comment.md's "Comment layout
/// (mockup)"). See module docs for the `<!--cc:...-->` truncation
/// markers' meaning.
const DEFAULT_TEMPLATE: &str = r#"{%- macro coverage_text(cov) -%}
Lines **{{ cov.lines_pct|pct2 }}%** ({{ cov.lines_delta|signed2 }}){% if cov.branches_pct is not none %} · Branches **{{ cov.branches_pct|pct2 }}%** ({{ cov.branches_delta|signed2 }}){% endif %}{% if cov.baseline %} vs `{{ cov.baseline.sha }}` on `{{ cov.baseline.branch }}`{% endif %}
{%- endmacro -%}
{{ marker }}
### cloud-ci: {% if is_placeholder %}running{% elif all_passed %}all {{ job_counts.passed }} jobs passed{% else %}{{ job_counts.failed }} failed, {{ job_counts.passed }} passed{% if job_counts.running > 0 %}, {{ job_counts.running }} running{% endif %}{% endif %} on `{{ head_sha }}`

Base `{{ base_branch }}` @ `{{ base_sha }}`{% if updated_at %} · updated {{ updated_at }}{% endif %} · [Dashboard]({{ links.dashboard_url }}){% if links.full_report_url %} · [Full report]({{ links.full_report_url }}){% endif %}
{%- if not is_placeholder and all_passed %}
{% for scope in scopes %}{% for cov in scope.coverage %}
<!--cc:c-->**{{ scope.name }}** — Coverage (`{{ cov.report_name }}`): {{ coverage_text(cov) }}<!--/cc:c-->
{% endfor %}{% endfor -%}
{%- endif -%}
{%- if not is_placeholder and not all_passed %}
{% for scope in scopes %}{% if scope.failures %}
#### {{ scope.name }} — {{ scope.failures|length }} failed
{% for f in scope.failures %}{% set oneline = "**" ~ f.job ~ "**" ~ (" (external)" if f.external else "") ~ " · " ~ f.test ~ ": " ~ f.message %}
<!--cc:f idx={{ loop.index0 }} line={{ oneline|b64 }}-->
<details open><summary><code>{{ f.job }}</code>{% if f.external %} (external){% endif %} · {{ f.test }}</summary>

{% if f.ai_summary %}> **AI summary** (may be wrong): {{ f.ai_summary }}

{% endif -%}
```text
{{ f.message }}{% if f.stack %}
{{ f.stack }}{% endif %}
```
{% if f.shard %}Shard {{ f.shard.index }}/{{ f.shard.total }} · {% endif %}attempt {{ f.attempt }}{% if f.first_failure_on_pr %} · first failure on this PR{% endif %}{% if f.log_url %} · [log]({{ f.log_url }}){% endif %}{% if f.history_url %} · [history]({{ f.history_url }}){% endif %}
</details>
<!--/cc:f-->
{% endfor %}
<!--cc:t-->**Tests:** {{ scope.tests.passed|num }} passed · {% if scope.tests.failed > 0 %}**{{ scope.tests.failed|num }} failed**{% else %}{{ scope.tests.failed }} failed{% endif %}{% if scope.tests.skipped > 0 %} · {{ scope.tests.skipped|num }} skipped{% endif %}{% if scope.tests.flaky > 0 %} · {{ scope.tests.flaky }} flaky{% endif %}{% if scope.tests.duration %} · {{ scope.tests.duration }}{% endif %}{% if scope.tests.partial %} · partial{% endif %}<!--/cc:t-->
{% for cov in scope.coverage %}
<!--cc:c-->**Coverage** (`{{ cov.report_name }}`): {{ coverage_text(cov) }}<!--/cc:c-->
{% endfor %}
{% else %}
<details><summary>{{ scope.name }} — all {{ scope.tests.passed|num }} passed</summary>

<!--cc:t-->**Tests:** {{ scope.tests.passed|num }} passed · {{ scope.tests.failed }} failed<!--/cc:t-->
{% for cov in scope.coverage %}
<!--cc:c-->**Coverage** (`{{ cov.report_name }}`): {{ coverage_text(cov) }}<!--/cc:c-->
{% endfor %}
</details>
{% endif %}
{% endfor -%}
{% if reports %}
#### Reports
<!--cc:r-->{% for r in reports %}[{{ r.label }}]({{ r.url }}){% if not loop.last %} · {% endif %}{% endfor %}<!--/cc:r-->
{% endif -%}
{% if deployments %}
#### Previews
{% for d in deployments %}[{{ d.name }}]({{ d.preview_url }}){% if not loop.last %} · {% endif %}{% endfor %}
{% endif -%}
{% if flaky %}
#### Flaky
{% for entry in flaky %}
<!--cc:fl idx={{ loop.index0 }}-->- `{{ entry.job }}` {{ entry.test }}: {{ entry.detail }}<!--/cc:fl-->
{% endfor %}
{% endif -%}
{% if actions %}
#### Actions
{% for a in actions %}- [ ] {{ a.label }} <!-- cloud-ci:action {{ a.id }} -->
{% endfor %}
{% endif -%}
{% if previous_head %}
<sub>Previous head `{{ previous_head.sha }}`: {{ previous_head.failed }} failed, {{ previous_head.passed }} passed{% if previous_head.superseded %} (superseded){% endif %}. Commands: `/cloud-ci help`.</sub>
{% else %}
<sub>Commands: `/cloud-ci help`.</sub>
{% endif -%}
{%- endif -%}
"#;

fn build_environment() -> Result<Environment<'static>, PrCommentError> {
    let mut env = Environment::new();
    env.add_filter("pct2", |v: f64| -> String { format!("{v:.2}") });
    env.add_filter("signed2", |v: f64| -> String { format!("{v:+.2}") });
    env.add_filter("num", |v: i64| -> String {
        if v < 0 {
            format!("-{}", format_thousands((-v) as u64))
        } else {
            format_thousands(v as u64)
        }
    });
    env.add_filter("b64", |v: String| -> String {
        base64::engine::general_purpose::STANDARD.encode(v.as_bytes())
    });
    env.add_template("pr-comment", DEFAULT_TEMPLATE)
        .map_err(|err| PrCommentError(err.to_string()))?;
    Ok(env)
}

/// The rendered sticky comment body, plus whether it was truncated or
/// (should-not-happen) hit the final overflow guard. See module docs'
/// "Size budget and truncation".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedReport {
    pub markdown: String,
    pub truncated: bool,
    pub overflow: bool,
}

/// Renders `ctx` with [`DEFAULT_TEMPLATE`], then applies
/// [`truncate_markdown`]. This is the only entry point real callers
/// (none exist yet this round) should use.
pub fn render_pr_report(ctx: &PrReport) -> Result<RenderedReport, PrCommentError> {
    let env = build_environment()?;
    let tmpl = env
        .get_template("pr-comment")
        .map_err(|err| PrCommentError(err.to_string()))?;

    #[derive(Serialize)]
    struct TemplateCtx<'a> {
        #[serde(flatten)]
        report: &'a PrReport,
        marker: String,
    }
    let marker = crate::github_checks::sticky_comment_marker(ctx.repo_id, ctx.pr_number);
    let value = minijinja::Value::from_serialize(TemplateCtx {
        report: ctx,
        marker,
    });
    let rendered = tmpl
        .render(value)
        .map_err(|err| PrCommentError(err.to_string()))?;

    Ok(truncate_markdown(rendered, ctx))
}

// --- Truncation ------------------------------------------------------

/// GitHub's own documented 65,536-character ceiling (see module docs'
/// citation), reduced to cloud-ci's own stricter internal byte budget
/// per pr-comment.md's "Size budget and truncation" table.
pub const MAX_BODY_BYTES: usize = 60_000;

const FAILURE_FULL_KEPT: usize = 10;
const FAILURE_ONE_LINE_KEPT: usize = 40;

enum TaggedKind {
    Failure { oneline: String },
    Coverage,
    Reports,
    Flaky,
    Tests,
}

struct Tagged {
    kind: TaggedKind,
    whole: String,
    replacement: Option<String>,
}

enum Segment {
    Literal(String),
    Tagged(Tagged),
}

fn segment_bytes(seg: &Segment) -> usize {
    match seg {
        Segment::Literal(s) => s.len(),
        Segment::Tagged(t) => t.replacement.as_deref().unwrap_or(&t.whole).len(),
    }
}

fn total_bytes(segments: &[Segment]) -> usize {
    segments.iter().map(segment_bytes).sum()
}

fn render_segments(segments: &[Segment]) -> String {
    let mut out = String::with_capacity(total_bytes(segments));
    for seg in segments {
        match seg {
            Segment::Literal(s) => out.push_str(s),
            Segment::Tagged(t) => out.push_str(t.replacement.as_deref().unwrap_or(&t.whole)),
        }
    }
    out
}

/// Decodes a failure marker's `line=<base64>` attribute, embedded by
/// [`DEFAULT_TEMPLATE`] at render time from the same [`Failure`] the
/// full `<details>` block came from. Returns an empty string on any
/// malformed input rather than failing the whole truncation pass — a
/// best-effort one-line fallback beats aborting truncation entirely.
fn decode_oneline(open_tag: &str) -> String {
    let Some(start) = open_tag.find("line=") else {
        return String::new();
    };
    let b64 = &open_tag[start + "line=".len()..open_tag.len().saturating_sub(3)];
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

/// Parses `markdown`'s `<!--cc:...-->` markers (see module docs) into an
/// ordered list of literal text and tagged blocks. Markers are expected
/// non-nested and in document order, which [`DEFAULT_TEMPLATE`]
/// guarantees by construction (one marker kind per loop iteration, never
/// two kinds wrapping the same span).
fn parse_segments(markdown: &str) -> Vec<Segment> {
    const OPEN_F: &str = "<!--cc:f ";
    const CLOSE_F: &str = "<!--/cc:f-->";
    const OPEN_C: &str = "<!--cc:c-->";
    const CLOSE_C: &str = "<!--/cc:c-->";
    const OPEN_R: &str = "<!--cc:r-->";
    const CLOSE_R: &str = "<!--/cc:r-->";
    const OPEN_FL: &str = "<!--cc:fl ";
    const CLOSE_FL: &str = "<!--/cc:fl-->";
    const OPEN_T: &str = "<!--cc:t-->";
    const CLOSE_T: &str = "<!--/cc:t-->";

    let mut segments = Vec::new();
    let mut pos = 0usize;
    while pos < markdown.len() {
        let rest = &markdown[pos..];
        let candidates = [
            rest.find(OPEN_F),
            rest.find(OPEN_C),
            rest.find(OPEN_R),
            rest.find(OPEN_FL),
            rest.find(OPEN_T),
        ];
        let Some(next_rel) = candidates.iter().filter_map(|c| *c).min() else {
            segments.push(Segment::Literal(rest.to_string()));
            break;
        };
        if next_rel > 0 {
            segments.push(Segment::Literal(rest[..next_rel].to_string()));
        }
        let abs_start = pos + next_rel;
        let tail = &markdown[abs_start..];

        let (kind, open_len, close_tag) = if tail.starts_with(OPEN_F) {
            let Some(open_end) = tail.find("-->") else {
                segments.push(Segment::Literal(tail.to_string()));
                break;
            };
            let open_tag = &tail[..open_end + 3];
            (
                TaggedKind::Failure {
                    oneline: decode_oneline(open_tag),
                },
                open_tag.len(),
                CLOSE_F,
            )
        } else if tail.starts_with(OPEN_C) {
            (TaggedKind::Coverage, OPEN_C.len(), CLOSE_C)
        } else if tail.starts_with(OPEN_R) {
            (TaggedKind::Reports, OPEN_R.len(), CLOSE_R)
        } else if tail.starts_with(OPEN_FL) {
            let Some(open_end) = tail.find("-->") else {
                segments.push(Segment::Literal(tail.to_string()));
                break;
            };
            (TaggedKind::Flaky, open_end + 3, CLOSE_FL)
        } else {
            (TaggedKind::Tests, OPEN_T.len(), CLOSE_T)
        };

        let Some(close_rel) = tail[open_len..].find(close_tag) else {
            // Unterminated marker: treat the rest as literal rather than
            // panic — should not happen given DEFAULT_TEMPLATE always
            // pairs open/close tags.
            segments.push(Segment::Literal(tail.to_string()));
            break;
        };
        let close_end = open_len + close_rel + close_tag.len();
        let whole = tail[..close_end].to_string();
        segments.push(Segment::Tagged(Tagged {
            kind,
            whole,
            replacement: None,
        }));
        pos = abs_start + close_end;
    }
    segments
}

/// Applies pr-comment.md's "Size budget and truncation" rules to
/// `rendered` (already-produced markdown, never the template — see
/// module docs). Returns the (possibly unchanged) final body.
fn truncate_markdown(rendered: String, ctx: &PrReport) -> RenderedReport {
    if rendered.len() <= MAX_BODY_BYTES {
        return RenderedReport {
            markdown: rendered,
            truncated: false,
            overflow: false,
        };
    }

    let link_target = ctx
        .links
        .full_report_url
        .clone()
        .unwrap_or_else(|| ctx.links.dashboard_url.clone());

    let mut segments = parse_segments(&rendered);

    // Step 1: failures soft-budget degradation (first 10 full, next 40
    // one-line, rest dropped with an "and N more" note).
    let failure_positions: Vec<usize> = segments
        .iter()
        .enumerate()
        .filter_map(|(i, s)| match s {
            Segment::Tagged(t) => matches!(t.kind, TaggedKind::Failure { .. }).then_some(i),
            Segment::Literal(_) => None,
        })
        .collect();
    let total_failures = failure_positions.len();

    for (rank, &pos) in failure_positions.iter().enumerate() {
        if rank < FAILURE_FULL_KEPT {
            continue;
        }
        let Segment::Tagged(t) = &mut segments[pos] else {
            unreachable!()
        };
        let TaggedKind::Failure { oneline } = &t.kind else {
            unreachable!()
        };
        if rank < FAILURE_FULL_KEPT + FAILURE_ONE_LINE_KEPT {
            t.replacement = Some(format!("- {oneline}\n"));
        } else {
            t.replacement = Some(String::new());
        }
    }
    if total_failures > FAILURE_FULL_KEPT + FAILURE_ONE_LINE_KEPT {
        let dropped = total_failures - (FAILURE_FULL_KEPT + FAILURE_ONE_LINE_KEPT);
        let note = format!("- and {dropped} more failures — [Full report]({link_target})\n",);
        let insert_after = failure_positions[FAILURE_FULL_KEPT + FAILURE_ONE_LINE_KEPT - 1];
        segments.insert(insert_after + 1, Segment::Literal(note));
    }
    if total_bytes(&segments) <= MAX_BODY_BYTES {
        return RenderedReport {
            markdown: render_segments(&segments),
            truncated: true,
            overflow: false,
        };
    }

    // Step 2: drop the whole Flaky section (doc's fallback drop order:
    // flaky, reports, coverage, then force all failures one-line).
    for seg in &mut segments {
        if let Segment::Tagged(t) = seg
            && matches!(t.kind, TaggedKind::Flaky)
        {
            t.replacement = Some(String::new());
        }
    }
    if total_bytes(&segments) <= MAX_BODY_BYTES {
        return RenderedReport {
            markdown: render_segments(&segments),
            truncated: true,
            overflow: false,
        };
    }

    // Step 3: collapse Reports to a single link (doc: "a single Reports
    // link stays").
    for seg in &mut segments {
        if let Segment::Tagged(t) = seg
            && matches!(t.kind, TaggedKind::Reports)
        {
            t.replacement = Some(format!("[Full report]({link_target})"));
        }
    }
    if total_bytes(&segments) <= MAX_BODY_BYTES {
        return RenderedReport {
            markdown: render_segments(&segments),
            truncated: true,
            overflow: false,
        };
    }

    // Step 4: drop coverage.
    for seg in &mut segments {
        if let Segment::Tagged(t) = seg
            && matches!(t.kind, TaggedKind::Coverage)
        {
            t.replacement = Some(String::new());
        }
    }
    if total_bytes(&segments) <= MAX_BODY_BYTES {
        return RenderedReport {
            markdown: render_segments(&segments),
            truncated: true,
            overflow: false,
        };
    }

    // Step 5: force every remaining failure (including the first 10) to
    // its one-line form.
    for (rank, &pos) in failure_positions.iter().enumerate() {
        if rank >= FAILURE_FULL_KEPT + FAILURE_ONE_LINE_KEPT {
            continue; // already dropped in step 1
        }
        let Segment::Tagged(t) = &mut segments[pos] else {
            unreachable!()
        };
        let TaggedKind::Failure { oneline } = &t.kind else {
            unreachable!()
        };
        t.replacement = Some(format!("- {oneline}\n"));
    }
    if total_bytes(&segments) <= MAX_BODY_BYTES {
        return RenderedReport {
            markdown: render_segments(&segments),
            truncated: true,
            overflow: false,
        };
    }

    // Final guard: should not happen. Minimal header-only form.
    let header_end = rendered.find("<!--cc:").unwrap_or(rendered.len());
    let header = &rendered[..header_end];
    let minimal = format!("{header}\n*Report truncated — [Full report]({link_target})*\n",);
    RenderedReport {
        markdown: minimal,
        truncated: true,
        overflow: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholder_report() -> PrReport {
        PrReport {
            repo_id: 8812,
            pr_number: 412,
            head_sha: "def5678".to_string(),
            base_branch: "main".to_string(),
            base_sha: "abc1234".to_string(),
            links: Links {
                dashboard_url: "https://ci.example.com/acme/web/pull/412".to_string(),
                full_report_url: None,
            },
            is_placeholder: true,
            job_counts: JobCounts::default(),
            all_passed: false,
            updated_at: None,
            scopes: vec![],
            reports: vec![],
            deployments: vec![],
            flaky: vec![],
            actions: vec![],
            previous_head: None,
            runs: vec![],
            pipelines: vec![],
            checks: vec![],
            perf: None,
        }
    }

    #[test]
    fn renders_exact_placeholder_mockup() -> Result<(), PrCommentError> {
        let ctx = placeholder_report();
        let out = render_pr_report(&ctx)?;
        assert!(!out.truncated);
        assert!(!out.overflow);
        let expected = "<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->\n\
### cloud-ci: running on `def5678`\n\
\n\
Base `main` @ `abc1234` · [Dashboard](https://ci.example.com/acme/web/pull/412)";
        assert_eq!(out.markdown.trim_end(), expected);
        Ok(())
    }

    fn mid_run_report() -> PrReport {
        PrReport {
            repo_id: 8812,
            pr_number: 412,
            head_sha: "def5678".to_string(),
            base_branch: "main".to_string(),
            base_sha: "abc1234".to_string(),
            links: Links {
                dashboard_url: "https://ci.example.com/acme/web/pull/412".to_string(),
                full_report_url: Some(
                    "https://ci.example.com/acme/web/pull/412/report/def5678".to_string(),
                ),
            },
            is_placeholder: false,
            job_counts: JobCounts {
                passed: 14,
                failed: 2,
                running: 1,
            },
            all_passed: false,
            updated_at: Some("18:42:10 UTC".to_string()),
            scopes: vec![
                Scope {
                    name: "web".to_string(),
                    tests: TestTotals {
                        passed: 4812,
                        failed: 2,
                        skipped: 37,
                        flaky: 1,
                        duration: Some("11m 04s total across shards".to_string()),
                        partial: false,
                    },
                    failures: vec![
                        Failure {
                            job: "ci / unit".to_string(),
                            external: false,
                            test: "src/cart/total.test.ts › applies discount › rounds half-even"
                                .to_string(),
                            ai_summary: Some(
                                "`roundHalfEven` now gets the pre-tax subtotal because of the \
                                 reordering in `src/cart/total.ts:41`; the expected value \
                                 assumes post-tax."
                                    .to_string(),
                            ),
                            message: "AssertionError: expected 10.05 to equal 10.04".to_string(),
                            stack: Some("    at src/cart/total.test.ts:88:21".to_string()),
                            shard: Some(Shard { index: 3, total: 8 }),
                            attempt: 1,
                            first_failure_on_pr: true,
                            log_url: Some("https://ci.example.com/l/r_9f2/unit-3#L210".to_string()),
                            history_url: Some(
                                "https://ci.example.com/acme/web/tests/t_77a".to_string(),
                            ),
                        },
                        Failure {
                            job: "gha/build".to_string(),
                            external: true,
                            test: "api/handlers_test.go › TestCreateOrder/duplicate_id".to_string(),
                            ai_summary: None,
                            message: "duplicate id".to_string(),
                            stack: None,
                            shard: None,
                            attempt: 1,
                            first_failure_on_pr: false,
                            log_url: None,
                            history_url: None,
                        },
                    ],
                    coverage: vec![CoverageReport {
                        report_name: "unit".to_string(),
                        lines_pct: 84.12,
                        lines_delta: 0.31,
                        branches_pct: Some(71.02),
                        branches_delta: Some(-0.40),
                        baseline: Some(Baseline {
                            sha: "abc1234".to_string(),
                            branch: "main".to_string(),
                        }),
                    }],
                },
                Scope {
                    name: "api".to_string(),
                    tests: TestTotals {
                        passed: 312,
                        failed: 0,
                        skipped: 0,
                        flaky: 0,
                        duration: None,
                        partial: false,
                    },
                    failures: vec![],
                    coverage: vec![CoverageReport {
                        report_name: "unit".to_string(),
                        lines_pct: 91.0,
                        lines_delta: 0.0,
                        branches_pct: None,
                        branches_delta: None,
                        baseline: None,
                    }],
                },
            ],
            reports: vec![
                ReportLink {
                    label: "Playwright report (merged)".to_string(),
                    url: "https://assets.example.com/s/acme/web/pr-412/latest/playwright/"
                        .to_string(),
                },
                ReportLink {
                    label: "Vitest HTML".to_string(),
                    url: "https://assets.example.com/s/acme/web/pr-412/latest/vitest/".to_string(),
                },
                ReportLink {
                    label: "Coverage HTML".to_string(),
                    url: "https://assets.example.com/s/acme/web/pr-412/latest/coverage/"
                        .to_string(),
                },
                ReportLink {
                    label: "12 artifacts".to_string(),
                    url: "https://ci.example.com/acme/web/runs/r_9f2/artifacts".to_string(),
                },
            ],
            deployments: vec![
                Deployment {
                    name: "storybook".to_string(),
                    preview_url: "https://chromatic.com/build?appId=...&number=412".to_string(),
                    inspect_url: None,
                    scope: None,
                },
                Deployment {
                    name: "docs".to_string(),
                    preview_url: "https://docs-pr-412.pages.dev".to_string(),
                    inspect_url: None,
                    scope: None,
                },
            ],
            flaky: vec![FlakyEntry {
                job: "e2e".to_string(),
                test: "checkout.spec.ts › pays with card".to_string(),
                detail: "failed attempt 1, passed attempt 2. Flake rate on `main` over 30 days: \
                          4.1%."
                    .to_string(),
            }],
            actions: vec![
                Action {
                    id: "rerun_failed".to_string(),
                    label: "Rerun failed jobs".to_string(),
                },
                Action {
                    id: "autofix".to_string(),
                    label: "Autofix".to_string(),
                },
            ],
            previous_head: Some(PreviousHead {
                sha: "9a0c1e2".to_string(),
                failed: 3,
                passed: 13,
                superseded: true,
            }),
            runs: vec![],
            pipelines: vec![],
            checks: vec![],
            perf: None,
        }
    }

    #[test]
    fn renders_mid_run_mockup_structure() -> Result<(), PrCommentError> {
        let ctx = mid_run_report();
        let out = render_pr_report(&ctx)?;
        assert!(!out.truncated);
        let md = out.markdown;

        assert!(md.starts_with("<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->\n"));
        assert!(md.contains("### cloud-ci: 2 failed, 14 passed, 1 running on `def5678`"));
        assert!(md.contains(
            "Base `main` @ `abc1234` · updated 18:42:10 UTC · \
             [Dashboard](https://ci.example.com/acme/web/pull/412) · \
             [Full report](https://ci.example.com/acme/web/pull/412/report/def5678)"
        ));

        // Failing scope: expanded, with AI summary and error text.
        assert!(md.contains("#### web — 2 failed"));
        assert!(md.contains("<details open><summary><code>ci / unit</code> · src/cart/total.test.ts › applies discount › rounds half-even</summary>"));
        assert!(md.contains(
            "> **AI summary** (may be wrong): `roundHalfEven` now gets the pre-tax subtotal"
        ));
        assert!(md.contains("AssertionError: expected 10.05 to equal 10.04"));
        assert!(md.contains("    at src/cart/total.test.ts:88:21"));
        assert!(md.contains("Shard 3/8 · attempt 1 · first failure on this PR · [log](https://ci.example.com/l/r_9f2/unit-3#L210) · [history](https://ci.example.com/acme/web/tests/t_77a)"));
        assert!(md.contains("<code>gha/build</code> (external) · api/handlers_test.go › TestCreateOrder/duplicate_id"));

        assert!(md.contains("**Tests:** 4,812 passed · **2 failed** · 37 skipped · 1 flaky · 11m 04s total across shards"));
        assert!(md.contains("**Coverage** (`unit`): Lines **84.12%** (+0.31) · Branches **71.02%** (-0.40) vs `abc1234` on `main`"));

        // Passing scope: collapsed one-liner.
        assert!(md.contains("<details><summary>api — all 312 passed</summary>"));
        assert!(md.contains("**Tests:** 312 passed · 0 failed"));
        assert!(md.contains("**Coverage** (`unit`): Lines **91.00%** (+0.00)"));

        assert!(md.contains("#### Reports"));
        assert!(md.contains("[Playwright report (merged)](https://assets.example.com/s/acme/web/pr-412/latest/playwright/)"));
        assert!(
            md.contains("[12 artifacts](https://ci.example.com/acme/web/runs/r_9f2/artifacts)")
        );

        assert!(md.contains("#### Previews"));
        assert!(md.contains("[storybook](https://chromatic.com/build?appId=...&number=412)"));
        assert!(md.contains("[docs](https://docs-pr-412.pages.dev)"));

        assert!(md.contains("#### Flaky"));
        assert!(md.contains("- `e2e` checkout.spec.ts › pays with card: failed attempt 1, passed attempt 2. Flake rate on `main` over 30 days: 4.1%."));

        assert!(md.contains("#### Actions"));
        assert!(md.contains("- [ ] Rerun failed jobs <!-- cloud-ci:action rerun_failed -->"));
        assert!(md.contains("- [ ] Autofix <!-- cloud-ci:action autofix -->"));

        assert!(md.contains("<sub>Previous head `9a0c1e2`: 3 failed, 13 passed (superseded). Commands: `/cloud-ci help`.</sub>"));

        Ok(())
    }

    #[test]
    fn all_passed_collapses_to_header_and_coverage_one_liners() -> Result<(), PrCommentError> {
        let mut ctx = mid_run_report();
        ctx.all_passed = true;
        ctx.job_counts = JobCounts {
            passed: 17,
            failed: 0,
            running: 0,
        };
        for scope in &mut ctx.scopes {
            scope.failures.clear();
        }
        let out = render_pr_report(&ctx)?;
        let md = out.markdown;

        assert!(md.contains("### cloud-ci: all 17 jobs passed on `def5678`"));
        assert!(md.contains("**web** — Coverage (`unit`): Lines **84.12%** (+0.31)"));
        assert!(md.contains("**api** — Coverage (`unit`): Lines **91.00%** (+0.00)"));

        // Everything else omitted.
        assert!(!md.contains("#### web"));
        assert!(!md.contains("<details"));
        assert!(!md.contains("**Tests:**"));
        assert!(!md.contains("#### Reports"));
        assert!(!md.contains("#### Previews"));
        assert!(!md.contains("#### Flaky"));
        assert!(!md.contains("#### Actions"));
        assert!(!md.contains("Previous head"));
        Ok(())
    }

    #[test]
    fn truncates_oversized_report_and_keeps_full_report_link() -> Result<(), PrCommentError> {
        let mut ctx = mid_run_report();
        // Blow the budget with many failures in the web scope.
        let template_failure = ctx.scopes[0].failures[0].clone();
        ctx.scopes[0].failures = (0..200)
            .map(|i| Failure {
                test: format!("{} #{i}", template_failure.test),
                ..template_failure.clone()
            })
            .collect();
        ctx.job_counts.failed = 200;

        let out = render_pr_report(&ctx)?;
        assert!(out.truncated);
        assert!(!out.overflow);
        assert!(out.markdown.len() <= MAX_BODY_BYTES);
        // Escape hatch: the full report link always survives.
        assert!(
            out.markdown
                .contains("[Full report](https://ci.example.com/acme/web/pull/412/report/def5678)")
        );
        // First 10 failures stay fully expanded.
        assert!(out.markdown.contains("<details open>"));
        // Beyond 50, failures collapse to an "and N more" note.
        assert!(out.markdown.contains("more failures"));
        Ok(())
    }
}
