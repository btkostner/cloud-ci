//! Pure decision logic for `RunCoordinator`: idempotency rules and state
//! transitions, with no `worker`/Durable-Object dependency, so it is
//! unit-testable with plain `cargo test`. The Durable Object in
//! [`super::durable_object`] is the only caller; it supplies the current
//! persisted state and applies whatever these functions decide.

use cloud_ci_proto::ingest::v1::RunStatus;

/// Mirrors architecture.md's "Run states" table. The wire-level
/// [`RunStatus`] only distinguishes queued / in-progress / completed
/// (`cloud_ci.ingest.v1.RunStatus`); this richer, DO-internal enum is what
/// `RunCoordinator` actually persists, and [`RunState::to_proto_status`]
/// narrows it for the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Queued,
    Running,
    Merging,
    Succeeded,
    Failed,
    Cancelled,
    Abandoned,
}

impl RunState {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Merging => "merging",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "merging" => Some(Self::Merging),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }

    pub fn to_proto_status(self) -> RunStatus {
        match self {
            Self::Queued => RunStatus::RUN_STATUS_QUEUED,
            Self::Running | Self::Merging => RunStatus::RUN_STATUS_IN_PROGRESS,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Abandoned => {
                RunStatus::RUN_STATUS_COMPLETED
            }
        }
    }
}

/// `BeginRun`'s `expect_jobs` is rejected when a later call supplies a
/// different non-empty value than what is already stored
/// (docs/design/byo-ci.md: "The first non-empty `expect_jobs` wins; a later
/// different value is rejected").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectJobsConflict;

/// Resolves what `runs.expect_jobs` should become after a `BeginRun` call.
///
/// `incoming` empty means "no opinion" (the proto `repeated string` field
/// simply has no entries) and is always accepted as a no-op. Returns:
/// - `Ok(None)` — nothing to persist (incoming empty, or already matches).
/// - `Ok(Some(list))` — set `expect_jobs` to `list` (first time it's set).
/// - `Err` — `incoming` is non-empty and differs from what is already set.
pub fn resolve_expect_jobs(
    existing: Option<&[String]>,
    incoming: &[String],
) -> Result<Option<Vec<String>>, ExpectJobsConflict> {
    if incoming.is_empty() {
        return Ok(None);
    }
    match existing {
        None => Ok(Some(incoming.to_vec())),
        Some(current) if current == incoming => Ok(None),
        Some(_) => Err(ExpectJobsConflict),
    }
}

/// `StartJob`'s `shard_total` is rejected when a later call for the same
/// `(run_id, job_name)` supplies a different total than what is already
/// stored (docs/design/byo-ci.md: "A `shard_total` different from the stored
/// one is rejected").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardTotalConflict;

/// Resolves the `shard_total` to persist for a `StartJob` call. `incoming`
/// of `0` means "unspecified" and defaults to `1` (docs/design/byo-ci.md:
/// "`shard_total` (default 1)").
pub fn resolve_shard_total(
    existing: Option<u32>,
    incoming: u32,
) -> Result<u32, ShardTotalConflict> {
    let incoming = if incoming == 0 { 1 } else { incoming };
    match existing {
        None => Ok(incoming),
        Some(current) if current == incoming => Ok(incoming),
        Some(_) => Err(ShardTotalConflict),
    }
}

/// A job starting moves a run from `queued` to `running`
/// (architecture.md's Run states table: `running` is "At least one job
/// started"). Any other current state is left alone — this function never
/// moves a run backward out of a later or terminal state.
pub fn run_state_after_start_job(current: RunState) -> RunState {
    if current == RunState::Queued {
        RunState::Running
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_state_round_trips_through_db_string() {
        for state in [
            RunState::Queued,
            RunState::Running,
            RunState::Merging,
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Abandoned,
        ] {
            assert_eq!(RunState::from_db_str(state.as_db_str()), Some(state));
        }
        assert_eq!(RunState::from_db_str("bogus"), None);
    }

    #[test]
    fn run_state_maps_down_to_three_wire_statuses() {
        assert_eq!(
            RunState::Queued.to_proto_status(),
            RunStatus::RUN_STATUS_QUEUED
        );
        for state in [RunState::Running, RunState::Merging] {
            assert_eq!(state.to_proto_status(), RunStatus::RUN_STATUS_IN_PROGRESS);
        }
        for state in [
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Abandoned,
        ] {
            assert_eq!(state.to_proto_status(), RunStatus::RUN_STATUS_COMPLETED);
        }
    }

    #[test]
    fn begin_run_first_call_sets_expect_jobs() {
        let incoming = vec!["unit".to_string(), "e2e".to_string()];
        assert_eq!(
            resolve_expect_jobs(None, &incoming),
            Ok(Some(incoming.clone()))
        );
    }

    #[test]
    fn begin_run_repeat_call_with_empty_expect_jobs_is_a_no_op() {
        assert_eq!(resolve_expect_jobs(None, &[]), Ok(None));
        let existing = vec!["unit".to_string()];
        assert_eq!(resolve_expect_jobs(Some(&existing), &[]), Ok(None));
    }

    #[test]
    fn begin_run_repeat_call_with_identical_expect_jobs_is_idempotent() {
        let existing = vec!["unit".to_string(), "e2e".to_string()];
        assert_eq!(resolve_expect_jobs(Some(&existing), &existing), Ok(None));
    }

    #[test]
    fn begin_run_different_expect_jobs_is_rejected() {
        let existing = vec!["unit".to_string()];
        let incoming = vec!["unit".to_string(), "e2e".to_string()];
        assert_eq!(
            resolve_expect_jobs(Some(&existing), &incoming),
            Err(ExpectJobsConflict)
        );
    }

    #[test]
    fn start_job_first_call_sets_shard_total() {
        assert_eq!(resolve_shard_total(None, 4), Ok(4));
    }

    #[test]
    fn start_job_unspecified_shard_total_defaults_to_one() {
        assert_eq!(resolve_shard_total(None, 0), Ok(1));
    }

    #[test]
    fn start_job_repeat_call_with_same_total_is_idempotent() {
        assert_eq!(resolve_shard_total(Some(4), 4), Ok(4));
    }

    #[test]
    fn start_job_different_shard_total_is_rejected() {
        assert_eq!(resolve_shard_total(Some(4), 3), Err(ShardTotalConflict));
    }

    #[test]
    fn starting_a_job_moves_a_queued_run_to_running() {
        assert_eq!(
            run_state_after_start_job(RunState::Queued),
            RunState::Running
        );
    }

    #[test]
    fn starting_a_job_does_not_move_a_run_already_past_queued() {
        for state in [
            RunState::Running,
            RunState::Merging,
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Abandoned,
        ] {
            assert_eq!(run_state_after_start_job(state), state);
        }
    }
}
