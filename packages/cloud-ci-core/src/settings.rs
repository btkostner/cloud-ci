//! Pure, no-I/O parser and validator for `.cloud-ci/settings.yml`, per
//! `docs/design/settings.md`. "Limits: 64 KiB file, 100 pipeline files per
//! repo, 50 secret names per pipeline entry. The parser is a pure module
//! with no I/O, shared by the Worker and the CLI (`cloud-ci lint`)."
//! (settings.md "### Validation"). The 100-pipeline-file limit belongs to
//! pass 3 (below) and is not checked here.
//!
//! # Scope: passes 1 and 2 only
//!
//! `docs/design/settings.md`'s "### Validation" section describes three
//! passes. This module implements only the first two, which are "a pure
//! function of `settings.yml`'s bytes" (same doc):
//!
//! 1. YAML 1.2 core schema (`on`/`yes`/`no` parse as strings, not
//!    booleans), duplicate-map-key errors, and unknown-key errors (each
//!    with a suggested nearest known key).
//! 2. Semantic validation: enum values, name patterns, numeric bounds.
//!
//! Pass 3 — "Every `secrets.<pipeline>` key must name a file that exists in
//! `.cloud-ci/pipelines/` at the resolved default-branch sha" — needs the
//! live pipeline tree (a GitHub API read), which this crate has no access
//! to (it is a "pure module with no I/O"). It is intentionally NOT
//! implemented here, and `cloud-ci lint` (this crate's CLI consumer) does
//! not perform it either; both say so explicitly rather than silently
//! skipping it. A caller with I/O access (the Worker) must run pass 3
//! itself against its own `pipeline_manifest`.
//!
//! # YAML crate choice
//!
//! `serde_yaml` is deprecated/archived (as of 2024) and is not used here,
//! nor is any other dependency anywhere in this workspace a YAML crate (no
//! existing vendored choice to reuse — checked via `Cargo.toml`/`Cargo.lock`
//! across every package before adding one). Candidates considered:
//!
//! - `serde_yml` (a `serde_yaml` fork): had an unresolved supply-chain
//!   compromise/maintainer scare in its history; rejected on that basis.
//! - `serde-saphyr` (serde integration over the `saphyr`/`granit-parser`
//!   stack): gives precise `line:col` locations and configurable duplicate
//!   -key policy, but — like any `serde::Deserialize`-driven parser — stops
//!   at the *first* error per `deserialize` call. This module must collect
//!   up to 50 independent errors (structural *and* semantic) from one file
//!   in one pass, which a single top-down `Deserialize` cannot do.
//! - `saphyr` (the YAML 1.2 tree builder): builds a `LinkedHashMap` per
//!   mapping, which silently last-wins a duplicate key — the fact of the
//!   duplicate is gone by the time the tree is handed back, so it cannot be
//!   reported as an error after the fact without re-scanning anyway.
//!
//! This module instead drives `saphyr-parser` (the low-level YAML 1.2
//! event/push-parser that both `saphyr` and `serde-saphyr`/`granit-parser`
//! are themselves built on) directly: `Parser::load` into a
//! [`SpannedEventReceiver`][saphyr_parser::SpannedEventReceiver] that we
//! implement ourselves ([`TreeBuilder`]) to build our own span-tracked tree
//! while flagging duplicate keys *as they are inserted*. Every subsequent
//! structural and semantic check walks that tree and pushes a
//! [`Diagnostic`] rather than returning early, which is what makes
//! "collect up to 50 errors, don't stop at the first" possible at all.
//! `saphyr-parser` is YAML 1.2 by construction: unlike many YAML libraries
//! (which default to YAML 1.1-style implicit-boolean coercion for `on`
//! /`yes`/`no`/`y`/`n`), it hands us raw scalars with their style (plain vs
//! quoted) and lets *us* resolve plain scalars against the YAML 1.2 core
//! schema ([`resolve_plain_scalar`]) — only `true`/`false` ever become
//! booleans; `on`/`yes`/`no` and everything else plain-scalar stays a
//! string. This is exercised directly by the
//! `on_yes_no_parse_as_strings_not_booleans` test below.
//!
//! # Deployment-wide-bound clamping (warnings, never errors)
//!
//! `docs/design/settings.md`'s "### Deployment-wide limits" table clamps a
//! handful of fields (`runners.auto.{min,max}`, `concurrency.*`,
//! `retention.*_days`, `cache.max_size_per_repo`) against bounds that only
//! exist at deploy time (wrangler `vars`). This round's `cloud-ci lint` has
//! no deployment context at all — there is no live Worker, no `wrangler
//! .toml` vars, and no sanctioned source of "the" bounds for an arbitrary
//! repo being linted from a laptop. Rather than invent placeholder bounds
//! (which would make `cloud-ci lint` clamp/warn using numbers that have no
//! relationship to any real deployment), deployment-wide clamping is
//! skipped entirely by `cloud-ci lint` this round and is implemented here
//! only as a separate, explicitly-opt-in pure function,
//! [`clamp_to_deployment_bounds`], that takes the bounds as a plain
//! argument. A caller that *does* have deployment context (the Worker,
//! reading its own `env` bindings) can call it; `cloud-ci lint` does not,
//! and says so in its own `--help` text.

use std::collections::BTreeMap;

use saphyr_parser::{Event, Marker, Parser, ScalarStyle, Span, SpannedEventReceiver};

/// Hard cap on the file size this parser will accept, per settings.md's
/// "Limits: 64 KiB file...".
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// Hard cap on secret names per `secrets.<pipeline>` entry, per settings.md's
/// "...50 secret names per pipeline entry.".
pub const MAX_SECRETS_PER_PIPELINE: usize = 50;

/// Cap on the number of errors collected before giving up on finding more,
/// per settings.md's "### Validation": "All collect errors (up to 50)
/// instead of stopping at the first".
pub const MAX_ERRORS: usize = 50;

/// One problem found while parsing/validating `settings.yml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number.
    pub col: usize,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Diagnostic {
    fn new(severity: Severity, line: usize, col: usize, message: impl Into<String>) -> Self {
        Self {
            severity,
            line,
            col,
            message: message.into(),
        }
    }
}

/// Result of [`parse`]: the normalized settings (only present when there
/// were zero errors — warnings alone still produce a usable `Settings`,
/// matching settings.md's "clamping... only ever produces warnings, never
/// errors") plus every diagnostic collected.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseOutcome {
    pub settings: Option<Settings>,
    pub diagnostics: Vec<Diagnostic>,
}

impl ParseOutcome {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

// ---------------------------------------------------------------------
// Normalized settings, per settings.md's "### Field reference".
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub version: i64,
    pub pr_comment: PrComment,
    pub ai: Ai,
    pub commands: Commands,
    pub runners: Runners,
    pub concurrency: Concurrency,
    pub retention: Retention,
    pub cache: Cache,
    /// Pipeline file name (no `.ts`) -> secret names it may request. An
    /// empty map (the default) denies every request — secure default, per
    /// settings.md's minimal-example note.
    pub secrets: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrComment {
    pub enabled: bool,
    pub template: String,
}

impl Default for PrComment {
    fn default() -> Self {
        Self {
            enabled: true,
            template: ".cloud-ci/templates/pr-comment.md".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Summaries {
    Off,
    Pr,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfSuggestions {
    Off,
    Weekly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autofix {
    Off,
    Suggest,
    PullRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutofixOnForks {
    Off,
    Suggest,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ai {
    pub enabled: bool,
    pub summaries: Summaries,
    pub flaky_hints: bool,
    pub perf_suggestions: PerfSuggestions,
    pub autofix: Autofix,
    pub autofix_allow_push_to_pr_branch: bool,
    pub autofix_on_forks: AutofixOnForks,
    pub exclude_paths: Vec<String>,
    pub max_failures_summarized: i64,
    pub daily_neuron_cap: i64,
    pub model_summary: Option<String>,
    pub model_flaky: Option<String>,
    pub model_perf: Option<String>,
    pub model_autofix: Option<String>,
}

impl Default for Ai {
    fn default() -> Self {
        Self {
            enabled: false,
            summaries: Summaries::Pr,
            flaky_hints: true,
            perf_suggestions: PerfSuggestions::Weekly,
            autofix: Autofix::Off,
            autofix_allow_push_to_pr_branch: false,
            autofix_on_forks: AutofixOnForks::Off,
            exclude_paths: Vec::new(),
            max_failures_summarized: 20,
            daily_neuron_cap: 20000,
            model_summary: None,
            model_flaky: None,
            model_perf: None,
            model_autofix: None,
        }
    }
}

/// `operator | admin`; raise-only above the `operator` default.
///
/// There is no `Viewer` variant here on purpose: per `docs/design/auth.md`
/// ("Values can only be raised... viewer cannot invoke commands, so it is
/// not a valid value here" — read in a prior round), a slash-command role
/// minimum can only ever be raised above `operator`, never lowered to
/// `viewer`. Restricting the enum to exactly `{Operator, Admin}` enforces
/// that "raise-only" rule structurally: there is no value this type can
/// hold that would lower the minimum below `operator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Operator,
    Admin,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandRoles {
    pub rerun: Role,
    pub cancel: Role,
    pub autofix: Role,
}

impl Default for CommandRoles {
    fn default() -> Self {
        Self {
            rerun: Role::Operator,
            cancel: Role::Operator,
            autofix: Role::Operator,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Commands {
    pub roles: CommandRoles,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunnersAuto {
    pub min: Option<String>,
    pub max: Option<String>,
    pub initial: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Runners {
    pub default: String,
    pub auto: RunnersAuto,
}

impl Default for Runners {
    fn default() -> Self {
        Self {
            default: "auto".to_string(),
            auto: RunnersAuto::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Concurrency {
    pub repository: Option<i64>,
    pub pipelines: Option<i64>,
    pub pipeline: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Retention {
    pub artifacts_days: i64,
    pub reports_days: i64,
    pub sites_days: i64,
    pub logs_days: i64,
    pub snapshots_days: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            artifacts_days: 30,
            reports_days: 30,
            sites_days: 14,
            logs_days: 30,
            snapshots_days: 14,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cache {
    pub max_size_per_repo_bytes: u64,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            max_size_per_repo_bytes: 5 * 1024 * 1024 * 1024,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            pr_comment: PrComment::default(),
            ai: Ai::default(),
            commands: Commands::default(),
            runners: Runners::default(),
            concurrency: Concurrency::default(),
            retention: Retention::default(),
            cache: Cache::default(),
            secrets: BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------
// Deployment-wide-bound clamping (warnings, never errors). See module docs.
// ---------------------------------------------------------------------

/// Deployment-wide bounds, per settings.md's "### Deployment-wide limits"
/// table. Supplied by a caller with real deployment context (the Worker,
/// from its own `env` bindings) — this crate has no I/O and invents none
/// of these numbers itself.
#[derive(Debug, Clone, PartialEq)]
pub struct DeploymentBounds {
    /// Ordered smallest-to-largest instance-size ladder the deployment
    /// makes available, used to clamp `runners.auto.{min,max}`.
    pub runner_ladder: Vec<String>,
    pub concurrency_repository_max: i64,
    pub concurrency_pipelines_max: i64,
    pub concurrency_pipeline_max: i64,
    pub retention_max_days: i64,
    pub cache_max_bytes: u64,
}

/// Clamp `settings`' deployment-bound-sensitive fields against `bounds` in
/// place, returning one warning [`Diagnostic`] per clamped field. Never
/// produces an error — per settings.md: "A value outside its
/// deployment-wide bound is clamped, with a warning annotation... never a
/// hard failure". Diagnostics from this function carry `line: 0, col: 0`
/// since the original YAML location of a since-normalized field is no
/// longer tracked on `Settings`; a caller wanting the original location
/// should run this against the diagnostics already captured during
/// `parse`.
pub fn clamp_to_deployment_bounds(
    settings: &mut Settings,
    bounds: &DeploymentBounds,
) -> Vec<Diagnostic> {
    let mut warnings = Vec::new();
    let mut warn = |message: String| {
        warnings.push(Diagnostic::new(Severity::Warning, 0, 0, message));
    };

    let ladder_index = |name: &str| bounds.runner_ladder.iter().position(|s| s == name);
    if let Some(min) = &settings.runners.auto.min
        && ladder_index(min).is_none()
        && let Some(nearest) = bounds.runner_ladder.first()
    {
        warn(format!(
            "runners.auto.min {min:?} is not in the deployment's instance-size ladder; clamped \
             to {nearest:?}"
        ));
        settings.runners.auto.min = Some(nearest.clone());
    }
    if let Some(max) = &settings.runners.auto.max
        && ladder_index(max).is_none()
        && let Some(nearest) = bounds.runner_ladder.last()
    {
        warn(format!(
            "runners.auto.max {max:?} is not in the deployment's instance-size ladder; clamped \
             to {nearest:?}"
        ));
        settings.runners.auto.max = Some(nearest.clone());
    }

    if let Some(v) = settings.concurrency.repository
        && v > bounds.concurrency_repository_max
    {
        warn(format!(
            "concurrency.repository {v} exceeds the deployment max {}; clamped",
            bounds.concurrency_repository_max
        ));
        settings.concurrency.repository = Some(bounds.concurrency_repository_max);
    }
    if let Some(v) = settings.concurrency.pipelines
        && v > bounds.concurrency_pipelines_max
    {
        warn(format!(
            "concurrency.pipelines {v} exceeds the deployment max {}; clamped",
            bounds.concurrency_pipelines_max
        ));
        settings.concurrency.pipelines = Some(bounds.concurrency_pipelines_max);
    }
    if let Some(v) = settings.concurrency.pipeline
        && v > bounds.concurrency_pipeline_max
    {
        warn(format!(
            "concurrency.pipeline {v} exceeds the deployment max {}; clamped",
            bounds.concurrency_pipeline_max
        ));
        settings.concurrency.pipeline = Some(bounds.concurrency_pipeline_max);
    }

    macro_rules! clamp_retention {
        ($field:ident, $name:literal) => {
            if settings.retention.$field > bounds.retention_max_days {
                warn(format!(
                    concat!($name, " {} exceeds the deployment max {}; clamped"),
                    settings.retention.$field, bounds.retention_max_days
                ));
                settings.retention.$field = bounds.retention_max_days;
            }
        };
    }
    clamp_retention!(artifacts_days, "retention.artifacts_days");
    clamp_retention!(reports_days, "retention.reports_days");
    clamp_retention!(sites_days, "retention.sites_days");
    clamp_retention!(logs_days, "retention.logs_days");
    clamp_retention!(snapshots_days, "retention.snapshots_days");

    if settings.cache.max_size_per_repo_bytes > bounds.cache_max_bytes {
        warn(format!(
            "cache.max_size_per_repo ({} bytes) exceeds the deployment max ({} bytes); clamped",
            settings.cache.max_size_per_repo_bytes, bounds.cache_max_bytes
        ));
        settings.cache.max_size_per_repo_bytes = bounds.cache_max_bytes;
    }

    warnings
}

// ---------------------------------------------------------------------
// Minimal span-tracked YAML tree, built by hand over saphyr-parser's event
// stream so duplicate keys and every subsequent structural/semantic error
// can be collected instead of failing fast. See module docs.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum NodeValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Seq(Vec<Node>),
    /// Insertion order, duplicates included (a duplicate key still
    /// produces a second entry here; the duplicate itself was already
    /// reported as a diagnostic at build time).
    Map(Vec<(Node, Node)>),
}

#[derive(Debug, Clone, PartialEq)]
struct Node {
    value: NodeValue,
    line: usize,
    col: usize,
}

impl Node {
    fn at(marker: &Marker, value: NodeValue) -> Self {
        Self {
            value,
            line: marker.line(),
            col: marker.col(),
        }
    }

    fn as_map(&self) -> Option<&[(Node, Node)]> {
        match &self.value {
            NodeValue::Map(entries) => Some(entries),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match &self.value {
            NodeValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// Resolve a *plain* (unquoted) scalar against the YAML 1.2 core schema
/// (https://yaml.org/spec/1.2.2/#103-core-schema): only `true`/`false` are
/// booleans; `null`/`~`/empty is null; a handful of int/float shapes are
/// numbers; everything else — including YAML 1.1's `on`/`off`/`yes`/`no`
/// /`y`/`n` — is a string. Quoted/block scalars never reach this function;
/// callers treat them as plain strings unconditionally.
fn resolve_plain_scalar(raw: &str) -> NodeValue {
    match raw {
        "~" | "null" | "Null" | "NULL" | "" => return NodeValue::Null,
        "true" | "True" | "TRUE" => return NodeValue::Bool(true),
        "false" | "False" | "FALSE" => return NodeValue::Bool(false),
        _ => {}
    }
    if let Ok(i) = raw.parse::<i64>() {
        return NodeValue::Int(i);
    }
    let looks_numeric = raw.starts_with(['-', '+'])
        || raw
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
    if looks_numeric && let Ok(f) = raw.parse::<f64>() {
        return NodeValue::Float(f);
    }
    NodeValue::Str(raw.to_string())
}

enum Frame {
    Seq {
        items: Vec<Node>,
        start: Marker,
    },
    Map {
        entries: Vec<(Node, Node)>,
        seen_keys: std::collections::HashSet<String>,
        pending_key: Option<Node>,
        start: Marker,
    },
}

struct TreeBuilder {
    stack: Vec<Frame>,
    doc: Option<Node>,
    document_count: usize,
    extra_document: bool,
    diagnostics: Vec<Diagnostic>,
}

impl TreeBuilder {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            doc: None,
            document_count: 0,
            extra_document: false,
            diagnostics: Vec::new(),
        }
    }

    fn push_error(&mut self, line: usize, col: usize, message: impl Into<String>) {
        if self
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
            < MAX_ERRORS
        {
            self.diagnostics
                .push(Diagnostic::new(Severity::Error, line, col, message));
        }
    }

    fn attach(&mut self, node: Node) {
        match self.stack.last_mut() {
            None => {
                self.doc = Some(node);
            }
            Some(Frame::Seq { items, .. }) => {
                items.push(node);
            }
            Some(Frame::Map {
                entries,
                seen_keys,
                pending_key,
                ..
            }) => match pending_key.take() {
                None => {
                    if let NodeValue::Str(key) = &node.value {
                        if !seen_keys.insert(key.clone()) {
                            self.diagnostics.push(Diagnostic::new(
                                Severity::Error,
                                node.line,
                                node.col,
                                format!("duplicate key `{key}`"),
                            ));
                        }
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            Severity::Error,
                            node.line,
                            node.col,
                            "mapping keys must be plain strings",
                        ));
                    }
                    *pending_key = Some(node);
                }
                Some(key) => {
                    entries.push((key, node));
                }
            },
        }
    }
}

impl<'input> SpannedEventReceiver<'input> for TreeBuilder {
    fn on_event(&mut self, ev: Event<'input>, span: Span) {
        if self.extra_document {
            // Second document: see push_error below at DocumentStart.
            // Nothing in a second document is attached to the first.
            if matches!(ev, Event::DocumentEnd) {
                self.extra_document = false;
            }
            return;
        }
        match ev {
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
            Event::DocumentStart(_) => {
                self.document_count += 1;
                if self.document_count > 1 {
                    self.push_error(
                        span.start.line(),
                        span.start.col(),
                        "settings.yml must contain exactly one YAML document",
                    );
                    self.extra_document = true;
                }
            }
            Event::Alias(_) => {
                self.push_error(
                    span.start.line(),
                    span.start.col(),
                    "YAML anchors/aliases are not supported in settings.yml",
                );
                self.attach(Node::at(&span.start, NodeValue::Null));
            }
            Event::Scalar(value, style, _anchor_id, _tag) => {
                let resolved = if style == ScalarStyle::Plain {
                    resolve_plain_scalar(value.as_ref())
                } else {
                    NodeValue::Str(value.into_owned())
                };
                self.attach(Node::at(&span.start, resolved));
            }
            Event::SequenceStart(..) => {
                self.stack.push(Frame::Seq {
                    items: Vec::new(),
                    start: span.start,
                });
            }
            Event::SequenceEnd => {
                if let Some(Frame::Seq { items, start }) = self.stack.pop() {
                    self.attach(Node::at(&start, NodeValue::Seq(items)));
                }
            }
            Event::MappingStart(..) => {
                self.stack.push(Frame::Map {
                    entries: Vec::new(),
                    seen_keys: std::collections::HashSet::new(),
                    pending_key: None,
                    start: span.start,
                });
            }
            Event::MappingEnd => {
                if let Some(Frame::Map { entries, start, .. }) = self.stack.pop() {
                    self.attach(Node::at(&start, NodeValue::Map(entries)));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Validation context + generic field helpers.
// ---------------------------------------------------------------------

struct Ctx {
    diagnostics: Vec<Diagnostic>,
}

impl Ctx {
    fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    fn error(&mut self, line: usize, col: usize, message: impl Into<String>) {
        if self.error_count() < MAX_ERRORS {
            self.diagnostics
                .push(Diagnostic::new(Severity::Error, line, col, message));
        }
    }
}

/// Levenshtein edit distance, used for "nearest known key" suggestions on
/// an unknown-key error (settings.md: "Unknown keys are errors, not
/// warnings — each suggests the nearest known key").
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let tmp = row[j];
            row[j] = if a[i - 1] == b[j - 1] {
                prev
            } else {
                1 + prev.min(row[j]).min(row[j - 1])
            };
            prev = tmp;
        }
    }
    row[b.len()]
}

fn nearest_key<'a>(unknown: &str, known: &[&'a str]) -> Option<&'a str> {
    known
        .iter()
        .copied()
        .min_by_key(|candidate| levenshtein(unknown, candidate))
}

/// Walk every entry of `map`, erroring on unknown keys (with a suggestion)
/// and returning only the entries whose key is in `known`, keyed by name.
/// Last-wins on a duplicate (which was already reported separately by the
/// tree builder) so later validation only has to consider one value.
fn known_entries<'a>(
    ctx: &mut Ctx,
    map: &'a [(Node, Node)],
    known: &[&'a str],
) -> BTreeMap<&'a str, &'a Node> {
    let mut out = BTreeMap::new();
    for (key_node, value_node) in map {
        let Some(key) = key_node.as_str() else {
            // Already reported as "mapping keys must be plain strings" by
            // the tree builder.
            continue;
        };
        match known.iter().find(|k| **k == key) {
            Some(matched) => {
                out.insert(*matched, value_node);
            }
            None => {
                let suggestion = nearest_key(key, known);
                let hint = match suggestion {
                    Some(s) => format!(", did you mean `{s}`?"),
                    None => String::new(),
                };
                ctx.error(
                    key_node.line,
                    key_node.col,
                    format!("unknown key `{key}`{hint}"),
                );
            }
        }
    }
    out
}

fn expect_bool(ctx: &mut Ctx, node: &Node, field: &str) -> Option<bool> {
    match &node.value {
        NodeValue::Bool(b) => Some(*b),
        NodeValue::Str(s) if matches!(s.as_str(), "on" | "off" | "yes" | "no" | "y" | "n") => {
            ctx.error(
                node.line,
                node.col,
                format!(
                    "{field} must be `true` or `false` (YAML 1.2 core schema; `{s}` is a string \
                     here, not a boolean)"
                ),
            );
            None
        }
        _ => {
            ctx.error(node.line, node.col, format!("{field} must be a boolean"));
            None
        }
    }
}

fn expect_string(ctx: &mut Ctx, node: &Node, field: &str) -> Option<String> {
    match &node.value {
        NodeValue::Str(s) => Some(s.clone()),
        _ => {
            ctx.error(node.line, node.col, format!("{field} must be a string"));
            None
        }
    }
}

fn expect_int(ctx: &mut Ctx, node: &Node, field: &str) -> Option<i64> {
    match &node.value {
        NodeValue::Int(i) => Some(*i),
        _ => {
            ctx.error(node.line, node.col, format!("{field} must be an integer"));
            None
        }
    }
}

fn expect_int_bounds(ctx: &mut Ctx, node: &Node, field: &str, min: i64, max: i64) -> Option<i64> {
    let v = expect_int(ctx, node, field)?;
    if v < min || v > max {
        ctx.error(
            node.line,
            node.col,
            format!("{field} must be between {min} and {max}, got {v}"),
        );
        return None;
    }
    Some(v)
}

fn expect_enum(ctx: &mut Ctx, node: &Node, field: &str, allowed: &[&str]) -> Option<String> {
    let s = expect_string(ctx, node, field)?;
    if allowed.contains(&s.as_str()) {
        Some(s)
    } else {
        ctx.error(
            node.line,
            node.col,
            format!("{field} must be one of {}, got `{s}`", allowed.join(", ")),
        );
        None
    }
}

fn expect_string_seq(ctx: &mut Ctx, node: &Node, field: &str) -> Option<Vec<String>> {
    match &node.value {
        NodeValue::Seq(items) => {
            let mut out = Vec::with_capacity(items.len());
            let mut ok = true;
            for item in items {
                match &item.value {
                    NodeValue::Str(s) => out.push(s.clone()),
                    _ => {
                        ctx.error(
                            item.line,
                            item.col,
                            format!("{field} entries must be strings"),
                        );
                        ok = false;
                    }
                }
            }
            if ok { Some(out) } else { None }
        }
        _ => {
            ctx.error(
                node.line,
                node.col,
                format!("{field} must be a list of strings"),
            );
            None
        }
    }
}

/// Parse a `size` field, e.g. `10GiB`, per settings.md's `cache
/// .max_size_per_repo` type.
fn parse_byte_size(raw: &str) -> Option<u64> {
    let split_at = raw.find(|c: char| !c.is_ascii_digit())?;
    if split_at == 0 {
        return None;
    }
    let (digits, unit) = raw.split_at(split_at);
    let n: u64 = digits.parse().ok()?;
    let multiplier: u64 = match unit {
        "B" => 1,
        "KiB" => 1024,
        "MiB" => 1024 * 1024,
        "GiB" => 1024 * 1024 * 1024,
        "TiB" => 1024u64 * 1024 * 1024 * 1024,
        _ => return None,
    };
    n.checked_mul(multiplier)
}

fn expect_byte_size(ctx: &mut Ctx, node: &Node, field: &str) -> Option<u64> {
    let s = expect_string(ctx, node, field)?;
    match parse_byte_size(&s) {
        Some(bytes) => Some(bytes),
        None => {
            ctx.error(
                node.line,
                node.col,
                format!("{field} must be a size like `10GiB`, got `{s}`"),
            );
            None
        }
    }
}

/// `^[A-Z][A-Z0-9_]{0,63}$`, per settings.md's `secrets.<pipeline_file_name>`
/// constraint.
fn is_valid_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    if name.len() > 64 {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Workers AI model id: must start with `@cf/`, per settings.md's
/// `ai.model_summary`/`.model_flaky`/`.model_perf`/`.model_autofix`
/// constraint.
fn is_valid_model_id(id: &str) -> bool {
    id.starts_with("@cf/")
}

// ---------------------------------------------------------------------
// Section validators.
// ---------------------------------------------------------------------

fn validate_pr_comment(ctx: &mut Ctx, node: &Node) -> PrComment {
    let mut out = PrComment::default();
    let Some(map) = node.as_map() else {
        ctx.error(node.line, node.col, "pr_comment must be a mapping");
        return out;
    };
    let entries = known_entries(ctx, map, &["enabled", "template"]);
    if let Some(n) = entries.get("enabled")
        && let Some(v) = expect_bool(ctx, n, "pr_comment.enabled")
    {
        out.enabled = v;
    }
    if let Some(n) = entries.get("template")
        && let Some(v) = expect_string(ctx, n, "pr_comment.template")
    {
        out.template = v;
    }
    out
}

fn validate_ai(ctx: &mut Ctx, node: &Node) -> Ai {
    let mut out = Ai::default();
    let Some(map) = node.as_map() else {
        ctx.error(node.line, node.col, "ai must be a mapping");
        return out;
    };
    let entries = known_entries(
        ctx,
        map,
        &[
            "enabled",
            "summaries",
            "flaky_hints",
            "perf_suggestions",
            "autofix",
            "autofix_allow_push_to_pr_branch",
            "autofix_on_forks",
            "exclude_paths",
            "max_failures_summarized",
            "daily_neuron_cap",
            "model_summary",
            "model_flaky",
            "model_perf",
            "model_autofix",
        ],
    );

    if let Some(n) = entries.get("enabled")
        && let Some(v) = expect_bool(ctx, n, "ai.enabled")
    {
        out.enabled = v;
    }
    if let Some(n) = entries.get("summaries")
        && let Some(v) = expect_enum(ctx, n, "ai.summaries", &["off", "pr", "all"])
    {
        out.summaries = match v.as_str() {
            "off" => Summaries::Off,
            "all" => Summaries::All,
            _ => Summaries::Pr,
        };
    }
    if let Some(n) = entries.get("flaky_hints")
        && let Some(v) = expect_bool(ctx, n, "ai.flaky_hints")
    {
        out.flaky_hints = v;
    }
    if let Some(n) = entries.get("perf_suggestions")
        && let Some(v) = expect_enum(ctx, n, "ai.perf_suggestions", &["off", "weekly"])
    {
        out.perf_suggestions = match v.as_str() {
            "off" => PerfSuggestions::Off,
            _ => PerfSuggestions::Weekly,
        };
    }
    let mut autofix_node_value: Option<String> = None;
    if let Some(n) = entries.get("autofix")
        && let Some(v) = expect_enum(ctx, n, "ai.autofix", &["off", "suggest", "pull_request"])
    {
        out.autofix = match v.as_str() {
            "suggest" => Autofix::Suggest,
            "pull_request" => Autofix::PullRequest,
            _ => Autofix::Off,
        };
        autofix_node_value = Some(v);
    }
    if let Some(n) = entries.get("autofix_allow_push_to_pr_branch")
        && let Some(v) = expect_bool(ctx, n, "ai.autofix_allow_push_to_pr_branch")
    {
        if v && matches!(out.autofix, Autofix::Off) {
            ctx.error(
                n.line,
                n.col,
                "ai.autofix_allow_push_to_pr_branch requires ai.autofix != off",
            );
        } else {
            out.autofix_allow_push_to_pr_branch = v;
        }
    }
    let _ = autofix_node_value;
    if let Some(n) = entries.get("autofix_on_forks")
        && let Some(v) = expect_enum(ctx, n, "ai.autofix_on_forks", &["off", "suggest"])
    {
        out.autofix_on_forks = match v.as_str() {
            "suggest" => AutofixOnForks::Suggest,
            _ => AutofixOnForks::Off,
        };
    }
    if let Some(n) = entries.get("exclude_paths")
        && let Some(v) = expect_string_seq(ctx, n, "ai.exclude_paths")
    {
        out.exclude_paths = v;
    }
    if let Some(n) = entries.get("max_failures_summarized")
        && let Some(v) = expect_int_bounds(ctx, n, "ai.max_failures_summarized", 1, 100)
    {
        out.max_failures_summarized = v;
    }
    if let Some(n) = entries.get("daily_neuron_cap")
        && let Some(v) = expect_int(ctx, n, "ai.daily_neuron_cap")
    {
        out.daily_neuron_cap = v;
    }
    for (key, field) in [
        ("model_summary", "ai.model_summary"),
        ("model_flaky", "ai.model_flaky"),
        ("model_perf", "ai.model_perf"),
        ("model_autofix", "ai.model_autofix"),
    ] {
        if let Some(n) = entries.get(key)
            && let Some(v) = expect_string(ctx, n, field)
        {
            if is_valid_model_id(&v) {
                match key {
                    "model_summary" => out.model_summary = Some(v),
                    "model_flaky" => out.model_flaky = Some(v),
                    "model_perf" => out.model_perf = Some(v),
                    _ => out.model_autofix = Some(v),
                }
            } else {
                ctx.error(
                    n.line,
                    n.col,
                    format!(
                        "{field} must be a Workers AI model id starting with `@cf/`, got `{v}`"
                    ),
                );
            }
        }
    }
    out
}

fn validate_role(ctx: &mut Ctx, node: &Node, field: &str) -> Option<Role> {
    let v = expect_enum(ctx, node, field, &["operator", "admin"])?;
    Some(match v.as_str() {
        "admin" => Role::Admin,
        _ => Role::Operator,
    })
}

fn validate_commands(ctx: &mut Ctx, node: &Node) -> Commands {
    let mut out = Commands::default();
    let Some(map) = node.as_map() else {
        ctx.error(node.line, node.col, "commands must be a mapping");
        return out;
    };
    let entries = known_entries(ctx, map, &["roles"]);
    let Some(roles_node) = entries.get("roles") else {
        return out;
    };
    let Some(roles_map) = roles_node.as_map() else {
        ctx.error(
            roles_node.line,
            roles_node.col,
            "commands.roles must be a mapping",
        );
        return out;
    };
    let role_entries = known_entries(ctx, roles_map, &["rerun", "cancel", "autofix"]);
    if let Some(n) = role_entries.get("rerun")
        && let Some(v) = validate_role(ctx, n, "commands.roles.rerun")
    {
        out.roles.rerun = v;
    }
    if let Some(n) = role_entries.get("cancel")
        && let Some(v) = validate_role(ctx, n, "commands.roles.cancel")
    {
        out.roles.cancel = v;
    }
    if let Some(n) = role_entries.get("autofix")
        && let Some(v) = validate_role(ctx, n, "commands.roles.autofix")
    {
        out.roles.autofix = v;
    }
    out
}

fn validate_runners(ctx: &mut Ctx, node: &Node) -> Runners {
    let mut out = Runners::default();
    let Some(map) = node.as_map() else {
        ctx.error(node.line, node.col, "runners must be a mapping");
        return out;
    };
    let entries = known_entries(ctx, map, &["default", "auto"]);
    if let Some(n) = entries.get("default")
        && let Some(v) = expect_string(ctx, n, "runners.default")
    {
        if v.is_empty() {
            ctx.error(n.line, n.col, "runners.default must not be empty");
        } else {
            out.default = v;
        }
    }
    if let Some(auto_node) = entries.get("auto") {
        let Some(auto_map) = auto_node.as_map() else {
            ctx.error(
                auto_node.line,
                auto_node.col,
                "runners.auto must be a mapping",
            );
            return out;
        };
        let auto_entries = known_entries(ctx, auto_map, &["min", "max", "initial"]);
        if let Some(n) = auto_entries.get("min")
            && let Some(v) = expect_string(ctx, n, "runners.auto.min")
        {
            out.auto.min = Some(v);
        }
        if let Some(n) = auto_entries.get("max")
            && let Some(v) = expect_string(ctx, n, "runners.auto.max")
        {
            out.auto.max = Some(v);
        }
        if let Some(n) = auto_entries.get("initial")
            && let Some(v) = expect_string(ctx, n, "runners.auto.initial")
        {
            out.auto.initial = Some(v);
        }
    }
    out
}

fn validate_concurrency(ctx: &mut Ctx, node: &Node) -> Concurrency {
    let mut out = Concurrency::default();
    let Some(map) = node.as_map() else {
        ctx.error(node.line, node.col, "concurrency must be a mapping");
        return out;
    };
    let entries = known_entries(ctx, map, &["repository", "pipelines", "pipeline"]);
    if let Some(n) = entries.get("repository")
        && let Some(v) = expect_int_bounds(ctx, n, "concurrency.repository", 1, 200)
    {
        out.repository = Some(v);
    }
    if let Some(n) = entries.get("pipelines")
        && let Some(v) = expect_int_bounds(ctx, n, "concurrency.pipelines", 1, 50)
    {
        out.pipelines = Some(v);
    }
    if let Some(n) = entries.get("pipeline")
        && let Some(v) = expect_int_bounds(ctx, n, "concurrency.pipeline", 1, 100)
    {
        out.pipeline = Some(v);
    }
    out
}

fn validate_retention(ctx: &mut Ctx, node: &Node) -> Retention {
    let mut out = Retention::default();
    let Some(map) = node.as_map() else {
        ctx.error(node.line, node.col, "retention must be a mapping");
        return out;
    };
    let entries = known_entries(
        ctx,
        map,
        &[
            "artifacts_days",
            "reports_days",
            "sites_days",
            "logs_days",
            "snapshots_days",
        ],
    );
    macro_rules! day_field {
        ($key:literal, $field:ident, $name:literal) => {
            if let Some(n) = entries.get($key)
                && let Some(v) = expect_int_bounds(ctx, n, $name, 1, i64::MAX)
            {
                out.$field = v;
            }
        };
    }
    day_field!("artifacts_days", artifacts_days, "retention.artifacts_days");
    day_field!("reports_days", reports_days, "retention.reports_days");
    day_field!("sites_days", sites_days, "retention.sites_days");
    day_field!("logs_days", logs_days, "retention.logs_days");
    day_field!("snapshots_days", snapshots_days, "retention.snapshots_days");
    out
}

fn validate_cache(ctx: &mut Ctx, node: &Node) -> Cache {
    let mut out = Cache::default();
    let Some(map) = node.as_map() else {
        ctx.error(node.line, node.col, "cache must be a mapping");
        return out;
    };
    let entries = known_entries(ctx, map, &["max_size_per_repo"]);
    if let Some(n) = entries.get("max_size_per_repo") {
        match expect_byte_size(ctx, n, "cache.max_size_per_repo") {
            Some(0) => {
                ctx.error(
                    n.line,
                    n.col,
                    "cache.max_size_per_repo must be at least 1 byte",
                );
            }
            Some(bytes) => out.max_size_per_repo_bytes = bytes,
            None => {}
        }
    }
    out
}

fn validate_secrets(ctx: &mut Ctx, node: &Node) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let Some(map) = node.as_map() else {
        ctx.error(node.line, node.col, "secrets must be a mapping");
        return out;
    };
    for (key_node, value_node) in map {
        let Some(pipeline) = key_node.as_str() else {
            continue;
        };
        let Some(names) = expect_string_seq(ctx, value_node, &format!("secrets.{pipeline}")) else {
            continue;
        };
        if names.len() > MAX_SECRETS_PER_PIPELINE {
            ctx.error(
                value_node.line,
                value_node.col,
                format!(
                    "secrets.{pipeline} lists {} secrets, maximum is {MAX_SECRETS_PER_PIPELINE}",
                    names.len()
                ),
            );
        }
        for (i, name) in names.iter().enumerate() {
            if !is_valid_secret_name(name) {
                // Best-effort location: the list's own location, since
                // individual scalar nodes aren't retained past
                // expect_string_seq. Still carries a usable line:col.
                let _ = i;
                ctx.error(
                    value_node.line,
                    value_node.col,
                    format!("secrets.{pipeline} name `{name}` must match ^[A-Z][A-Z0-9_]{{0,63}}$"),
                );
            }
        }
        out.insert(pipeline.to_string(), names);
    }
    out
}

// ---------------------------------------------------------------------
// Top-level entry point.
// ---------------------------------------------------------------------

/// Parse and validate raw `settings.yml` bytes. Pure: no I/O, no network,
/// deterministic for identical bytes. See module docs for scope (passes 1
/// and 2 only).
pub fn parse(bytes: &[u8]) -> ParseOutcome {
    if bytes.len() > MAX_FILE_BYTES {
        return ParseOutcome {
            settings: None,
            diagnostics: vec![Diagnostic::new(
                Severity::Error,
                1,
                1,
                format!(
                    "settings.yml is {} bytes, exceeding the {MAX_FILE_BYTES}-byte limit",
                    bytes.len()
                ),
            )],
        };
    }

    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            return ParseOutcome {
                settings: None,
                diagnostics: vec![Diagnostic::new(
                    Severity::Error,
                    1,
                    1,
                    "settings.yml is not valid UTF-8",
                )],
            };
        }
    };

    let mut builder = TreeBuilder::new();
    let mut parser = Parser::new_from_str(text);
    if let Err(scan_err) = parser.load(&mut builder, false) {
        builder.diagnostics.push(Diagnostic::new(
            Severity::Error,
            scan_err.marker().line(),
            scan_err.marker().col(),
            scan_err.info().to_string(),
        ));
    }

    let mut ctx = Ctx {
        diagnostics: builder.diagnostics,
    };

    let Some(root) = builder.doc else {
        ctx.error(1, 1, "settings.yml is empty");
        return ParseOutcome {
            settings: None,
            diagnostics: ctx.diagnostics,
        };
    };
    let Some(map) = root.as_map() else {
        ctx.error(
            root.line,
            root.col,
            "settings.yml's top level must be a mapping",
        );
        return ParseOutcome {
            settings: None,
            diagnostics: ctx.diagnostics,
        };
    };

    let entries = known_entries(
        &mut ctx,
        map,
        &[
            "version",
            "pr_comment",
            "ai",
            "commands",
            "runners",
            "concurrency",
            "retention",
            "cache",
            "secrets",
        ],
    );

    let mut settings = Settings::default();

    match entries.get("version") {
        None => ctx.error(root.line, root.col, "version is required"),
        Some(n) => match &n.value {
            NodeValue::Int(1) => settings.version = 1,
            NodeValue::Int(other) => {
                ctx.error(n.line, n.col, format!("version must be `1`, got `{other}`"))
            }
            _ => ctx.error(n.line, n.col, "version must be the integer `1`"),
        },
    }

    if let Some(n) = entries.get("pr_comment") {
        settings.pr_comment = validate_pr_comment(&mut ctx, n);
    }
    if let Some(n) = entries.get("ai") {
        settings.ai = validate_ai(&mut ctx, n);
    }
    if let Some(n) = entries.get("commands") {
        settings.commands = validate_commands(&mut ctx, n);
    }
    if let Some(n) = entries.get("runners") {
        settings.runners = validate_runners(&mut ctx, n);
    }
    if let Some(n) = entries.get("concurrency") {
        settings.concurrency = validate_concurrency(&mut ctx, n);
    }
    if let Some(n) = entries.get("retention") {
        settings.retention = validate_retention(&mut ctx, n);
    }
    if let Some(n) = entries.get("cache") {
        settings.cache = validate_cache(&mut ctx, n);
    }
    if let Some(n) = entries.get("secrets") {
        settings.secrets = validate_secrets(&mut ctx, n);
    }

    let has_errors = ctx
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error);
    ParseOutcome {
        settings: if has_errors { None } else { Some(settings) },
        diagnostics: ctx.diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANNOTATED_EXAMPLE: &str = r#"
version: 1

pr_comment:
  enabled: true
  template: .cloud-ci/templates/pr-comment.md

ai:
  enabled: true
  summaries: pr
  flaky_hints: true
  perf_suggestions: weekly
  autofix: suggest
  autofix_allow_push_to_pr_branch: false
  autofix_on_forks: off
  exclude_paths: ["vendor/**", "**/*.lock"]
  max_failures_summarized: 20
  daily_neuron_cap: 20000

commands:
  roles:
    rerun: operator
    cancel: operator
    autofix: operator

runners:
  default: auto
  auto: { min: basic, max: standard-3, initial: basic }

concurrency:
  repository: 40
  pipelines: 4
  pipeline: 12

retention:
  artifacts_days: 14
  reports_days: 30
  sites_days: 14
  logs_days: 30
  snapshots_days: 7

cache:
  max_size_per_repo: 10GiB

secrets:
  ci: [E2E_LOGIN_PASSWORD]
  deploy: [CLOUDFLARE_API_TOKEN]
"#;

    #[test]
    fn annotated_example_parses_with_zero_diagnostics() {
        let outcome = parse(ANNOTATED_EXAMPLE.as_bytes());
        assert_eq!(outcome.diagnostics, Vec::new(), "{:?}", outcome.diagnostics);
        let Some(settings) = outcome.settings else {
            unreachable!("should parse: {:?}", outcome.diagnostics)
        };
        assert_eq!(settings.version, 1);
        assert!(settings.pr_comment.enabled);
        assert_eq!(
            settings.pr_comment.template,
            ".cloud-ci/templates/pr-comment.md"
        );
        assert!(settings.ai.enabled);
        assert!(matches!(settings.ai.summaries, Summaries::Pr));
        assert!(settings.ai.flaky_hints);
        assert!(matches!(
            settings.ai.perf_suggestions,
            PerfSuggestions::Weekly
        ));
        assert!(matches!(settings.ai.autofix, Autofix::Suggest));
        assert!(!settings.ai.autofix_allow_push_to_pr_branch);
        assert!(matches!(settings.ai.autofix_on_forks, AutofixOnForks::Off));
        assert_eq!(
            settings.ai.exclude_paths,
            vec!["vendor/**".to_string(), "**/*.lock".to_string()]
        );
        assert_eq!(settings.ai.max_failures_summarized, 20);
        assert_eq!(settings.ai.daily_neuron_cap, 20000);
        assert!(matches!(settings.commands.roles.rerun, Role::Operator));
        assert!(matches!(settings.commands.roles.cancel, Role::Operator));
        assert!(matches!(settings.commands.roles.autofix, Role::Operator));
        assert_eq!(settings.runners.default, "auto");
        assert_eq!(settings.runners.auto.min.as_deref(), Some("basic"));
        assert_eq!(settings.runners.auto.max.as_deref(), Some("standard-3"));
        assert_eq!(settings.runners.auto.initial.as_deref(), Some("basic"));
        assert_eq!(settings.concurrency.repository, Some(40));
        assert_eq!(settings.concurrency.pipelines, Some(4));
        assert_eq!(settings.concurrency.pipeline, Some(12));
        assert_eq!(settings.retention.artifacts_days, 14);
        assert_eq!(settings.retention.reports_days, 30);
        assert_eq!(settings.retention.sites_days, 14);
        assert_eq!(settings.retention.logs_days, 30);
        assert_eq!(settings.retention.snapshots_days, 7);
        assert_eq!(
            settings.cache.max_size_per_repo_bytes,
            10 * 1024 * 1024 * 1024
        );
        assert_eq!(
            settings.secrets.get("ci"),
            Some(&vec!["E2E_LOGIN_PASSWORD".to_string()])
        );
        assert_eq!(
            settings.secrets.get("deploy"),
            Some(&vec!["CLOUDFLARE_API_TOKEN".to_string()])
        );
    }

    #[test]
    fn minimal_example_parses_with_every_documented_default() {
        let outcome = parse(b"version: 1\n");
        assert_eq!(outcome.diagnostics, Vec::new());
        let Some(settings) = outcome.settings else {
            unreachable!("should parse: {:?}", outcome.diagnostics)
        };
        assert_eq!(settings, Settings::default());
        assert!(settings.pr_comment.enabled);
        assert_eq!(
            settings.pr_comment.template,
            ".cloud-ci/templates/pr-comment.md"
        );
        assert!(!settings.ai.enabled);
        assert!(matches!(settings.commands.roles.rerun, Role::Operator));
        assert_eq!(settings.runners.default, "auto");
        assert_eq!(settings.runners.auto.min, None);
        assert_eq!(settings.concurrency.repository, None);
        assert_eq!(settings.retention.snapshots_days, 14);
        assert_eq!(
            settings.cache.max_size_per_repo_bytes,
            5 * 1024 * 1024 * 1024
        );
        assert!(settings.secrets.is_empty());
    }

    #[test]
    fn missing_version_is_an_error() {
        let outcome = parse(b"pr_comment:\n  enabled: true\n");
        assert!(outcome.has_errors());
        assert!(outcome.settings.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("version is required"))
        );
    }

    #[test]
    fn wrong_version_is_an_error() {
        let outcome = parse(b"version: 2\n");
        assert!(outcome.has_errors());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("version must be"))
        );
    }

    #[test]
    fn duplicate_top_level_key_is_an_error() {
        let outcome = parse(b"version: 1\nversion: 1\n");
        assert!(outcome.has_errors());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate key"))
        );
    }

    #[test]
    fn duplicate_nested_key_is_an_error_with_location() {
        let outcome = parse(b"version: 1\nai:\n  enabled: true\n  enabled: false\n");
        let Some(dup) = outcome
            .diagnostics
            .iter()
            .find(|d| d.message.contains("duplicate key"))
        else {
            unreachable!(
                "expected a duplicate key error, got {:?}",
                outcome.diagnostics
            )
        };
        assert_eq!(dup.line, 4);
    }

    #[test]
    fn unknown_key_suggests_nearest_known_key() {
        let outcome = parse(b"version: 1\nai:\n  enabled: true\n");
        let Some(err) = outcome
            .diagnostics
            .iter()
            .find(|d| d.message.contains("unknown key"))
        else {
            unreachable!(
                "expected an unknown key error, got {:?}",
                outcome.diagnostics
            )
        };
        assert!(
            err.message.contains("did you mean `enabled`"),
            "{}",
            err.message
        );
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let outcome = parse(b"version: 1\nsecrtes:\n  ci: []\n");
        assert!(outcome.has_errors());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown key `secrtes`")
                    && d.message.contains("did you mean `secrets`"))
        );
    }

    #[test]
    fn on_yes_no_parse_as_strings_not_booleans() {
        // The YAML-1.2-vs-1.1 case: many YAML libraries default to 1.1
        // implicit booleans for these words. Here they must stay strings
        // when the field expects a string (ai.autofix_on_forks is an enum
        // represented as a string), and must be *rejected* (not silently
        // coerced) when the field expects an actual boolean.
        let outcome = parse(b"version: 1\nai:\n  autofix_on_forks: off\n");
        assert_eq!(outcome.diagnostics, Vec::new(), "{:?}", outcome.diagnostics);
        let Some(settings) = outcome.settings else {
            unreachable!("should parse: {:?}", outcome.diagnostics)
        };
        assert!(matches!(settings.ai.autofix_on_forks, AutofixOnForks::Off));

        for word in ["on", "off", "yes", "no", "y", "n"] {
            let yaml = format!("version: 1\npr_comment:\n  enabled: {word}\n");
            let outcome = parse(yaml.as_bytes());
            assert!(
                outcome.has_errors(),
                "expected `{word}` to be rejected as a boolean, got {:?}",
                outcome.diagnostics
            );
        }
    }

    #[test]
    fn each_enum_field_has_a_bad_value_case() {
        let cases: &[(&str, &str)] = &[
            ("ai:\n  summaries: everything\n", "ai.summaries"),
            ("ai:\n  perf_suggestions: daily\n", "ai.perf_suggestions"),
            ("ai:\n  autofix: always\n", "ai.autofix"),
            ("ai:\n  autofix_on_forks: allow\n", "ai.autofix_on_forks"),
            (
                "commands:\n  roles:\n    rerun: viewer\n",
                "commands.roles.rerun",
            ),
        ];
        for (body, field) in cases {
            let yaml = format!("version: 1\n{body}");
            let outcome = parse(yaml.as_bytes());
            assert!(outcome.has_errors(), "{field}: expected error for {yaml:?}");
            assert!(
                outcome
                    .diagnostics
                    .iter()
                    .any(|d| d.message.starts_with(&format!("{field} must be one of"))),
                "{field}: got {:?}",
                outcome.diagnostics
            );
        }
    }

    #[test]
    fn each_numeric_field_has_an_out_of_range_case() {
        let cases: &[&str] = &[
            "ai:\n  max_failures_summarized: 0\n",
            "ai:\n  max_failures_summarized: 101\n",
            "concurrency:\n  repository: 0\n",
            "concurrency:\n  repository: 201\n",
            "concurrency:\n  pipelines: 51\n",
            "concurrency:\n  pipeline: 101\n",
        ];
        for body in cases {
            let yaml = format!("version: 1\n{body}");
            let outcome = parse(yaml.as_bytes());
            assert!(outcome.has_errors(), "expected error for {yaml:?}");
        }
    }

    #[test]
    fn malformed_secret_name_is_an_error() {
        let outcome = parse(b"version: 1\nsecrets:\n  ci: [lowercase_secret]\n");
        assert!(outcome.has_errors());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("must match"))
        );
    }

    #[test]
    fn too_many_secrets_for_one_pipeline_is_an_error() {
        let names: Vec<String> = (0..51).map(|i| format!("SECRET_{i}")).collect();
        let yaml = format!("version: 1\nsecrets:\n  ci: [{}]\n", names.join(", "));
        let outcome = parse(yaml.as_bytes());
        assert!(outcome.has_errors());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("maximum is 50"))
        );
    }

    #[test]
    fn invalid_commands_roles_value_is_rejected() {
        let outcome = parse(b"version: 1\ncommands:\n  roles:\n    rerun: viewer\n");
        assert!(outcome.has_errors());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("commands.roles.rerun must be one of"))
        );
    }

    #[test]
    fn autofix_allow_push_requires_autofix_enabled() {
        let outcome =
            parse(b"version: 1\nai:\n  autofix: off\n  autofix_allow_push_to_pr_branch: true\n");
        assert!(outcome.has_errors());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("requires ai.autofix != off"))
        );
    }

    #[test]
    fn fifty_error_cap_is_enforced() {
        // 60 duplicate top-level unknown keys -> 60 potential errors, capped at 50.
        let mut yaml = String::from("version: 1\n");
        for i in 0..60 {
            yaml.push_str(&format!("bogus_key_{i}: 1\n"));
        }
        let outcome = parse(yaml.as_bytes());
        let error_count = outcome
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        assert_eq!(error_count, MAX_ERRORS);
    }

    #[test]
    fn model_id_pattern_is_enforced() {
        let outcome = parse(b"version: 1\nai:\n  model_summary: not-a-model\n");
        assert!(outcome.has_errors());
        let outcome_ok = parse(b"version: 1\nai:\n  model_summary: \"@cf/meta/llama\"\n");
        assert_eq!(outcome_ok.diagnostics, Vec::new());
    }

    #[test]
    fn file_over_size_limit_is_rejected() {
        let mut yaml = String::from("version: 1\n# ");
        yaml.push_str(&"x".repeat(MAX_FILE_BYTES + 1));
        let outcome = parse(yaml.as_bytes());
        assert!(outcome.has_errors());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("exceeding the"))
        );
    }

    #[test]
    fn deployment_clamping_produces_warnings_never_errors() {
        let mut settings = Settings {
            concurrency: Concurrency {
                repository: Some(999),
                pipelines: None,
                pipeline: None,
            },
            ..Settings::default()
        };
        let bounds = DeploymentBounds {
            runner_ladder: vec!["basic".to_string(), "standard-3".to_string()],
            concurrency_repository_max: 40,
            concurrency_pipelines_max: 4,
            concurrency_pipeline_max: 12,
            retention_max_days: 90,
            cache_max_bytes: 20 * 1024 * 1024 * 1024,
        };
        let warnings = clamp_to_deployment_bounds(&mut settings, &bounds);
        assert!(warnings.iter().all(|w| w.severity == Severity::Warning));
        assert_eq!(settings.concurrency.repository, Some(40));
        assert_eq!(warnings.len(), 1);
    }
}
