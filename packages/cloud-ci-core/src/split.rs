//! Deterministic shard-assignment algorithm shared by `cloud-ci-worker`
//! (`RunCoordinator`'s managed-run split) and `cloud-ci-cli`'s
//! `cloud-ci split` (BYO CI), per `docs/design/parallelization.md`'s
//! "### Split strategies", "### Shard count resolution", "### Fallback when
//! no history exists", and "### Deterministic assignment, end to end".
//!
//! Pure Rust, no I/O: every function here is a deterministic function of its
//! inputs (no randomness, no wall-clock dependence, no filesystem/network
//! access, no reliance on map/hash iteration order), so the same inputs
//! produce byte-identical output on both the worker and the CLI, and on any
//! BYO CI matrix leg that invokes the CLI independently and concurrently.
//!
//! # Historical duration data
//!
//! `timing`'s real data source is the `test_stats` D1 table
//! (`docs/design/analytics.md`), read via a point lookup per matched item
//! (parallelization.md's "`cloud-ci split` (also usable from BYO CI)"). That
//! table, and the query endpoint that would serve it to this CLI, do not
//! exist yet — no D1 migration, no worker endpoint, no rollup cron. Rather
//! than have this crate (or its caller) reach out to a nonexistent service,
//! [`HistoryLookup`] abstracts "does this item have a known duration" behind
//! a trait; the only implementation that exists today is
//! [`NoHistoryLookup`], which reports every item as unknown. Per "Fallback
//! when no history exists" below, that degrades `timing` all the way down
//! to `file` behavior for every split run until a real D1-backed
//! `HistoryLookup` is wired in (expected to live in `cloud-ci-worker`,
//! satisfying this same trait without this module changing).

/// Hard platform bounds on shard count, per parallelization.md's `count`
/// column ("int 1-64") and "Shard count resolution": "Clamp shard count to
/// the documented `1..64` range regardless of how it was resolved."
pub const MIN_SHARDS: u32 = 1;
pub const MAX_SHARDS: u32 = 64;

/// One item to be assigned to a shard: a whole file (this round's only
/// supported granularity — see `cloud-ci-cli::split`'s module docs for why
/// `--granularity test` is out of scope) or, in a future per-test
/// granularity, a `file::test-name` pair. `duration_ms` is `None` when no
/// historical duration is known for this item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub duration_ms: Option<u64>,
}

/// Split strategy, per parallelization.md's "### Split strategies" table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Timing,
    File,
    Count,
}

/// How the caller specifies shard count, per "### Shard count resolution".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardCountSpec {
    /// A fixed integer, used as-is (after the `1..64` clamp).
    Fixed(u32),
    /// `{ min, max, target }`: `shard_count = clamp(ceil(total_duration_ms /
    /// target_ms), min, max)`. Only meaningful for `timing` — `file`/`count`
    /// have no duration data to size against, so resolving this spec against
    /// either of those strategies is a usage error ([`SplitError::AutoSizingNeedsTiming`]).
    Auto { min: u32, max: u32, target_ms: u64 },
}

/// A usage error in shard-count resolution; never a result of the input
/// item list's shape (that always produces *some* valid assignment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitError {
    /// `{ min, max, target }` was requested with `file`/`count`, which has
    /// no duration data to size against — per this round's scope decision,
    /// a fixed integer shard count is required for those strategies instead.
    AutoSizingNeedsTiming,
    /// `{ min, max, target }`'s `target_ms` was zero, making
    /// `ceil(total_duration_ms / target_ms)` undefined.
    ZeroTarget,
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitError::AutoSizingNeedsTiming => write!(
                f,
                "{{min, max, target}} shard-count auto-sizing requires --strategy timing \
                 (file/count have no duration data to size against); use a fixed shard count instead"
            ),
            SplitError::ZeroTarget => write!(f, "shard-count target duration must be nonzero"),
        }
    }
}

impl std::error::Error for SplitError {}

/// Historical per-item duration lookup. The only implementation today is
/// [`NoHistoryLookup`] — see this module's docs for why. A future D1-backed
/// implementation (reading `test_stats`, docs/design/analytics.md) satisfies
/// this same trait without [`assign`]/[`resolve_shard_count`] changing.
pub trait HistoryLookup {
    fn duration_ms(&self, item_name: &str) -> Option<u64>;
}

/// Reports every item as having no known duration. Temporary stand-in for a
/// real `test_stats`-backed [`HistoryLookup`] until the query endpoint named
/// in parallelization.md's "`cloud-ci split`" section exists — see this
/// module's docs.
pub struct NoHistoryLookup;

impl HistoryLookup for NoHistoryLookup {
    fn duration_ms(&self, _item_name: &str) -> Option<u64> {
        None
    }
}

/// Builds [`Item`]s from bare item names, resolving each one's duration
/// through `lookup`.
pub fn items_from_names(names: &[String], lookup: &dyn HistoryLookup) -> Vec<Item> {
    names
        .iter()
        .map(|name| Item {
            name: name.clone(),
            duration_ms: lookup.duration_ms(name),
        })
        .collect()
}

fn clamp_to_platform_bounds(n: u32) -> u32 {
    n.clamp(MIN_SHARDS, MAX_SHARDS)
}

/// Median of the durations of items that have one, or `None` if no item has
/// a known duration. Even-length lists average the two middle values.
fn median_duration_ms(items: &[Item]) -> Option<u64> {
    let mut durations: Vec<u64> = items.iter().filter_map(|i| i.duration_ms).collect();
    if durations.is_empty() {
        return None;
    }
    durations.sort_unstable();
    let mid = durations.len() / 2;
    if durations.len().is_multiple_of(2) {
        Some((durations[mid - 1] + durations[mid]) / 2)
    } else {
        Some(durations[mid])
    }
}

/// Per-item effective durations, aligned 1:1 with `items`: items with a
/// known duration keep it, items without get the median of the ones that
/// do. Returns `None` when *no* item has a known duration at all — the
/// "degrade to `file`" case, per "### Fallback when no history exists".
fn imputed_durations(items: &[Item]) -> Option<Vec<u64>> {
    let median = median_duration_ms(items)?;
    Some(
        items
            .iter()
            .map(|item| item.duration_ms.unwrap_or(median))
            .collect(),
    )
}

/// Resolves `spec` to a concrete shard count, clamped to `1..64` regardless
/// of how it was resolved, per "### Shard count resolution".
pub fn resolve_shard_count(
    strategy: Strategy,
    spec: ShardCountSpec,
    items: &[Item],
) -> Result<u32, SplitError> {
    match spec {
        ShardCountSpec::Fixed(n) => Ok(clamp_to_platform_bounds(n)),
        ShardCountSpec::Auto {
            min,
            max,
            target_ms,
        } => {
            if strategy != Strategy::Timing {
                return Err(SplitError::AutoSizingNeedsTiming);
            }
            if target_ms == 0 {
                return Err(SplitError::ZeroTarget);
            }
            // "falls back to min when there is no history for any matched item"
            let total_duration_ms: u64 = match imputed_durations(items) {
                None => return Ok(clamp_to_platform_bounds(min)),
                Some(durations) => durations.iter().sum(),
            };
            let raw = total_duration_ms.div_ceil(target_ms);
            let bounded = raw.clamp(u64::from(min), u64::from(max));
            // `bounded` is already clamped into `[min, max]`, both `u32`s,
            // so this cast cannot lose information.
            #[allow(clippy::cast_possible_truncation)]
            Ok(clamp_to_platform_bounds(bounded as u32))
        }
    }
}

/// Round-robin assignment: items sorted by name ascending, `index = position
/// % shard_count`. Used directly for `file`/`count`, and as `timing`'s
/// degrade-to-`file` fallback when no item has a known duration.
fn round_robin(items: &[Item], shard_count: usize) -> Vec<Vec<String>> {
    let mut sorted: Vec<&Item> = items.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let mut shards: Vec<Vec<String>> = vec![Vec::new(); shard_count];
    for (position, item) in sorted.into_iter().enumerate() {
        shards[position % shard_count].push(item.name.clone());
    }
    shards
}

/// LPT (Longest Processing Time) greedy bin-packing: sort items by duration
/// descending (tie-break by name ascending), assign each item in that order
/// to the currently least-loaded shard (tie-break by lowest shard index).
fn lpt_assign(items: &[Item], durations: &[u64], shard_count: usize) -> Vec<Vec<String>> {
    let mut paired: Vec<(&Item, u64)> = items.iter().zip(durations.iter().copied()).collect();
    paired.sort_by(|(a_item, a_dur), (b_item, b_dur)| {
        b_dur.cmp(a_dur).then_with(|| a_item.name.cmp(&b_item.name))
    });

    let mut loads = vec![0u64; shard_count];
    let mut shards: Vec<Vec<String>> = vec![Vec::new(); shard_count];
    for (item, duration) in paired {
        let mut least_loaded = 0;
        for (idx, load) in loads.iter().enumerate().skip(1) {
            if *load < loads[least_loaded] {
                least_loaded = idx;
            }
        }
        shards[least_loaded].push(item.name.clone());
        loads[least_loaded] += duration;
    }
    shards
}

/// Computes the full per-shard assignment for `items` under `strategy`,
/// into `shard_count` shards. Returns one `Vec<String>` of item names per
/// shard, index 0 = shard 1 (callers map `--index` to `assignment[index -
/// 1]`). `shard_count` is used as given — callers resolve/clamp it via
/// [`resolve_shard_count`] first.
pub fn assign(strategy: Strategy, items: &[Item], shard_count: u32) -> Vec<Vec<String>> {
    let shard_count = (shard_count.max(1)) as usize;
    match strategy {
        Strategy::File | Strategy::Count => round_robin(items, shard_count),
        Strategy::Timing => match imputed_durations(items) {
            None => round_robin(items, shard_count),
            Some(durations) => lpt_assign(items, &durations, shard_count),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, duration_ms: Option<u64>) -> Item {
        Item {
            name: name.to_string(),
            duration_ms,
        }
    }

    // Worked example: 7 items, 3 shards. Durations (ms): a=50, b=40, c=30,
    // d=30, e=20, f=10, g=10. LPT sorts desc (ties by name asc): a(50)
    // b(40) c(30) d(30) e(20) f(10) g(10).
    //
    // Greedy least-loaded assignment, loads start [0,0,0]:
    //   a(50) -> shard0 [0,0,0] tie -> idx0        loads [50,0,0]
    //   b(40) -> least loaded idx1                 loads [50,40,0]
    //   c(30) -> least loaded idx2                 loads [50,40,30]
    //   d(30) -> least loaded idx2 (30==30==40? no: loads are [50,40,30],
    //            least is idx2)                     loads [50,40,60]
    //   e(20) -> least loaded idx1 (40)             loads [50,60,60]
    //   f(10) -> least loaded idx0 (50)             loads [60,60,60]
    //   g(10) -> least loaded idx0 (tie among all three, lowest index wins)
    //                                               loads [70,60,60]
    // shard0 = [a, f, g], shard1 = [b, e], shard2 = [c, d]
    fn worked_items() -> Vec<Item> {
        vec![
            item("a", Some(50)),
            item("b", Some(40)),
            item("c", Some(30)),
            item("d", Some(30)),
            item("e", Some(20)),
            item("f", Some(10)),
            item("g", Some(10)),
        ]
    }

    #[test]
    fn lpt_worked_example() {
        let items = worked_items();
        let shards = assign(Strategy::Timing, &items, 3);
        assert_eq!(
            shards,
            vec![
                vec!["a".to_string(), "f".to_string(), "g".to_string()],
                vec!["b".to_string(), "e".to_string()],
                vec!["c".to_string(), "d".to_string()],
            ]
        );
    }

    #[test]
    fn lpt_is_deterministic_regardless_of_input_order() {
        let mut shuffled = worked_items();
        // Reverse plus an interior swap: not sorted by name, not sorted by
        // duration, not the original insertion order.
        shuffled.reverse();
        shuffled.swap(0, 4);

        let shards = assign(Strategy::Timing, &shuffled, 3);
        assert_eq!(
            shards,
            vec![
                vec!["a".to_string(), "f".to_string(), "g".to_string()],
                vec!["b".to_string(), "e".to_string()],
                vec!["c".to_string(), "d".to_string()],
            ]
        );
    }

    #[test]
    fn lpt_tie_breaks_equal_duration_items_by_name_ascending() {
        let items = vec![
            item("z", Some(10)),
            item("a", Some(10)),
            item("m", Some(10)),
        ];
        // All equal duration: processed in name order a, m, z; each goes to
        // the next least-loaded (every shard starts at 0, so round-robins
        // by lowest index).
        let shards = assign(Strategy::Timing, &items, 3);
        assert_eq!(
            shards,
            vec![
                vec!["a".to_string()],
                vec!["m".to_string()],
                vec!["z".to_string()],
            ]
        );
    }

    #[test]
    fn round_robin_sorts_by_name_and_wraps_by_position() {
        let items = vec![
            item("c.spec.ts", None),
            item("a.spec.ts", None),
            item("b.spec.ts", None),
            item("d.spec.ts", None),
            item("e.spec.ts", None),
        ];
        let shards = assign(Strategy::File, &items, 2);
        // sorted: a, b, c, d, e -> positions 0..4 -> index = position % 2
        assert_eq!(
            shards,
            vec![
                vec![
                    "a.spec.ts".to_string(),
                    "c.spec.ts".to_string(),
                    "e.spec.ts".to_string()
                ],
                vec!["b.spec.ts".to_string(), "d.spec.ts".to_string()],
            ]
        );
    }

    #[test]
    fn count_strategy_matches_round_robin() {
        let items = vec![item("b", None), item("a", None), item("c", None)];
        assert_eq!(
            assign(Strategy::Count, &items, 3),
            assign(Strategy::File, &items, 3)
        );
    }

    #[test]
    fn median_imputation_fills_missing_durations() -> Result<(), String> {
        // Known durations: 10, 20, 30 -> median 20. Unknown items get 20.
        let items = vec![
            item("known-10", Some(10)),
            item("known-20", Some(20)),
            item("known-30", Some(30)),
            item("unknown-1", None),
            item("unknown-2", None),
        ];
        let durations =
            imputed_durations(&items).ok_or_else(|| "should have a median".to_string())?;
        assert_eq!(durations, vec![10, 20, 30, 20, 20]);
        Ok(())
    }

    #[test]
    fn median_imputation_averages_even_count() {
        let items = vec![item("a", Some(10)), item("b", Some(20))];
        assert_eq!(median_duration_ms(&items), Some(15));
    }

    #[test]
    fn full_degrade_to_file_order_when_no_item_has_duration() {
        let items = vec![
            item("c.spec.ts", None),
            item("a.spec.ts", None),
            item("b.spec.ts", None),
        ];
        let timing = assign(Strategy::Timing, &items, 2);
        let file = assign(Strategy::File, &items, 2);
        assert_eq!(timing, file);
    }

    #[test]
    fn shard_count_clamps_above_max() -> Result<(), SplitError> {
        let items = vec![item("a", None)];
        assert_eq!(
            resolve_shard_count(Strategy::File, ShardCountSpec::Fixed(1000), &items)?,
            MAX_SHARDS
        );
        Ok(())
    }

    #[test]
    fn shard_count_clamps_below_min() -> Result<(), SplitError> {
        let items = vec![item("a", None)];
        assert_eq!(
            resolve_shard_count(Strategy::File, ShardCountSpec::Fixed(0), &items)?,
            MIN_SHARDS
        );
        Ok(())
    }

    #[test]
    fn shard_count_auto_sizes_by_ceiling_division() -> Result<(), SplitError> {
        // total = 50+40+30+30+20+10+10 = 190ms, target 60ms -> ceil(190/60)=4
        let items = worked_items();
        let n = resolve_shard_count(
            Strategy::Timing,
            ShardCountSpec::Auto {
                min: 1,
                max: 16,
                target_ms: 60,
            },
            &items,
        )?;
        assert_eq!(n, 4);
        Ok(())
    }

    #[test]
    fn shard_count_auto_clamps_to_max() -> Result<(), SplitError> {
        let items = worked_items(); // total 190ms
        let n = resolve_shard_count(
            Strategy::Timing,
            ShardCountSpec::Auto {
                min: 1,
                max: 2,
                target_ms: 1,
            },
            &items,
        )?;
        assert_eq!(n, 2);
        Ok(())
    }

    #[test]
    fn shard_count_auto_clamps_to_min() -> Result<(), SplitError> {
        let items = worked_items(); // total 190ms
        let n = resolve_shard_count(
            Strategy::Timing,
            ShardCountSpec::Auto {
                min: 10,
                max: 20,
                target_ms: 1_000_000,
            },
            &items,
        )?;
        assert_eq!(n, 10);
        Ok(())
    }

    #[test]
    fn shard_count_auto_falls_back_to_min_with_no_history() -> Result<(), SplitError> {
        let items = vec![item("a", None), item("b", None)];
        let n = resolve_shard_count(
            Strategy::Timing,
            ShardCountSpec::Auto {
                min: 3,
                max: 16,
                target_ms: 60_000,
            },
            &items,
        )?;
        assert_eq!(n, 3);
        Ok(())
    }

    #[test]
    fn shard_count_auto_ceiling_edge_exact_division() -> Result<(), SplitError> {
        let items = vec![item("a", Some(60)), item("b", Some(60))]; // total 120
        let n = resolve_shard_count(
            Strategy::Timing,
            ShardCountSpec::Auto {
                min: 1,
                max: 16,
                target_ms: 60,
            },
            &items,
        )?;
        assert_eq!(n, 2); // 120/60 = 2 exactly, no rounding up needed
        Ok(())
    }

    #[test]
    fn shard_count_auto_rejected_for_file_strategy() {
        let items = vec![item("a", None)];
        let result = resolve_shard_count(
            Strategy::File,
            ShardCountSpec::Auto {
                min: 1,
                max: 4,
                target_ms: 1000,
            },
            &items,
        );
        assert_eq!(result, Err(SplitError::AutoSizingNeedsTiming));
    }

    #[test]
    fn shard_count_auto_rejected_for_count_strategy() {
        let items = vec![item("a", None)];
        let result = resolve_shard_count(
            Strategy::Count,
            ShardCountSpec::Auto {
                min: 1,
                max: 4,
                target_ms: 1000,
            },
            &items,
        );
        assert_eq!(result, Err(SplitError::AutoSizingNeedsTiming));
    }

    #[test]
    fn shard_count_auto_rejects_zero_target() {
        let items = vec![item("a", Some(10))];
        let result = resolve_shard_count(
            Strategy::Timing,
            ShardCountSpec::Auto {
                min: 1,
                max: 4,
                target_ms: 0,
            },
            &items,
        );
        assert_eq!(result, Err(SplitError::ZeroTarget));
    }

    #[test]
    fn no_history_lookup_reports_every_item_unknown() {
        let lookup = NoHistoryLookup;
        assert_eq!(lookup.duration_ms("anything"), None);
        let items = items_from_names(&["a.spec.ts".to_string(), "b.spec.ts".to_string()], &lookup);
        assert_eq!(items[0].duration_ms, None);
        assert_eq!(items[1].duration_ms, None);
    }
}
