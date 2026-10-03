//! Pure decision logic for `runner: "auto"`, per
//! `docs/design/analytics.md`'s "Rightsizing algorithm (`runner: \"auto\"`)"
//! section and its mermaid flowchart: per-run resource reduction, cross-run
//! p95 aggregation, instance selection against `[min, max]` with memory
//! headroom and a CPU ceiling, 3-night hysteresis, and OOM-retry (which
//! bypasses both hysteresis and p95).
//!
//! Lives in `cloud-ci-core`, not `cloud-ci-worker`, for the same reason as
//! [`crate::split`]/[`crate::sampler`]/[`crate::cgroup`]: no
//! `worker`/Durable-Object/Workers-runtime dependency, unit-testable with
//! plain `cargo test`, and — per the doc's own "The algorithm below is
//! executor-agnostic; the concrete sizes are not" — shaped so a future
//! non-default [ADR 0010](../../../docs/adr/0010-pluggable-executors.md)
//! `Executor` can hand this module its own [`InstanceSize`] ladder without
//! this module changing. Unlike `split`, this logic is not actually called
//! from `cloud-ci-cli` today (rightsizing is a no-op for `external`/BYO-CI
//! runs — analytics.md: "`runner: \"auto\"` and OOM-retry are no-ops for
//! `external` runs (no container to size)"), but it is still domain logic
//! with no Worker dependency, and `cloud-ci-core`'s own module doc already
//! named this as the intended home ("The future rightsizer ... are not
//! added here since nothing in this round of work needs them" — that round
//! is this one).
//!
//! # Scope boundary — what this round builds and what it does not
//!
//! This module is **only** the pure decision math: given already-computed
//! per-node `p95_peak_memory`/`p95_cpu_saturation`/current instance/run
//! count (or, for the per-run reduction step, raw per-sample inputs for one
//! run), decide what to recommend, whether hysteresis lets a resize apply,
//! and what an OOM retry does. It deliberately does **not** build:
//!
//! - The Analytics Engine `cloud_ci_metrics` `sample` row ingest
//!   (`writeDataPoint`/`writeDataPoints()` calls from the job/sample/test/
//!   cache events analytics.md's "Analytics Engine schema" table
//!   describes) — grepping the whole `cloud-ci-worker` crate for
//!   `writeDataPoint`/`AnalyticsEngineDataset`/`cloud_ci_metrics` finds none
//!   of it; there is no real per-run resource-sample data for this module's
//!   functions to be called with yet.
//! - The nightly cron's Analytics Engine SQL API query (analytics.md's
//!   `quantileExactWeighted(0.95)(double1, _sample_interval)` over the AE
//!   `sample` rows) that would produce this module's `p95_peak_memory`/
//!   `p95_cpu_saturation` inputs from real data.
//! - The `sizing_decisions` D1 table write, or any D1 read of a node's
//!   prior hysteresis state.
//!
//! A future round wires the real AE ingest (the `writeDataPoint` calls) and
//! the nightly cron that queries it and calls this round's pure functions
//! — [`reduce_run`], [`quantile_u64`]/[`quantile_f64`], [`recommend_naive`],
//! [`apply_hysteresis`], [`oom_retry`] — with real data, persisting their
//! results to `sizing_decisions`. Until then, nothing in the running Worker
//! calls this module — same "foundation ahead of caller" pattern as
//! `cloud-ci-worker`'s early `repo_state`/`pull_request_state` rounds.

// ---------------------------------------------------------------------------
// The instance-size ladder (executor-agnostic input, not a hardcoded const)
// ---------------------------------------------------------------------------

/// One rung of an executor's instance-size ladder — analytics.md's
/// Cloudflare Containers table (`lite`/`basic`/`standard-1`..`standard-4`)
/// is one concrete example, but every function in this module takes the
/// ladder as a parameter (`&[InstanceSize]`) rather than hardcoding it, per
/// the doc's "The algorithm below is executor-agnostic; the concrete sizes
/// are not" — a future non-default `Executor` passes its own ladder.
///
/// Callers MUST order a ladder slice smallest-to-largest by `vcpu`/
/// `memory_bytes` (analytics.md: "ordered smallest to largest by this
/// table's row order"); nothing here re-sorts a ladder, matching
/// [`crate::split`]'s existing precedent of trusting caller-supplied order
/// rather than silently re-deriving it.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceSize {
    pub name: String,
    pub vcpu: f64,
    pub memory_bytes: u64,
}

/// Returns the inclusive sub-slice of `ladder` from `min_name` to
/// `max_name` — a node's configured `[min, max]` bounds
/// (analytics.md: "Choose the smallest instance size, within `[min, max]`
/// from the node's config"). `None` if either name is absent from the
/// ladder, or if `min_name` sorts after `max_name` in the ladder's own
/// order (a misconfigured node) — callers must have something to fall back
/// to in that case; this function does not guess one.
pub fn ladder_range<'a>(
    ladder: &'a [InstanceSize],
    min_name: &str,
    max_name: &str,
) -> Option<&'a [InstanceSize]> {
    let min_idx = ladder.iter().position(|s| s.name == min_name)?;
    let max_idx = ladder.iter().position(|s| s.name == max_name)?;
    if min_idx > max_idx {
        return None;
    }
    Some(&ladder[min_idx..=max_idx])
}

// ---------------------------------------------------------------------------
// Per-run reduction (analytics.md: "Per-run reduction (nightly cron, before
// cross-run aggregation)")
// ---------------------------------------------------------------------------

/// One resource sample for a single run, matching the fields analytics.md's
/// Analytics Engine `sample` row carries (`cpu_usage_usec_delta`,
/// `memory_peak_bytes`) plus the sample's own interval, needed to turn a
/// cumulative CPU delta into a saturation fraction. A future AE-ingest round
/// reads these out of real `sample` rows for one `(run_id, job_id)`; this
/// module only ever sees them as plain fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSample {
    pub cpu_usage_usec_delta: f64,
    pub sample_interval_usec: f64,
    pub memory_peak_bytes: u64,
}

/// One run's reduced resource profile for one node — analytics.md's
/// "Per-run reduction" bullets, collapsed from a run's full sample sequence
/// down to the two numbers cross-run aggregation needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunReduction {
    pub peak_memory_bytes: u64,
    pub cpu_saturation: f64,
}

/// Reduces one run's samples to `peak_memory = MAX(memory_peak_bytes)` and
/// `cpu_saturation = MAX(cpu_usage_usec_delta / sample_interval_usec /
/// instance_vcpu)`, capped at `1.0` (analytics.md: "the highest fraction of
/// the instance's allotted vCPU actually used in any one sample window,
/// capped at 1.0"). `None` for an empty sample slice — there is no `MAX` of
/// nothing, and a run with zero resource samples should never reach
/// cross-run aggregation as if it contributed a real (and misleadingly
/// zero) data point.
///
/// A `sample_interval_usec` of `0.0` is treated defensively as contributing
/// no saturation for that one sample (rather than dividing by zero) — this
/// should not happen for a real cgroup-sampled run (the agent samples every
/// 2s), but a malformed input must not produce `NaN`/`inf` propagating into
/// a cross-run p95.
pub fn reduce_run(samples: &[ResourceSample], instance_vcpu: f64) -> Option<RunReduction> {
    if samples.is_empty() || instance_vcpu <= 0.0 {
        return None;
    }

    let mut peak_memory_bytes = 0u64;
    let mut cpu_saturation = 0.0f64;

    for sample in samples {
        if sample.memory_peak_bytes > peak_memory_bytes {
            peak_memory_bytes = sample.memory_peak_bytes;
        }
        let saturation = if sample.sample_interval_usec > 0.0 {
            (sample.cpu_usage_usec_delta / sample.sample_interval_usec / instance_vcpu).min(1.0)
        } else {
            0.0
        };
        if saturation > cpu_saturation {
            cpu_saturation = saturation;
        }
    }

    Some(RunReduction {
        peak_memory_bytes,
        cpu_saturation,
    })
}

// ---------------------------------------------------------------------------
// Cross-run aggregation (analytics.md: "Cross-run aggregation")
// ---------------------------------------------------------------------------

/// `quantileExactWeighted(0.95)`-equivalent over the last 20 runs' per-run
/// reductions. analytics.md's rollup-cron section mandates
/// `quantileExactWeighted` specifically because *that* query runs directly
/// against Analytics Engine's raw, equitably-sampled rows, where each row
/// can represent more than one real event (`_sample_interval` weighting —
/// see "Analytics Engine schema"'s sampling paragraph). This module is one
/// layer downstream of that: its input is already one [`RunReduction`] per
/// completed run (each run counts exactly once, weight 1), so there is no
/// `_sample_interval` to carry here — this is the per-node nightly-cron
/// math that *consumes* p95 values, not the AE SQL query that *produces*
/// them (that query is explicitly out of this round's scope, see module
/// docs).
///
/// With every element weighted equally, `quantileExactWeighted`'s result
/// degenerates to an ordinary quantile over the plain values. No
/// weighted-quantile crate is already a dependency of this workspace, and
/// pulling one in for a degenerate (unweighted) case would be overkill, so
/// this is a small nearest-rank implementation written directly: sort
/// ascending, take the `ceil(q * n)`-th smallest (1-indexed), clamped to the
/// slice. This is a reasonable close approximation of ClickHouse's own
/// `quantileExactWeighted` for a *weight-1* input — it will not always
/// byte-for-byte match ClickHouse's own interpolation between the two
/// nearest ranks, but for p95 sizing decisions (not billing-exact numbers)
/// nearest-rank is an accepted, simpler choice; documented here rather than
/// silently assumed identical.
pub fn quantile_u64(values: &[u64], q: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<u64> = values.to_vec();
    sorted.sort_unstable();
    Some(sorted[nearest_rank_index(sorted.len(), q)])
}

/// [`quantile_u64`]'s `f64` counterpart, for `cpu_saturation`.
pub fn quantile_f64(values: &[f64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    Some(sorted[nearest_rank_index(sorted.len(), q)])
}

fn nearest_rank_index(len: usize, q: f64) -> usize {
    let rank = (q * len as f64).ceil() as usize;
    rank.saturating_sub(1).min(len - 1)
}

// ---------------------------------------------------------------------------
// Instance selection (analytics.md: "Instance selection", steps 1-5)
// ---------------------------------------------------------------------------

/// 30% memory headroom (analytics.md step 1: "chosen because `memory.peak`
/// is a high-water mark over the whole job... a tighter margin risks the
/// *next* run's peak exceeding it even with no real growth").
pub const MEMORY_HEADROOM: f64 = 1.3;

/// 85% CPU ceiling (analytics.md step 3: "leaves headroom for scheduler
/// jitter and avoids flapping right at 100%").
pub const CPU_CEILING: f64 = 0.85;

/// Fewer than this many completed runs for a node: "use `initial` from the
/// config and do not resize (not enough signal)" (analytics.md step 5).
pub const MIN_RUNS_FOR_SIZING: u32 = 5;

/// This round's hysteresis threshold (analytics.md: "3 consecutive nightly
/// recalculations").
pub const HYSTERESIS_NIGHTS: u8 = 3;

/// One night's naive (pre-hysteresis) instance-selection outcome for a
/// node, analytics.md's "Instance selection" steps 1-4, once step 5's
/// run-count gate has already passed (see [`NaiveOutcome::UseInitial`] for
/// that gate).
#[derive(Debug, Clone, PartialEq)]
pub struct NaiveRecommendation {
    /// The chosen size: the smallest candidate satisfying both the memory
    /// and CPU constraints, or — if `fits` is `false` — the ladder-range's
    /// `max`, clamped per step 4.
    pub size: InstanceSize,
    /// `p95_peak_memory * 1.3` (step 1), carried through for the
    /// explainability reason string.
    pub target_memory_bytes: u64,
    /// `p95_cpu_saturation * current_instance_vcpu` (step 2), carried
    /// through for the explainability reason string.
    pub target_cpu_vcpu: f64,
    /// `true` if `size` actually satisfies both constraints; `false` means
    /// this is step 4's clamp-to-`max`-with-shortfall case — no candidate
    /// in the configured `[min, max]` range fit both the memory headroom
    /// and the CPU ceiling.
    pub fits: bool,
}

/// One night's full naive-selection result, before hysteresis.
#[derive(Debug, Clone, PartialEq)]
pub enum NaiveOutcome {
    /// Step 5: fewer than [`MIN_RUNS_FOR_SIZING`] completed runs — use the
    /// node's configured `initial` size, no resize, no hysteresis tracking.
    UseInitial,
    /// Steps 1-4 ran; this is tonight's candidate (fitting or clamped).
    Recommendation(NaiveRecommendation),
}

/// Runs analytics.md's "Instance selection" steps 1-5 for one node, for one
/// night. `candidates` MUST already be the node's `[min, max]`-bounded,
/// smallest-to-largest ladder slice (see [`ladder_range`]) — this function
/// does no further bounds filtering. Returns `None` only for a malformed
/// (empty) `candidates` slice, which means the node's `min`/`max` config
/// resolved to nothing selectable — a configuration error upstream of this
/// function, not a case this algorithm has a documented answer for.
pub fn recommend_naive(
    candidates: &[InstanceSize],
    p95_peak_memory_bytes: u64,
    p95_cpu_saturation: f64,
    current_instance_vcpu: f64,
    completed_run_count: u32,
) -> Option<NaiveOutcome> {
    let max = candidates.last()?;

    if completed_run_count < MIN_RUNS_FOR_SIZING {
        return Some(NaiveOutcome::UseInitial);
    }

    let target_memory_bytes = (p95_peak_memory_bytes as f64 * MEMORY_HEADROOM).ceil() as u64;
    let target_cpu_vcpu = p95_cpu_saturation * current_instance_vcpu;

    let fitting = candidates.iter().find(|candidate| {
        candidate.memory_bytes >= target_memory_bytes
            && target_cpu_vcpu / candidate.vcpu <= CPU_CEILING
    });

    let (size, fits) = match fitting {
        Some(candidate) => (candidate.clone(), true),
        None => (max.clone(), false),
    };

    Some(NaiveOutcome::Recommendation(NaiveRecommendation {
        size,
        target_memory_bytes,
        target_cpu_vcpu,
        fits,
    }))
}

// ---------------------------------------------------------------------------
// Hysteresis (analytics.md: "Hysteresis")
// ---------------------------------------------------------------------------

/// Persisted hysteresis state for one node, between nightly runs — this
/// round's chosen representation of "the last 1-2 nights' prior
/// recommendations": rather than storing a list of past nights, only the
/// single most recent *distinct* candidate name and how many consecutive
/// nights (1 or 2, since 3 would already have applied and cleared this)
/// it has been recommended needs to survive to the next night, because a
/// differing naive recommendation on any night immediately resets the
/// count to 1 for the new candidate (see [`apply_hysteresis`]'s doc comment
/// for why, and the exact doc wording it is read against). A future cron
/// round persists this as a column (or two) on `sizing_decisions` rather
/// than a separate table, since it is exactly this shape: one
/// `(candidate_name, nights)` pair per node.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCandidate {
    pub size_name: String,
    /// How many consecutive nights (including the night this was last
    /// recorded) this exact candidate has been the naive recommendation.
    /// Always `1` or `2` as *persisted* state (a `3` would have applied and
    /// been cleared the night it was reached).
    pub nights: u8,
}

/// One night's hysteresis-gated result for a node whose naive
/// recommendation differs from its current instance size.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingResult {
    pub size: InstanceSize,
    pub nights: u8,
}

/// One night's hysteresis-gated result for a node whose naive
/// recommendation differs from its current instance size for the 3rd
/// consecutive night — the resize actually applies.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyResult {
    pub size: InstanceSize,
    pub reason: String,
}

/// Tonight's hysteresis outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum HysteresisOutcome {
    /// Tonight's naive candidate matches the current instance size — no
    /// pending state to track (and any previously-pending different
    /// candidate is implicitly abandoned; a caller persisting
    /// [`PendingCandidate`] should clear it).
    NoChange,
    /// Tonight's naive candidate differs from current, but has not yet been
    /// recommended for [`HYSTERESIS_NIGHTS`] consecutive nights.
    Pending(PendingResult),
    /// Tonight's naive candidate differs from current and has now been
    /// recommended for [`HYSTERESIS_NIGHTS`] consecutive nights — apply the
    /// resize.
    Apply(ApplyResult),
}

/// Gates tonight's [`NaiveRecommendation`] against the node's current
/// instance size and whatever [`PendingCandidate`] state survived from the
/// previous night, per analytics.md's "Hysteresis": "a resize (up or down)
/// only takes effect if the newly selected size differs from
/// `sizing_decisions.current_instance_type` for 3 consecutive nightly
/// recalculations (i.e. the recommendation must be stable across 3
/// nights)... this prevents a node that oscillates near a boundary ...
/// from resizing every night."
///
/// **Flip resets the counter.** The doc does not spell out the reset case
/// in as many words, but "the recommendation must be *stable* across 3
/// nights" only has one sensible reading once a *different* candidate shows
/// up on night 2: night 2's new candidate has been recommended for exactly
/// one night, not two, so it is treated as a fresh night 1 for the new
/// candidate, not an increment of the old candidate's count (which would
/// let a since-abandoned size apply spuriously) and not a continuation at
/// the same count (which would apply after only 2 nights of the *actual*
/// winning candidate — contradicting "3 consecutive"). This matches the
/// mermaid flowchart's `F -- yes, 1st or 2nd night --> H[record candidate,
/// wait for confirmation]` edge, which records *a* candidate each time,
/// implying each new candidate restarts its own count rather than
/// inheriting a different one's.
pub fn apply_hysteresis(
    current: &InstanceSize,
    naive: &NaiveRecommendation,
    prior_pending: Option<&PendingCandidate>,
) -> HysteresisOutcome {
    if naive.size.name == current.name {
        return HysteresisOutcome::NoChange;
    }

    let nights = match prior_pending {
        Some(pending) if pending.size_name == naive.size.name => pending.nights.saturating_add(1),
        _ => 1,
    };

    if nights >= HYSTERESIS_NIGHTS {
        HysteresisOutcome::Apply(ApplyResult {
            size: naive.size.clone(),
            reason: resize_reason(current, naive),
        })
    } else {
        HysteresisOutcome::Pending(PendingResult {
            size: naive.size.clone(),
            nights,
        })
    }
}

/// Builds the resize reason string, matching analytics.md's own example
/// shape as closely as the implementation allows: `"standard-1 ->
/// standard-2: p95 peak memory 4.9 GiB x1.3 = 6.4 GiB > standard-1's 4 GiB
/// (stable 3/3 nights)"` for a fitting candidate, or a parallel
/// clamped-to-max shortfall phrasing (not given an example in the doc —
/// step 4 only says "record the shortfall in `reason`") for `fits == false`.
fn resize_reason(from: &InstanceSize, to: &NaiveRecommendation) -> String {
    let target_gib = gib(to.target_memory_bytes);
    let p95_gib = gib(((to.target_memory_bytes as f64) / MEMORY_HEADROOM).round() as u64);
    if to.fits {
        format!(
            "{} -> {}: p95 peak memory {p95_gib} x1.3 = {target_gib} > {}'s {} (stable {}/{} nights)",
            from.name,
            to.size.name,
            from.name,
            gib(from.memory_bytes),
            HYSTERESIS_NIGHTS,
            HYSTERESIS_NIGHTS,
        )
    } else {
        let implied_cpu_pct = (to.target_cpu_vcpu / to.size.vcpu * 100.0).round();
        format!(
            "{} -> {} (max, shortfall): p95 peak memory {p95_gib} x1.3 = {target_gib} > {}'s {} \
             and/or implied CPU {implied_cpu_pct}% > {}% ceiling, clamped to configured max \
             (stable {}/{} nights)",
            from.name,
            to.size.name,
            to.size.name,
            gib(to.size.memory_bytes),
            (CPU_CEILING * 100.0).round(),
            HYSTERESIS_NIGHTS,
            HYSTERESIS_NIGHTS,
        )
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

// ---------------------------------------------------------------------------
// OOM retry (analytics.md: "OOM retry")
// ---------------------------------------------------------------------------

/// Tonight's — rather, right-now's — OOM-retry outcome. Bypasses hysteresis
/// and the p95 computation entirely (analytics.md: "`RunCoordinator`
/// immediately retries that node once on the next size up ... bypassing
/// hysteresis and the p95 computation").
#[derive(Debug, Clone, PartialEq)]
pub enum OomRetryOutcome {
    /// Retry at the next size up; this becomes the new
    /// `current_instance_type` immediately, per analytics.md: "This
    /// retry-sized instance becomes the new `current_instance_type`
    /// immediately (not just for the one retry)".
    RetryAt { to: InstanceSize, reason: String },
    /// Already at the ladder's top size when OOM-killed again — terminal
    /// failure, per analytics.md: "If the node OOMs again at `max`, it
    /// fails with a message naming the configured `max` and the measured
    /// peak, rather than retrying indefinitely." `measured_peak_bytes` is
    /// threaded through from the caller (the OOM event itself, not a p95)
    /// so the failure message can name it.
    AlreadyAtMax {
        max: InstanceSize,
        measured_peak_bytes: u64,
    },
}

/// Resolves an OOM-kill event for a node currently at `current_name` within
/// `ladder` (the node's *full* executor ladder — analytics.md: "the next
/// size up from the one it just used (per the active executor's size
/// ladder)" — not clamped to the node's configured `max`, since an OOM is
/// "stronger evidence than any number of p95 samples that stayed under
/// threshold" and the doc does not say OOM-retry respects the configured
/// max; only the ladder's own top is named as the terminal case). `None` if
/// `current_name` is not present in `ladder` at all (a configuration error
/// upstream of this function).
///
/// # Reason string
///
/// analytics.md states the stored format two different ways in two places:
/// `"oom-retry: <from> -> <to>"` (the "OOM retry" bullet's own sentence) and
/// `"standard-2 -> standard-3: oom-retry"` (the "Explainability" bullet's
/// worked example, which also matches the resize reason's own `"<from> ->
/// <to>: <explanation>"` shape). This implementation follows the worked
/// example / shared convention — `"<from> -> <to>: oom-retry"` — since it
/// is the format demonstrated as actual rendered output, and keeps one
/// consistent `"from -> to: reason"` shape across both OOM-retry and
/// 3-night resizes rather than two incompatible shapes for the same field.
pub fn oom_retry(
    ladder: &[InstanceSize],
    current_name: &str,
    measured_peak_bytes: u64,
) -> Option<OomRetryOutcome> {
    let idx = ladder.iter().position(|s| s.name == current_name)?;

    if idx + 1 < ladder.len() {
        let to = ladder[idx + 1].clone();
        let reason = format!("{} -> {}: oom-retry", current_name, to.name);
        Some(OomRetryOutcome::RetryAt { to, reason })
    } else {
        Some(OomRetryOutcome::AlreadyAtMax {
            max: ladder[idx].clone(),
            measured_peak_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    // Indices: 0 lite, 1 basic, 2 standard-1, 3 standard-2, 4 standard-3, 5 standard-4.
    fn cloudflare_containers_ladder() -> Vec<InstanceSize> {
        vec![
            InstanceSize {
                name: "lite".to_string(),
                vcpu: 1.0 / 16.0,
                memory_bytes: 256 * MIB,
            },
            InstanceSize {
                name: "basic".to_string(),
                vcpu: 1.0 / 4.0,
                memory_bytes: GIB,
            },
            InstanceSize {
                name: "standard-1".to_string(),
                vcpu: 1.0 / 2.0,
                memory_bytes: 4 * GIB,
            },
            InstanceSize {
                name: "standard-2".to_string(),
                vcpu: 1.0,
                memory_bytes: 6 * GIB,
            },
            InstanceSize {
                name: "standard-3".to_string(),
                vcpu: 2.0,
                memory_bytes: 8 * GIB,
            },
            InstanceSize {
                name: "standard-4".to_string(),
                vcpu: 4.0,
                memory_bytes: 12 * GIB,
            },
        ]
    }

    // -- ladder_range --------------------------------------------------

    #[test]
    fn ladder_range_slices_inclusive() {
        let ladder = cloudflare_containers_ladder();
        assert_eq!(
            ladder_range(&ladder, "basic", "standard-3"),
            Some(&ladder[1..=4])
        );
    }

    #[test]
    fn ladder_range_missing_name_is_none() {
        let ladder = cloudflare_containers_ladder();
        assert_eq!(ladder_range(&ladder, "nonexistent", "standard-3"), None);
    }

    #[test]
    fn ladder_range_inverted_bounds_is_none() {
        let ladder = cloudflare_containers_ladder();
        assert_eq!(ladder_range(&ladder, "standard-3", "basic"), None);
    }

    // -- reduce_run ------------------------------------------------------

    #[test]
    fn reduce_run_takes_max_memory_and_caps_cpu_at_one() {
        let samples = [
            ResourceSample {
                cpu_usage_usec_delta: 2_000_000.0, // 2s of CPU time
                sample_interval_usec: 1_000_000.0, // in a 1s window -> 200% on a 1-vCPU instance
                memory_peak_bytes: 3 * GIB,
            },
            ResourceSample {
                cpu_usage_usec_delta: 500_000.0,
                sample_interval_usec: 1_000_000.0,
                memory_peak_bytes: 5 * GIB, // highest peak
            },
        ];
        assert_eq!(
            reduce_run(&samples, 1.0),
            Some(RunReduction {
                peak_memory_bytes: 5 * GIB,
                cpu_saturation: 1.0, // capped, not 2.0
            })
        );
    }

    #[test]
    fn reduce_run_empty_samples_is_none() {
        assert_eq!(reduce_run(&[], 1.0), None);
    }

    // -- quantiles ---------------------------------------------------------

    #[test]
    fn quantile_u64_p95_of_twenty_runs() {
        let values: Vec<u64> = (1..=20).collect(); // 1..=20
        // ceil(0.95 * 20) = 19th smallest (1-indexed) = 19
        assert_eq!(quantile_u64(&values, 0.95), Some(19));
    }

    #[test]
    fn quantile_f64_empty_is_none() {
        assert_eq!(quantile_f64(&[], 0.95), None);
    }

    // -- recommend_naive: run-count gate ------------------------------------

    #[test]
    fn fewer_than_five_runs_uses_initial_no_resize() {
        let ladder = cloudflare_containers_ladder();
        let candidates = &ladder[1..=5]; // basic..standard-4
        assert_eq!(
            recommend_naive(candidates, 5 * GIB, 0.5, 1.0, 4),
            Some(NaiveOutcome::UseInitial)
        );
    }

    #[test]
    fn five_runs_is_enough_to_size() {
        let ladder = cloudflare_containers_ladder();
        let candidates = &ladder[1..=5]; // basic..standard-4
        let outcome = recommend_naive(candidates, 100 * MIB, 0.1, 0.25, 5);
        assert!(matches!(outcome, Some(NaiveOutcome::Recommendation(_))));
    }

    // -- recommend_naive: selection + clamp-to-max shortfall ----------------

    #[test]
    fn selects_smallest_fitting_candidate() {
        let ladder = cloudflare_containers_ladder();
        let candidates = &ladder[1..=5]; // basic..standard-4
        // p95 peak memory 3.8 GiB * 1.3 = ~4.94 GiB -> needs standard-2 (6 GiB),
        // since standard-1's 4 GiB < 4.94 GiB target.
        let p95_peak_memory_bytes = (3.8 * GIB as f64) as u64;
        let target_memory_bytes = (p95_peak_memory_bytes as f64 * MEMORY_HEADROOM).ceil() as u64;
        let outcome = recommend_naive(candidates, p95_peak_memory_bytes, 0.1, 0.5, 20);
        assert_eq!(
            outcome,
            Some(NaiveOutcome::Recommendation(NaiveRecommendation {
                size: ladder[3].clone(), // standard-2
                target_memory_bytes,
                target_cpu_vcpu: 0.1 * 0.5,
                fits: true,
            }))
        );
    }

    #[test]
    fn no_candidate_fits_clamps_to_max_with_shortfall() {
        let ladder = cloudflare_containers_ladder();
        // Bound the node to basic..standard-1 only, but demand far more
        // memory than standard-1 (4 GiB) offers.
        let candidates = &ladder[1..=2]; // basic..standard-1
        let p95_peak_memory_bytes = 20 * GIB;
        let target_memory_bytes = (p95_peak_memory_bytes as f64 * MEMORY_HEADROOM).ceil() as u64;
        let outcome = recommend_naive(candidates, p95_peak_memory_bytes, 0.1, 0.5, 20);
        assert_eq!(
            outcome,
            Some(NaiveOutcome::Recommendation(NaiveRecommendation {
                size: ladder[2].clone(), // standard-1, the range's max
                target_memory_bytes,
                target_cpu_vcpu: 0.1 * 0.5,
                fits: false,
            }))
        );
    }

    #[test]
    fn cpu_ceiling_alone_can_force_a_shortfall() {
        let ladder = cloudflare_containers_ladder();
        let candidates = &ladder[1..=2]; // basic..standard-1
        // Tiny memory need, but p95 CPU saturation on a 4-vCPU instance
        // implies more vCPU than standard-1's 0.5 can ever satisfy at 85%.
        let p95_peak_memory_bytes = 1024u64;
        let target_memory_bytes = (p95_peak_memory_bytes as f64 * MEMORY_HEADROOM).ceil() as u64;
        let outcome = recommend_naive(candidates, p95_peak_memory_bytes, 1.0, 4.0, 20);
        assert_eq!(
            outcome,
            Some(NaiveOutcome::Recommendation(NaiveRecommendation {
                size: ladder[2].clone(), // standard-1, the range's max
                target_memory_bytes,
                target_cpu_vcpu: 4.0,
                fits: false,
            }))
        );
    }

    // -- apply_hysteresis ----------------------------------------------------

    #[test]
    fn naive_matches_current_is_no_change() {
        let ladder = cloudflare_containers_ladder();
        let current = ladder[3].clone(); // standard-2
        let naive = NaiveRecommendation {
            size: current.clone(),
            target_memory_bytes: 5 * GIB,
            target_cpu_vcpu: 0.5,
            fits: true,
        };
        assert_eq!(
            apply_hysteresis(&current, &naive, None),
            HysteresisOutcome::NoChange
        );
    }

    #[test]
    fn stable_recommendation_applies_on_third_night() {
        let ladder = cloudflare_containers_ladder();
        let current = ladder[2].clone(); // standard-1
        let candidate = ladder[3].clone(); // standard-2
        let naive = NaiveRecommendation {
            size: candidate.clone(),
            target_memory_bytes: (4.9 * GIB as f64 * MEMORY_HEADROOM).round() as u64,
            target_cpu_vcpu: 0.3,
            fits: true,
        };

        // Night 1: nothing pending yet.
        assert_eq!(
            apply_hysteresis(&current, &naive, None),
            HysteresisOutcome::Pending(PendingResult {
                size: candidate.clone(),
                nights: 1,
            })
        );

        // Night 2: same candidate again.
        let prior_night1 = PendingCandidate {
            size_name: "standard-2".to_string(),
            nights: 1,
        };
        assert_eq!(
            apply_hysteresis(&current, &naive, Some(&prior_night1)),
            HysteresisOutcome::Pending(PendingResult {
                size: candidate.clone(),
                nights: 2,
            })
        );

        // Night 3: same candidate a third consecutive night -> applies.
        let prior_night2 = PendingCandidate {
            size_name: "standard-2".to_string(),
            nights: 2,
        };
        let expected_reason = resize_reason(&current, &naive);
        assert_eq!(
            apply_hysteresis(&current, &naive, Some(&prior_night2)),
            HysteresisOutcome::Apply(ApplyResult {
                size: candidate.clone(),
                reason: expected_reason.clone(),
            })
        );
        assert!(expected_reason.starts_with("standard-1 -> standard-2:"));
        assert!(expected_reason.ends_with("(stable 3/3 nights)"));
    }

    #[test]
    fn a_flip_before_night_three_resets_the_counter() {
        let ladder = cloudflare_containers_ladder();
        let current = ladder[2].clone(); // standard-1
        let candidate_a = ladder[3].clone(); // standard-2
        let candidate_b = ladder[4].clone(); // standard-3

        // Night 1: candidate A recommended.
        let naive_a = NaiveRecommendation {
            size: candidate_a.clone(),
            target_memory_bytes: 5 * GIB,
            target_cpu_vcpu: 0.3,
            fits: true,
        };
        assert_eq!(
            apply_hysteresis(&current, &naive_a, None),
            HysteresisOutcome::Pending(PendingResult {
                size: candidate_a.clone(),
                nights: 1,
            })
        );

        // Night 2: a *different* candidate B is recommended instead.
        let naive_b = NaiveRecommendation {
            size: candidate_b.clone(),
            target_memory_bytes: 9 * GIB,
            target_cpu_vcpu: 1.0,
            fits: true,
        };
        let prior_night1 = PendingCandidate {
            size_name: "standard-2".to_string(),
            nights: 1,
        };
        // Resets to night 1 of the *new* candidate, not night 2 of the old one.
        assert_eq!(
            apply_hysteresis(&current, &naive_b, Some(&prior_night1)),
            HysteresisOutcome::Pending(PendingResult {
                size: candidate_b.clone(),
                nights: 1,
            })
        );

        // Night 3: candidate B again, now its 2nd night -> still pending, not applied.
        let prior_night2 = PendingCandidate {
            size_name: "standard-3".to_string(),
            nights: 1,
        };
        assert_eq!(
            apply_hysteresis(&current, &naive_b, Some(&prior_night2)),
            HysteresisOutcome::Pending(PendingResult {
                size: candidate_b.clone(),
                nights: 2,
            })
        );
    }

    #[test]
    fn clamp_shortfall_reason_mentions_max_and_shortfall() {
        let ladder = cloudflare_containers_ladder();
        let max = ladder[2].clone(); // standard-1, the bounded range's own max
        let naive = NaiveRecommendation {
            size: max.clone(),
            target_memory_bytes: 20 * GIB,
            target_cpu_vcpu: 1.0,
            fits: false,
        };
        let prior = PendingCandidate {
            size_name: max.name.clone(),
            nights: 2,
        };
        let current = ladder[1].clone(); // basic, differs from max -> Apply, not NoChange
        let expected_reason = resize_reason(&current, &naive);
        assert_eq!(
            apply_hysteresis(&current, &naive, Some(&prior)),
            HysteresisOutcome::Apply(ApplyResult {
                size: max.clone(),
                reason: expected_reason.clone(),
            })
        );
        assert!(expected_reason.contains("shortfall"));
        assert!(expected_reason.contains("clamped to configured max"));
    }

    // -- oom_retry -----------------------------------------------------------

    #[test]
    fn oom_retry_at_non_max_size_steps_up_one() {
        let ladder = cloudflare_containers_ladder();
        assert_eq!(
            oom_retry(&ladder, "standard-1", 5 * GIB),
            Some(OomRetryOutcome::RetryAt {
                to: ladder[3].clone(), // standard-2
                reason: "standard-1 -> standard-2: oom-retry".to_string(),
            })
        );
    }

    #[test]
    fn oom_retry_already_at_max_is_terminal() {
        let ladder = cloudflare_containers_ladder();
        assert_eq!(
            oom_retry(&ladder, "standard-4", 13 * GIB),
            Some(OomRetryOutcome::AlreadyAtMax {
                max: ladder[5].clone(),
                measured_peak_bytes: 13 * GIB,
            })
        );
    }

    #[test]
    fn oom_retry_unknown_current_size_is_none() {
        let ladder = cloudflare_containers_ladder();
        assert_eq!(oom_retry(&ladder, "nonexistent", 1024), None);
    }

    #[test]
    fn resize_reason_matches_doc_example_shape() {
        let ladder = cloudflare_containers_ladder();
        let current = ladder[2].clone(); // standard-1
        let candidate = ladder[3].clone(); // standard-2
        let p95_peak_memory_bytes = (4.9 * GIB as f64) as u64;
        let naive = NaiveRecommendation {
            size: candidate,
            target_memory_bytes: (p95_peak_memory_bytes as f64 * MEMORY_HEADROOM).round() as u64,
            target_cpu_vcpu: 0.3,
            fits: true,
        };
        let reason = resize_reason(&current, &naive);
        // Doc example: "standard-1 -> standard-2: p95 peak memory 4.9 GiB
        // x1.3 = 6.4 GiB > standard-1's 4 GiB (stable 3/3 nights)"
        assert!(reason.starts_with("standard-1 -> standard-2: p95 peak memory"));
        assert!(reason.contains("x1.3"));
        assert!(reason.contains("standard-1's 4.0 GiB"));
        assert!(reason.ends_with("(stable 3/3 nights)"));
    }
}
