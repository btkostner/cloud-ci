//! Pure decision logic for `PullRequestState`: which path a `notify_dirty`
//! takes (seed/ignore/fold, docs/design/pr-comment.md's "`PullRequestState`
//! ignores a `notify_dirty` whose `head_sha` does not match what it has
//! tracked") and the debounce/coalescing alarm-timing table (pr-comment.md's
//! "Debounce and coalescing"). No `worker`/Durable-Object dependency, so it
//! is unit-testable with plain `cargo test`; the Durable Object in
//! [`super`] is the only caller, supplying the current persisted state and
//! applying whatever these functions decide.

/// Which path a `notify_dirty(head_sha, ..)` call takes, per
/// pr-comment.md's "`PullRequestState` ignores a `notify_dirty` whose
/// `head_sha` does not match what it has tracked" and the "first-ever
/// notify_dirty seeds tracked head_sha" rule this module implements (see
/// `pull_request_state` module docs for why: the doc only says the
/// placeholder comment goes up "the instant the first run ... is
/// created", which requires *something* to seed `head_sha` on a
/// brand-new instance, and `notify_dirty` is the only event this round
/// wires before `pull_request.synchronize`/`update_head_sha` exists as a
/// webhook-driven caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyPath {
    /// No `head_sha` tracked yet: seed it to the incoming value.
    Seed,
    /// Incoming `head_sha` differs from tracked: ignore entirely, no
    /// state change, no alarm touch.
    Ignored,
    /// Incoming `head_sha` matches tracked: fold into the pending dirty
    /// burst and (re)schedule the alarm.
    Fold,
}

/// Classifies a `notify_dirty` call against the instance's currently
/// tracked `head_sha` (`None` when the DO has never been seeded).
pub fn classify_notify_dirty(tracked_head_sha: Option<&str>, incoming_head_sha: &str) -> DirtyPath {
    match tracked_head_sha {
        None => DirtyPath::Seed,
        Some(tracked) if tracked == incoming_head_sha => DirtyPath::Fold,
        Some(_) => DirtyPath::Ignored,
    }
}

/// Dirty reasons pr-comment.md's "Debounce and coalescing" calls out as
/// getting the 1 s terminal-flush quiet window instead of the normal 4 s
/// one: "when the last run on sha turns terminal or `reason =
/// head_changed`". The doc never gives real-terminal-detection a wire
/// shape (that needs the D1 aggregate this round doesn't build yet — see
/// `pull_request_state` module docs' scope boundary), so this round
/// models "terminal-ish" as a fixed set of reason strings rather than an
/// extra `is_terminal: bool` field on the wire request: simpler for the
/// one caller that exists so far (`update_head_sha`, which always passes
/// `"head_changed"`), and there is no real run-terminal caller yet to
/// require a richer signal. `"run_terminal"` is this module's own
/// placeholder name for "the last run on this sha just turned terminal"
/// until a real caller (wired in a later round, once `RunCoordinator`
/// calls `notify_dirty`) picks a reason string from pr-comment.md's
/// typed list.
pub fn is_terminal_reason(reason: &str) -> bool {
    matches!(reason, "head_changed" | "run_terminal")
}

/// pr-comment.md's "Debounce and coalescing" table, parameter column.
pub const QUIET_WINDOW_MS: i64 = 4_000;
pub const MAX_DELAY_MS: i64 = 20_000;
pub const MIN_INTERVAL_MS: i64 = 10_000;
pub const TERMINAL_QUIET_MS: i64 = 1_000;

/// What the alarm should be set to as a result of one dirty/head-change
/// event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmTarget {
    /// Fire now: the "initial placeholder" row's 0 s bypass.
    Immediate,
    /// Fire at this ms-epoch timestamp.
    At(i64),
}

/// The subset of `comment_state` the debounce decision needs, as of just
/// before the triggering event is folded in.
#[derive(Debug, Clone, Copy, Default)]
pub struct DebounceState {
    /// Start of the current unflushed dirty burst, `None` if nothing is
    /// pending (never dirtied yet, or the last burst already flushed).
    pub first_dirty_at: Option<i64>,
    /// When the comment was last actually patched, `None` if never.
    pub last_patched_at: Option<i64>,
}

/// Resolves the alarm target for a `notify_dirty`/`update_head_sha` event
/// that is folding into the pending burst (i.e. not a mismatched-head-sha
/// no-op), implementing every row of pr-comment.md's debounce table
/// except "hash skip" (that is a flush-time decision, see
/// [`should_skip_flush`]):
///
/// - **initial placeholder**: `is_initial_placeholder` short-circuits to
///   [`AlarmTarget::Immediate`], bypassing quiet window, max delay, and
///   min interval, per the table's explicit "bypasses the quiet window
///   and min interval" — the caller is responsible for only setting this
///   `true` at most once per head sha (no `comment_id` yet *and* no
///   burst already in progress; see `pull_request_state` module docs).
/// - **quiet window / terminal flush**: `now + 4000` normally, `now +
///   1000` when `is_terminal` (terminal flush row).
/// - **max delay**: capped at `first_dirty_at + 20000`, using `now` as
///   `first_dirty_at` when this is the first event of a new burst
///   (`state.first_dirty_at` is `None`).
/// - **min interval**: raised to at least `last_patched_at + 10000` when
///   set. This implementation applies the min-interval floor *after* the
///   max-delay cap — i.e. the floor can push the target back out past
///   the cap — since min interval is a hard GitHub rate-limit guard
///   (pr-comment.md: "Purpose: Rate-limit guard") while max delay is
///   only a staleness bound; the doc does not state an explicit ordering
///   between the two, so the harder constraint wins.
pub fn next_alarm_target(
    now_ms: i64,
    is_terminal: bool,
    is_initial_placeholder: bool,
    state: DebounceState,
) -> AlarmTarget {
    if is_initial_placeholder {
        return AlarmTarget::Immediate;
    }
    let quiet_ms = if is_terminal {
        TERMINAL_QUIET_MS
    } else {
        QUIET_WINDOW_MS
    };
    let first_dirty_at = state.first_dirty_at.unwrap_or(now_ms);
    let mut target = now_ms + quiet_ms;
    let max_delay_cap = first_dirty_at + MAX_DELAY_MS;
    if target > max_delay_cap {
        target = max_delay_cap;
    }
    if let Some(last_patched_at) = state.last_patched_at {
        let floor = last_patched_at + MIN_INTERVAL_MS;
        if target < floor {
            target = floor;
        }
    }
    AlarmTarget::At(target)
}

/// pr-comment.md's "hash skip" row: "no PATCH if `sha256(body) ==
/// rendered_hash`". This round has no real renderer (see
/// `pull_request_state` module docs — `render_pr_report` wiring is
/// future work), so there is no real `body` to hash here; this function
/// is the decision shape a real render step will plug into, exercised by
/// a unit test with an injected hash-equal/hash-different pair. The live
/// `PullRequestState::alarm` flush stub does not call this — see its
/// own doc comment for why.
pub fn should_skip_flush(rendered_hash: Option<&str>, candidate_hash: &str) -> bool {
    rendered_hash == Some(candidate_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_seeds_when_nothing_tracked() {
        assert_eq!(classify_notify_dirty(None, "abc123"), DirtyPath::Seed);
    }

    #[test]
    fn classify_folds_matching_head_sha() {
        assert_eq!(
            classify_notify_dirty(Some("abc123"), "abc123"),
            DirtyPath::Fold
        );
    }

    #[test]
    fn classify_ignores_mismatched_head_sha() {
        assert_eq!(
            classify_notify_dirty(Some("abc123"), "def456"),
            DirtyPath::Ignored
        );
    }

    /// (a) first-ever notify_dirty -> 0 s immediate flush.
    #[test]
    fn initial_placeholder_is_immediate() {
        let target = next_alarm_target(1_000, false, true, DebounceState::default());
        assert_eq!(target, AlarmTarget::Immediate);
    }

    /// (b) second notify_dirty shortly after, same head_sha -> quiet
    /// window extends, no immediate flush.
    #[test]
    fn second_dirty_event_extends_quiet_window() {
        let state = DebounceState {
            first_dirty_at: Some(1_000),
            last_patched_at: None,
        };
        // A dirty event arrives 500ms after the burst started; the quiet
        // window should extend from *this* event's time, not the burst
        // start.
        let target = next_alarm_target(1_500, false, false, state);
        assert_eq!(target, AlarmTarget::At(1_500 + QUIET_WINDOW_MS));
    }

    /// (c) burst of events approaching the max-delay cap -> alarm target
    /// never exceeds `first_dirty_at + 20000`.
    #[test]
    fn max_delay_caps_the_quiet_window() {
        let state = DebounceState {
            first_dirty_at: Some(0),
            last_patched_at: None,
        };
        // 18s into the burst, a new dirty event would normally push the
        // quiet window to 18000 + 4000 = 22000, past the 20000 cap.
        let target = next_alarm_target(18_000, false, false, state);
        assert_eq!(target, AlarmTarget::At(MAX_DELAY_MS));
    }

    /// (d) min-interval floor after a recent patch.
    #[test]
    fn min_interval_floor_after_recent_patch() {
        let state = DebounceState {
            first_dirty_at: Some(5_000),
            last_patched_at: Some(5_000),
        };
        // Quiet window alone would target 5000 + 4000 = 9000, but the
        // comment was just patched at 5000, so the floor is 5000 +
        // 10000 = 15000.
        let target = next_alarm_target(5_000, false, false, state);
        assert_eq!(target, AlarmTarget::At(5_000 + MIN_INTERVAL_MS));
    }

    /// (e) terminal/head_changed reason -> 1 s quiet window.
    #[test]
    fn terminal_reason_uses_one_second_quiet_window() {
        assert!(is_terminal_reason("head_changed"));
        assert!(is_terminal_reason("run_terminal"));
        assert!(!is_terminal_reason("job_state"));

        let state = DebounceState {
            first_dirty_at: Some(1_000),
            last_patched_at: None,
        };
        let target = next_alarm_target(1_000, true, false, state);
        assert_eq!(target, AlarmTarget::At(1_000 + TERMINAL_QUIET_MS));
    }

    #[test]
    fn hash_skip_detects_unchanged_body() {
        assert!(should_skip_flush(Some("abc"), "abc"));
        assert!(!should_skip_flush(Some("abc"), "def"));
        assert!(!should_skip_flush(None, "abc"));
    }
}
