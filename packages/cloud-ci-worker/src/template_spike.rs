//! Phase 0 spike: does `minijinja` (crate `minijinja`, stable `2.24.0`)
//! actually compile to and execute correctly on `wasm32-unknown-unknown`
//! inside this Worker's real runtime? (docs/roadmap.md Phase 0 spikes
//! table follows the same pattern as the resolved "GitHub App JWT: RS256
//! signing via WebCrypto from Rust" row; question originates in
//! docs/design/pr-comment.md's "Template rendering" section, which marks
//! minijinja's in-Worker execution `[unverified]` until this spike.)
//!
//! This is **not** the real PR-comment renderer. There is no `PrReport`
//! struct, no built-in template matching pr-comment.md's mockup, no
//! repo-override template loading, and no truncation/byte-budget logic
//! here — all of that is separate, later work. This module exists only
//! to answer the capability question and to be a reference for whoever
//! builds the real renderer.
//!
//! # Investigation
//!
//! `minijinja` 2.24.0's own `Cargo.toml`
//! (`minijinja-2.24.0/Cargo.toml`, verified 2026-10-02) shows its
//! `default` feature set (`builtins`, `debug`, `deserialization`,
//! `macros`, `multi_template`, `adjacent_loop_items`, `std_collections`,
//! `serde`) pulls in exactly two dependencies: `memo-map` (pure Rust, no
//! platform APIs) and `serde` (already a direct dependency of this
//! crate). None of the optional features this crate opts out of here
//! (`loader` — filesystem template loading, not needed since the
//! template is a compiled-in string this round; `json` — pulls in
//! `serde_json` only for its own `Value` JSON helpers, redundant with
//! this crate's own `serde_json` dependency and `Value::from_serialize`;
//! `urlencode` — pulls in `percent-encoding`, unused by this template;
//! `custom_syntax`/`fuel`/`preserve_order`/`unicode`/`speedups` — unused
//! knobs) are enabled. The one default feature deliberately dropped here
//! is `debug` (richer error messages with source snippets) — pure
//! overhead for a spike with no template-authoring UX to support yet.
//! Critically, **`stacker`** (`minijinja-2.24.0/Cargo.toml`'s
//! `[dependencies.stacker]`, optional, guards deep recursion with native
//! stack-probing via the `psm` crate's platform-specific assembly) is
//! never pulled in by any feature used here — `psm` is the kind of
//! dependency that would be the actual wasm32 risk, and this feature
//! selection avoids it entirely.
//!
//! # Result
//!
//! Compiles to `wasm32-unknown-unknown` (`cargo build --target
//! wasm32-unknown-unknown`) and, live under `wrangler dev`, actually
//! renders: variable interpolation, a `{% for %}` loop over a `Vec`, and
//! an `{% if %}/{% else %}` conditional all produced correct output
//! against a real `serde`-derived context converted via
//! `minijinja::Value::from_serialize` — see the debug route this spike
//! was wired to (removed before commit; see commit message for the
//! captured output). **Verdict: minijinja works on this Worker's wasm32
//! runtime**, with no WASI or browser-API assumptions anywhere in its
//! dependency graph at the feature set used here.

use minijinja::Environment;
use serde::Serialize;

/// Deliberately small stand-in for the real `PrReport` context
/// (docs/design/pr-comment.md's "Template rendering" section) — just
/// enough shape to exercise interpolation, a loop, and a conditional,
/// the same three constructs a real PR-comment template needs for its
/// header line and failures section.
#[derive(Debug, Clone, Serialize)]
pub struct SpikeFailure {
    pub job: String,
    pub test: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpikeContext {
    pub head_sha: String,
    pub all_passed: bool,
    pub total_jobs: u32,
    pub failures: Vec<SpikeFailure>,
}

/// A template string exercising the three Jinja2 constructs a real
/// PR-comment template needs (docs/design/pr-comment.md's mockup):
/// `{{ }}` interpolation for the header/sha, `{% if %}/{% else %}` for
/// the "all passed" collapse, and `{% for %}` over a list for the
/// per-failure lines.
const SPIKE_TEMPLATE: &str = "\
### cloud-ci: {% if all_passed %}all {{ total_jobs }} jobs passed{% else %}{{ failures|length }} failed, {{ total_jobs - failures|length }} passed{% endif %} on `{{ head_sha }}`
{% if not all_passed %}
{% for f in failures %}
- **{{ f.job }}** · {{ f.test }}: {{ f.message }}
{% endfor %}
{% endif %}";

#[derive(Debug, PartialEq, Eq)]
pub struct TemplateSpikeError(String);

impl std::fmt::Display for TemplateSpikeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TemplateSpikeError {}

/// Renders [`SPIKE_TEMPLATE`] against `ctx`, proving `minijinja::Environment`
/// construction, template registration, `Value::from_serialize`, and
/// rendering all work end to end. Pure Rust/no Workers-runtime API use —
/// unlike `github_app::sign_rs256`, this half doesn't need `wrangler dev`
/// to prove correctness (`cargo test` already does), but the question
/// this spike answers is whether it *compiles and runs under wasm32 at
/// all*, so it is still wired to a debug route for a live `wrangler dev`
/// smoke run (removed before commit).
pub fn render_spike(ctx: &SpikeContext) -> Result<String, TemplateSpikeError> {
    let mut env = Environment::new();
    env.add_template("spike", SPIKE_TEMPLATE)
        .map_err(|err| TemplateSpikeError(err.to_string()))?;
    let tmpl = env
        .get_template("spike")
        .map_err(|err| TemplateSpikeError(err.to_string()))?;
    let value = minijinja::Value::from_serialize(ctx);
    tmpl.render(value)
        .map_err(|err| TemplateSpikeError(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_failures_branch_with_loop() -> Result<(), TemplateSpikeError> {
        let ctx = SpikeContext {
            head_sha: "def5678".to_string(),
            all_passed: false,
            total_jobs: 16,
            failures: vec![SpikeFailure {
                job: "web".to_string(),
                test: "cart total rounds half-even".to_string(),
                message: "expected 10.05 to equal 10.04".to_string(),
            }],
        };
        let rendered = render_spike(&ctx)?;
        assert!(rendered.contains("1 failed, 15 passed on `def5678`"));
        assert!(
            rendered
                .contains("**web** · cart total rounds half-even: expected 10.05 to equal 10.04")
        );
        Ok(())
    }

    #[test]
    fn renders_all_passed_branch_without_failures_section() -> Result<(), TemplateSpikeError> {
        let ctx = SpikeContext {
            head_sha: "abc1234".to_string(),
            all_passed: true,
            total_jobs: 17,
            failures: vec![],
        };
        let rendered = render_spike(&ctx)?;
        assert!(rendered.contains("all 17 jobs passed on `abc1234`"));
        assert!(!rendered.contains("**"));
        Ok(())
    }
}
