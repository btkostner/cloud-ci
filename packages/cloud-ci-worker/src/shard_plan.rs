//! Pure request/response construction for `ResolveShardPlan`, the RPC
//! `ci.shard` (`packages/cloud-ci-pipeline-sdk`'s `ShardPlanner` injection
//! point) calls to resolve a shard count and per-shard file assignment for
//! a managed run, per docs/design/parallelization.md's "### Deterministic
//! assignment, end to end" step 2.
//!
//! This module does not reimplement LPT bin-packing, round-robin
//! assignment, or median-fallback imputation — every one of those lives in
//! `cloud_ci_core::split`, the same pure crate `cloud-ci-cli`'s own
//! `cloud-ci split` command calls (`packages/cloud-ci-cli/src/split.rs`).
//! A second, independent implementation here would risk silently drifting
//! from the Rust original, which is exactly what
//! docs/design/parallelization.md's "### Deterministic assignment, end to
//! end" goal rules out: "the same binary and the same split algorithm ...
//! used in both places, so a BYO CI matrix and a cloud-ci-managed shard
//! group produce byte-identical assignments for the same inputs."
//! [`resolve_plan`] below is a thin adapter: proto request in, proto
//! response out, `cloud_ci_core::split::resolve_shard_count`/`assign` doing
//! all of the actual decision-making in between.
//!
//! Historical duration lookup (`test_stats`, the D1-touching, Workers-
//! runtime-only half of this RPC) is deliberately *not* this module's job —
//! `cloud-ci-worker::lib::handle_resolve_shard_plan` fetches durations via
//! `test_stats::lookup_file_timings` first and passes the resulting map in,
//! same thin-handler/pure-logic split `test_stats.rs`'s own module docs
//! describe (`lookup_file_timings` vs. `parse_file_timing_rows`). That
//! keeps everything in this file synchronous and exercisable by plain
//! `cargo test`, with no D1/Workers runtime dependency.

use std::collections::HashMap;

use cloud_ci_core::split::{
    self, HistoryLookup, Item, ShardCountSpec as CoreShardCountSpec, Strategy,
};
use cloud_ci_proto::ingest::v1::{
    ResolveShardPlanRequest, ResolveShardPlanResponse, ShardCountSpec as ProtoShardCountSpec,
    ShardFiles, SplitStrategy, shard_count_spec::Spec as ProtoShardCountSpecOneof,
};

/// Usage errors in `ResolveShardPlan`'s request shape, distinct from
/// `cloud_ci_core::split::SplitError` (which only ever reports a usage
/// error in the *count spec*, never the item list's shape — see that
/// crate's docs). Wraps `SplitError` so callers get one error type to
/// match on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardPlanError {
    /// `strategy` was `SPLIT_STRATEGY_UNSPECIFIED` — never a valid
    /// resolved value, only ever the zero-value a client forgot to set.
    UnspecifiedStrategy,
    /// `count` (the `oneof spec`) was absent entirely — `ci.shard`'s
    /// `count` option is required (dynamic-pipelines.md's "### Splitting
    /// tests across shards": "a required count spec"), so an absent oneof
    /// is a caller bug, not a valid "use some default" case.
    MissingCountSpec,
    /// `cloud_ci_core::split::resolve_shard_count`'s own usage error
    /// (`{min, max, target}` requested with `file`/`count`, or a zero
    /// target duration) — passed through unchanged, not re-derived.
    Split(split::SplitError),
}

impl std::fmt::Display for ShardPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShardPlanError::UnspecifiedStrategy => {
                write!(
                    f,
                    "strategy must be one of timing/file/count, not unspecified"
                )
            }
            ShardPlanError::MissingCountSpec => write!(f, "count is required"),
            ShardPlanError::Split(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ShardPlanError {}

/// Maps `SplitStrategy` (proto) to `Strategy` (`cloud_ci_core::split`),
/// rejecting the zero value rather than silently defaulting it to one of
/// the three real strategies.
fn core_strategy(strategy: SplitStrategy) -> Result<Strategy, ShardPlanError> {
    match strategy {
        SplitStrategy::SPLIT_STRATEGY_TIMING => Ok(Strategy::Timing),
        SplitStrategy::SPLIT_STRATEGY_FILE => Ok(Strategy::File),
        SplitStrategy::SPLIT_STRATEGY_COUNT => Ok(Strategy::Count),
        SplitStrategy::SPLIT_STRATEGY_UNSPECIFIED => Err(ShardPlanError::UnspecifiedStrategy),
    }
}

/// Maps `ShardCountSpec` (proto `oneof`) to `ShardCountSpec`
/// (`cloud_ci_core::split`). `target`'s `google.protobuf.Duration` is
/// converted to whole milliseconds via its `seconds`/`nanos` fields —
/// `cloud_ci_core::split`'s `target_ms: u64` has no `Duration` type of its
/// own (same pure-crate boundary `resolve_timeout_seconds`'s doc comment
/// describes for `BeginRun`'s `timeout` field).
fn core_count_spec(
    spec: Option<ProtoShardCountSpec>,
) -> Result<CoreShardCountSpec, ShardPlanError> {
    let spec = spec
        .and_then(|s| s.spec)
        .ok_or(ShardPlanError::MissingCountSpec)?;
    match spec {
        ProtoShardCountSpecOneof::Fixed(n) => Ok(CoreShardCountSpec::Fixed(n)),
        ProtoShardCountSpecOneof::Range(range) => {
            let seconds = range.target.seconds.max(0) as u64;
            let nanos_ms = u64::from(range.target.nanos.max(0) as u32) / 1_000_000;
            let target_ms = seconds.saturating_mul(1000).saturating_add(nanos_ms);
            Ok(CoreShardCountSpec::Auto {
                min: range.min,
                max: range.max,
                target_ms,
            })
        }
    }
}

/// A [`HistoryLookup`] backed by an already-fetched `file_path ->
/// duration_ms` map (`test_stats::lookup_file_timings`'s result, converted
/// by the caller) — same role `cloud-ci-cli::split::RemoteHistoryLookup`
/// plays for the CLI side of this same pure algorithm.
struct MapHistoryLookup<'a>(&'a HashMap<String, u64>);

impl HistoryLookup for MapHistoryLookup<'_> {
    fn duration_ms(&self, item_name: &str) -> Option<u64> {
        self.0.get(item_name).copied()
    }
}

/// Resolves `req` into a shard count and per-shard file assignment, given
/// `durations_ms` (the caller's already-fetched `test_stats` lookup —
/// empty when `strategy` is `file`/`count`, which never consult it). Pure:
/// no I/O, no randomness, deterministic for the same inputs — the
/// `cloud_ci_core::split` functions it calls are the same ones
/// `cloud-ci-cli`'s `cloud-ci split` calls, so a managed run's
/// `ResolveShardPlan` response and a BYO CI matrix leg's local `cloud-ci
/// split --index` output agree byte-for-byte on the same `file_paths`,
/// `strategy`, `count`, and `test_stats` snapshot.
pub fn resolve_plan(
    req: &ResolveShardPlanRequest,
    durations_ms: &HashMap<String, u64>,
) -> Result<ResolveShardPlanResponse, ShardPlanError> {
    let strategy = core_strategy(req.strategy.as_known().unwrap_or_default())?;
    let count_spec = core_count_spec(req.count.clone().into_option())?;

    let lookup = MapHistoryLookup(durations_ms);
    let items: Vec<Item> = split::items_from_names(&req.file_paths, &lookup);

    let shard_count =
        split::resolve_shard_count(strategy, count_spec, &items).map_err(ShardPlanError::Split)?;
    let assignment = split::assign(strategy, &items, shard_count);

    Ok(ResolveShardPlanResponse {
        shard_count,
        shards: assignment
            .into_iter()
            .map(|files| ShardFiles {
                files,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_ci_proto::ingest::v1::ShardCountRange;

    fn fixed(n: u32) -> ProtoShardCountSpec {
        ProtoShardCountSpec {
            spec: Some(ProtoShardCountSpecOneof::Fixed(n)),
            ..Default::default()
        }
    }

    fn req(
        strategy: SplitStrategy,
        count: ProtoShardCountSpec,
        files: &[&str],
    ) -> ResolveShardPlanRequest {
        ResolveShardPlanRequest {
            repo_id: 1,
            file_paths: files.iter().map(|s| s.to_string()).collect(),
            strategy: strategy.into(),
            count: Some(count).into(),
            ..Default::default()
        }
    }

    #[test]
    fn file_strategy_round_robins_with_no_duration_data() -> Result<(), ShardPlanError> {
        let resp = resolve_plan(
            &req(
                SplitStrategy::SPLIT_STRATEGY_FILE,
                fixed(2),
                &["b.spec.ts", "a.spec.ts", "c.spec.ts"],
            ),
            &HashMap::new(),
        )?;
        assert_eq!(resp.shard_count, 2);
        assert_eq!(resp.shards.len(), 2);
        // Round-robin, sorted by path ascending: a->0, b->1, c->0.
        assert_eq!(resp.shards[0].files, vec!["a.spec.ts", "c.spec.ts"]);
        assert_eq!(resp.shards[1].files, vec!["b.spec.ts"]);
        Ok(())
    }

    #[test]
    fn timing_strategy_uses_lpt_bin_packing_with_real_durations() -> Result<(), ShardPlanError> {
        let mut durations = HashMap::new();
        durations.insert("slow.spec.ts".to_string(), 9000u64);
        durations.insert("fast.spec.ts".to_string(), 1000u64);
        let resp = resolve_plan(
            &req(
                SplitStrategy::SPLIT_STRATEGY_TIMING,
                fixed(2),
                &["slow.spec.ts", "fast.spec.ts"],
            ),
            &durations,
        )?;
        assert_eq!(resp.shard_count, 2);
        assert_eq!(resp.shards[0].files, vec!["slow.spec.ts"]);
        assert_eq!(resp.shards[1].files, vec!["fast.spec.ts"]);
        Ok(())
    }

    #[test]
    fn auto_count_spec_resolves_against_timing_data() -> Result<(), ShardPlanError> {
        let mut durations = HashMap::new();
        durations.insert("a.spec.ts".to_string(), 5000u64);
        durations.insert("b.spec.ts".to_string(), 5000u64);
        let count = ProtoShardCountSpec {
            spec: Some(ProtoShardCountSpecOneof::Range(Box::new(ShardCountRange {
                min: 1,
                max: 8,
                target: buffa_types::google::protobuf::Duration {
                    seconds: 5,
                    nanos: 0,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }))),
            ..Default::default()
        };
        let resp = resolve_plan(
            &req(
                SplitStrategy::SPLIT_STRATEGY_TIMING,
                count,
                &["a.spec.ts", "b.spec.ts"],
            ),
            &durations,
        )?;
        // total 10s of work / 5s target = ceil(2) = 2 shards.
        assert_eq!(resp.shard_count, 2);
        Ok(())
    }

    #[test]
    fn unspecified_strategy_is_rejected() {
        let result = resolve_plan(
            &req(
                SplitStrategy::SPLIT_STRATEGY_UNSPECIFIED,
                fixed(2),
                &["a.spec.ts"],
            ),
            &HashMap::new(),
        );
        assert_eq!(result, Err(ShardPlanError::UnspecifiedStrategy));
    }

    #[test]
    fn missing_count_spec_is_rejected() {
        let req = ResolveShardPlanRequest {
            repo_id: 1,
            file_paths: vec!["a.spec.ts".to_string()],
            strategy: SplitStrategy::SPLIT_STRATEGY_FILE.into(),
            count: None.into(),
            ..Default::default()
        };
        let result = resolve_plan(&req, &HashMap::new());
        assert_eq!(result, Err(ShardPlanError::MissingCountSpec));
    }

    #[test]
    fn auto_sizing_with_file_strategy_is_rejected_same_as_the_core_crate() {
        let count = ProtoShardCountSpec {
            spec: Some(ProtoShardCountSpecOneof::Range(Box::new(ShardCountRange {
                min: 1,
                max: 8,
                target: buffa_types::google::protobuf::Duration {
                    seconds: 5,
                    nanos: 0,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }))),
            ..Default::default()
        };
        let result = resolve_plan(
            &req(SplitStrategy::SPLIT_STRATEGY_FILE, count, &["a.spec.ts"]),
            &HashMap::new(),
        );
        assert_eq!(
            result,
            Err(ShardPlanError::Split(
                split::SplitError::AutoSizingNeedsTiming
            ))
        );
    }
}
