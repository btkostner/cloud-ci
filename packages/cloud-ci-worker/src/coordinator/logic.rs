//! Pure decision logic for `RunCoordinator`: idempotency rules and state
//! transitions, with no `worker`/Durable-Object dependency, so it is
//! unit-testable with plain `cargo test`. The Durable Object in
//! [`super::durable_object`] is the only caller; it supplies the current
//! persisted state and applies whatever these functions decide.

use cloud_ci_proto::ingest::v1::{Conclusion, RunStatus};

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

    /// Whether uploads/`CompleteShard` calls for this run must be rejected
    /// (docs/design/byo-ci.md's Failure modes: "...or for a run that is
    /// already terminal, is also rejected (400)").
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Abandoned
        )
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

// ---------------------------------------------------------------------------
// Uploads (`CreateUpload`/`CompleteUpload`), docs/design/byo-ci.md "Idempotency"
// ---------------------------------------------------------------------------

/// `uploads.state`, per the Data model table (`received_parts` is trimmed
/// this round — see `coordinator` module docs — so there is no partial
/// state between `pending` and `complete`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadState {
    Pending,
    Complete,
}

/// The row already occupying the exact `(job_id, shard_index, kind, name,
/// sha256)` key a `CreateUpload` call is resolving, if one exists.
#[derive(Debug, Clone)]
pub struct ExistingUpload {
    pub upload_id: String,
    pub state: UploadState,
    pub scope: String,
}

/// What `CreateUpload` should do once the exact-identity lookup and the
/// 32 MiB size cap have both been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateUploadDecision {
    /// No new row or parts needed; return this existing upload's identity.
    ReturnExisting {
        upload_id: String,
        already_complete: bool,
    },
    /// Mint a new upload row and ask the caller to `PUT` part 1.
    CreateNew,
}

/// A part's `size_bytes` exceeds the fixed 32 MiB single-part limit
/// (docs/design/byo-ci.md "Upload mechanics and resumability"); multipart
/// chunking for larger files is out of scope this round, matching the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadTooLarge;

pub const MAX_SINGLE_PART_BYTES: u64 = 32 * 1024 * 1024;

/// `(job_id, shard_index, kind, name, sha256)` is the Worker's dedupe key
/// (`uploads.UNIQUE`); `scope` is a plain column on that row, immutable for
/// one piece of content — a retry that resends the identical key under a
/// *different* `scope` is a conflict, not a silent relabel
/// (docs/design/byo-ci.md "Idempotency" and its Failure modes row for this
/// exact case). A *different* `sha256` under the same `(job_id,
/// shard_index, kind, name)` is a legitimate replacement and never reaches
/// this function as `existing_exact_match`, since the caller looks the row
/// up by the full key including `sha256`.
pub fn resolve_create_upload(
    size_bytes: u64,
    incoming_scope: &str,
    existing_exact_match: Option<&ExistingUpload>,
) -> Result<CreateUploadDecision, CreateUploadError> {
    if size_bytes > MAX_SINGLE_PART_BYTES {
        return Err(CreateUploadError::TooLarge(UploadTooLarge));
    }
    match existing_exact_match {
        None => Ok(CreateUploadDecision::CreateNew),
        Some(existing) if existing.scope == incoming_scope => {
            Ok(CreateUploadDecision::ReturnExisting {
                upload_id: existing.upload_id.clone(),
                already_complete: existing.state == UploadState::Complete,
            })
        }
        Some(_) => Err(CreateUploadError::ScopeConflict),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateUploadError {
    TooLarge(UploadTooLarge),
    ScopeConflict,
}

/// Validates `CompleteUpload`'s `parts` list. Single-part only this round
/// (see module docs): exactly one part, numbered 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidParts;

pub fn validate_complete_upload_parts(part_numbers: &[u32]) -> Result<(), InvalidParts> {
    if part_numbers == [1] {
        Ok(())
    } else {
        Err(InvalidParts)
    }
}

// ---------------------------------------------------------------------------
// Shards (`CompleteShard`), docs/design/byo-ci.md "Idempotency"/"Completion
// semantics"
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardState {
    Pending,
    Uploaded,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteShardError {
    /// The run is already in a terminal state.
    RunTerminal,
    /// This shard was already marked `missing`.
    ShardMissing,
    /// The shard already concluded with a different conclusion.
    ConflictingConclusion,
}

/// Resolves whether a `CompleteShard` call is accepted, and with which
/// conclusion, per "`CompleteShard` is idempotent: calling it again with the
/// same `conclusion` is a no-op; calling it with a *different* conclusion
/// after the job is concluded is rejected (400)... An upload for a shard
/// already marked `missing`, or for a run that is already terminal, is also
/// rejected (400)."
pub fn resolve_complete_shard(
    run_terminal: bool,
    shard_state: ShardState,
    existing_conclusion: Option<Conclusion>,
    incoming: Conclusion,
) -> Result<Conclusion, CompleteShardError> {
    if run_terminal {
        return Err(CompleteShardError::RunTerminal);
    }
    match shard_state {
        ShardState::Missing => Err(CompleteShardError::ShardMissing),
        ShardState::Uploaded => match existing_conclusion {
            Some(existing) if existing == incoming => Ok(existing),
            _ => Err(CompleteShardError::ConflictingConclusion),
        },
        ShardState::Pending => Ok(incoming),
    }
}

/// Whether an upload/report may still land for this shard — the same
/// terminal/`missing` guard `CompleteShard` applies, per byo-ci.md's
/// Failure modes: "Shard uploads after it was marked `missing`, or after
/// the run is terminal | Rejected (400)".
pub fn upload_allowed_for_shard(run_terminal: bool, shard_state: ShardState) -> bool {
    !run_terminal && shard_state != ShardState::Missing
}

/// Severity ordering used to pick a job's conclusion from its shards'
/// conclusions (byo-ci.md: "the job concludes with the worst shard
/// conclusion", without pinning a numeric order). Chosen order, worst
/// first: `failure` (something is broken — the most actionable signal) >
/// `cancelled` (work never finished, but not because it was broken) >
/// `skipped` (an explicit, intentional opt-out — the mildest non-success)
/// > `success`.
pub fn conclusion_severity(c: Conclusion) -> u8 {
    match c {
        Conclusion::CONCLUSION_FAILURE => 3,
        Conclusion::CONCLUSION_CANCELLED => 2,
        Conclusion::CONCLUSION_SKIPPED => 1,
        Conclusion::CONCLUSION_SUCCESS | Conclusion::CONCLUSION_UNSPECIFIED => 0,
    }
}

pub fn worst_conclusion(a: Conclusion, b: Conclusion) -> Conclusion {
    if conclusion_severity(b) > conclusion_severity(a) {
        b
    } else {
        a
    }
}

/// Folds every shard's conclusion into the job's own, per "the job
/// concludes with the worst shard conclusion". `None` (no shards at all)
/// never happens in practice — `shard_total` is always at least 1 — but is
/// expressed rather than panicking on an empty slice.
pub fn job_conclusion_from_shards(shard_conclusions: &[Conclusion]) -> Option<Conclusion> {
    shard_conclusions.iter().copied().reduce(worst_conclusion)
}

// ---------------------------------------------------------------------------
// Run close (`workflow_run` webhook this round; `--expect-jobs` counting
// and the timeout alarm are separate, later signals — see the
// `workflow_run` module docs for why only the webhook is built this
// round), docs/design/byo-ci.md "Completion semantics"
// ---------------------------------------------------------------------------

/// One already-known shard row, as input to [`close_job`]. An index in
/// `1..=shard_total` with no entry here is implicitly `pending` — never
/// reached by any upload/`CompleteShard` call — same as everywhere else
/// in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardRecord {
    pub shard_index: u32,
    pub state: ShardState,
    pub conclusion: Option<Conclusion>,
}

/// What closing a run should do to one of its jobs, per "while a job
/// still has shards that did not upload, those shards are marked
/// `missing` at once... A job with a missing shard concludes `failure`
/// with the summary 'N of total shards missing'."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCloseDecision {
    /// Shard indices in `1..=shard_total` that have no `uploaded` or
    /// `missing` row yet and must be written as `missing` now.
    pub newly_missing: Vec<u32>,
    pub conclusion: Conclusion,
    /// `Some("N of total shards missing")` when at least one shard
    /// (already `missing`, or newly so) exists; `None` for a job that
    /// concluded from its own shard uploads alone.
    pub summary: Option<String>,
}

/// Decides how to close one job when its run closes. Idempotent by
/// construction: calling this again for a job every one of whose shards
/// is already `uploaded` or `missing` recomputes the same decision from
/// the same inputs, so redelivery of the same close signal is safe to
/// replay all the way down to this function, not just at the run level.
pub fn close_job(shard_total: u32, shards: &[ShardRecord]) -> JobCloseDecision {
    let mut newly_missing = Vec::new();
    let mut already_missing: u32 = 0;
    let mut uploaded_conclusions = Vec::new();

    for idx in 1..=shard_total {
        match shards.iter().find(|s| s.shard_index == idx) {
            Some(s) if s.state == ShardState::Uploaded => {
                if let Some(c) = s.conclusion {
                    uploaded_conclusions.push(c);
                }
            }
            Some(s) if s.state == ShardState::Missing => already_missing += 1,
            _ => newly_missing.push(idx),
        }
    }

    let missing_total = already_missing + newly_missing.len() as u32;
    let conclusion = if missing_total > 0 {
        Conclusion::CONCLUSION_FAILURE
    } else {
        uploaded_conclusions
            .into_iter()
            .reduce(worst_conclusion)
            .unwrap_or(Conclusion::CONCLUSION_SUCCESS)
    };
    let summary =
        (missing_total > 0).then(|| format!("{missing_total} of {shard_total} shards missing"));

    JobCloseDecision {
        newly_missing,
        conclusion,
        summary,
    }
}

/// Rolls every job's conclusion up to the run's own, mirroring
/// [`job_conclusion_from_shards`]'s "worst of its children" pattern
/// (byo-ci.md: "the run's conclusion is the worst job conclusion"). A run
/// closed with no jobs at all has nothing to be worse than success.
pub fn run_conclusion_from_jobs(job_conclusions: &[Conclusion]) -> Conclusion {
    job_conclusions
        .iter()
        .copied()
        .reduce(worst_conclusion)
        .unwrap_or(Conclusion::CONCLUSION_SUCCESS)
}

/// Narrows a computed run conclusion to the two terminal states the
/// webhook/`--expect-jobs` close path can produce (byo-ci.md's Completion
/// semantics flowchart: "closed: succeeded / failed" — `cancelled`/
/// `abandoned` are reached by other, unbuilt paths, not this one).
/// Deliberately computed from cloud-ci's own job/shard data, never from
/// GitHub's `workflow_run.conclusion` — see the `workflow_run` module
/// docs for why.
pub fn run_state_from_conclusion(conclusion: Conclusion) -> RunState {
    if conclusion == Conclusion::CONCLUSION_SUCCESS {
        RunState::Succeeded
    } else {
        RunState::Failed
    }
}

// ---------------------------------------------------------------------------
// Check Runs (`StartJob.check_names`, `CompleteShard`, run close),
// docs/design/byo-ci.md "Checks and scopes" and docs/design/pr-comment.md
// "Check Runs". Deliberately independent of `pr_comment.rs`'s template/
// context (see `coordinator` module docs' scope boundary) — this is its
// own simpler shard-table markdown.
// ---------------------------------------------------------------------------

/// Which of `incoming` check names this run has not already created a
/// Check Run for, per byo-ci.md's "Checks and scopes": "the Worker
/// creates any check name it hasn't seen yet for this run on the first
/// `StartJob` that names it" — first-seen-wins, keyed by `(run,
/// check_name)`. Also dedupes `incoming` against itself, so one
/// `StartJob` call naming the same check twice only creates it once.
/// Order-preserving.
pub fn new_check_names(existing: &[String], incoming: &[String]) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = existing.iter().map(String::as_str).collect();
    let mut result = Vec::new();
    for name in incoming {
        if seen.insert(name.as_str()) {
            result.push(name.clone());
        }
    }
    result
}

/// One job's row in a Check Run's shard-table summary
/// (pr-comment.md's "Check Runs": "A shard group maps to a single Check
/// Run, and the summary holds a shard table" — "if a check name is
/// shared by several jobs, the summary covers all of them"). `None`
/// `conclusion` means the job is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSummaryJobRow {
    pub job_name: String,
    pub completed_shards: u32,
    pub shard_total: u32,
    pub conclusion: Option<Conclusion>,
}

fn conclusion_label(c: Conclusion) -> &'static str {
    match c {
        Conclusion::CONCLUSION_SUCCESS => "success",
        Conclusion::CONCLUSION_FAILURE => "failure",
        Conclusion::CONCLUSION_CANCELLED => "cancelled",
        Conclusion::CONCLUSION_SKIPPED => "skipped",
        Conclusion::CONCLUSION_UNSPECIFIED => "unknown",
    }
}

/// Builds the markdown shard table a Check Run's `output.summary` holds
/// (pr-comment.md's "Check Runs": "the summary holds a shard table").
/// Rows are emitted in the order `rows` is given — this function does no
/// sorting, same as every other pure formatter in this module; the
/// caller controls row order.
pub fn render_check_summary(rows: &[CheckSummaryJobRow]) -> String {
    let mut out = String::from("| Job | Shards | Conclusion |\n| --- | --- | --- |\n");
    for row in rows {
        let conclusion = match row.conclusion {
            Some(c) => conclusion_label(c),
            None => "running",
        };
        out.push_str(&format!(
            "| {} | {}/{} | {} |\n",
            row.job_name, row.completed_shards, row.shard_total, conclusion
        ));
    }
    out
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
    fn is_terminal_is_true_only_for_the_four_terminal_states() {
        for state in [
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Abandoned,
        ] {
            assert!(state.is_terminal(), "{state:?}");
        }
        for state in [RunState::Queued, RunState::Running, RunState::Merging] {
            assert!(!state.is_terminal(), "{state:?}");
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

    #[test]
    fn create_upload_rejects_sizes_over_32_mib() {
        assert_eq!(
            resolve_create_upload(MAX_SINGLE_PART_BYTES + 1, "scope", None),
            Err(CreateUploadError::TooLarge(UploadTooLarge))
        );
        assert_eq!(
            resolve_create_upload(MAX_SINGLE_PART_BYTES, "scope", None),
            Ok(CreateUploadDecision::CreateNew)
        );
    }

    #[test]
    fn create_upload_with_no_existing_identity_creates_new() {
        assert_eq!(
            resolve_create_upload(10, "scope", None),
            Ok(CreateUploadDecision::CreateNew)
        );
    }

    #[test]
    fn create_upload_retry_with_same_scope_returns_existing() {
        let existing = ExistingUpload {
            upload_id: "up_1".to_string(),
            state: UploadState::Pending,
            scope: "web".to_string(),
        };
        assert_eq!(
            resolve_create_upload(10, "web", Some(&existing)),
            Ok(CreateUploadDecision::ReturnExisting {
                upload_id: "up_1".to_string(),
                already_complete: false,
            })
        );
    }

    #[test]
    fn create_upload_retry_of_complete_upload_reports_already_complete() {
        let existing = ExistingUpload {
            upload_id: "up_1".to_string(),
            state: UploadState::Complete,
            scope: "web".to_string(),
        };
        assert_eq!(
            resolve_create_upload(10, "web", Some(&existing)),
            Ok(CreateUploadDecision::ReturnExisting {
                upload_id: "up_1".to_string(),
                already_complete: true,
            })
        );
    }

    #[test]
    fn create_upload_resend_with_different_scope_is_rejected() {
        let existing = ExistingUpload {
            upload_id: "up_1".to_string(),
            state: UploadState::Complete,
            scope: "web".to_string(),
        };
        assert_eq!(
            resolve_create_upload(10, "api", Some(&existing)),
            Err(CreateUploadError::ScopeConflict)
        );
    }

    #[test]
    fn complete_upload_requires_exactly_one_part_numbered_one() {
        assert_eq!(validate_complete_upload_parts(&[1]), Ok(()));
        assert_eq!(validate_complete_upload_parts(&[]), Err(InvalidParts));
        assert_eq!(validate_complete_upload_parts(&[2]), Err(InvalidParts));
        assert_eq!(validate_complete_upload_parts(&[1, 2]), Err(InvalidParts));
    }

    #[test]
    fn complete_shard_first_call_is_accepted() {
        assert_eq!(
            resolve_complete_shard(
                false,
                ShardState::Pending,
                None,
                Conclusion::CONCLUSION_SUCCESS
            ),
            Ok(Conclusion::CONCLUSION_SUCCESS)
        );
    }

    #[test]
    fn complete_shard_repeat_call_with_same_conclusion_is_a_no_op() {
        assert_eq!(
            resolve_complete_shard(
                false,
                ShardState::Uploaded,
                Some(Conclusion::CONCLUSION_SUCCESS),
                Conclusion::CONCLUSION_SUCCESS
            ),
            Ok(Conclusion::CONCLUSION_SUCCESS)
        );
    }

    #[test]
    fn complete_shard_different_conclusion_after_upload_is_rejected() {
        assert_eq!(
            resolve_complete_shard(
                false,
                ShardState::Uploaded,
                Some(Conclusion::CONCLUSION_SUCCESS),
                Conclusion::CONCLUSION_FAILURE
            ),
            Err(CompleteShardError::ConflictingConclusion)
        );
    }

    #[test]
    fn complete_shard_for_missing_shard_is_rejected() {
        assert_eq!(
            resolve_complete_shard(
                false,
                ShardState::Missing,
                None,
                Conclusion::CONCLUSION_SUCCESS
            ),
            Err(CompleteShardError::ShardMissing)
        );
    }

    #[test]
    fn complete_shard_for_terminal_run_is_rejected() {
        assert_eq!(
            resolve_complete_shard(
                true,
                ShardState::Pending,
                None,
                Conclusion::CONCLUSION_SUCCESS
            ),
            Err(CompleteShardError::RunTerminal)
        );
    }

    #[test]
    fn upload_allowed_for_shard_rejects_missing_or_terminal() {
        assert!(upload_allowed_for_shard(false, ShardState::Pending));
        assert!(!upload_allowed_for_shard(false, ShardState::Missing));
        assert!(!upload_allowed_for_shard(true, ShardState::Pending));
    }

    #[test]
    fn conclusion_severity_orders_failure_above_cancelled_above_skipped_above_success() {
        assert!(
            conclusion_severity(Conclusion::CONCLUSION_FAILURE)
                > conclusion_severity(Conclusion::CONCLUSION_CANCELLED)
        );
        assert!(
            conclusion_severity(Conclusion::CONCLUSION_CANCELLED)
                > conclusion_severity(Conclusion::CONCLUSION_SKIPPED)
        );
        assert!(
            conclusion_severity(Conclusion::CONCLUSION_SKIPPED)
                > conclusion_severity(Conclusion::CONCLUSION_SUCCESS)
        );
    }

    #[test]
    fn job_conclusion_is_the_worst_of_its_shards() {
        assert_eq!(
            job_conclusion_from_shards(&[
                Conclusion::CONCLUSION_SUCCESS,
                Conclusion::CONCLUSION_FAILURE,
                Conclusion::CONCLUSION_SKIPPED,
            ]),
            Some(Conclusion::CONCLUSION_FAILURE)
        );
        assert_eq!(
            job_conclusion_from_shards(&[
                Conclusion::CONCLUSION_SUCCESS,
                Conclusion::CONCLUSION_SKIPPED,
            ]),
            Some(Conclusion::CONCLUSION_SKIPPED)
        );
        assert_eq!(job_conclusion_from_shards(&[]), None);
    }

    #[test]
    fn close_job_with_every_shard_uploaded_computes_the_worst_conclusion() {
        let shards = [
            ShardRecord {
                shard_index: 1,
                state: ShardState::Uploaded,
                conclusion: Some(Conclusion::CONCLUSION_SUCCESS),
            },
            ShardRecord {
                shard_index: 2,
                state: ShardState::Uploaded,
                conclusion: Some(Conclusion::CONCLUSION_SKIPPED),
            },
        ];
        let decision = close_job(2, &shards);
        assert_eq!(decision.newly_missing, Vec::<u32>::new());
        assert_eq!(decision.conclusion, Conclusion::CONCLUSION_SKIPPED);
        assert_eq!(decision.summary, None);
    }

    #[test]
    fn close_job_with_no_shards_uploaded_at_all_defaults_to_success() {
        // A job with zero shards ever reaching `uploaded` is a degenerate
        // case this function still answers deterministically: every index
        // becomes `newly_missing`, which forces `failure` regardless of
        // the (empty) uploaded-conclusions fold — covered by the next
        // test. This one exercises the fold's own default in isolation
        // via a `shard_total` of 0 (no indices to iterate at all).
        let decision = close_job(0, &[]);
        assert_eq!(decision.newly_missing, Vec::<u32>::new());
        assert_eq!(decision.conclusion, Conclusion::CONCLUSION_SUCCESS);
        assert_eq!(decision.summary, None);
    }

    #[test]
    fn close_job_marks_never_uploaded_shards_missing_and_concludes_failure() {
        let shards = [ShardRecord {
            shard_index: 1,
            state: ShardState::Uploaded,
            conclusion: Some(Conclusion::CONCLUSION_SUCCESS),
        }];
        // shard_total is 3; shard 1 uploaded, 2 and 3 never did.
        let decision = close_job(3, &shards);
        assert_eq!(decision.newly_missing, vec![2, 3]);
        assert_eq!(decision.conclusion, Conclusion::CONCLUSION_FAILURE);
        assert_eq!(decision.summary, Some("2 of 3 shards missing".to_string()));
    }

    #[test]
    fn close_job_is_idempotent_when_shards_are_already_marked_missing() {
        // Redelivery: a previous close already wrote shard 2 as `missing`.
        // Closing again must recompute the identical decision, including
        // not re-adding shard 2 to `newly_missing` (it already is
        // `missing`, nothing new to write).
        let shards = [
            ShardRecord {
                shard_index: 1,
                state: ShardState::Uploaded,
                conclusion: Some(Conclusion::CONCLUSION_SUCCESS),
            },
            ShardRecord {
                shard_index: 2,
                state: ShardState::Missing,
                conclusion: None,
            },
        ];
        let decision = close_job(2, &shards);
        assert_eq!(decision.newly_missing, Vec::<u32>::new());
        assert_eq!(decision.conclusion, Conclusion::CONCLUSION_FAILURE);
        assert_eq!(decision.summary, Some("1 of 2 shards missing".to_string()));
    }

    #[test]
    fn run_conclusion_is_the_worst_of_its_jobs() {
        assert_eq!(
            run_conclusion_from_jobs(&[
                Conclusion::CONCLUSION_SUCCESS,
                Conclusion::CONCLUSION_FAILURE,
            ]),
            Conclusion::CONCLUSION_FAILURE
        );
        assert_eq!(
            run_conclusion_from_jobs(&[Conclusion::CONCLUSION_SUCCESS]),
            Conclusion::CONCLUSION_SUCCESS
        );
    }

    #[test]
    fn run_conclusion_with_no_jobs_at_all_defaults_to_success() {
        assert_eq!(
            run_conclusion_from_jobs(&[]),
            Conclusion::CONCLUSION_SUCCESS
        );
    }

    #[test]
    fn run_state_from_conclusion_only_maps_success_to_succeeded() {
        assert_eq!(
            run_state_from_conclusion(Conclusion::CONCLUSION_SUCCESS),
            RunState::Succeeded
        );
        for conclusion in [
            Conclusion::CONCLUSION_FAILURE,
            Conclusion::CONCLUSION_CANCELLED,
            Conclusion::CONCLUSION_SKIPPED,
            Conclusion::CONCLUSION_UNSPECIFIED,
        ] {
            assert_eq!(run_state_from_conclusion(conclusion), RunState::Failed);
        }
    }

    #[test]
    fn new_check_names_returns_only_names_not_already_created() {
        let existing = vec!["lint".to_string()];
        let incoming = vec!["lint".to_string(), "e2e".to_string()];
        assert_eq!(
            new_check_names(&existing, &incoming),
            vec!["e2e".to_string()]
        );
    }

    #[test]
    fn new_check_names_dedupes_repeats_within_incoming() {
        let incoming = vec!["e2e".to_string(), "e2e".to_string(), "lint".to_string()];
        assert_eq!(
            new_check_names(&[], &incoming),
            vec!["e2e".to_string(), "lint".to_string()]
        );
    }

    #[test]
    fn new_check_names_empty_incoming_is_a_no_op() {
        assert_eq!(
            new_check_names(&["lint".to_string()], &[]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn new_check_names_preserves_incoming_order() {
        let incoming = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(new_check_names(&[], &incoming), incoming);
    }

    #[test]
    fn render_check_summary_shows_running_for_unconcluded_jobs() {
        let rows = [CheckSummaryJobRow {
            job_name: "e2e".to_string(),
            completed_shards: 2,
            shard_total: 4,
            conclusion: None,
        }];
        let summary = render_check_summary(&rows);
        assert!(summary.contains("| e2e | 2/4 | running |"));
    }

    #[test]
    fn render_check_summary_shows_conclusion_label_once_a_job_concludes() {
        let rows = [CheckSummaryJobRow {
            job_name: "unit".to_string(),
            completed_shards: 4,
            shard_total: 4,
            conclusion: Some(Conclusion::CONCLUSION_FAILURE),
        }];
        let summary = render_check_summary(&rows);
        assert!(summary.contains("| unit | 4/4 | failure |"));
    }

    #[test]
    fn render_check_summary_covers_every_job_sharing_a_check() {
        let rows = [
            CheckSummaryJobRow {
                job_name: "unit".to_string(),
                completed_shards: 4,
                shard_total: 4,
                conclusion: Some(Conclusion::CONCLUSION_SUCCESS),
            },
            CheckSummaryJobRow {
                job_name: "e2e".to_string(),
                completed_shards: 1,
                shard_total: 4,
                conclusion: None,
            },
        ];
        let summary = render_check_summary(&rows);
        assert!(summary.contains("unit"));
        assert!(summary.contains("e2e"));
    }
}
