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

/// "First sha wins": `BeginRun`'s admission-time `settings_sha` resolve
/// (coordinator/mod.rs's "Settings SHA (frozen at admission)" module doc
/// section) is an `await` boundary a duplicate, reordered, or
/// redelivered `BeginRun` can race across. `existing_sha` is whatever
/// this run's row already has stored by the time the race is checked —
/// `Some` only once some delivery (this one or a race winner) has ever
/// successfully inserted a row; `resolved_sha` is what *this* delivery
/// just resolved. A stored sha, once set, is never replaced: `Some`
/// always wins regardless of what `resolved_sha` is (even if the
/// repo's default branch moved between two deliveries' own resolves) —
/// only a brand-new row (`existing_sha: None`) ever gets this
/// delivery's freshly resolved value.
pub fn resolve_admission_settings_sha<'a>(
    existing_sha: Option<&'a str>,
    resolved_sha: &'a str,
) -> &'a str {
    existing_sha.unwrap_or(resolved_sha)
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
// `--expect-jobs` job-count closing (docs/design/byo-ci.md's Completion
// semantics: "`--expect-jobs N`: N jobs complete" — a second, independent
// trigger for the same run-close operation `workflow_run.rs`'s completed
// webhook also triggers; see `coordinator` module docs for the shared
// `handle_close_run` both callers reuse). The timeout alarm, byo-ci.md's
// third close trigger, still needs a Durable Object alarm and is not
// built by either this function or its caller this round.
// ---------------------------------------------------------------------------

/// Whether a run's declared `expect_jobs` are all satisfied, so its
/// caller should trigger the same close-run operation the
/// `workflow_run.completed` webhook triggers.
///
/// `expect_jobs` in this codebase is already a *named* job list, not a
/// bare count (see [`resolve_expect_jobs`] — `BeginRun`'s `repeated
/// string expect_jobs` field, "the first non-empty `expect_jobs` wins").
/// The doc's CLI-facing "`--expect-jobs N`: N jobs complete" wording is
/// read through that existing shape: "N jobs complete" becomes "every
/// job named in `expect_jobs` has started (`StartJob`) and reached a
/// concluded state" — set-completeness over the declared names, not a
/// raw count comparison.
///
/// A run with no `expect_jobs` stored (`None`, or an empty list — same
/// as [`resolve_expect_jobs`]'s "no opinion" meaning) never closes via
/// this path, only via the webhook or (later) the timeout alarm.
///
/// If a caller starts *more* jobs than it declared in `expect_jobs`,
/// that is not an error this round: `expect_jobs` is treated as a floor
/// (the complete set to wait for), not a hard cap on what may run — once
/// every declared name is started and concluded, the run closes
/// regardless of any extra, undeclared job still running. This matches
/// the doc's literal wording ("once N jobs are complete, the run
/// closes") by substituting named-job completeness for count
/// completeness, since the field already stores names, not a number.
pub fn expect_jobs_satisfied(
    expect_jobs: Option<&[String]>,
    started_jobs: &[(String, bool)],
) -> bool {
    let Some(expected) = expect_jobs else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }
    expected.iter().all(|name| {
        started_jobs
            .iter()
            .any(|(started_name, concluded)| started_name == name && *concluded)
    })
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
// Run close (`workflow_run` webhook and `--expect-jobs` counting both
// trigger this round, via `coordinator::mod`'s `handle_close_run` and
// `maybe_close_for_expect_jobs` — see those module docs; the timeout
// alarm is still separate, later work), docs/design/byo-ci.md
// "Completion semantics"
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
// Timeout (`BeginRun`'s `timeout`, docs/design/byo-ci.md's `BeginRun` row:
// "timeout (optional, default 30 minutes, clamped to the deployment-wide
// maximum)") and its close trigger's forced `abandoned` state (Failure
// modes row: "A shard never uploads ... The completion webhook or
// `--expect-jobs` marks it `missing` when the run closes; otherwise the
// timeout marks it `missing` and the run moves to `abandoned`" —
// unconditional, unlike the webhook/`--expect-jobs` triggers' computed
// `succeeded`/`failed`). `coordinator::mod`'s `handle_begin_run` sets a DO
// alarm for `now + resolve_timeout_seconds(...)` the first time a run is
// created; the alarm handler calls `handle_close_run` with `by_timeout:
// true`, which uses `run_state_for_close` below instead of
// `run_state_from_conclusion`.
// ---------------------------------------------------------------------------

/// byo-ci.md's `BeginRun` row default: "default 30 minutes".
pub const DEFAULT_TIMEOUT_SECONDS: i64 = 30 * 60;

/// byo-ci.md names a "deployment-wide maximum" clamp (both in the
/// `BeginRun` row and again in the Completion semantics' timeout bullet)
/// but never gives it a number anywhere in the doc. 6 hours is this
/// implementation's own choice, not the doc's: generous enough for a
/// slow full matrix/e2e suite while still bounding how long a
/// `RunCoordinator` can sit with an open DO alarm.
pub const MAX_TIMEOUT_SECONDS: i64 = 6 * 60 * 60;

/// Resolves `BeginRun`'s requested timeout (seconds, from the proto
/// `google.protobuf.Duration`) to what gets stored and used for the DO
/// alarm. `requested <= 0` means "omitted" (the `Duration` defaults to
/// zero when the field is absent), using [`DEFAULT_TIMEOUT_SECONDS`];
/// anything above [`MAX_TIMEOUT_SECONDS`] is clamped down to it.
pub fn resolve_timeout_seconds(requested: i64) -> i64 {
    let base = if requested <= 0 {
        DEFAULT_TIMEOUT_SECONDS
    } else {
        requested
    };
    base.min(MAX_TIMEOUT_SECONDS)
}

/// Chooses the run's terminal state when closing, per byo-ci.md's three
/// completion triggers. The webhook and `--expect-jobs` triggers compute
/// `succeeded`/`failed` from the worst job conclusion
/// ([`run_state_from_conclusion`]); the timeout trigger is unconditional
/// `abandoned`, per the Failure modes row for a never-uploaded shard:
/// "otherwise the timeout marks it `missing` and the run moves to
/// `abandoned`" — regardless of what the jobs' conclusions would
/// otherwise compute to.
pub fn run_state_for_close(by_timeout: bool, conclusion: Conclusion) -> RunState {
    if by_timeout {
        RunState::Abandoned
    } else {
        run_state_from_conclusion(conclusion)
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

// ---------------------------------------------------------------------------
// Nodes (`startNode`/completion/`ack`, dynamic-pipelines.md's "### Execution
// model" idempotency table). Pure decision logic only this round, backing
// `RunCoordinator`'s node RPCs — see `coordinator` module docs' scope
// boundary for what is and is not built yet.
// ---------------------------------------------------------------------------

/// A node's lifecycle state, per dynamic-pipelines.md's `startNode`/
/// completion/timeout rows: `Pending`/`Running` are non-terminal;
/// `Succeeded`/`Failed`/`Skipped`/`Cancelled`/`TimedOut` are terminal.
/// Matches D1 `nodes.status`'s documented values
/// (`pending`/`cached`/`running`/`succeeded`/`failed`/`skipped`) plus
/// `cancelled`/`timed_out` from the failure-modes table — `Cached` is not
/// included here: it is `turbo.execute`'s own cache-hit skip decision
/// (dynamic-pipelines.md's "User experience" `ci.cached`), never a state
/// `startNode`/`completeNode` themselves transition a node through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
    TimedOut,
}

impl NodeState {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "skipped" => Some(Self::Skipped),
            "cancelled" => Some(Self::Cancelled),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }

    /// `Cancelled`/`TimedOut` are terminal alongside the three ordinary
    /// conclusions, per the failure-modes table: a cancelled or
    /// timed-out node never has further work done to it (completion
    /// events for it are dropped — see [`resolve_complete_node`]).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Skipped | Self::Cancelled | Self::TimedOut
        )
    }
}

/// The row already occupying `node_id` within this run, if one exists —
/// [`resolve_start_node`]'s only input besides the incoming spec hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingNode {
    pub spec_hash: String,
    pub status: NodeState,
}

/// What a `startNode` call should do, once an existing row (if any) has
/// been looked up by `node_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartNodeDecision {
    /// No row existed for this `node_id` yet: create the row and start
    /// the node's real container (`coordinator::mod`'s
    /// `start_node_container`, backed by `node_container.rs`).
    Started,
    /// A row already exists with the same spec hash: per
    /// "`startNode` retried after the container already started ->
    /// returns the existing node; never starts a second container",
    /// this is the idempotent replay path. The caller never calls
    /// `start_node_container` again for this decision.
    AlreadyStarted { status: NodeState },
}

/// A `startNode` call named an id that already has a *different* spec
/// hash recorded — "`startNode` with the same id but a different spec
/// hash -> Rejected as nondeterministic; run fails". This function
/// itself never fails the run: see [`resolve_start_node`]'s doc comment
/// for why that is deliberately the caller's job, not this one's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NondeterministicReplay;

/// Resolves a `startNode(run_id, node_id, spec_hash)` call against
/// whatever row (if any) already exists for `node_id` in this run,
/// implementing dynamic-pipelines.md's idempotency table's first two
/// rows exactly.
///
/// **Who fails the run on `Err`.** The doc says a spec-hash mismatch is
/// "rejected as nondeterministic; run fails". This function returns a
/// typed [`NondeterministicReplay`] error and does *not* itself decide
/// to transition the whole run to a failed terminal state — there is no
/// real Dynamic Workflow caller yet (see `coordinator` module docs'
/// scope boundary) to have asked for that, and guessing at the shape of
/// "the run is now failed" (does the workflow instance get terminated?
/// do other in-flight nodes get cancelled too, same as whole-run
/// cancellation?) without that caller would be exactly the kind
/// of speculative API this round must not build. The future
/// Workflow-integration round's caller owns turning this error into an
/// actual run failure, once it exists to decide how.
pub fn resolve_start_node(
    existing: Option<&ExistingNode>,
    incoming_spec_hash: &str,
) -> Result<StartNodeDecision, NondeterministicReplay> {
    match existing {
        None => Ok(StartNodeDecision::Started),
        Some(node) if node.spec_hash == incoming_spec_hash => {
            Ok(StartNodeDecision::AlreadyStarted {
                status: node.status,
            })
        }
        Some(_) => Err(NondeterministicReplay),
    }
}

/// A `completeNode` call supplied a terminal status that conflicts with
/// one already recorded for this node (not the cancelled-drop case —
/// see [`resolve_complete_node`]'s `DroppedCancelled` arm for that).
/// Mirrors `CompleteShardError::ConflictingConclusion`'s "same shape,
/// different domain" pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteNodeError {
    ConflictingStatus,
}

/// What `completeNode` should do, once the node's current status is
/// known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteNodeDecision {
    /// Record `incoming_status` as this node's terminal state (a no-op
    /// rewrite if it already matches — "calling it again with the same
    /// conclusion is a no-op", same idempotency shape as
    /// `resolve_complete_shard`).
    Recorded,
    /// The node is already `Cancelled`: "late completion events for
    /// cancelled nodes are dropped". The caller must not overwrite the
    /// `Cancelled` status with `incoming_status`, and must treat this
    /// as a successful ack-worthy no-op, not an error — a redelivered
    /// completion for a cancelled node is expected, not exceptional.
    DroppedCancelled,
}

/// Resolves a `completeNode(run_id, node_id, status, result)` call
/// against the node's current status, implementing dynamic-pipelines.md's
/// "Run cancelled" row's drop clause and the general "completion event
/// delivered twice" idempotency: `incoming_status` must itself already be
/// one of [`NodeState::is_terminal`]'s terminal values — validating that
/// is the caller's job, since a `completeNode` call can never report a
/// node as still `Pending`/`Running`.
pub fn resolve_complete_node(
    current_status: NodeState,
    incoming_status: NodeState,
) -> Result<CompleteNodeDecision, CompleteNodeError> {
    if current_status == NodeState::Cancelled {
        return Ok(CompleteNodeDecision::DroppedCancelled);
    }
    if current_status.is_terminal() && current_status != incoming_status {
        return Err(CompleteNodeError::ConflictingStatus);
    }
    Ok(CompleteNodeDecision::Recorded)
}

/// Resolves an `ackNode` call against the node's current `acked` flag.
/// Returns whether the caller needs to write `acked = true` at all:
/// `false` once a node is already acked, so a redelivered `ack` (the
/// doc's "`ack` is recorded on the node by the next step after
/// `waitForEvent`") never issues a second write. Always `Ok` — acking
/// an already-terminal, or even an already-cancelled, node is never a
/// conflict; unlike [`resolve_complete_node`], there is no status to
/// disagree about here.
pub fn resolve_ack_node(current_acked: bool) -> bool {
    !current_acked
}

/// Which of `nodes`' ids a run cancellation should mark `Cancelled` and attempt to *stop* —
/// "Coordinator stops containers, marks nodes `cancelled`...". A non-terminal node is always
/// included: it needs both the status write and the stop. An already-`Cancelled` node with a
/// real `physical_address` is included too, even though its status write already happened on an
/// earlier call: [`crate::coordinator::RunCoordinator::ensure_sibling_cancelled`] writes
/// `Cancelled` *before* attempting the real stop (so a destroy-induced completion racing in
/// behind it lands on an already-terminal node and is dropped, not recorded over), which means a
/// node whose stop genuinely failed is already `Cancelled` by the time a redelivered
/// `cancel-run` call runs again — excluding it here would leak its container forever. Retrying
/// it is safe: `NodeContainer`'s `/stop` is idempotent on an already-exited/-destroyed container
/// (always 200, `stopped: false`), so retrying a node whose container really was already
/// destroyed is a harmless no-op, while a node whose stop genuinely failed gets the retry it
/// would otherwise never receive. A `None` address is excluded even when `Cancelled`:
/// [`resolve_stop_outcome`] already treats a missing address as permanently unresolvable, so
/// retrying it here would only repeat the same unresolvable warning forever for no benefit. A
/// node that reached a genuinely different terminal outcome
/// (`Succeeded`/`Failed`/`Skipped`/`TimedOut`) keeps that real outcome, never relabeled
/// `Cancelled` after the fact regardless of `physical_address`. Order-preserving.
pub fn nodes_to_retry_cancel(nodes: &[(String, NodeState, bool)]) -> Vec<String> {
    nodes
        .iter()
        .filter(|(_, status, has_physical_address)| {
            !status.is_terminal() || (*status == NodeState::Cancelled && *has_physical_address)
        })
        .map(|(id, _, _)| id.clone())
        .collect()
}

/// Maps a real container's exit code to the node's terminal status —
/// `node_container.rs`'s `run_and_report` uses this to decide
/// `completeNode`'s `status` once `exec()` resolves: `0` is
/// `Succeeded`, anything else is `Failed`. A container that fails to
/// even *start* (bad image, Docker/runtime error) never reaches this
/// function at all — that is `start_node_container`'s own `Err` path,
/// mapped to `Failed` directly (`coordinator::mod`'s `handle_start_node`),
/// since there is no exit code to map in that case.
pub fn node_status_for_exit_code(exit_code: u32) -> NodeState {
    if exit_code == 0 {
        NodeState::Succeeded
    } else {
        NodeState::Failed
    }
}

// ---------------------------------------------------------------------------
// `test_stats` finalization (docs/design/analytics.md's "D1 rollup
// tables" / "Idempotency"), backing `coordinator::mod`'s
// `finalize_test_stats` — the once-per-run write path that applies every
// canonical, parsed report's test outcomes to the `test_stats` rolling
// aggregate. Pure, no-I/O logic only; `finalize_test_stats` supplies the
// parsed report bytes (via `cloud-ci-reports`) and does the actual D1
// `batch()` call.
// ---------------------------------------------------------------------------

/// `duration_ewma_ms`'s smoothing factor (analytics.md: "exponential
/// moving average, alpha = 0.2").
pub const EWMA_ALPHA: f64 = 0.2;

/// `recent_outcomes`'s fixed capacity (analytics.md: "last 20 outcomes").
pub const RECENT_OUTCOMES_CAP: usize = 20;

/// `test_id = hex(sha256(file_path || 0x1f || full_test_name))[0:16]`
/// (analytics.md's `test_failures`/`test_stats` schema comment). `0x1f`
/// (ASCII Unit Separator) joins the two fields unambiguously — unlike a
/// printable delimiter, it cannot itself appear in a legitimate file path
/// or test name, so two distinct `(file_path, full_test_name)` pairs never
/// collide by one field "swallowing" the separator. Truncated to the
/// first 16 hex characters (8 bytes) of the digest, per the doc's
/// `[0:16]`.
pub fn test_id(file_path: &str, full_test_name: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(file_path.as_bytes());
    hasher.update([0x1f]);
    hasher.update(full_test_name.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The "full test name" half of [`test_id`]'s input. `cloud-ci-reports`'
/// `TestCase` separates `classname` (xUnit-family formats' enclosing
/// class/module, e.g. pytest's `--junitxml` output) from `name` (the bare
/// test name, which on its own can collide across classes/modules in the
/// same file); joining them with `::` when a classname is present mirrors
/// Rust/pytest's own conventional "module::test" display format and keeps
/// `test_id` stable across reports that do/don't populate `classname` for
/// the exact same test, as long as both agree on it. A `None` or empty
/// classname (Vitest/Playwright, which populate `file` but not
/// `classname`) falls back to the bare name.
pub fn full_test_name(classname: Option<&str>, name: &str) -> String {
    match classname {
        Some(c) if !c.is_empty() => format!("{c}::{name}"),
        _ => name.to_string(),
    }
}

/// `recent_outcomes`'s per-run character and `test_stats.last_status`'s
/// word, derived once from a parsed [`cloud_ci_reports::Outcome`] (see
/// `coordinator::mod`'s `test_outcome_of`). `Errored` is folded into
/// `Failed` — analytics.md's `recent_outcomes` comment only names three
/// outcomes (`'P'/'F'/'S'`), and `parse_report`'s existing
/// `summarize_test_suites` already treats `Errored` as a failure-shaped
/// outcome for aggregate counting, so this matches that precedent rather
/// than inventing a fourth bucket nothing downstream expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestOutcomeKind {
    Passed,
    Failed,
    Skipped,
}

impl TestOutcomeKind {
    pub fn as_char(self) -> char {
        match self {
            TestOutcomeKind::Passed => 'P',
            TestOutcomeKind::Failed => 'F',
            TestOutcomeKind::Skipped => 'S',
        }
    }

    pub fn as_status_word(self) -> &'static str {
        match self {
            TestOutcomeKind::Passed => "pass",
            TestOutcomeKind::Failed => "fail",
            TestOutcomeKind::Skipped => "skip",
        }
    }

    /// Inverse of [`Self::as_char`] — `coordinator::mod`'s
    /// `test_event_overflow` table round-trips a persisted
    /// [`TestEventPoint`] through this exact char, so a later
    /// alarm-triggered flush reconstructs the same outcome it stored.
    pub fn from_char(c: char) -> Option<Self> {
        match c {
            'P' => Some(TestOutcomeKind::Passed),
            'F' => Some(TestOutcomeKind::Failed),
            'S' => Some(TestOutcomeKind::Skipped),
            _ => None,
        }
    }
}

/// One test case's outcome, reduced to exactly what `finalize_test_stats`
/// (`coordinator::mod`) needs to upsert into `test_stats` — file/name
/// already combined into [`test_id`], duration already in whole
/// milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct TestOutcomeRow {
    pub test_id: String,
    pub file_path: String,
    pub test_name: String,
    pub duration_ms: i64,
    pub outcome: TestOutcomeKind,
}

/// One `test`-kind Analytics Engine data point, in
/// docs/design/analytics.md's "Analytics Engine schema" table's exact
/// column order for that row kind: `blob1="test"`, `blob2=run_id`,
/// `blob3=job_id`, `blob4=test_id`, `double1=duration_ms`,
/// `double2=pass(1)/fail(0)/skip(-1)`, `double3` unused (the table's `—`
/// cell), `index1=repo_id`. Pure data, no `worker`-crate dependency — the
/// thin `write_test_events` caller in `coordinator::mod` turns this into
/// the actual `AnalyticsEngineDataPointBuilder` calls.
#[derive(Debug, Clone, PartialEq)]
pub struct TestEventPoint {
    pub run_id: String,
    pub job_id: String,
    pub test_id: String,
    pub duration_ms: i64,
    pub outcome: TestOutcomeKind,
    pub repo_id: i64,
}

impl TestOutcomeKind {
    /// analytics.md's `test` row `double2`: `pass(1)/fail(0)/skip(-1)`.
    pub fn as_test_double(self) -> f64 {
        match self {
            TestOutcomeKind::Passed => 1.0,
            TestOutcomeKind::Failed => 0.0,
            TestOutcomeKind::Skipped => -1.0,
        }
    }
}

/// Builds one `test` Analytics Engine data point per parsed test-case
/// outcome ([`TestOutcomeRow`], from `coordinator::mod`'s
/// `parse_test_outcomes` — the same per-test data `handle_submit_report`
/// already produces while parsing an uploaded report). `run_id`/`job_id`/
/// `repo_id` are shared across every row in one report; `rows` is
/// whatever `parse_test_outcomes` returned for that report's bytes.
pub fn build_test_events(
    run_id: &str,
    job_id: &str,
    repo_id: i64,
    rows: &[TestOutcomeRow],
) -> Vec<TestEventPoint> {
    rows.iter()
        .map(|row| TestEventPoint {
            run_id: run_id.to_string(),
            job_id: job_id.to_string(),
            test_id: row.test_id.clone(),
            duration_ms: row.duration_ms,
            outcome: row.outcome,
            repo_id,
        })
        .collect()
}

/// Splits `events` into the slice one Worker invocation writes
/// immediately and the slice that must be deferred — persisted to
/// overflow storage for a later invocation to drain
/// (`coordinator::mod`'s `write_test_events`/`test_event_overflow`).
/// Pure arithmetic, no `worker`-crate dependency: the two returned
/// slices always partition `events` exactly (same total length, same
/// order, no element dropped or duplicated) — the property that makes
/// the overflow path loss-free is provable here without a Durable
/// Object.
pub fn split_for_invocation_budget<T>(events: &[T], budget: usize) -> (&[T], &[T]) {
    let split = events.len().min(budget);
    events.split_at(split)
}

/// One `sample`-kind Analytics Engine data point, in
/// docs/design/analytics.md's "Analytics Engine schema" table's `sample`
/// row shape, extended beyond that table's original four-blob/three-
/// double sketch to carry the per-sample identity/timing fields a real
/// `SubmitResourceSamples` request supplies: `blob1="sample"`,
/// `blob2=run_id`, `blob3=job_id`, `blob4=instance_type`,
/// `blob5=shard_index` (decimal string), `blob6=attempt` (decimal
/// string), `double1=cpu_usage_usec_delta`, `double2=memory_current_bytes`,
/// `double3=memory_peak_bytes`, `double4=elapsed_usec`,
/// `double5=timestamp_unix_ms`, `double6=memory_peak_known`
/// (`1.0`/`0.0`), `index1=repo_id`. Pure data, no `worker`-crate
/// dependency — the thin `write_sample_events`/`flush_sample_event_overflow`
/// callers in `coordinator::mod` turn this into the actual
/// `AnalyticsEngineDataPointBuilder` calls, same split as [`TestEventPoint`].
///
/// **Why a presence flag instead of a `NaN` sentinel.** An earlier
/// version of this struct used `f64::NAN` for "the kernel never
/// recorded a peak". That is unsafe for this exact transport: `double3`
/// round-trips through both a real `write_data_point` call (Cloudflare's
/// documented behavior for non-finite doubles there is not verified
/// against this deployment, and some analytics/telemetry backends
/// reject or silently coerce non-finite values) and this DO's own
/// `sample_event_overflow` SQLite table (a value can sit queued there
/// across a crash/restart before ever reaching AE). `JSON.stringify`
/// also cannot represent `NaN` at all (it serializes to `null`), which
/// would silently corrupt it the moment it touched *any* JSON encoding
/// path. `memory_peak_bytes` instead always holds a finite number
/// (`0.0` when unknown — never treated as "the real peak" on its own),
/// and `memory_peak_known` is the only thing a query should ever branch
/// on to tell a real zero-byte peak apart from "not recorded"; a
/// rollup/dashboard query MUST filter or group on `double6` before
/// trusting `double3`, exactly as `recent_outcomes`/`flakiness_score`
/// readers already have to respect this schema's other derived-field
/// conventions.
///
/// `memory_peak_bytes`/`memory_peak_known` are deliberately the *same*
/// value on every point built from one batch (job-level, not
/// per-sample). Analytics Engine rows are flat and unjoinable — every
/// other kind in the schema table already repeats its own
/// row-identifying context (`blob2=run_id`, `blob3=job_id`,
/// `index1=repo_id`) on every single row rather than storing it once
/// and joining later — so repeating the batch's one peak value/presence
/// pair across all of its sample rows follows the same
/// denormalized-by-design pattern, not an accident.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleEventPoint {
    pub run_id: String,
    pub job_id: String,
    pub shard_index: u32,
    pub attempt: u32,
    pub instance_type: String,
    pub timestamp_unix_ms: f64,
    pub elapsed_usec: f64,
    pub cpu_usage_usec_delta: f64,
    pub memory_current_bytes: f64,
    /// Always finite. `0.0` and `memory_peak_known: false` together mean
    /// "not recorded" — never treat this field alone as "the real peak"
    /// without checking [`Self::memory_peak_known`] first.
    pub memory_peak_bytes: f64,
    pub memory_peak_known: bool,
    pub repo_id: i64,
}

/// One batched sample's input shape to [`build_sample_events`] — the
/// per-sample fields a `SubmitResourceSamplesRequest`'s
/// `repeated ResourceSample samples` carries. Mirrors
/// `cloud_ci_proto::ingest::v1::ResourceSample`'s fields exactly; kept
/// as its own small struct here (rather than depending on the generated
/// proto type directly) so this module's pure functions stay provable
/// with plain Rust values in `cargo test`, independent of `buffa`'s
/// generated code shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSampleInput {
    pub timestamp_unix_ms: u64,
    pub elapsed_usec: u64,
    pub cpu_usage_usec_delta: u64,
    pub memory_current_bytes: u64,
}

/// Builds one `sample` Analytics Engine data point per batched
/// [`ResourceSampleInput`] — the complete per-2s-tick series a real
/// agent accumulates for one job (or shard attempt) and submits once,
/// at job end, via `SubmitResourceSamples` (docs/design/analytics.md's
/// "Data flow"). `memory_peak_bytes: None` (the kernel never recorded
/// one) becomes `(0.0, false)` on every row — see [`SampleEventPoint`]'s
/// own doc comment for why a presence flag replaces an earlier `NaN`
/// sentinel.
#[allow(clippy::too_many_arguments)]
pub fn build_sample_events(
    run_id: &str,
    job_id: &str,
    shard_index: u32,
    attempt: u32,
    repo_id: i64,
    instance_type: &str,
    memory_peak_bytes: Option<u64>,
    samples: &[ResourceSampleInput],
) -> Vec<SampleEventPoint> {
    let memory_peak_known = memory_peak_bytes.is_some();
    let memory_peak_bytes = memory_peak_bytes.map(|v| v as f64).unwrap_or(0.0);
    samples
        .iter()
        .map(|sample| SampleEventPoint {
            run_id: run_id.to_string(),
            job_id: job_id.to_string(),
            shard_index,
            attempt,
            instance_type: instance_type.to_string(),
            timestamp_unix_ms: sample.timestamp_unix_ms as f64,
            elapsed_usec: sample.elapsed_usec as f64,
            cpu_usage_usec_delta: sample.cpu_usage_usec_delta as f64,
            memory_current_bytes: sample.memory_current_bytes as f64,
            memory_peak_bytes,
            memory_peak_known,
            repo_id,
        })
        .collect()
}

/// `SubmitResourceSamplesRequest.node_id`'s documented bound (`cloud-ci-proto`):
/// at most 256 bytes, no control characters.
pub const MAX_NODE_ID_BYTES: usize = 256;

/// Why [`validate_node_id`] rejected a `node_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIdError {
    /// Over [`MAX_NODE_ID_BYTES`] bytes.
    TooLong { len: usize },
    /// Contains a Unicode control character (`char::is_control`) --
    /// embedding one in a node id that later appears in logs/responses
    /// risks terminal/log injection.
    ControlCharacter,
}

impl std::fmt::Display for NodeIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeIdError::TooLong { len } => write!(
                f,
                "node_id must be at most {MAX_NODE_ID_BYTES} bytes, got {len}"
            ),
            NodeIdError::ControlCharacter => {
                write!(f, "node_id must not contain control characters")
            }
        }
    }
}

impl std::error::Error for NodeIdError {}

/// Validates `node_id` against its own documented bound. An empty string
/// is always valid -- it already folds to "no node" in
/// [`resource_sample_batch_content_hash`], same as an absent value.
pub fn validate_node_id(node_id: &str) -> Result<(), NodeIdError> {
    if node_id.is_empty() {
        return Ok(());
    }
    if node_id.len() > MAX_NODE_ID_BYTES {
        return Err(NodeIdError::TooLong { len: node_id.len() });
    }
    if node_id.chars().any(|c| c.is_control()) {
        return Err(NodeIdError::ControlCharacter);
    }
    Ok(())
}
// ---------------------------------------------------------------------------
// `SubmitResourceSamples` idempotency (`resource_sample_batch`'s DO row,
// keyed by `(job_id, shard_index, attempt)` — this execution identity
// accepts exactly one immutable batch; `coordinator::mod`'s own module
// docs cover the full DO-local-atomicity/overflow/AE-delivery mechanism).
// ---------------------------------------------------------------------------

/// What a `SubmitResourceSamples` call should do, once the execution
/// identity's existing content hash (if any) is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitResourceSamplesDecision {
    /// No row exists yet for this `(job_id, shard_index, attempt)` —
    /// accept it: insert the batch row and its AE points/overflow rows.
    Accepted,
    /// A row already exists with the identical content hash — a clean
    /// no-op replay; nothing is re-inserted or re-delivered.
    AlreadyAccepted,
    /// A row already exists with a *different* content hash — rejected,
    /// never silently overwritten (same "same shape, different domain"
    /// pattern as [`resolve_complete_shard`]/[`resolve_shard_terminal`]).
    Conflict,
}

/// Resolves a `SubmitResourceSamples` call against whatever content hash
/// (if any) `resource_sample_batch` already has for this exact
/// `(job_id, shard_index, attempt)` key.
pub fn resolve_submit_resource_samples(
    existing_hash: Option<&str>,
    incoming_hash: &str,
) -> SubmitResourceSamplesDecision {
    match existing_hash {
        None => SubmitResourceSamplesDecision::Accepted,
        Some(existing) if existing == incoming_hash => {
            SubmitResourceSamplesDecision::AlreadyAccepted
        }
        Some(_) => SubmitResourceSamplesDecision::Conflict,
    }
}

/// Deterministic content hash over `SubmitResourceSamplesRequest`'s own
/// *typed* fields — never over raw wire bytes. `buffa`'s Connect
/// transport supports more than one codec (JSON, binary protobuf); two
/// requests carrying identical logical content encoded through different
/// codecs would hash differently if this hashed the raw request body,
/// which would make [`resolve_submit_resource_samples`]'s idempotency
/// check codec-dependent — a client retry that happens to switch codec
/// would then look like a content *conflict* instead of the identical
/// replay it actually is. Hashing a fixed, length-prefixed concatenation
/// of the decoded fields instead (same length-prefixing discipline
/// `coordinator::do_name` already uses, for the same "two different
/// inputs must never collide" reason) is codec-independent by
/// construction.
///
/// `node_id` is hashed **only when it is non-empty**, as one extra
/// length-prefixed field appended *after* the samples. An absent value
/// and an explicit empty string therefore produce exactly the bytes the
/// pre-`node_id` function produced (pinned by
/// `content_hash_without_node_id_matches_the_pre_node_id_bytes`, whose
/// expected bytes were computed from the function at commit `8ac47a7`),
/// so a batch accepted before this field existed and redelivered after an
/// upgrade is still an identical replay, never a spurious 409 conflict.
/// Appending (rather than inserting mid-buffer) keeps the old prefix
/// byte-for-byte intact; the sample count is length-framed, so the
/// suffix cannot be confused with sample data.
#[allow(clippy::too_many_arguments)]
pub fn resource_sample_batch_content_hash(
    job_id: &str,
    shard_index: u32,
    attempt: u32,
    instance_type: &str,
    memory_peak_bytes: Option<u64>,
    oom_detected: bool,
    node_id: Option<&str>,
    samples: &[ResourceSampleInput],
) -> Vec<u8> {
    let mut buf = Vec::new();
    push_len_prefixed(&mut buf, job_id.as_bytes());
    buf.extend_from_slice(&shard_index.to_le_bytes());
    buf.extend_from_slice(&attempt.to_le_bytes());
    push_len_prefixed(&mut buf, instance_type.as_bytes());
    buf.extend_from_slice(&memory_peak_bytes.unwrap_or(u64::MAX).to_le_bytes());
    buf.push(u8::from(memory_peak_bytes.is_some()));
    buf.push(u8::from(oom_detected));
    buf.extend_from_slice(&(samples.len() as u64).to_le_bytes());
    for sample in samples {
        buf.extend_from_slice(&sample.timestamp_unix_ms.to_le_bytes());
        buf.extend_from_slice(&sample.elapsed_usec.to_le_bytes());
        buf.extend_from_slice(&sample.cpu_usage_usec_delta.to_le_bytes());
        buf.extend_from_slice(&sample.memory_current_bytes.to_le_bytes());
    }
    if let Some(node_id) = node_id.filter(|n| !n.is_empty()) {
        push_len_prefixed(&mut buf, node_id.as_bytes());
    }
    buf
}

fn push_len_prefixed(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// Splits one Worker invocation's shared `budget` (docs/design/
/// analytics.md's confirmed 250-data-point-per-invocation Analytics
/// Engine limit) between two overflow backlogs being drained in the
/// *same* invocation — `coordinator::mod`'s `alarm()`, which must never
/// let `test_event_overflow` and `sample_event_overflow` each spend a
/// full independent 250 in one fire (that would write up to 500 points
/// in one invocation, over the real platform cap). Drains `first_pending`
/// up to `budget`, then whatever budget remains (if any) from
/// `second_pending` — first-backlog priority is an arbitrary but
/// deterministic tie-break (older DO code already drains
/// `test_event_overflow` first), not a fairness guarantee; a backlog
/// that is never first keeps draining across subsequent alarm fires
/// either way, since nothing here drops anything.
pub fn allocate_shared_overflow_budget(
    budget: usize,
    first_pending: usize,
    second_pending: usize,
) -> (usize, usize) {
    let first = first_pending.min(budget);
    let second = second_pending.min(budget - first);
    (first, second)
}

/// Whether `candidate_ms` (an epoch-ms deadline a caller wants this DO's one alarm slot armed
/// for) should overwrite `current_ms` (the alarm presently armed, if any) — `coordinator::mod`'s
/// `schedule_overflow_flush_alarm`'s only decision, factored out pure so the "never push an
/// earlier alarm later" guarantee is independently testable: a pending `shard_terminal_effect`
/// retry or overflow flush must win against a later run timeout deadline, and a run timeout
/// deadline that is already earlier than a proposed retry must never be pushed later by it.
/// `current_ms = None` (no alarm armed yet) always advances. Equal deadlines never advance
/// (nothing to gain from re-arming an identical epoch).
pub fn should_advance_alarm(current_ms: Option<i64>, candidate_ms: i64) -> bool {
    current_ms.is_none_or(|c| candidate_ms < c)
}

/// Clamps a flat retry delay (`alarm()`'s own pending-effects branch) to the run's deadline: a
/// flat 5s retry must never outlive a deadline that is already closer than that, or a run whose
/// deadline falls inside this retry window would close up to 5s late. The very next alarm fire
/// still re-checks `now >= deadline` before anything else (`alarm()`'s own deadline-wins fix),
/// so this only bounds *how late* that fire can be, never reintroduces the starvation it fixed.
/// `deadline_ms - now_ms` is clamped to `0` defensively — in practice `alarm()` only reaches
/// this branch after its own `now_ms >= deadline_ms` check already returned early, so the
/// subtraction is always positive by construction, but this never assumes a caller upholds that.
pub fn clamp_retry_delay_to_deadline(flat_delay_ms: i64, deadline_ms: i64, now_ms: i64) -> i64 {
    flat_delay_ms.min((deadline_ms - now_ms).max(0))
}

/// Partitions a drained `test_event_overflow` batch's own `seq`s by
/// whether their paired write attempt succeeded, so the deferred-flush
/// caller (`coordinator::mod`'s `flush_test_event_overflow`) deletes
/// exactly the rows it actually wrote and leaves the rest queued for
/// the next alarm-triggered retry. `seqs` and `succeeded` must be the
/// same length and share index-for-index correspondence with the
/// batch `flush_test_event_overflow` read and attempted to write — a
/// precondition the caller upholds by building both from the same
/// `Vec` in the same order, not something this pure function can
/// check. A mixed-outcome batch need not leave a contiguous surviving
/// prefix (an older row can fail while a newer one in the same batch
/// succeeds), which is exactly why the caller must delete by exact
/// `seq` set rather than the simpler `seq <= max` range-delete a
/// strictly-successful batch would allow.
pub fn partition_seqs_by_write_result(seqs: &[i64], succeeded: &[bool]) -> (Vec<i64>, Vec<i64>) {
    seqs.iter().zip(succeeded.iter()).fold(
        (Vec::new(), Vec::new()),
        |(mut ok, mut failed), (seq, ok_flag)| {
            if *ok_flag {
                ok.push(*seq);
            } else {
                failed.push(*seq);
            }
            (ok, failed)
        },
    )
}

/// `duration_ewma_ms = alpha * new + (1 - alpha) * old` (analytics.md).
/// `existing` is `None` for a test's first-ever occurrence for a
/// `(repo_id, test_id)` — analytics.md's upsert pattern's `INSERT` branch
/// (no prior row to blend with) simply sets `duration_ewma_ms` to the
/// first duration, which this models directly rather than blending
/// against an arbitrary starting value like `0.0`.
pub fn ewma(existing: Option<f64>, new_duration_ms: f64) -> f64 {
    match existing {
        None => new_duration_ms,
        Some(prev) => EWMA_ALPHA * new_duration_ms + (1.0 - EWMA_ALPHA) * prev,
    }
}

/// Appends one outcome char to `recent_outcomes`, dropping the oldest
/// char(s) once the string exceeds [`RECENT_OUTCOMES_CAP`] — analytics.md:
/// "last 20 outcomes ... newest last", matching the doc's own
/// `substr(... || ..., -20)` SQL expression's behavior exactly (keep the
/// last 20 characters of the concatenation).
pub fn append_capped_outcome(existing: &str, ch: char) -> String {
    let mut combined: Vec<char> = existing.chars().collect();
    combined.push(ch);
    if combined.len() > RECENT_OUTCOMES_CAP {
        let drop = combined.len() - RECENT_OUTCOMES_CAP;
        combined.drain(0..drop);
    }
    combined.into_iter().collect()
}

/// `flipscount / (len(recent_outcomes) - 1)` (analytics.md: "flips /
/// (len(recent_outcomes) - 1)") — a "flip" is any adjacent pair of
/// differing outcome chars, counted over the *current* (already-capped,
/// already-appended) `recent_outcomes` string. Fewer than two outcomes has
/// no adjacent pair to flip between, so the score is `0.0` rather than a
/// division by zero.
pub fn flakiness_score(recent_outcomes: &str) -> f64 {
    let chars: Vec<char> = recent_outcomes.chars().collect();
    if chars.len() < 2 {
        return 0.0;
    }
    let flips = chars.windows(2).filter(|pair| pair[0] != pair[1]).count();
    flips as f64 / (chars.len() - 1) as f64
}

/// De-duplicates a run's concatenated test-outcome rows by `test_id`,
/// keeping the first occurrence — analytics.md's "Upsert pattern for
/// `test_stats`": "Two groups can describe the same test ... so before
/// binding the array it is deduplicated by `test_id`, keeping one entry
/// per distinct test ... an array with the same `test_id` twice would
/// silently count one run as two." Order-preserving over first
/// occurrences, same convention as [`new_check_names`] above.
pub fn dedupe_test_outcomes(rows: Vec<TestOutcomeRow>) -> Vec<TestOutcomeRow> {
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|row| seen.insert(row.test_id.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Shard groups / merge barrier (`RunCoordinator`'s own `shard_state`/
// `job_group` rows, docs/design/parallelization.md's "### Merge barrier
// (RunCoordinator)" and "### Failed-shard retry semantics"). Pure decision
// logic only, same split as every other section in this module — see
// `coordinator` module docs for the DO-side storage wiring.
//
// **Scope boundary for this round.** This section decides, for a shard
// group: (1) whether a terminal shard-ingest call is new or a duplicate
// redelivery (idempotency, matching `resolve_complete_node`'s exact
// discipline: same status replayed is a no-op, a *different* terminal
// status for the same key is a conflict); (2) whether `fail_fast` should
// fire immediately, and if so which shard indices need cancelling; (3)
// once every shard (by its latest attempt only) is terminal, whether to
// merge and which shards' reports to include. It does **not** build real
// merge execution (no JUnit/coverage parsing, no generated `<id>/merge`
// node, no Queue consumer — docs/design/parallelization.md's "Merge
// strategies per report type" table is entirely out of scope here).
// [`evaluate_barrier`]'s `FailFastTriggered` arm itself still only
// returns the *set* of shard indices a caller should cancel — it has no
// `node` table dependency, same as every other function in this file —
// but [`shard_node_id`]/[`shard_nodes_to_cancel`] (below) now give
// `coordinator::mod`'s `handle_shard_terminal` a real id to look each
// cancelled index up by and stop, via `cancel_shard_nodes`/`ensure_sibling_cancelled` — the
// same mark-`Cancelled`-then-stop-with-retry discipline `handle_cancel_run` already uses. See
// those two functions' own doc comments for the id scheme and the
// "register a shard" story (reusing the existing `startNode` RPC, not a
// new parallel one).
// ---------------------------------------------------------------------------

/// `job_group.merge_on_failure`'s three documented values exactly
/// (parallelization.md: "`if_any_passed` (default), `always`, or
/// `never`").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOnFailure {
    IfAnyPassed,
    Always,
    Never,
}

impl MergeOnFailure {
    pub fn as_db_str(self) -> &'static str {
        match self {
            MergeOnFailure::IfAnyPassed => "if_any_passed",
            MergeOnFailure::Always => "always",
            MergeOnFailure::Never => "never",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "if_any_passed" => Some(MergeOnFailure::IfAnyPassed),
            "always" => Some(MergeOnFailure::Always),
            "never" => Some(MergeOnFailure::Never),
            _ => None,
        }
    }
}

/// `job_group.job_name`'s server-side bound -- not independently
/// documented anywhere else; chosen generously above any real `ci.shard`
/// id while still being a stated, enforced cap rather than "whatever D1
/// happens to accept" (`job_group` is DO-local SQLite, no D1 column to
/// defer to).
pub const MAX_JOB_NAME_BYTES: usize = 128;

/// Why [`validate_register_shard_group`] rejected a `RegisterShardGroup`
/// call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterShardGroupValidationError {
    EmptyJobName,
    JobNameTooLong {
        len: usize,
    },
    JobNameControlCharacter,
    /// `0` means nothing will ever satisfy the merge barrier
    /// (`count(terminal) == expected_total`) -- a group that can never
    /// complete.
    ExpectedTotalZero,
    /// Over `cloud_ci_core::split::MAX_SHARDS` -- the same `1..=64`
    /// platform bound every other shard-count path in this service
    /// enforces (`ResolveShardPlan`/`cloud-ci split`), applied here too
    /// rather than letting a caller register a barrier no real split
    /// could ever produce.
    ExpectedTotalTooLarge {
        max: u32,
    },
    UnknownMergeOnFailure,
}

impl std::fmt::Display for RegisterShardGroupValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterShardGroupValidationError::EmptyJobName => {
                write!(f, "job_name must be a non-empty string")
            }
            RegisterShardGroupValidationError::JobNameTooLong { len } => write!(
                f,
                "job_name must be at most {MAX_JOB_NAME_BYTES} bytes, got {len}"
            ),
            RegisterShardGroupValidationError::JobNameControlCharacter => {
                write!(f, "job_name must not contain control characters")
            }
            RegisterShardGroupValidationError::ExpectedTotalZero => {
                write!(f, "expected_total must be 1 or greater")
            }
            RegisterShardGroupValidationError::ExpectedTotalTooLarge { max } => {
                write!(f, "expected_total must be at most {max}")
            }
            RegisterShardGroupValidationError::UnknownMergeOnFailure => {
                write!(f, "unknown merge_on_failure value")
            }
        }
    }
}

impl std::error::Error for RegisterShardGroupValidationError {}

/// Validates a `RegisterShardGroup` call's fields, independent of any
/// already-registered config for this `job_name` -- [`resolve_register_shard_group`]'s
/// Insert/AlreadyRegistered/Conflict decision only ever runs once these
/// pass, so a malformed call never reaches the "does it match the
/// existing row" comparison at all.
pub fn validate_register_shard_group(
    job_name: &str,
    expected_total: u32,
    merge_on_failure: &str,
) -> Result<(), RegisterShardGroupValidationError> {
    if job_name.is_empty() {
        return Err(RegisterShardGroupValidationError::EmptyJobName);
    }
    if job_name.len() > MAX_JOB_NAME_BYTES {
        return Err(RegisterShardGroupValidationError::JobNameTooLong {
            len: job_name.len(),
        });
    }
    if job_name.chars().any(|c| c.is_control()) {
        return Err(RegisterShardGroupValidationError::JobNameControlCharacter);
    }
    if expected_total == 0 {
        return Err(RegisterShardGroupValidationError::ExpectedTotalZero);
    }
    if expected_total > cloud_ci_core::split::MAX_SHARDS {
        return Err(RegisterShardGroupValidationError::ExpectedTotalTooLarge {
            max: cloud_ci_core::split::MAX_SHARDS,
        });
    }
    if MergeOnFailure::from_db_str(merge_on_failure).is_none() {
        return Err(RegisterShardGroupValidationError::UnknownMergeOnFailure);
    }
    Ok(())
}

/// A `job_group` row's merge-barrier configuration, for comparing an
/// incoming `RegisterShardGroup` call against whatever is already stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardGroupConfig {
    pub expected_total: u32,
    pub fail_fast: bool,
    pub merge_on_failure: String,
}

/// What `registerShardGroup` should do, given whether a `job_group` row
/// already exists for this `job_name` and, if so, its stored config.
/// `coordinator::mod::handle_register_shard_group`'s own doc comment: a
/// redelivered call carrying the *identical* config is a clean no-op
/// (existing config untouched); a redelivered call for the same
/// `job_name` carrying a *different* `expected_total`/`fail_fast`/
/// `merge_on_failure` is a conflict, same "same shape, different domain"
/// pattern [`resolve_submit_resource_samples`]/[`resolve_shard_terminal`]
/// already use for their own identity keys -- a stale or buggy caller
/// must never silently rewrite an already-registered merge barrier out
/// from under shards that may already be reporting against it.
/// Deliberately total over only `existing`, not a third "which call
/// arrived first" input: a reordered or duplicate redelivery (same
/// `job_name`, same field values, arriving in any order) always resolves
/// to [`AlreadyRegistered`](RegisterShardGroupDecision::AlreadyRegistered)
/// once a matching row exists, so two concurrent `RegisterShardGroup`
/// calls racing for the same never-yet-registered group can never both
/// decide [`Insert`](RegisterShardGroupDecision::Insert) against a row
/// that only one of them actually created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterShardGroupDecision {
    /// No `job_group` row exists yet for this `job_name` — the caller
    /// should insert one.
    Insert,
    /// A row already exists with an identical config — a no-op; nothing
    /// is written.
    AlreadyRegistered,
    /// A row already exists with a *different* config — rejected, never
    /// silently overwritten.
    Conflict,
}

pub fn resolve_register_shard_group(
    existing: Option<&ShardGroupConfig>,
    incoming: &ShardGroupConfig,
) -> RegisterShardGroupDecision {
    match existing {
        None => RegisterShardGroupDecision::Insert,
        Some(existing) if existing == incoming => RegisterShardGroupDecision::AlreadyRegistered,
        Some(_) => RegisterShardGroupDecision::Conflict,
    }
}

/// A shard's terminal outcome — `shard_state.status`'s two terminal
/// values (the broader `queued | running | passed | failed | retrying`
/// column, per the Data model section, is narrowed to just the two this
/// module's barrier math cares about: "terminal" means one of these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardTerminalStatus {
    Passed,
    Failed,
}

impl ShardTerminalStatus {
    pub fn as_db_str(self) -> &'static str {
        match self {
            ShardTerminalStatus::Passed => "passed",
            ShardTerminalStatus::Failed => "failed",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "passed" => Some(ShardTerminalStatus::Passed),
            "failed" => Some(ShardTerminalStatus::Failed),
            _ => None,
        }
    }
}

/// A terminal shard-ingest call for a `(job_name, idx, attempt)` key that
/// already has a *different* terminal status recorded — mirrors
/// `CompleteNodeError::ConflictingStatus`'s "same shape, different
/// domain" pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictingShardStatus;

/// Whether this terminal-ingest call is new or an already-applied
/// redelivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardTerminalDecision {
    /// Fresh terminal status for this exact `(job_name, idx, attempt)` —
    /// the caller should write the row and go on to evaluate the barrier.
    Recorded,
    /// The exact same status was already recorded for this key — a
    /// redelivered call. The caller must not write again and must not
    /// re-run [`evaluate_barrier`]: "a redelivered/duplicate
    /// terminal-ingest call ... must not double-count or re-trigger the
    /// barrier logic".
    AlreadyRecorded,
}

/// Resolves a terminal shard-ingest call against whatever status (if any)
/// is already recorded for the exact same `(job_name, idx, attempt)` key
/// — the same idempotency discipline [`resolve_complete_node`] already
/// established for node completions: a replayed call with the identical
/// terminal value is a clean no-op, a replayed call with a *different*
/// terminal value is a conflict, never a silent overwrite.
pub fn resolve_shard_terminal(
    existing_status: Option<ShardTerminalStatus>,
    incoming: ShardTerminalStatus,
) -> Result<ShardTerminalDecision, ConflictingShardStatus> {
    match existing_status {
        None => Ok(ShardTerminalDecision::Recorded),
        Some(existing) if existing == incoming => Ok(ShardTerminalDecision::AlreadyRecorded),
        Some(_) => Err(ConflictingShardStatus),
    }
}

/// One shard's terminal row, as input to [`latest_attempt_per_shard`]/
/// [`evaluate_barrier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardStateRow {
    pub idx: u32,
    pub attempt: u32,
    pub status: ShardTerminalStatus,
}

/// Collapses every terminal `(idx, attempt)` row down to only its
/// highest-`attempt` row per `idx` — "Failed-shard retry semantics": "An
/// OOM retry reuses the exact same `shard_plan` entry for that index ...
/// only the LATEST attempt's terminal status counts toward
/// `expected_total`, not both attempts." Order is not meaningful on the
/// input (every terminal row for every attempt of every shard so far);
/// the output is sorted by `idx` for deterministic, order-independent
/// comparison by callers (including these tests).
pub fn latest_attempt_per_shard(rows: &[ShardStateRow]) -> Vec<ShardStateRow> {
    let mut by_idx: std::collections::BTreeMap<u32, ShardStateRow> =
        std::collections::BTreeMap::new();
    for row in rows {
        match by_idx.get(&row.idx) {
            Some(existing) if existing.attempt >= row.attempt => {}
            _ => {
                by_idx.insert(row.idx, *row);
            }
        }
    }
    by_idx.into_values().collect()
}

/// A shard group's fixed configuration — `job_group`'s row, minus
/// `merge_job_id` (that column is reserved for the later merge-execution
/// round that actually dispatches the generated `<id>/merge` node; this
/// round never sets or reads it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobGroupConfig {
    pub expected_total: u32,
    pub fail_fast: bool,
    pub merge_on_failure: MergeOnFailure,
}

/// [`evaluate_barrier`]'s full output: this round's scope boundary is
/// exactly this decision — "the full output of this round's scope" per
/// the brief — never real cancellation/merge execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BarrierOutcome {
    /// Fewer than `expected_total` shards (by latest attempt) are
    /// terminal yet, and no fail-fast trigger fired on this call — the
    /// group keeps waiting for the rest ("`fail_fast: false` (default)
    /// lets every shard run to its own terminal state").
    Waiting,
    /// `fail_fast = true` and the shard just recorded by this call went
    /// terminal-`failed` before every shard in the group reached its own
    /// terminal state: the whole group is immediately `failed`.
    /// `cancel_idxs` are every shard index (0-based, per
    /// `shard_plan.idx`) in `0..expected_total` that has not yet reached
    /// a terminal status — "flag every other `running`/`queued` shard in
    /// that group for cancellation". Sorted ascending.
    FailFastTriggered { cancel_idxs: Vec<u32> },
    /// Every shard (by latest attempt) reached a terminal status: the
    /// barrier is satisfied. `merge` and `included_idxs` are the
    /// `merge_on_failure`-dependent decision below. `included_idxs` is
    /// sorted ascending and empty whenever `merge` is `false` (nothing is
    /// merged, so there is nothing to include).
    Satisfied {
        merge: bool,
        included_idxs: Vec<u32>,
    },
}

/// Every idx in `0..expected_total` with no row in `latest_terminal` — the immutable-row-derived
/// fail-fast cancellation set shared by [`evaluate_barrier`]'s live `FailFastTriggered` branch
/// and `coordinator::mod`'s `legacy_decision_for` reconstructed `"failed"` branch, so a shard
/// that already reached its own terminal state (by latest attempt) is never included in either's
/// cancel set, regardless of which caller is asking. `latest_terminal` must already be
/// [`latest_attempt_per_shard`]'s output.
pub fn not_yet_terminal_idxs(expected_total: u32, latest_terminal: &[ShardStateRow]) -> Vec<u32> {
    let present: std::collections::BTreeSet<u32> =
        latest_terminal.iter().map(|row| row.idx).collect();
    (0..expected_total)
        .filter(|idx| !present.contains(idx))
        .collect()
}

/// Evaluates the merge barrier for one shard group, per
/// parallelization.md's "### Merge barrier (RunCoordinator)" bullets,
/// after a terminal shard-ingest call has resolved
/// [`ShardTerminalDecision::Recorded`] (a duplicate/`AlreadyRecorded` call
/// must never reach this function — that is exactly how "must not ...
/// re-trigger the barrier logic" is enforced).
///
/// `latest_terminal` must already be [`latest_attempt_per_shard`]'s
/// output (every shard's *latest* attempt only). `just_recorded` is the
/// status this exact call just recorded (used only to decide whether a
/// fail-fast trigger fires *now*, not merely because some earlier call
/// already left a failed shard in `latest_terminal` — fail-fast fires "as
/// soon as one shard goes terminal-failed", i.e. on the call that causes
/// it, not retroactively on every later call for the same group).
pub fn evaluate_barrier(
    config: JobGroupConfig,
    latest_terminal: &[ShardStateRow],
    just_recorded: ShardTerminalStatus,
) -> BarrierOutcome {
    if config.fail_fast && just_recorded == ShardTerminalStatus::Failed {
        let cancel_idxs = not_yet_terminal_idxs(config.expected_total, latest_terminal);
        return BarrierOutcome::FailFastTriggered { cancel_idxs };
    }

    if (latest_terminal.len() as u32) < config.expected_total {
        return BarrierOutcome::Waiting;
    }

    let any_passed = latest_terminal
        .iter()
        .any(|row| row.status == ShardTerminalStatus::Passed);
    let merge = match config.merge_on_failure {
        MergeOnFailure::Always => true,
        MergeOnFailure::Never => false,
        MergeOnFailure::IfAnyPassed => any_passed,
    };
    let mut included_idxs: Vec<u32> = if !merge {
        Vec::new()
    } else {
        match config.merge_on_failure {
            // "runs the merge using only the successful shards' reports"
            // — `if_any_passed` never includes a failed shard's report.
            MergeOnFailure::IfAnyPassed => latest_terminal
                .iter()
                .filter(|row| row.status == ShardTerminalStatus::Passed)
                .map(|row| row.idx)
                .collect(),
            MergeOnFailure::Always | MergeOnFailure::Never => {
                latest_terminal.iter().map(|row| row.idx).collect()
            }
        }
    };
    included_idxs.sort_unstable();
    BarrierOutcome::Satisfied {
        merge,
        included_idxs,
    }
}

/// Deterministic `node` row id for one shard's running attempt —
/// `(job_name, idx, attempt)`'s "node identity" for the real-container
/// cancellation gap this round closes (see this section's module doc
/// comment). A shard is registered as a running node by calling the
/// *existing* `startNode` RPC with this id as `node_id`, rather than
/// through a new, parallel "register a shard" RPC: the `node`/`NodeRow`
/// table already has everything a running shard needs — spec-hash
/// idempotency, a `status` a cancellation can flip, and (the whole
/// reason this function exists) a real container
/// `node_container.rs`'s `start_container`/`stop_container` already
/// know how to start/stop given just this id.
///
/// Plain colon-joining `job_name`/`idx`/`attempt` — not `do_name`'s
/// length-prefixed hashing — is safe here: unlike `do_name`, whose
/// hashed tuple must stay injective across every repo/sha/run_key/attempt
/// combination in a single, Worker-wide Durable Object namespace, this
/// id only needs to be unique among *this one DO instance's* own `node`
/// rows (one `RunCoordinator` instance is one run). `job_group` already
/// uses `job_name` itself, unhashed, as a SQL primary key elsewhere in
/// this same DO's storage, so reusing it raw inside a `node_id` is no
/// weaker a guarantee than that existing key already relies on.
pub fn shard_node_id(job_name: &str, idx: u32, attempt: u32) -> String {
    format!("shard:{job_name}:{idx}:{attempt}")
}

fn shard_node_prefix(job_name: &str, idx: u32) -> String {
    format!("shard:{job_name}:{idx}:")
}

/// Job names can contain colons; the suffix must be one canonical attempt number.
fn shard_node_matches(node_id: &str, prefix: &str) -> bool {
    match node_id.strip_prefix(prefix) {
        Some(rest) => {
            let bytes = rest.as_bytes();
            !(bytes.is_empty() || bytes[0] == b'0' && bytes.len() > 1)
                && bytes.iter().all(u8::is_ascii_digit)
                && rest.parse::<u32>().is_ok()
        }
        None => false,
    }
}

/// Which of `nodes`' ids are shard `idx` of `job_name`'s own running
/// node(s) — [`evaluate_barrier`]'s `FailFastTriggered { cancel_idxs }`
/// decision names only shard *indices*, never node ids or attempts (it
/// has no `node` table dependency at all, by design — see this
/// section's module doc comment), so `handle_shard_terminal`'s real
/// cancellation wiring maps each cancelled index back to a node id via
/// [`shard_node_id`]'s own id scheme. Only non-terminal nodes are
/// returned — the same "already concluded, never relabel" skip
/// [`nodes_to_retry_cancel`] applies to whole-run cancellation — so a shard
/// that already finished (successfully or not) on its own before the
/// fail-fast decision landed is never touched, and a `cancel_idxs` entry
/// for a shard that was never dispatched as a real node at all simply
/// matches nothing and is silently skipped. Order-preserving over
/// `nodes`.
pub fn shard_nodes_to_cancel(
    job_name: &str,
    idx: u32,
    nodes: &[(String, NodeState)],
) -> Vec<String> {
    let prefix = shard_node_prefix(job_name, idx);
    nodes
        .iter()
        .filter(|(id, status)| shard_node_matches(id, &prefix) && !status.is_terminal())
        .map(|(id, _)| id.clone())
        .collect()
}

/// Maps a shard's own reported terminal outcome to the terminal
/// [`NodeState`] its registered node (if any) should be completed
/// with — [`shard_self_completion_target`]'s only caller. Deliberately
/// distinct from [`shard_nodes_to_cancel`]'s blanket `Cancelled`: a
/// shard reporting its own terminal status here was never cancelled by
/// anyone, it genuinely concluded with this outcome, so its node must
/// record the real conclusion, not a cancellation. `ShardTerminalStatus`
/// has only these two values (`from_db_str`), so this mapping is total.
pub fn shard_terminal_node_state(status: ShardTerminalStatus) -> NodeState {
    match status {
        ShardTerminalStatus::Passed => NodeState::Succeeded,
        ShardTerminalStatus::Failed => NodeState::Failed,
    }
}

/// Whether a shard's own terminal ingest call should bring its own
/// registered node ([`shard_node_id`]) to a real terminal state —
/// `handle_shard_terminal`'s self-completion gap. `current` is that
/// node's current state, if a node was ever registered. Returns
/// `Some(target)` only when `current` is `Some(non-terminal)`. Returns
/// `None`, a no-op write (the caller still always attempts the real
/// stop separately), when no node was ever registered or it already
/// reached a terminal state of its own.
pub fn shard_self_completion_target(
    current: Option<NodeState>,
    outcome: ShardTerminalStatus,
) -> Option<NodeState> {
    match current {
        Some(state) if !state.is_terminal() => Some(shard_terminal_node_state(outcome)),
        _ => None,
    }
}

/// What a stop attempt against a node's own `physical_address` should resolve to, once the
/// real `stop_container` call's own success/failure is known — `coordinator::mod`'s
/// `stop_node_container` callers' decision, factored out pure. A `None` address (a node row
/// from before `node_physical_address`'s addressing scheme existed) can never be resolved by
/// retrying — there is no way to know this node's real container address, so no future attempt
/// differs from this one — unlike a `Some` address's genuine stop failure (a container-runtime
/// fault, a dropped DO-to-DO fetch), which may well succeed on a later retry and must not be
/// silently treated as done. Mirrors `legacy_decision_for`'s own `"legacy_unknowable"` case:
/// both mean "nothing further this code can do, stop retrying forever" rather than leaving a
/// `shard_terminal_effect` permanently pending and starving the run's own timeout close
/// (`coordinator::mod::alarm`'s deadline-still-wins fix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The real stop succeeded, or there was never a real address to attempt it against —
    /// nothing further for a caller to retry.
    Resolved,
    /// A real stop was attempted against a known address and failed — worth retrying later.
    Pending,
}

pub fn resolve_stop_outcome(physical_address: Option<&str>, stop_succeeded: bool) -> StopOutcome {
    if physical_address.is_none() || stop_succeeded {
        StopOutcome::Resolved
    } else {
        StopOutcome::Pending
    }
}

// ---------------------------------------------------------------------------
// OOM recovery (Phase 4: `runner: "auto"` real-time OOM retry) -- PURE, NOT WIRED.
// ---------------------------------------------------------------------------
//
// Builds on `cloud_ci_core::rightsizing::oom_retry`'s per-event math (next size up, or
// terminal failure at the configured max) with the one piece that module deliberately
// leaves to its caller: whether *this* OOM event should be acted on at all, given whatever
// decision (if any) a prior OOM event for the same node already produced. analytics.md's
// "OOM retry" bullet grants exactly one retry per node ("rather than retrying indefinitely")
// -- not "retry until the configured max" -- so a node that OOMs again on its retried
// attempt fails even if the retried size was not itself the configured max.
//
// Status (2026-10-03): the pure decision logic below (`decide_oom_recovery`,
// `resolve_oom_node_id`, `oom_event_admissible`, `resolve_auto_start`, and the
// `OomEffect`/`OomEffectFlags` state machine) is kept, reviewed, and tested, but the
// Durable-Object wiring that would call it (`coordinator::mod`'s `maybe_decide_oom_recovery`/
// `drain_oom_lineage`/`execute_oom_effect`, the `oom_lineage_decision` table, the
// `node_container.rs`/`executor.rs` size-override plumbing) has been removed. It is not
// reachable from any real caller. See docs/design/parallelization.md's "`runner: \"auto\"`
// OOM recovery: pure logic only, not wired" section for the open prerequisites and why the
// wiring was pulled back out: a review found two concrete bugs in it -- a wrong node could be
// resolved during a shard-terminal race, and the agent env var the wiring relied on to carry a
// shard's attempt number does not actually carry one.

use cloud_ci_core::rightsizing::{self, InstanceSize, OomRetryOutcome};

/// One already-persisted OOM-recovery outcome for a node -- `sizing_decision`'s row shape,
/// once that table exists. `RetryAt`'s `to` and `FailedAtMax`'s `max` are instance-size
/// *names*, not `InstanceSize`s: the only thing a caller needs to dispatch or report, and
/// trivially `Clone`/`PartialEq` without dragging a borrowed ladder slice's lifetime along.
#[derive(Debug, Clone, PartialEq)]
pub enum OomRecoveryOutcome {
    /// Retry once more, at `to` -- analytics.md: "retries that node once on the next size up
    /// ... bypassing hysteresis and the p95 computation". `reason` is
    /// [`rightsizing::oom_retry`]'s own `"<from> -> <to>: oom-retry"` string, unchanged.
    RetryAt { to: String, reason: String },
    /// Terminal: either this OOM happened at the bounded ladder's own top, or a prior OOM for
    /// this same node already spent the one retry analytics.md grants. `max` always names the
    /// *configured* max (the bounded ladder's own top), never the size this particular OOM
    /// happened at -- matching analytics.md's "a message naming the configured max", a
    /// second-attempt OOM below the configured max still reports that configured max, not its
    /// own (lower) size, so the message plainly says "the one retry already happened and it
    /// wasn't enough" rather than naming an arbitrary intermediate rung.
    FailedAtMax {
        max: String,
        measured_peak_bytes: Option<u64>,
    },
}

/// A decision already recorded for one shard lineage (`(job_name, idx)`, never a specific
/// node id -- a retried node's real id does not equal `shard_node_id(job_name, idx,
/// retried_attempt)` for its own attempt number, so keying this by node id would make a
/// second OOM on the retried node unfindable). `base_node_id` is the
/// *original* (never-retried) node's id -- [`resolve_oom_node_id`]'s own input, letting a
/// caller reconstruct the retry's real id (`RetryAt`) or know which node to clean up
/// (`FailedAtMax`, where no retry was ever created) without storing a second id. At most one
/// row per lineage, ever: analytics.md's single-retry rule means a shard has at most one
/// OOM-recovery decision in its whole lifetime, regardless of how many attempts it goes
/// through.
#[derive(Debug, Clone, PartialEq)]
pub struct OomDecisionRecord {
    pub attempt: u32,
    pub base_node_id: String,
    pub outcome: OomRecoveryOutcome,
}

/// One `SubmitResourceSamples` delivery's OOM-relevant fields -- [`decide_oom_recovery`]'s
/// per-event input.
#[derive(Debug, Clone, PartialEq)]
pub struct OomObservation {
    pub attempt: u32,
    pub current_size: String,
    pub measured_peak_bytes: Option<u64>,
}

/// [`decide_oom_recovery`]'s full output.
#[derive(Debug, Clone, PartialEq)]
pub enum OomRecoveryDecision {
    /// `incoming.attempt` already has a recorded decision -- a duplicate delivery of an
    /// already-accepted batch must still resolve to the exact same OOM-recovery outcome, not
    /// recompute or re-dispatch. The stored outcome, unchanged.
    AlreadyDecided(OomRecoveryOutcome),
    /// A decision already exists for a *later* attempt than `incoming.attempt` -- a
    /// reordered/late-arriving delivery for an attempt this node has since moved past. No
    /// effect: acting on stale per-attempt evidence here would race the newer decision a
    /// caller may already have dispatched against.
    Stale,
    /// `incoming.current_size` is not present in the bounded ladder passed in -- a
    /// configuration error upstream (the node's own reported instance type is not between its
    /// configured min/max), not a shape [`decide_oom_recovery`] can resolve on its own
    /// ([`rightsizing::oom_retry`]'s own `None` case, passed through).
    NotInLadder,
    /// A new decision for `incoming.attempt`, not previously recorded.
    New(OomRecoveryOutcome),
}

/// Resolves one OOM event against whatever [`OomDecisionRecord`] (if any) this node already
/// has, and -- only when none exists yet -- [`rightsizing::oom_retry`]'s own next-size-up/
/// already-at-max math. `ladder` MUST already be the node's `[min, max]`-bounded slice
/// ([`rightsizing::ladder_range`]'s output), exactly like `oom_retry` itself requires.
///
/// The one-retry-total rule ("first retry only"): once `existing` is `Some(_)` at all, this
/// node has already spent its one retry regardless of what size that retry OOM'd at, so a
/// further OOM for it is unconditionally [`OomRecoveryOutcome::FailedAtMax`] naming the
/// *configured* max -- never a second [`rightsizing::oom_retry`] call, which would otherwise
/// keep climbing the ladder one rung per OOM ("retrying indefinitely", which analytics.md
/// explicitly rules out).
///
/// The "further OOM" match is `incoming.attempt == existing.attempt + 1` exactly, not
/// "any attempt greater than `existing.attempt`" -- there is no `node_id` field on
/// `SubmitResourceSamplesRequest` (`cloud-ci-proto`) to key this on directly, so
/// `existing.attempt + 1` (the one, exact attempt number
/// [`oom_retry_node_id`]'s own dispatched retry was given) is the closest identity check this
/// layer can make: an attempt that jumped by more than one (a different, unrelated retry
/// mechanism bumping the counter, or a reordered delivery skipping ahead) is `Stale`, never
/// silently mistaken for the dispatched retry's own second OOM. This does **not** fully solve
/// identity confusion: a retry that reports the *same* attempt as the original (its real
/// dispatch never told it to use a new one -- there is no wiring that does this; an earlier
/// attempt at it assumed `CLOUD_CI_ATTEMPT` carries a shard attempt, which is wrong, that env
/// var is the *run* attempt used by `cloud-ci upload`/`split`, and the agent does not read it
/// for this purpose at all) is indistinguishable from a duplicate delivery of the original's
/// own decision -- `Equal` below -- and safely resolves to `AlreadyDecided` (the original
/// `RetryAt`), not a crash or a wrong dispatch, but also not a detected second OOM. Closing
/// that gap for real needs a node id on the wire plus a real agent-side attempt-carrying
/// mechanism; see docs/design/parallelization.md's "not wired" section for the open
/// prerequisites.
pub fn decide_oom_recovery(
    ladder: &[InstanceSize],
    existing: Option<&OomDecisionRecord>,
    incoming: &OomObservation,
) -> OomRecoveryDecision {
    if let Some(existing) = existing {
        if matches!(existing.outcome, OomRecoveryOutcome::FailedAtMax { .. }) {
            // Terminal: no further decision for this lineage, regardless of which attempt
            // reports in -- analytics.md's "rather than retrying indefinitely".
            return OomRecoveryDecision::AlreadyDecided(existing.outcome.clone());
        }
        return match incoming.attempt.cmp(&existing.attempt) {
            std::cmp::Ordering::Less => OomRecoveryDecision::Stale,
            std::cmp::Ordering::Equal => {
                OomRecoveryDecision::AlreadyDecided(existing.outcome.clone())
            }
            std::cmp::Ordering::Greater if incoming.attempt == existing.attempt + 1 => {
                match ladder.last() {
                    None => OomRecoveryDecision::NotInLadder,
                    Some(max) => OomRecoveryDecision::New(OomRecoveryOutcome::FailedAtMax {
                        max: max.name.clone(),
                        measured_peak_bytes: incoming.measured_peak_bytes,
                    }),
                }
            }
            std::cmp::Ordering::Greater => OomRecoveryDecision::Stale,
        };
    }

    match rightsizing::oom_retry(
        ladder,
        &incoming.current_size,
        incoming.measured_peak_bytes.unwrap_or(0),
    ) {
        None => OomRecoveryDecision::NotInLadder,
        Some(OomRetryOutcome::RetryAt { to, reason }) => {
            OomRecoveryDecision::New(OomRecoveryOutcome::RetryAt {
                to: to.name,
                reason,
            })
        }
        Some(OomRetryOutcome::AlreadyAtMax { max, .. }) => {
            OomRecoveryDecision::New(OomRecoveryOutcome::FailedAtMax {
                max: max.name,
                // Carries the real `Option<u64>` through rather than `oom_retry`'s own `u64`
                // (which a `None` peak would otherwise have silently become `0` in, via the
                // `unwrap_or(0)` fed into it above) -- a genuinely unknown peak must render as
                // "unknown" in a failure message, never a fabricated zero.
                measured_peak_bytes: incoming.measured_peak_bytes,
            })
        }
    }
}

/// A distinct, run-scoped `node_id` for an OOM-retried node's new attempt -- analytics.md's
/// "the new attempt gets a distinct, run-scoped container address". Run-scoping itself comes
/// from `node_physical_address(run_do_name, node_id)` hashing the *run*'s own DO name
/// alongside whatever this returns, exactly like every other node id; this function only needs
/// to guarantee the `node_id` half changes per attempt, which the trailing `:attempt` suffix
/// does unconditionally, regardless of `base_node_id`'s own shape.
///
/// Shard nodes do not need this: [`shard_node_id`] already embeds `attempt` in its own id
/// scheme, and shard-terminal handling already resolves OOM-retried shard attempts through it
/// directly. This helper is for a plain (non-shard) `runner: "auto"` node, whose `node_id` has
/// no attempt of its own yet.
pub fn oom_retry_node_id(base_node_id: &str, attempt: u32) -> String {
    format!("{base_node_id}:oom-retry:{attempt}")
}

/// Resolves which real `node_id` a `SubmitResourceSamples` delivery's `(job_name, idx,
/// attempt)` refers to, given whatever [`OomDecisionRecord`] already exists for this shard
/// lineage. No decision yet: the delivery is for the original, never-retried node
/// ([`shard_node_id`]'s own scheme). A `RetryAt` decision already exists: the *real* current
/// node is the retry [`oom_retry_node_id`] actually dispatched, reconstructed from the
/// decision's own `base_node_id`/`attempt` -- not recomputed from the delivery's own
/// `attempt` via `shard_node_id`, which does not produce a retried node's real id at all. A
/// `FailedAtMax` decision already exists: no retry was ever created, so the node needing
/// cleanup is still the base node.
pub fn resolve_oom_node_id(
    job_name: &str,
    idx: u32,
    attempt: u32,
    existing: Option<&OomDecisionRecord>,
) -> String {
    match existing {
        Some(record) => match &record.outcome {
            OomRecoveryOutcome::RetryAt { .. } => {
                oom_retry_node_id(&record.base_node_id, record.attempt + 1)
            }
            OomRecoveryOutcome::FailedAtMax { .. } => record.base_node_id.clone(),
        },
        None => shard_node_id(job_name, idx, attempt),
    }
}

/// Whether an OOM event for a node should even be considered: a late `oom_detected` batch
/// for a node that already reached a terminal state on its own (naturally succeeded, or was
/// cancelled) must never be overwritten to `failed`, and a terminal run must never have a
/// new container started for it. `false` is a silent, stale no-op for the caller -- never an
/// error, matching every other late-delivery guard in this file.
pub fn oom_event_admissible(run_terminal: bool, node_state: NodeState) -> bool {
    !run_terminal && !node_state.is_terminal()
}

/// One auto-sized node's resolved starting instance size, plus the `[min, max]` bounds a
/// future `runner: "auto"` start path would freeze on the node's row.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoStart {
    pub initial: String,
    pub min: String,
    pub max: String,
}

/// Why [`resolve_auto_start`] could not resolve a starting size. The configured
/// `Settings.runners.auto` bounds must be checked against the *executor's* real ladder before
/// ever reaching a per-call container size override (whose own accepted-names list does not
/// include every name `Settings.runners.auto` accepts, e.g. the documented default
/// `"basic"`), so a node whose bounds fall outside that ladder fails closed here rather than
/// either crashing the real container start or silently never participating in OOM
/// recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStartError {
    /// `min`/`max` (explicit, or the ladder's own first/last rung when unset) do not both
    /// resolve against `ladder` via [`rightsizing::ladder_range`] -- an inverted range, or a
    /// name absent from this executor's real sizes (`"basic"` on the default Cloudflare
    /// Containers executor, whose `CapabilityDescriptor` does not list it).
    BoundsNotInLadder { min: String, max: String },
    /// `initial` (explicit, or `min` when unset) is not within the resolved `[min, max]`
    /// bound.
    InitialOutOfBounds {
        initial: String,
        min: String,
        max: String,
    },
}

/// Resolves a `runner: "auto"` node's starting instance size against the *executor's* real
/// ladder (e.g. `CapabilityDescriptor::sizes`, supplied by a future caller).
/// `Settings.runners.auto.{min,max,initial}` are deployment-author-supplied strings that may
/// name a size the active executor does not actually support for a per-call
/// `durable_object` start, so a caller must resolve and validate against `ladder` before ever
/// reaching a container start, never pass a `Settings.runners.auto` string through
/// directly. `min`/`max` unset defaults to the ladder's own first/last rung (the deployment's
/// full range); `initial` unset defaults to the resolved `min`.
pub fn resolve_auto_start(
    ladder: &[InstanceSize],
    min: Option<&str>,
    max: Option<&str>,
    initial: Option<&str>,
) -> Result<AutoStart, AutoStartError> {
    let min_name = min
        .map(str::to_string)
        .or_else(|| ladder.first().map(|s| s.name.clone()));
    let max_name = max
        .map(str::to_string)
        .or_else(|| ladder.last().map(|s| s.name.clone()));
    let (Some(min_name), Some(max_name)) = (min_name, max_name) else {
        return Err(AutoStartError::BoundsNotInLadder {
            min: min.unwrap_or_default().to_string(),
            max: max.unwrap_or_default().to_string(),
        });
    };
    let Some(bounded) = rightsizing::ladder_range(ladder, &min_name, &max_name) else {
        return Err(AutoStartError::BoundsNotInLadder {
            min: min_name,
            max: max_name,
        });
    };
    let initial_name = initial
        .map(str::to_string)
        .unwrap_or_else(|| min_name.clone());
    if !bounded.iter().any(|s| s.name == initial_name) {
        return Err(AutoStartError::InitialOutOfBounds {
            initial: initial_name,
            min: min_name,
            max: max_name,
        });
    }
    Ok(AutoStart {
        initial: initial_name,
        min: min_name,
        max: max_name,
    })
}

/// Which of an OOM-recovery decision's durable side effects have already completed. A future
/// wiring would persist these flags per decision and re-drive the remaining effects to
/// completion on every call (a replayed delivery as well as a background sweep, not only the
/// call that first created the decision), so a crash or a failed D1 write between any two
/// of them heals later instead of silently leaving the system half-applied. Each effect is
/// tracked independently for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OomEffectFlags {
    /// The OOM'd node's own row has been marked terminal (`failed`).
    pub old_marked: bool,
    /// The OOM'd node's real container has been stopped (or confirmed never addressable).
    /// A retry must never be dispatched before this, so the old and new attempts never run
    /// concurrently for the same shard.
    pub old_stopped: bool,
    /// The OOM'd node's now-terminal row has been projected to D1.
    pub old_projected: bool,
    /// The decision itself has been projected to a future `sizing_decisions` D1 table --
    /// no such table exists today (the wiring that would create and write it was removed).
    pub decision_projected: bool,
    /// Only meaningful when the decision is `RetryAt`: the retry's `node` row exists.
    pub retry_inserted: bool,
    /// Only meaningful when the decision is `RetryAt`: the retry's real container start has
    /// been attempted (succeeded, or failed and the row was marked `failed`). Tracked
    /// separately from `retry_inserted` so a crash between inserting the row and starting the
    /// container is detectable and retried, rather than a bare row-exists check wrongly
    /// treating the retry as already dispatched.
    pub retry_started: bool,
    /// Only meaningful when the decision is `RetryAt`: the retry's row has been projected to
    /// D1.
    pub retry_projected: bool,
}

/// One durable side effect of an OOM-recovery decision, in the fixed order
/// [`next_oom_effect`] always applies them: every container-state-critical effect (mark,
/// stop, insert, start) comes before any D1 projection, so a D1 outage or an unapplied
/// migration can never block the retry itself; the retry (if any) still only runs after the
/// old node's container is confirmed stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OomEffect {
    MarkOldTerminal,
    StopOld,
    InsertRetry,
    StartRetry,
    ProjectOld,
    ProjectDecision,
    ProjectRetry,
}

impl OomEffect {
    /// Whether this effect is a best-effort D1 projection: a failure here must never stop the
    /// critical effects, only be retried later on its own.
    pub fn is_projection(self) -> bool {
        matches!(
            self,
            Self::ProjectOld | Self::ProjectDecision | Self::ProjectRetry
        )
    }
}

/// The next not-yet-completed effect for one OOM-recovery decision, or `None` once every
/// applicable effect (including the retry's, when `is_retry`) is done. A caller drains this by
/// executing the returned effect, recording its own success back into `flags`, and calling
/// this again -- "plan one step, apply it, re-plan".
pub fn next_oom_effect(is_retry: bool, flags: OomEffectFlags) -> Option<OomEffect> {
    next_oom_effect_excluding(is_retry, flags, &[])
}

/// [`next_oom_effect`], skipping every effect in `skipped` (projections that already failed
/// during the current drain) so one failing projection does not hide the remaining pending
/// ones.
pub fn next_oom_effect_excluding(
    is_retry: bool,
    flags: OomEffectFlags,
    skipped: &[OomEffect],
) -> Option<OomEffect> {
    let candidates = [
        (!flags.old_marked, OomEffect::MarkOldTerminal, false),
        (!flags.old_stopped, OomEffect::StopOld, false),
        (!flags.retry_inserted, OomEffect::InsertRetry, true),
        (!flags.retry_started, OomEffect::StartRetry, true),
        (!flags.old_projected, OomEffect::ProjectOld, false),
        (!flags.decision_projected, OomEffect::ProjectDecision, false),
        (!flags.retry_projected, OomEffect::ProjectRetry, true),
    ];
    for (pending, effect, retry_only) in candidates {
        if !pending || (retry_only && !is_retry) || skipped.contains(&effect) {
            continue;
        }
        // A critical effect that is still pending blocks everything after it: only
        // projections (which never gate anything) may be reached past a skipped sibling.
        return Some(effect);
    }
    None
}

/// Whether any effect still needs draining: a future caller's check for whether a decision
/// still needs background work, true until [`next_oom_effect`] returns `None`.
pub fn oom_effects_pending(is_retry: bool, flags: OomEffectFlags) -> bool {
    next_oom_effect(is_retry, flags).is_some()
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
    fn test_outcome_kind_maps_to_analytics_engine_double2() {
        assert_eq!(TestOutcomeKind::Passed.as_test_double(), 1.0);
        assert_eq!(TestOutcomeKind::Failed.as_test_double(), 0.0);
        assert_eq!(TestOutcomeKind::Skipped.as_test_double(), -1.0);
    }

    #[test]
    fn test_outcome_kind_round_trips_through_char() {
        for kind in [
            TestOutcomeKind::Passed,
            TestOutcomeKind::Failed,
            TestOutcomeKind::Skipped,
        ] {
            assert_eq!(TestOutcomeKind::from_char(kind.as_char()), Some(kind));
        }
        assert_eq!(TestOutcomeKind::from_char('?'), None);
    }

    #[test]
    fn build_test_events_maps_every_row_in_order_with_shared_ids() {
        let rows = vec![
            TestOutcomeRow {
                test_id: "abc123".to_string(),
                file_path: "src/foo.rs".to_string(),
                test_name: "it_works".to_string(),
                duration_ms: 42,
                outcome: TestOutcomeKind::Passed,
            },
            TestOutcomeRow {
                test_id: "def456".to_string(),
                file_path: "src/bar.rs".to_string(),
                test_name: "it_fails".to_string(),
                duration_ms: 17,
                outcome: TestOutcomeKind::Failed,
            },
        ];

        let events = build_test_events("run-1", "job-1", 99, &rows);

        assert_eq!(
            events,
            vec![
                TestEventPoint {
                    run_id: "run-1".to_string(),
                    job_id: "job-1".to_string(),
                    test_id: "abc123".to_string(),
                    duration_ms: 42,
                    outcome: TestOutcomeKind::Passed,
                    repo_id: 99,
                },
                TestEventPoint {
                    run_id: "run-1".to_string(),
                    job_id: "job-1".to_string(),
                    test_id: "def456".to_string(),
                    duration_ms: 17,
                    outcome: TestOutcomeKind::Failed,
                    repo_id: 99,
                },
            ]
        );
    }

    #[test]
    fn build_test_events_on_empty_rows_is_empty() {
        assert!(build_test_events("run-1", "job-1", 99, &[]).is_empty());
    }

    #[test]
    fn build_sample_events_repeats_job_level_peak_shard_attempt_instance_type_per_row() {
        let samples = vec![
            ResourceSampleInput {
                timestamp_unix_ms: 1_000,
                elapsed_usec: 2_000_000,
                cpu_usage_usec_delta: 500_000,
                memory_current_bytes: 100_000_000,
            },
            ResourceSampleInput {
                timestamp_unix_ms: 3_000,
                elapsed_usec: 2_000_000,
                cpu_usage_usec_delta: 600_000,
                memory_current_bytes: 120_000_000,
            },
        ];

        let events = build_sample_events(
            "run-1",
            "job-1",
            2,
            3,
            99,
            "standard-2",
            Some(150_000_000),
            &samples,
        );

        assert_eq!(
            events,
            vec![
                SampleEventPoint {
                    run_id: "run-1".to_string(),
                    job_id: "job-1".to_string(),
                    shard_index: 2,
                    attempt: 3,
                    instance_type: "standard-2".to_string(),
                    timestamp_unix_ms: 1_000.0,
                    elapsed_usec: 2_000_000.0,
                    cpu_usage_usec_delta: 500_000.0,
                    memory_current_bytes: 100_000_000.0,
                    memory_peak_bytes: 150_000_000.0,
                    memory_peak_known: true,
                    repo_id: 99,
                },
                SampleEventPoint {
                    run_id: "run-1".to_string(),
                    job_id: "job-1".to_string(),
                    shard_index: 2,
                    attempt: 3,
                    instance_type: "standard-2".to_string(),
                    timestamp_unix_ms: 3_000.0,
                    elapsed_usec: 2_000_000.0,
                    cpu_usage_usec_delta: 600_000.0,
                    memory_current_bytes: 120_000_000.0,
                    memory_peak_bytes: 150_000_000.0,
                    memory_peak_known: true,
                    repo_id: 99,
                },
            ]
        );
    }

    #[test]
    fn build_sample_events_unset_peak_becomes_zero_with_known_flag_false() {
        let samples = vec![ResourceSampleInput {
            timestamp_unix_ms: 1_000,
            elapsed_usec: 2_000_000,
            cpu_usage_usec_delta: 10,
            memory_current_bytes: 20,
        }];

        let events = build_sample_events("run-1", "job-1", 0, 1, 1, "basic", None, &samples);

        assert_eq!(events[0].memory_peak_bytes, 0.0);
        assert!(!events[0].memory_peak_known);
    }

    #[test]
    fn build_sample_events_on_empty_samples_is_empty() {
        assert!(build_sample_events("run-1", "job-1", 0, 1, 1, "basic", Some(1), &[]).is_empty());
    }

    #[test]
    fn resolve_submit_resource_samples_accepts_a_fresh_identity() {
        assert_eq!(
            resolve_submit_resource_samples(None, "hash-a"),
            SubmitResourceSamplesDecision::Accepted
        );
    }

    #[test]
    fn resolve_submit_resource_samples_identical_replay_is_a_noop() {
        assert_eq!(
            resolve_submit_resource_samples(Some("hash-a"), "hash-a"),
            SubmitResourceSamplesDecision::AlreadyAccepted
        );
    }

    #[test]
    fn resolve_submit_resource_samples_different_content_is_a_conflict() {
        assert_eq!(
            resolve_submit_resource_samples(Some("hash-a"), "hash-b"),
            SubmitResourceSamplesDecision::Conflict
        );
    }

    #[test]
    fn content_hash_is_stable_for_identical_typed_fields() {
        let samples = vec![ResourceSampleInput {
            timestamp_unix_ms: 1_000,
            elapsed_usec: 2_000_000,
            cpu_usage_usec_delta: 500,
            memory_current_bytes: 1024,
        }];
        let a = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "standard-2",
            Some(2048),
            false,
            None,
            &samples,
        );
        let b = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "standard-2",
            Some(2048),
            false,
            None,
            &samples,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_differs_for_different_sample_content() {
        let samples_a = vec![ResourceSampleInput {
            timestamp_unix_ms: 1_000,
            elapsed_usec: 2_000_000,
            cpu_usage_usec_delta: 500,
            memory_current_bytes: 1024,
        }];
        let samples_b = vec![ResourceSampleInput {
            timestamp_unix_ms: 1_000,
            elapsed_usec: 2_000_000,
            cpu_usage_usec_delta: 999,
            memory_current_bytes: 1024,
        }];
        let a = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "standard-2",
            Some(2048),
            false,
            None,
            &samples_a,
        );
        let b = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "standard-2",
            Some(2048),
            false,
            None,
            &samples_b,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn content_hash_distinguishes_unset_peak_from_a_genuine_zero_peak() {
        let samples: Vec<ResourceSampleInput> = vec![];
        let unset =
            resource_sample_batch_content_hash("job-1", 0, 1, "basic", None, false, None, &samples);
        let zero = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "basic",
            Some(0),
            false,
            None,
            &samples,
        );
        assert_ne!(unset, zero);
    }

    #[test]
    fn content_hash_treats_absent_node_id_identically_to_empty_string() {
        // `SubmitResourceSamplesRequest.node_id`'s own doc comment
        // (`cloud-ci-proto`): "an unset (or empty) value is treated
        // identically" -- an old agent that never sends this field must
        // hash exactly like a new agent that explicitly sends `""`, so
        // neither ever looks like a conflicting batch against the other.
        let samples: Vec<ResourceSampleInput> = vec![];
        let absent =
            resource_sample_batch_content_hash("job-1", 0, 1, "basic", None, false, None, &samples);
        let empty = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "basic",
            None,
            false,
            Some(""),
            &samples,
        );
        assert_eq!(absent, empty);
    }

    #[test]
    fn content_hash_without_node_id_matches_the_pre_node_id_bytes() {
        // Pins `resource_sample_batch_content_hash`'s output, for a
        // `node_id`-omitting call, to the exact bytes the pre-`node_id`
        // version of this function produced at commit `8ac47a7` (the
        // commit immediately before `node_id` was added) -- computed by
        // extracting that commit's `resource_sample_batch_content_hash`/
        // `push_len_prefixed` verbatim into a standalone throwaway
        // program and printing the hex of its two outputs. A pre-upgrade
        // batch redelivered after this deploy must still hash identically
        // here, or a legitimate retry looks like a 409 conflict.
        let samples_a = vec![ResourceSampleInput {
            timestamp_unix_ms: 1_000,
            elapsed_usec: 2_000_000,
            cpu_usage_usec_delta: 500,
            memory_current_bytes: 1024,
        }];
        let a = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "standard-2",
            Some(2048),
            false,
            None,
            &samples_a,
        );
        #[rustfmt::skip]
        let expected_a: Vec<u8> = vec![
            5, 0, 0, 0, 0, 0, 0, 0, 106, 111, 98, 45, 49, 0, 0, 0, 0, 1, 0, 0, 0, 10, 0, 0, 0, 0,
            0, 0, 0, 115, 116, 97, 110, 100, 97, 114, 100, 45, 50, 0, 8, 0, 0, 0, 0, 0, 0, 1, 0,
            1, 0, 0, 0, 0, 0, 0, 0, 232, 3, 0, 0, 0, 0, 0, 0, 128, 132, 30, 0, 0, 0, 0, 0, 244,
            1, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(a, expected_a);

        let b = resource_sample_batch_content_hash("job-2", 3, 2, "basic", None, true, None, &[]);
        #[rustfmt::skip]
        let expected_b: Vec<u8> = vec![
            5, 0, 0, 0, 0, 0, 0, 0, 106, 111, 98, 45, 50, 3, 0, 0, 0, 2, 0, 0, 0, 5, 0, 0, 0, 0, 0,
            0, 0, 98, 97, 115, 105, 99, 255, 255, 255, 255, 255, 255, 255, 255, 0, 1, 0, 0, 0, 0,
            0, 0, 0, 0,
        ];
        assert_eq!(b, expected_b);

        // Empty node_id must match the same pinned bytes too.
        let a_empty_node = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "standard-2",
            Some(2048),
            false,
            Some(""),
            &samples_a,
        );
        assert_eq!(a_empty_node, expected_a);
    }

    #[test]
    fn content_hash_with_node_id_differs_from_the_pinned_no_node_id_bytes() {
        let a_with_node = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "standard-2",
            Some(2048),
            false,
            Some("shard:job-1:0:1"),
            &[ResourceSampleInput {
                timestamp_unix_ms: 1_000,
                elapsed_usec: 2_000_000,
                cpu_usage_usec_delta: 500,
                memory_current_bytes: 1024,
            }],
        );
        #[rustfmt::skip]
        let pinned_no_node: Vec<u8> = vec![
            5, 0, 0, 0, 0, 0, 0, 0, 106, 111, 98, 45, 49, 0, 0, 0, 0, 1, 0, 0, 0, 10, 0, 0, 0, 0,
            0, 0, 0, 115, 116, 97, 110, 100, 97, 114, 100, 45, 50, 0, 8, 0, 0, 0, 0, 0, 0, 1, 0,
            1, 0, 0, 0, 0, 0, 0, 0, 232, 3, 0, 0, 0, 0, 0, 0, 128, 132, 30, 0, 0, 0, 0, 0, 244,
            1, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0,
        ];
        assert_ne!(a_with_node, pinned_no_node);
    }

    #[test]
    fn validate_node_id_accepts_empty() {
        assert_eq!(validate_node_id(""), Ok(()));
    }

    #[test]
    fn validate_node_id_accepts_a_normal_value() {
        assert_eq!(validate_node_id("shard:job-1:0:1"), Ok(()));
    }

    #[test]
    fn validate_node_id_accepts_exactly_the_max_length() {
        let node_id = "a".repeat(MAX_NODE_ID_BYTES);
        assert_eq!(validate_node_id(&node_id), Ok(()));
    }

    #[test]
    fn validate_node_id_rejects_one_byte_over_the_max() {
        let node_id = "a".repeat(MAX_NODE_ID_BYTES + 1);
        assert_eq!(
            validate_node_id(&node_id),
            Err(NodeIdError::TooLong {
                len: MAX_NODE_ID_BYTES + 1
            })
        );
    }

    #[test]
    fn validate_node_id_measures_length_in_bytes_not_chars() {
        // Each "é" is 2 UTF-8 bytes -- 129 of them is 258 bytes, over the
        // 256-byte bound, even though `.chars().count()` would say 129.
        let node_id = "é".repeat(129);
        assert_eq!(node_id.len(), 258);
        assert_eq!(
            validate_node_id(&node_id),
            Err(NodeIdError::TooLong { len: 258 })
        );
    }

    #[test]
    fn validate_node_id_rejects_a_nul_byte() {
        assert_eq!(
            validate_node_id("shard\0job"),
            Err(NodeIdError::ControlCharacter)
        );
    }

    #[test]
    fn validate_node_id_rejects_a_newline() {
        assert_eq!(
            validate_node_id("shard\njob"),
            Err(NodeIdError::ControlCharacter)
        );
    }

    #[test]
    fn validate_node_id_rejects_a_del_character() {
        assert_eq!(
            validate_node_id("shard\u{7f}job"),
            Err(NodeIdError::ControlCharacter)
        );
    }

    #[test]
    fn content_hash_differs_for_a_real_node_id() {
        let samples: Vec<ResourceSampleInput> = vec![];
        let no_node =
            resource_sample_batch_content_hash("job-1", 0, 1, "basic", None, false, None, &samples);
        let with_node = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "basic",
            None,
            false,
            Some("shard:job-1:0:1"),
            &samples,
        );
        assert_ne!(no_node, with_node);
    }

    #[test]
    fn content_hash_differs_between_two_distinct_node_ids() {
        let samples: Vec<ResourceSampleInput> = vec![];
        let node_a = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "basic",
            None,
            false,
            Some("node-a"),
            &samples,
        );
        let node_b = resource_sample_batch_content_hash(
            "job-1",
            0,
            1,
            "basic",
            None,
            false,
            Some("node-b"),
            &samples,
        );
        assert_ne!(node_a, node_b);
    }

    #[test]
    fn allocate_shared_overflow_budget_splits_without_exceeding_total() {
        assert_eq!(allocate_shared_overflow_budget(250, 300, 300), (250, 0));
        assert_eq!(allocate_shared_overflow_budget(250, 100, 300), (100, 150));
        assert_eq!(allocate_shared_overflow_budget(250, 0, 300), (0, 250));
        assert_eq!(allocate_shared_overflow_budget(250, 10, 5), (10, 5));
    }

    #[test]
    fn should_advance_alarm_arms_when_nothing_is_currently_scheduled() {
        assert!(should_advance_alarm(None, 1_000));
    }

    #[test]
    fn should_advance_alarm_prefers_an_earlier_pending_retry_over_a_later_run_deadline() {
        // The run's own far-future timeout deadline is already armed; a shard-terminal-effect
        // retry 5s out must win and overwrite it.
        let run_deadline_ms = 10_000_000;
        let retry_candidate_ms = 5_000;
        assert!(should_advance_alarm(
            Some(run_deadline_ms),
            retry_candidate_ms
        ));
    }

    #[test]
    fn should_advance_alarm_never_pushes_an_earlier_alarm_later() {
        // A pending retry (or the run's own closer deadline) is already armed sooner than this
        // candidate -- must not be pushed out to the later candidate.
        let earlier_pending_retry_ms = 5_000;
        let later_run_deadline_ms = 10_000_000;
        assert!(!should_advance_alarm(
            Some(earlier_pending_retry_ms),
            later_run_deadline_ms
        ));
    }

    #[test]
    fn should_advance_alarm_does_not_advance_for_an_identical_deadline() {
        assert!(!should_advance_alarm(Some(5_000), 5_000));
    }

    #[test]
    fn clamp_retry_delay_to_deadline_uses_the_flat_delay_when_the_deadline_is_far_away() {
        assert_eq!(clamp_retry_delay_to_deadline(5_000, 1_000_000, 0), 5_000);
    }

    #[test]
    fn clamp_retry_delay_to_deadline_shortens_to_a_sooner_deadline() {
        assert_eq!(clamp_retry_delay_to_deadline(5_000, 2_000, 0), 2_000);
    }

    #[test]
    fn clamp_retry_delay_to_deadline_never_goes_negative() {
        assert_eq!(clamp_retry_delay_to_deadline(5_000, 0, 1_000), 0);
    }

    #[test]
    fn split_for_invocation_budget_partitions_without_loss_or_duplication() {
        // A regression test for the real defect this splits off from
        // `write_test_events`: a report with more test outcomes than one
        // Worker invocation's `writeDataPoint` budget used to silently
        // truncate everything past the cap. This proves the split is a
        // true partition — every input index appears exactly once,
        // across the two halves, in original order — so the only thing
        // left for `coordinator::mod` to get right is persisting (not
        // dropping) the second half.
        let events: Vec<i32> = (0..300).collect();
        let (immediate, overflow) = split_for_invocation_budget(&events, 250);
        assert_eq!(immediate.len(), 250);
        assert_eq!(overflow.len(), 50);
        let recombined: Vec<i32> = immediate.iter().chain(overflow.iter()).copied().collect();
        assert_eq!(recombined, events);
    }

    #[test]
    fn split_for_invocation_budget_under_the_cap_has_no_overflow() {
        let events: Vec<i32> = (0..10).collect();
        let (immediate, overflow) = split_for_invocation_budget(&events, 250);
        assert_eq!(immediate, events.as_slice());
        assert!(overflow.is_empty());
    }

    #[test]
    fn split_for_invocation_budget_at_exactly_the_cap_has_no_overflow() {
        let events: Vec<i32> = (0..250).collect();
        let (immediate, overflow) = split_for_invocation_budget(&events, 250);
        assert_eq!(immediate, events.as_slice());
        assert!(overflow.is_empty());
    }

    #[test]
    fn partition_seqs_by_write_result_splits_mixed_outcomes_non_contiguously() {
        // The exact scenario `delete_test_event_overflow_through`'s old
        // `seq <= max_seq` range-delete got wrong: a failure in the
        // middle of a batch must not be swept away just because a later
        // `seq` in the same batch succeeded.
        let seqs = vec![10, 11, 12, 13];
        let succeeded = vec![true, false, true, false];
        let (ok, failed) = partition_seqs_by_write_result(&seqs, &succeeded);
        assert_eq!(ok, vec![10, 12]);
        assert_eq!(failed, vec![11, 13]);
    }

    #[test]
    fn partition_seqs_by_write_result_all_succeeded_keeps_order() {
        let seqs = vec![1, 2, 3];
        let succeeded = vec![true, true, true];
        let (ok, failed) = partition_seqs_by_write_result(&seqs, &succeeded);
        assert_eq!(ok, vec![1, 2, 3]);
        assert!(failed.is_empty());
    }

    #[test]
    fn partition_seqs_by_write_result_all_failed_deletes_nothing() {
        let seqs = vec![1, 2, 3];
        let succeeded = vec![false, false, false];
        let (ok, failed) = partition_seqs_by_write_result(&seqs, &succeeded);
        assert!(ok.is_empty());
        assert_eq!(failed, vec![1, 2, 3]);
    }

    #[test]
    fn partition_seqs_by_write_result_on_empty_batch_is_empty() {
        let (ok, failed) = partition_seqs_by_write_result(&[], &[]);
        assert!(ok.is_empty());
        assert!(failed.is_empty());
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
    fn resolve_admission_settings_sha_uses_the_resolved_sha_for_a_brand_new_row() {
        // `existing_sha: None` is the only case a delivery's own freshly resolved sha is
        // ever used — a row that has never been admitted yet.
        assert_eq!(
            resolve_admission_settings_sha(None, "resolveddeadbeef0000000000000000000000"),
            "resolveddeadbeef0000000000000000000000"
        );
    }

    #[test]
    fn resolve_admission_settings_sha_keeps_the_race_winners_stored_sha() {
        // Two `BeginRun` deliveries raced: this delivery's own resolve finished second,
        // but a race winner already stored its own sha first. The winner's value is kept,
        // this delivery's own `resolved_sha` is discarded entirely.
        assert_eq!(
            resolve_admission_settings_sha(
                Some("winnersha00000000000000000000000000000"),
                "thisdeliveryslosingsha00000000000000000",
            ),
            "winnersha00000000000000000000000000000"
        );
    }

    #[test]
    fn resolve_admission_settings_sha_never_overwrites_on_a_plain_resume() {
        // A later, ordinary resumed/redelivered `BeginRun` for an already-admitted run
        // resolves a *different* sha than what was first ever stored (the repo's default
        // branch moved on). The already-stored value still wins — "first sha wins" holds
        // even when the two values genuinely differ, not just when they happen to match.
        assert_eq!(
            resolve_admission_settings_sha(
                Some("firststoredsha0000000000000000000000000"),
                "newerliveheadsha000000000000000000000000",
            ),
            "firststoredsha0000000000000000000000000"
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

    #[test]
    fn expect_jobs_satisfied_is_false_when_not_set() {
        assert!(!expect_jobs_satisfied(None, &[("unit".to_string(), true)]));
        assert!(!expect_jobs_satisfied(
            Some(&[]),
            &[("unit".to_string(), true)]
        ));
    }

    #[test]
    fn expect_jobs_satisfied_is_false_until_every_named_job_has_started() {
        let expected = vec!["unit".to_string(), "e2e".to_string()];
        let started = [("unit".to_string(), true)];
        assert!(!expect_jobs_satisfied(Some(&expected), &started));
    }

    #[test]
    fn expect_jobs_satisfied_is_false_while_a_named_job_is_still_running() {
        let expected = vec!["unit".to_string(), "e2e".to_string()];
        let started = [("unit".to_string(), true), ("e2e".to_string(), false)];
        assert!(!expect_jobs_satisfied(Some(&expected), &started));
    }

    #[test]
    fn expect_jobs_satisfied_is_true_once_every_named_job_concluded() {
        let expected = vec!["unit".to_string(), "e2e".to_string()];
        let started = [("unit".to_string(), true), ("e2e".to_string(), true)];
        assert!(expect_jobs_satisfied(Some(&expected), &started));
    }

    #[test]
    fn expect_jobs_satisfied_treats_expect_jobs_as_a_floor_not_a_cap() {
        // A caller started a third, undeclared job ("lint") beyond the
        // two it named in `expect_jobs`. It is still running, but that
        // does not block the declared set from closing the run (module
        // docs: "treated as a floor ... not a hard cap").
        let expected = vec!["unit".to_string(), "e2e".to_string()];
        let started = [
            ("unit".to_string(), true),
            ("e2e".to_string(), true),
            ("lint".to_string(), false),
        ];
        assert!(expect_jobs_satisfied(Some(&expected), &started));
    }

    #[test]
    fn resolve_timeout_seconds_defaults_to_30_minutes_when_omitted() {
        assert_eq!(resolve_timeout_seconds(0), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(resolve_timeout_seconds(-1), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(DEFAULT_TIMEOUT_SECONDS, 1800);
    }

    #[test]
    fn resolve_timeout_seconds_passes_through_a_requested_value_under_the_max() {
        assert_eq!(resolve_timeout_seconds(60), 60);
        assert_eq!(resolve_timeout_seconds(3), 3);
    }

    #[test]
    fn resolve_timeout_seconds_clamps_to_the_deployment_wide_maximum() {
        assert_eq!(
            resolve_timeout_seconds(MAX_TIMEOUT_SECONDS + 1),
            MAX_TIMEOUT_SECONDS
        );
        assert_eq!(resolve_timeout_seconds(i64::MAX), MAX_TIMEOUT_SECONDS);
    }

    #[test]
    fn run_state_for_close_computes_succeeded_or_failed_when_not_by_timeout() {
        assert_eq!(
            run_state_for_close(false, Conclusion::CONCLUSION_SUCCESS),
            run_state_from_conclusion(Conclusion::CONCLUSION_SUCCESS)
        );
        assert_eq!(
            run_state_for_close(false, Conclusion::CONCLUSION_FAILURE),
            run_state_from_conclusion(Conclusion::CONCLUSION_FAILURE)
        );
    }

    #[test]
    fn run_state_for_close_is_unconditionally_abandoned_by_timeout() {
        // The timeout trigger forces `abandoned` regardless of what the
        // jobs' conclusions would otherwise compute to (byo-ci.md's
        // Failure modes row for a never-uploaded shard) — unlike the
        // webhook/`--expect-jobs` triggers, which compute succeeded/failed.
        assert_eq!(
            run_state_for_close(true, Conclusion::CONCLUSION_SUCCESS),
            RunState::Abandoned
        );
        assert_eq!(
            run_state_for_close(true, Conclusion::CONCLUSION_FAILURE),
            RunState::Abandoned
        );
    }

    #[test]
    fn start_node_creates_fresh_row_when_none_exists() {
        assert_eq!(
            resolve_start_node(None, "hash-1"),
            Ok(StartNodeDecision::Started)
        );
    }

    #[test]
    fn start_node_is_idempotent_for_a_retry_with_the_same_spec_hash() {
        let existing = ExistingNode {
            spec_hash: "hash-1".into(),
            status: NodeState::Running,
        };
        assert_eq!(
            resolve_start_node(Some(&existing), "hash-1"),
            Ok(StartNodeDecision::AlreadyStarted {
                status: NodeState::Running
            })
        );
    }

    #[test]
    fn start_node_rejects_a_different_spec_hash_as_nondeterministic() {
        let existing = ExistingNode {
            spec_hash: "hash-1".into(),
            status: NodeState::Running,
        };
        assert_eq!(
            resolve_start_node(Some(&existing), "hash-2"),
            Err(NondeterministicReplay)
        );
    }

    #[test]
    fn complete_node_records_a_fresh_terminal_status() {
        assert_eq!(
            resolve_complete_node(NodeState::Running, NodeState::Succeeded),
            Ok(CompleteNodeDecision::Recorded)
        );
    }

    #[test]
    fn complete_node_is_a_no_op_replay_of_the_same_status() {
        assert_eq!(
            resolve_complete_node(NodeState::Succeeded, NodeState::Succeeded),
            Ok(CompleteNodeDecision::Recorded)
        );
    }

    #[test]
    fn complete_node_rejects_a_conflicting_terminal_status() {
        assert_eq!(
            resolve_complete_node(NodeState::Succeeded, NodeState::Failed),
            Err(CompleteNodeError::ConflictingStatus)
        );
    }

    #[test]
    fn complete_node_drops_late_completions_for_cancelled_nodes() {
        // "late completion events for cancelled nodes are dropped" —
        // and never relabels the node back to a different terminal
        // status, regardless of what the late event claims.
        assert_eq!(
            resolve_complete_node(NodeState::Cancelled, NodeState::Succeeded),
            Ok(CompleteNodeDecision::DroppedCancelled)
        );
        assert_eq!(
            resolve_complete_node(NodeState::Cancelled, NodeState::Failed),
            Ok(CompleteNodeDecision::DroppedCancelled)
        );
    }

    #[test]
    fn ack_node_is_idempotent() {
        // Not yet acked: the caller must write `acked = true`.
        assert!(resolve_ack_node(false));
        // Already acked: a redelivered ack issues no second write.
        assert!(!resolve_ack_node(true));
    }

    #[test]
    fn nodes_to_retry_cancel_includes_non_terminal_nodes_regardless_of_address() {
        let nodes = vec![
            ("pending-node".to_string(), NodeState::Pending, false),
            ("running-node".to_string(), NodeState::Running, true),
        ];
        assert_eq!(
            nodes_to_retry_cancel(&nodes),
            vec!["pending-node".to_string(), "running-node".to_string()]
        );
    }

    #[test]
    fn nodes_to_retry_cancel_retries_an_already_cancelled_node_with_a_real_address() {
        // The exact redelivery regression: this node's stop already failed once, which still
        // marked it `Cancelled` before the container was actually destroyed -- it must be
        // retried, not silently dropped just because it is already terminal.
        let nodes = vec![("stuck-node".to_string(), NodeState::Cancelled, true)];
        assert_eq!(
            nodes_to_retry_cancel(&nodes),
            vec!["stuck-node".to_string()]
        );
    }

    #[test]
    fn nodes_to_retry_cancel_skips_an_already_cancelled_node_with_no_address() {
        // Unresolvable regardless of how many times it is retried -- `resolve_stop_outcome`
        // already treats this as resolved, so repeating it here would only generate noise.
        let nodes = vec![("legacy-node".to_string(), NodeState::Cancelled, false)];
        assert_eq!(nodes_to_retry_cancel(&nodes), Vec::<String>::new());
    }

    #[test]
    fn nodes_to_retry_cancel_skips_a_genuinely_different_terminal_outcome() {
        // Succeeded/Failed/Skipped/TimedOut are real outcomes, never a stop-retry candidate
        // regardless of `physical_address` -- only `Cancelled`-but-unconfirmed is retried.
        let nodes = vec![("done-node".to_string(), NodeState::Succeeded, true)];
        assert_eq!(nodes_to_retry_cancel(&nodes), Vec::<String>::new());
    }

    #[test]
    fn node_status_for_exit_code_maps_zero_to_succeeded() {
        assert_eq!(node_status_for_exit_code(0), NodeState::Succeeded);
    }

    #[test]
    fn node_status_for_exit_code_maps_any_nonzero_to_failed() {
        for code in [1, 2, 127, 255] {
            assert_eq!(node_status_for_exit_code(code), NodeState::Failed);
        }
    }

    #[test]
    fn node_state_round_trips_through_db_string() {
        for state in [
            NodeState::Pending,
            NodeState::Running,
            NodeState::Succeeded,
            NodeState::Failed,
            NodeState::Skipped,
            NodeState::Cancelled,
            NodeState::TimedOut,
        ] {
            assert_eq!(NodeState::from_db_str(state.as_db_str()), Some(state));
        }
    }

    #[test]
    fn test_id_is_stable_and_16_hex_chars() {
        let a = test_id("src/foo.rs", "mod::test_one");
        let b = test_id("src/foo.rs", "mod::test_one");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_id_distinguishes_field_boundary() {
        // Without an unambiguous separator, "ab"+"c" and "a"+"bc" would
        // collide; the 0x1f join must keep them distinct.
        let a = test_id("ab", "c");
        let b = test_id("a", "bc");
        assert_ne!(a, b);
    }

    #[test]
    fn test_id_differs_for_different_inputs() {
        let a = test_id("src/foo.rs", "test_one");
        let b = test_id("src/foo.rs", "test_two");
        let c = test_id("src/bar.rs", "test_one");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn full_test_name_joins_classname_and_name() {
        assert_eq!(full_test_name(Some("pkg.Mod"), "test_x"), "pkg.Mod::test_x");
    }

    #[test]
    fn full_test_name_falls_back_to_bare_name() {
        assert_eq!(full_test_name(None, "test_x"), "test_x");
        assert_eq!(full_test_name(Some(""), "test_x"), "test_x");
    }

    #[test]
    fn ewma_first_occurrence_sets_raw_duration() {
        assert_eq!(ewma(None, 123.0), 123.0);
    }

    #[test]
    fn ewma_blends_with_alpha_0_2() {
        // 0.2*200 + 0.8*100 = 120
        assert_eq!(ewma(Some(100.0), 200.0), 120.0);
    }

    #[test]
    fn append_capped_outcome_grows_under_cap() {
        assert_eq!(append_capped_outcome("PP", 'F'), "PPF");
    }

    #[test]
    fn append_capped_outcome_drops_oldest_at_cap() {
        let existing = "P".repeat(20);
        let result = append_capped_outcome(&existing, 'F');
        assert_eq!(result.chars().count(), 20);
        assert_eq!(result, format!("{}F", "P".repeat(19)));
    }

    #[test]
    fn flakiness_score_zero_for_fewer_than_two_outcomes() {
        assert_eq!(flakiness_score(""), 0.0);
        assert_eq!(flakiness_score("P"), 0.0);
    }

    #[test]
    fn flakiness_score_zero_when_stable() {
        assert_eq!(flakiness_score("PPPP"), 0.0);
    }

    #[test]
    fn flakiness_score_counts_adjacent_flips() {
        // P F P F: 3 flips over 3 adjacent pairs = 1.0
        assert_eq!(flakiness_score("PFPF"), 1.0);
        // P P F F: 1 flip over 3 adjacent pairs
        assert!((flakiness_score("PPFF") - (1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn dedupe_test_outcomes_keeps_first_occurrence() {
        let rows = vec![
            TestOutcomeRow {
                test_id: "a".into(),
                file_path: "f".into(),
                test_name: "t".into(),
                duration_ms: 10,
                outcome: TestOutcomeKind::Passed,
            },
            TestOutcomeRow {
                test_id: "a".into(),
                file_path: "f".into(),
                test_name: "t".into(),
                duration_ms: 9999,
                outcome: TestOutcomeKind::Failed,
            },
            TestOutcomeRow {
                test_id: "b".into(),
                file_path: "f2".into(),
                test_name: "t2".into(),
                duration_ms: 5,
                outcome: TestOutcomeKind::Skipped,
            },
        ];
        let deduped = dedupe_test_outcomes(rows);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].test_id, "a");
        assert_eq!(deduped[0].duration_ms, 10);
        assert_eq!(deduped[1].test_id, "b");
    }

    #[test]
    fn merge_on_failure_from_db_str_accepts_all_three_documented_values() {
        assert_eq!(
            MergeOnFailure::from_db_str("if_any_passed"),
            Some(MergeOnFailure::IfAnyPassed)
        );
        assert_eq!(
            MergeOnFailure::from_db_str("always"),
            Some(MergeOnFailure::Always)
        );
        assert_eq!(
            MergeOnFailure::from_db_str("never"),
            Some(MergeOnFailure::Never)
        );
    }

    #[test]
    fn merge_on_failure_from_db_str_rejects_a_malformed_value() {
        // `handle_register_shard_group`'s own validation: an unknown
        // `merge_on_failure` string (typo, wrong case, empty) is a 400,
        // never silently coerced to the default.
        assert_eq!(MergeOnFailure::from_db_str("IfAnyPassed"), None);
        assert_eq!(MergeOnFailure::from_db_str("sometimes"), None);
        assert_eq!(MergeOnFailure::from_db_str(""), None);
    }

    #[test]
    fn validate_register_shard_group_accepts_a_well_formed_request() {
        assert_eq!(
            validate_register_shard_group("e2e", 4, "if_any_passed"),
            Ok(())
        );
    }

    #[test]
    fn validate_register_shard_group_rejects_an_empty_job_name() {
        assert_eq!(
            validate_register_shard_group("", 1, "if_any_passed"),
            Err(RegisterShardGroupValidationError::EmptyJobName)
        );
    }

    #[test]
    fn validate_register_shard_group_accepts_a_job_name_at_exactly_the_max_length() {
        let job_name = "a".repeat(MAX_JOB_NAME_BYTES);
        assert_eq!(
            validate_register_shard_group(&job_name, 1, "if_any_passed"),
            Ok(())
        );
    }

    #[test]
    fn validate_register_shard_group_rejects_a_job_name_one_byte_over_the_max() {
        let job_name = "a".repeat(MAX_JOB_NAME_BYTES + 1);
        assert_eq!(
            validate_register_shard_group(&job_name, 1, "if_any_passed"),
            Err(RegisterShardGroupValidationError::JobNameTooLong {
                len: MAX_JOB_NAME_BYTES + 1
            })
        );
    }

    #[test]
    fn validate_register_shard_group_rejects_a_job_name_with_a_control_character() {
        assert_eq!(
            validate_register_shard_group("e2e\n", 1, "if_any_passed"),
            Err(RegisterShardGroupValidationError::JobNameControlCharacter)
        );
    }

    #[test]
    fn validate_register_shard_group_rejects_zero_expected_total() {
        assert_eq!(
            validate_register_shard_group("e2e", 0, "if_any_passed"),
            Err(RegisterShardGroupValidationError::ExpectedTotalZero)
        );
    }

    #[test]
    fn validate_register_shard_group_accepts_expected_total_at_the_platform_max() {
        assert_eq!(
            validate_register_shard_group("e2e", cloud_ci_core::split::MAX_SHARDS, "if_any_passed"),
            Ok(())
        );
    }

    #[test]
    fn validate_register_shard_group_rejects_expected_total_one_over_the_platform_max() {
        assert_eq!(
            validate_register_shard_group(
                "e2e",
                cloud_ci_core::split::MAX_SHARDS + 1,
                "if_any_passed"
            ),
            Err(RegisterShardGroupValidationError::ExpectedTotalTooLarge {
                max: cloud_ci_core::split::MAX_SHARDS
            })
        );
    }

    #[test]
    fn validate_register_shard_group_rejects_an_unknown_merge_on_failure() {
        assert_eq!(
            validate_register_shard_group("e2e", 1, "sometimes"),
            Err(RegisterShardGroupValidationError::UnknownMergeOnFailure)
        );
    }

    fn test_config(
        expected_total: u32,
        fail_fast: bool,
        merge_on_failure: &str,
    ) -> ShardGroupConfig {
        ShardGroupConfig {
            expected_total,
            fail_fast,
            merge_on_failure: merge_on_failure.to_string(),
        }
    }

    #[test]
    fn resolve_register_shard_group_inserts_when_no_row_exists() {
        let incoming = test_config(4, false, "if_any_passed");
        assert_eq!(
            resolve_register_shard_group(None, &incoming),
            RegisterShardGroupDecision::Insert
        );
    }

    #[test]
    fn resolve_register_shard_group_is_a_noop_for_an_identical_redelivery() {
        let existing = test_config(4, false, "if_any_passed");
        let incoming = test_config(4, false, "if_any_passed");
        assert_eq!(
            resolve_register_shard_group(Some(&existing), &incoming),
            RegisterShardGroupDecision::AlreadyRegistered
        );
    }

    #[test]
    fn resolve_register_shard_group_reordered_identical_delivery_still_resolves_to_the_same_decision()
     {
        // A "reordered" delivery here means: by the time either call is
        // actually evaluated against storage, a row may or may not exist
        // yet, regardless of which `RegisterShardGroup` call was issued
        // first by the caller -- the decision depends only on current
        // storage state (`existing`) compared against `incoming`, never
        // on delivery order, so evaluating the same `(existing,
        // incoming)` pair twice (simulating two redelivered calls racing
        // to observe the same storage snapshot) always agrees.
        let existing = test_config(4, false, "if_any_passed");
        let incoming = test_config(4, false, "if_any_passed");
        let first = resolve_register_shard_group(Some(&existing), &incoming);
        let second = resolve_register_shard_group(Some(&existing), &incoming);
        assert_eq!(first, second);
        assert_eq!(first, RegisterShardGroupDecision::AlreadyRegistered);
    }

    #[test]
    fn resolve_register_shard_group_conflicts_on_a_different_expected_total() {
        let existing = test_config(4, false, "if_any_passed");
        let incoming = test_config(8, false, "if_any_passed");
        assert_eq!(
            resolve_register_shard_group(Some(&existing), &incoming),
            RegisterShardGroupDecision::Conflict
        );
    }

    #[test]
    fn resolve_register_shard_group_conflicts_on_a_different_fail_fast() {
        let existing = test_config(4, false, "if_any_passed");
        let incoming = test_config(4, true, "if_any_passed");
        assert_eq!(
            resolve_register_shard_group(Some(&existing), &incoming),
            RegisterShardGroupDecision::Conflict
        );
    }

    #[test]
    fn resolve_register_shard_group_conflicts_on_a_different_merge_on_failure() {
        let existing = test_config(4, false, "if_any_passed");
        let incoming = test_config(4, false, "always");
        assert_eq!(
            resolve_register_shard_group(Some(&existing), &incoming),
            RegisterShardGroupDecision::Conflict
        );
    }

    #[test]
    fn resolve_register_shard_group_conflict_never_overwrites_the_stored_config() {
        // A conflicting call followed by a later call identical to the
        // *original* stored config must still resolve from that
        // untouched stored row, not from whatever the conflicting call
        // tried to write -- i.e. the conflict decision alone (which this
        // pure function returns) is what keeps the caller from ever
        // calling `insert_job_group` on it; re-evaluating against the
        // same still-original `existing` proves nothing was mutated.
        let existing = test_config(4, false, "if_any_passed");
        let conflicting = test_config(8, false, "if_any_passed");
        assert_eq!(
            resolve_register_shard_group(Some(&existing), &conflicting),
            RegisterShardGroupDecision::Conflict
        );
        let identical_to_original = test_config(4, false, "if_any_passed");
        assert_eq!(
            resolve_register_shard_group(Some(&existing), &identical_to_original),
            RegisterShardGroupDecision::AlreadyRegistered
        );
    }

    fn row(idx: u32, attempt: u32, status: ShardTerminalStatus) -> ShardStateRow {
        ShardStateRow {
            idx,
            attempt,
            status,
        }
    }

    #[test]
    fn resolve_shard_terminal_records_a_fresh_key() {
        assert_eq!(
            resolve_shard_terminal(None, ShardTerminalStatus::Passed),
            Ok(ShardTerminalDecision::Recorded)
        );
    }

    #[test]
    fn resolve_shard_terminal_is_a_noop_for_an_identical_redelivery() {
        assert_eq!(
            resolve_shard_terminal(
                Some(ShardTerminalStatus::Failed),
                ShardTerminalStatus::Failed
            ),
            Ok(ShardTerminalDecision::AlreadyRecorded)
        );
    }

    #[test]
    fn resolve_shard_terminal_rejects_a_conflicting_redelivery() {
        assert_eq!(
            resolve_shard_terminal(
                Some(ShardTerminalStatus::Passed),
                ShardTerminalStatus::Failed
            ),
            Err(ConflictingShardStatus)
        );
    }

    #[test]
    fn latest_attempt_per_shard_keeps_only_the_highest_attempt() {
        // idx 0's OOM-retried attempt 2 overrides its own attempt 1;
        // idx 1 only ever had one attempt.
        let rows = vec![
            row(0, 1, ShardTerminalStatus::Failed),
            row(0, 2, ShardTerminalStatus::Passed),
            row(1, 1, ShardTerminalStatus::Passed),
        ];
        let latest = latest_attempt_per_shard(&rows);
        // `latest_attempt_per_shard`'s output is sorted by idx (its doc
        // comment), so the whole vec can be compared directly rather
        // than searching it.
        assert_eq!(
            latest,
            vec![
                row(0, 2, ShardTerminalStatus::Passed),
                row(1, 1, ShardTerminalStatus::Passed),
            ],
            "attempt 1 of idx 0 must not double-count"
        );
    }

    fn config(
        expected_total: u32,
        fail_fast: bool,
        merge_on_failure: MergeOnFailure,
    ) -> JobGroupConfig {
        JobGroupConfig {
            expected_total,
            fail_fast,
            merge_on_failure,
        }
    }

    #[test]
    fn evaluate_barrier_satisfied_merge_yes_when_every_shard_passes() {
        let latest = vec![
            row(0, 1, ShardTerminalStatus::Passed),
            row(1, 1, ShardTerminalStatus::Passed),
        ];
        let outcome = evaluate_barrier(
            config(2, false, MergeOnFailure::IfAnyPassed),
            &latest,
            ShardTerminalStatus::Passed,
        );
        assert_eq!(
            outcome,
            BarrierOutcome::Satisfied {
                merge: true,
                included_idxs: vec![0, 1],
            }
        );
    }

    #[test]
    fn evaluate_barrier_if_any_passed_includes_only_passing_shards() {
        let latest = vec![
            row(0, 1, ShardTerminalStatus::Passed),
            row(1, 1, ShardTerminalStatus::Failed),
            row(2, 1, ShardTerminalStatus::Passed),
        ];
        let outcome = evaluate_barrier(
            config(3, false, MergeOnFailure::IfAnyPassed),
            &latest,
            ShardTerminalStatus::Failed,
        );
        assert_eq!(
            outcome,
            BarrierOutcome::Satisfied {
                merge: true,
                included_idxs: vec![0, 2],
            }
        );
    }

    #[test]
    fn evaluate_barrier_never_skips_merge_regardless_of_pass_fail_mix() {
        let latest = vec![
            row(0, 1, ShardTerminalStatus::Passed),
            row(1, 1, ShardTerminalStatus::Failed),
        ];
        let outcome = evaluate_barrier(
            config(2, false, MergeOnFailure::Never),
            &latest,
            ShardTerminalStatus::Failed,
        );
        assert_eq!(
            outcome,
            BarrierOutcome::Satisfied {
                merge: false,
                included_idxs: vec![],
            }
        );
    }

    #[test]
    fn evaluate_barrier_always_merges_even_if_every_shard_failed() {
        let latest = vec![
            row(0, 1, ShardTerminalStatus::Failed),
            row(1, 1, ShardTerminalStatus::Failed),
        ];
        let outcome = evaluate_barrier(
            config(2, false, MergeOnFailure::Always),
            &latest,
            ShardTerminalStatus::Failed,
        );
        assert_eq!(
            outcome,
            BarrierOutcome::Satisfied {
                merge: true,
                included_idxs: vec![0, 1],
            }
        );
    }

    #[test]
    fn evaluate_barrier_fail_fast_triggers_immediately_and_cancels_the_rest() {
        // 4-shard group, fail_fast=true, only shard 0 has reported so far
        // (as a failure) — the barrier must not wait for shards 1..3.
        let latest = vec![row(0, 1, ShardTerminalStatus::Failed)];
        let outcome = evaluate_barrier(
            config(4, true, MergeOnFailure::IfAnyPassed),
            &latest,
            ShardTerminalStatus::Failed,
        );
        assert_eq!(
            outcome,
            BarrierOutcome::FailFastTriggered {
                cancel_idxs: vec![1, 2, 3],
            }
        );
    }

    #[test]
    fn not_yet_terminal_idxs_excludes_every_idx_already_present() {
        let latest = vec![
            row(0, 1, ShardTerminalStatus::Failed),
            row(2, 1, ShardTerminalStatus::Passed),
        ];
        assert_eq!(not_yet_terminal_idxs(4, &latest), vec![1, 3]);
    }

    #[test]
    fn not_yet_terminal_idxs_matches_evaluate_barrier_fail_fast_cancel_set() {
        // `legacy_decision_for`'s reconstructed `"failed"` branch (coordinator/mod.rs) calls
        // this same helper the live `FailFastTriggered` path above does -- this proves the
        // reconstruction can never diverge from the live barrier's own cancel-set semantics,
        // and in particular never re-targets an idx that already has an immutable terminal row
        // (unlike a blind `0..expected_total` which would include it).
        let latest = vec![row(0, 1, ShardTerminalStatus::Failed)];
        assert_eq!(not_yet_terminal_idxs(4, &latest), vec![1, 2, 3]);
    }

    #[test]
    fn evaluate_barrier_fail_fast_false_waits_for_every_remaining_shard() {
        // Same first-shard failure, but fail_fast=false: the group must
        // keep waiting rather than short-circuiting.
        let latest = vec![row(0, 1, ShardTerminalStatus::Failed)];
        let outcome = evaluate_barrier(
            config(4, false, MergeOnFailure::IfAnyPassed),
            &latest,
            ShardTerminalStatus::Failed,
        );
        assert_eq!(outcome, BarrierOutcome::Waiting);
    }

    #[test]
    fn evaluate_barrier_oom_retried_attempt_counts_once_toward_expected_total() {
        // idx 0 OOM-retried to attempt 2 (now passed); idx 1 passed on
        // its only attempt. `latest_attempt_per_shard` must already have
        // collapsed attempt 1 of idx 0 away before this call, so
        // `expected_total = 2` is satisfied by exactly 2 entries, not 3.
        let all_terminal_rows = vec![
            row(0, 1, ShardTerminalStatus::Failed),
            row(0, 2, ShardTerminalStatus::Passed),
            row(1, 1, ShardTerminalStatus::Passed),
        ];
        let latest = latest_attempt_per_shard(&all_terminal_rows);
        let outcome = evaluate_barrier(
            config(2, false, MergeOnFailure::IfAnyPassed),
            &latest,
            ShardTerminalStatus::Passed,
        );
        assert_eq!(
            outcome,
            BarrierOutcome::Satisfied {
                merge: true,
                included_idxs: vec![0, 1],
            }
        );
    }

    #[test]
    fn shard_node_id_is_deterministic_and_distinct_per_attempt() {
        assert_eq!(shard_node_id("e2e", 2, 1), "shard:e2e:2:1");
        // A retried shard (OOM retry to attempt 2) gets a *different* id
        // than its first attempt — the two are different real containers.
        assert_ne!(shard_node_id("e2e", 2, 1), shard_node_id("e2e", 2, 2));
        // Deterministic: the same inputs always produce the same id, so a
        // redelivered `startNode` for the same shard attempt lands on the
        // same `node_id` (and thus `startNode`'s own idempotency guard).
        assert_eq!(shard_node_id("e2e", 2, 1), shard_node_id("e2e", 2, 1));
    }

    #[test]
    fn shard_nodes_to_cancel_matches_only_the_named_indexes_running_node() {
        let nodes = vec![
            (shard_node_id("e2e", 0, 1), NodeState::Running),
            (shard_node_id("e2e", 1, 1), NodeState::Running),
            // A different job's shard 0 must never match "e2e"'s shard 0.
            (shard_node_id("other-job", 0, 1), NodeState::Running),
        ];
        assert_eq!(
            shard_nodes_to_cancel("e2e", 0, &nodes),
            vec![shard_node_id("e2e", 0, 1)]
        );
        assert_eq!(
            shard_nodes_to_cancel("e2e", 1, &nodes),
            vec![shard_node_id("e2e", 1, 1)]
        );
    }

    #[test]
    fn shard_nodes_to_cancel_skips_already_terminal_nodes() {
        // The shard finished (successfully) on its own before the
        // fail-fast decision landed — never relabel it cancelled.
        let nodes = vec![(shard_node_id("e2e", 0, 1), NodeState::Succeeded)];
        assert_eq!(
            shard_nodes_to_cancel("e2e", 0, &nodes),
            Vec::<String>::new()
        );
    }
    #[test]
    fn shard_nodes_to_cancel_is_empty_when_the_shard_was_never_dispatched_as_a_node() {
        // No node was ever registered for this index — a documented
        // no-op for that index, not a panic or an error.
        let nodes = vec![(shard_node_id("e2e", 1, 1), NodeState::Running)];
        assert_eq!(
            shard_nodes_to_cancel("e2e", 0, &nodes),
            Vec::<String>::new()
        );
    }

    #[test]
    fn shard_nodes_to_cancel_does_not_match_a_different_colon_containing_job_name() {
        // Regression test for a real selection bug: `job_name` is
        // free-form, caller-supplied text and may itself contain `:` —
        // the same separator `shard_node_id` joins its own fields with.
        // A short job's `shard_node_prefix` used to be a literal
        // `starts_with` byte-prefix of a *longer, unrelated* job's own
        // shard node id: `shard_node_prefix("build", 1)` ==
        // `"shard:build:1:"`, which is a literal prefix of
        // `shard_node_id("build:1", 2, 3)` == `"shard:build:1:2:3"` — a
        // different job ("build:1", not "build"), index (2, not 1), and
        // attempt (3). Fail-fast for job "build" idx 1 must never select
        // job "build:1"'s node.
        let unrelated_node_id = shard_node_id("build:1", 2, 3);
        let nodes = vec![(unrelated_node_id, NodeState::Running)];
        assert_eq!(
            shard_nodes_to_cancel("build", 1, &nodes),
            Vec::<String>::new()
        );
    }

    #[test]
    fn shard_nodes_to_cancel_picks_the_right_attempt_among_colon_containing_job_names_and_terminal_siblings()
     {
        // Combines every selection edge case this function must get
        // right at once: an OOM-retried shard's *latest* running attempt
        // is selected, its own already-terminal earlier attempt is
        // skipped (never relabelled), and an unrelated job whose name
        // embeds "build:1:" is never matched despite sharing a byte
        // prefix with "build" idx 1's own node ids.
        let nodes = vec![
            (shard_node_id("build", 1, 1), NodeState::Succeeded),
            (shard_node_id("build", 1, 2), NodeState::Running),
            (shard_node_id("build:1", 2, 3), NodeState::Running),
        ];
        assert_eq!(
            shard_nodes_to_cancel("build", 1, &nodes),
            vec![shard_node_id("build", 1, 2)]
        );
    }

    #[test]
    fn shard_terminal_node_state_maps_passed_and_failed() {
        assert_eq!(
            shard_terminal_node_state(ShardTerminalStatus::Passed),
            NodeState::Succeeded
        );
        assert_eq!(
            shard_terminal_node_state(ShardTerminalStatus::Failed),
            NodeState::Failed
        );
    }

    #[test]
    fn shard_self_completion_target_completes_a_still_running_own_node() {
        // The shard's own node was dispatched and is still `running`
        // when its own terminal report arrives — complete it for real
        // with the shard's actual outcome, not `Cancelled`.
        assert_eq!(
            shard_self_completion_target(Some(NodeState::Running), ShardTerminalStatus::Failed),
            Some(NodeState::Failed)
        );
        assert_eq!(
            shard_self_completion_target(Some(NodeState::Pending), ShardTerminalStatus::Passed),
            Some(NodeState::Succeeded)
        );
    }

    #[test]
    fn shard_self_completion_target_is_a_noop_when_no_node_was_ever_registered() {
        assert_eq!(
            shard_self_completion_target(None, ShardTerminalStatus::Passed),
            None
        );
    }

    #[test]
    fn shard_self_completion_target_is_a_noop_for_an_already_terminal_node() {
        // The node already called `completeNode` itself (a real
        // container that already exited) — never re-stop it or
        // overwrite its already-recorded status.
        assert_eq!(
            shard_self_completion_target(Some(NodeState::Succeeded), ShardTerminalStatus::Failed),
            None
        );
        // Already `Cancelled` by some other mechanism — never relabel
        // it with the shard's own outcome either.
        assert_eq!(
            shard_self_completion_target(Some(NodeState::Cancelled), ShardTerminalStatus::Passed),
            None
        );
    }

    #[test]
    fn resolve_stop_outcome_is_resolved_for_a_missing_physical_address_regardless_of_result() {
        // No address was ever recorded -- no retry could ever do better, so this resolves even
        // though `stop_succeeded` is false, unlike a real address's own failure below.
        assert_eq!(resolve_stop_outcome(None, false), StopOutcome::Resolved);
        assert_eq!(resolve_stop_outcome(None, true), StopOutcome::Resolved);
    }

    #[test]
    fn resolve_stop_outcome_is_resolved_when_a_real_stop_succeeds() {
        assert_eq!(
            resolve_stop_outcome(Some("addr"), true),
            StopOutcome::Resolved
        );
    }

    #[test]
    fn resolve_stop_outcome_is_pending_when_a_real_stop_fails() {
        // A genuine address exists and the stop failed -- this may succeed on a later retry,
        // unlike the `None`-address case above, so it must not be silently treated as resolved.
        assert_eq!(
            resolve_stop_outcome(Some("addr"), false),
            StopOutcome::Pending
        );
    }

    // -- decide_oom_recovery / oom_retry_node_id -----------------------

    fn bounded_ladder(names: &[&str]) -> Vec<InstanceSize> {
        names
            .iter()
            .map(|name| InstanceSize {
                name: (*name).to_string(),
                vcpu: 1.0,
                memory_bytes: 1,
            })
            .collect()
    }

    #[test]
    fn oom_recovery_first_oom_within_bounds_retries_to_next_size() {
        let ladder = bounded_ladder(&["basic", "standard-1", "standard-2"]);
        let incoming = OomObservation {
            attempt: 1,
            current_size: "basic".to_string(),
            measured_peak_bytes: Some(2_000),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, None, &incoming),
            OomRecoveryDecision::New(OomRecoveryOutcome::RetryAt {
                to: "standard-1".to_string(),
                reason: "basic -> standard-1: oom-retry".to_string(),
            })
        );
    }

    #[test]
    fn oom_recovery_first_oom_at_configured_max_fails_with_measured_peak() {
        let ladder = bounded_ladder(&["basic", "standard-1", "standard-2"]);
        let incoming = OomObservation {
            attempt: 1,
            current_size: "standard-2".to_string(),
            measured_peak_bytes: Some(7_000_000_000),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, None, &incoming),
            OomRecoveryDecision::New(OomRecoveryOutcome::FailedAtMax {
                max: "standard-2".to_string(),
                measured_peak_bytes: Some(7_000_000_000),
            })
        );
    }

    #[test]
    fn oom_recovery_single_size_bound_fails_immediately_without_a_retry() {
        // `min == max`: the node's own configured bounds leave no room to retry at all, so
        // even the very first OOM is terminal.
        let ladder = bounded_ladder(&["basic"]);
        let incoming = OomObservation {
            attempt: 1,
            current_size: "basic".to_string(),
            measured_peak_bytes: Some(500),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, None, &incoming),
            OomRecoveryDecision::New(OomRecoveryOutcome::FailedAtMax {
                max: "basic".to_string(),
                measured_peak_bytes: Some(500),
            })
        );
    }

    #[test]
    fn oom_recovery_second_oom_after_the_one_retry_fails_even_below_configured_max() {
        // The retried attempt (now at standard-1, not the configured max standard-2) OOMs
        // again: analytics.md grants exactly one retry ("rather than retrying indefinitely"),
        // so this fails rather than climbing to standard-2 -- and the message still names the
        // *configured* max, not the lower size this second OOM actually happened at.
        let ladder = bounded_ladder(&["basic", "standard-1", "standard-2"]);
        let existing = OomDecisionRecord {
            attempt: 1,
            base_node_id: "shard:e2e:0:1".to_string(),
            outcome: OomRecoveryOutcome::RetryAt {
                to: "standard-1".to_string(),
                reason: "basic -> standard-1: oom-retry".to_string(),
            },
        };
        let incoming = OomObservation {
            attempt: 2,
            current_size: "standard-1".to_string(),
            measured_peak_bytes: Some(3_000_000_000),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, Some(&existing), &incoming),
            OomRecoveryDecision::New(OomRecoveryOutcome::FailedAtMax {
                max: "standard-2".to_string(),
                measured_peak_bytes: Some(3_000_000_000),
            })
        );
    }

    #[test]
    fn oom_recovery_duplicate_delivery_of_already_decided_attempt_returns_stored_outcome() {
        let ladder = bounded_ladder(&["basic", "standard-1"]);
        let outcome = OomRecoveryOutcome::RetryAt {
            to: "standard-1".to_string(),
            reason: "basic -> standard-1: oom-retry".to_string(),
        };
        let existing = OomDecisionRecord {
            attempt: 1,
            base_node_id: "shard:e2e:0:1".to_string(),
            outcome: outcome.clone(),
        };
        let incoming = OomObservation {
            attempt: 1,
            current_size: "basic".to_string(),
            measured_peak_bytes: Some(2_000),
        };
        // A redelivered batch for the exact same attempt must resolve to the already-stored
        // outcome, not recompute (which would otherwise be harmless here, but must never
        // re-dispatch a second real container start for the same attempt).
        assert_eq!(
            decide_oom_recovery(&ladder, Some(&existing), &incoming),
            OomRecoveryDecision::AlreadyDecided(outcome)
        );
    }

    #[test]
    fn oom_recovery_reordered_earlier_attempt_after_later_decision_is_stale() {
        let ladder = bounded_ladder(&["basic", "standard-1", "standard-2"]);
        let existing = OomDecisionRecord {
            attempt: 2,
            base_node_id: "shard:e2e:0:1".to_string(),
            outcome: OomRecoveryOutcome::RetryAt {
                to: "standard-1".to_string(),
                reason: "basic -> standard-1: oom-retry".to_string(),
            },
        };
        // A late-arriving delivery for attempt 1, after attempt 2's own decision already
        // landed -- must not overwrite or re-decide anything.
        let incoming = OomObservation {
            attempt: 1,
            current_size: "basic".to_string(),
            measured_peak_bytes: Some(2_000),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, Some(&existing), &incoming),
            OomRecoveryDecision::Stale
        );
    }

    #[test]
    fn oom_recovery_reordered_attempt_after_a_failed_at_max_decision_is_already_decided_not_stale()
    {
        // A terminal lineage's stored outcome is returned for *any* later-arriving delivery
        // regardless of whether its own attempt is older or newer than the one that decided
        // it -- `AlreadyDecided`, not `Stale`, since there genuinely is a decision to report.
        let ladder = bounded_ladder(&["basic", "standard-1", "standard-2"]);
        let outcome = OomRecoveryOutcome::FailedAtMax {
            max: "standard-2".to_string(),
            measured_peak_bytes: Some(1),
        };
        let existing = OomDecisionRecord {
            attempt: 2,
            base_node_id: "shard:e2e:0:1".to_string(),
            outcome: outcome.clone(),
        };
        let incoming = OomObservation {
            attempt: 1,
            current_size: "basic".to_string(),
            measured_peak_bytes: Some(2_000),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, Some(&existing), &incoming),
            OomRecoveryDecision::AlreadyDecided(outcome)
        );
    }

    #[test]
    fn oom_recovery_current_size_not_in_bounded_ladder_is_not_in_ladder() {
        let ladder = bounded_ladder(&["basic", "standard-1"]);
        let incoming = OomObservation {
            attempt: 1,
            current_size: "standard-4".to_string(),
            measured_peak_bytes: Some(1),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, None, &incoming),
            OomRecoveryDecision::NotInLadder
        );
    }

    #[test]
    fn oom_recovery_unknown_peak_stays_unknown_rather_than_a_fabricated_zero() {
        let ladder = bounded_ladder(&["basic"]);
        let incoming = OomObservation {
            attempt: 1,
            current_size: "basic".to_string(),
            measured_peak_bytes: None,
        };
        assert_eq!(
            decide_oom_recovery(&ladder, None, &incoming),
            OomRecoveryDecision::New(OomRecoveryOutcome::FailedAtMax {
                max: "basic".to_string(),
                measured_peak_bytes: None,
            })
        );
    }

    #[test]
    fn oom_retry_node_id_is_distinct_per_attempt_and_deterministic() {
        assert_eq!(oom_retry_node_id("auto:build", 2), "auto:build:oom-retry:2");
        assert_ne!(
            oom_retry_node_id("auto:build", 2),
            oom_retry_node_id("auto:build", 3)
        );
        assert_eq!(
            oom_retry_node_id("auto:build", 2),
            oom_retry_node_id("auto:build", 2)
        );
    }

    // -- resolve_oom_node_id ---------------------------------------------

    #[test]
    fn resolve_oom_node_id_with_no_decision_uses_shard_node_id() {
        assert_eq!(
            resolve_oom_node_id("e2e", 2, 1, None),
            shard_node_id("e2e", 2, 1)
        );
    }

    #[test]
    fn resolve_oom_node_id_with_a_retry_at_decision_reconstructs_the_retry_real_id() {
        let existing = OomDecisionRecord {
            attempt: 1,
            base_node_id: shard_node_id("e2e", 2, 1),
            outcome: OomRecoveryOutcome::RetryAt {
                to: "standard-1".to_string(),
                reason: "basic -> standard-1: oom-retry".to_string(),
            },
        };
        // The delivery's own reported `attempt` (3, say) is irrelevant here -- trusting it
        // instead would be the bug; the real id is reconstructed from the decision's own
        // `base_node_id`/`attempt`.
        assert_eq!(
            resolve_oom_node_id("e2e", 2, 3, Some(&existing)),
            oom_retry_node_id(&shard_node_id("e2e", 2, 1), 2)
        );
    }

    #[test]
    fn resolve_oom_node_id_with_a_failed_at_max_decision_points_back_at_the_base_node() {
        let existing = OomDecisionRecord {
            attempt: 2,
            base_node_id: oom_retry_node_id(&shard_node_id("e2e", 2, 1), 2),
            outcome: OomRecoveryOutcome::FailedAtMax {
                max: "standard-2".to_string(),
                measured_peak_bytes: Some(1),
            },
        };
        assert_eq!(
            resolve_oom_node_id("e2e", 2, 3, Some(&existing)),
            existing.base_node_id
        );
    }

    #[test]
    fn a_second_oom_on_the_retried_node_resolves_to_failed_at_max() {
        // A `RetryAt` decision already exists for shard e2e/2, and the node it dispatched
        // (not `shard_node_id("e2e", 2, 2)`) itself now OOMs. The real id must be found, and
        // the decision must be `FailedAtMax`, naming the configured max.
        let ladder = bounded_ladder(&["basic", "standard-1", "standard-2"]);
        let existing = OomDecisionRecord {
            attempt: 1,
            base_node_id: shard_node_id("e2e", 2, 1),
            outcome: OomRecoveryOutcome::RetryAt {
                to: "standard-1".to_string(),
                reason: "basic -> standard-1: oom-retry".to_string(),
            },
        };
        let retried_node_id = resolve_oom_node_id("e2e", 2, 999, Some(&existing));
        assert_eq!(
            retried_node_id,
            oom_retry_node_id(&shard_node_id("e2e", 2, 1), 2)
        );
        let incoming = OomObservation {
            attempt: 2,
            current_size: "standard-1".to_string(),
            measured_peak_bytes: Some(5_000_000_000),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, Some(&existing), &incoming),
            OomRecoveryDecision::New(OomRecoveryOutcome::FailedAtMax {
                max: "standard-2".to_string(),
                measured_peak_bytes: Some(5_000_000_000),
            })
        );
    }

    #[test]
    fn a_retried_node_reporting_the_same_attempt_as_the_original_resolves_to_already_decided() {
        // An honestly-unresolvable residual: without a `node_id` field on
        // `SubmitResourceSamplesRequest`, a retry whose real dispatch failed to carry its new
        // attempt number is
        // indistinguishable from a duplicate delivery of the original's own decision. This
        // must degrade *safely* -- `AlreadyDecided`, never a crash, never a wrong dispatch --
        // even though the real second OOM goes undetected in this specific broken-dispatch
        // case.
        let ladder = bounded_ladder(&["basic", "standard-1", "standard-2"]);
        let outcome = OomRecoveryOutcome::RetryAt {
            to: "standard-1".to_string(),
            reason: "basic -> standard-1: oom-retry".to_string(),
        };
        let existing = OomDecisionRecord {
            attempt: 1,
            base_node_id: shard_node_id("e2e", 2, 1),
            outcome: outcome.clone(),
        };
        let incoming = OomObservation {
            attempt: 1,
            current_size: "standard-1".to_string(),
            measured_peak_bytes: Some(5_000_000_000),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, Some(&existing), &incoming),
            OomRecoveryDecision::AlreadyDecided(outcome)
        );
    }

    #[test]
    fn an_attempt_past_the_retry_nodes_expected_attempt_is_stale_not_failed_at_max() {
        // Exact-match hardening: only `existing.attempt + 1` (the one attempt
        // `oom_retry_node_id` actually dispatched) is treated as the retry's own second OOM.
        // An attempt that jumped further ahead (a different, unrelated mechanism bumping the
        // counter) is `Stale`, never silently mistaken for it.
        let ladder = bounded_ladder(&["basic", "standard-1", "standard-2"]);
        let existing = OomDecisionRecord {
            attempt: 1,
            base_node_id: shard_node_id("e2e", 2, 1),
            outcome: OomRecoveryOutcome::RetryAt {
                to: "standard-1".to_string(),
                reason: "basic -> standard-1: oom-retry".to_string(),
            },
        };
        let incoming = OomObservation {
            attempt: 3,
            current_size: "standard-1".to_string(),
            measured_peak_bytes: Some(5_000_000_000),
        };
        assert_eq!(
            decide_oom_recovery(&ladder, Some(&existing), &incoming),
            OomRecoveryDecision::Stale
        );
    }

    // -- oom_event_admissible ----------------------------------------------

    #[test]
    fn oom_event_admissible_true_for_a_running_node_on_a_live_run() {
        assert!(oom_event_admissible(false, NodeState::Running));
        assert!(oom_event_admissible(false, NodeState::Pending));
    }

    #[test]
    fn oom_event_admissible_false_for_a_terminal_node() {
        assert!(!oom_event_admissible(false, NodeState::Succeeded));
        assert!(!oom_event_admissible(false, NodeState::Cancelled));
        assert!(!oom_event_admissible(false, NodeState::Failed));
    }

    #[test]
    fn oom_event_admissible_false_for_a_terminal_run_even_with_a_running_node() {
        assert!(!oom_event_admissible(true, NodeState::Running));
    }

    // -- resolve_auto_start --------------------------------------------------

    #[test]
    fn resolve_auto_start_with_explicit_bounds_and_initial() {
        let ladder = bounded_ladder(&["lite", "standard-1", "standard-2"]);
        assert_eq!(
            resolve_auto_start(&ladder, Some("standard-1"), Some("standard-2"), None),
            Ok(AutoStart {
                initial: "standard-1".to_string(),
                min: "standard-1".to_string(),
                max: "standard-2".to_string(),
            })
        );
    }

    #[test]
    fn resolve_auto_start_unset_bounds_default_to_the_ladders_own_ends() {
        let ladder = bounded_ladder(&["lite", "standard-1", "standard-2"]);
        assert_eq!(
            resolve_auto_start(&ladder, None, None, None),
            Ok(AutoStart {
                initial: "lite".to_string(),
                min: "lite".to_string(),
                max: "standard-2".to_string(),
            })
        );
    }

    #[test]
    fn resolve_auto_start_rejects_a_name_the_executor_ladder_does_not_support() {
        // The documented default settings example (`initial: basic`) against the real
        // Cloudflare Containers executor's ladder, which does not include `"basic"` for a
        // per-call `durable_object` size.
        let ladder = bounded_ladder(&["standard-4"]);
        assert_eq!(
            resolve_auto_start(&ladder, Some("basic"), Some("standard-3"), Some("basic")),
            Err(AutoStartError::BoundsNotInLadder {
                min: "basic".to_string(),
                max: "standard-3".to_string(),
            })
        );
    }

    #[test]
    fn resolve_auto_start_rejects_an_inverted_range() {
        let ladder = bounded_ladder(&["lite", "standard-1", "standard-2"]);
        assert_eq!(
            resolve_auto_start(&ladder, Some("standard-2"), Some("lite"), None),
            Err(AutoStartError::BoundsNotInLadder {
                min: "standard-2".to_string(),
                max: "lite".to_string(),
            })
        );
    }

    #[test]
    fn resolve_auto_start_rejects_an_initial_outside_the_resolved_bounds() {
        let ladder = bounded_ladder(&["lite", "standard-1", "standard-2"]);
        assert_eq!(
            resolve_auto_start(
                &ladder,
                Some("standard-1"),
                Some("standard-2"),
                Some("lite")
            ),
            Err(AutoStartError::InitialOutOfBounds {
                initial: "lite".to_string(),
                min: "standard-1".to_string(),
                max: "standard-2".to_string(),
            })
        );
    }

    // -- next_oom_effect / oom_effects_pending ------------------------------

    #[test]
    fn next_oom_effect_walks_the_fixed_critical_first_order_for_a_retry_outcome() {
        // Every container-state-critical effect (mark, stop, insert, start) is ordered
        // before any D1 projection, so a D1 outage can never block the retry itself.
        let mut flags = OomEffectFlags::default();
        let order = [
            OomEffect::MarkOldTerminal,
            OomEffect::StopOld,
            OomEffect::InsertRetry,
            OomEffect::StartRetry,
            OomEffect::ProjectOld,
            OomEffect::ProjectDecision,
            OomEffect::ProjectRetry,
        ];
        for expected in order {
            assert_eq!(next_oom_effect(true, flags), Some(expected));
            assert!(oom_effects_pending(true, flags));
            match expected {
                OomEffect::MarkOldTerminal => flags.old_marked = true,
                OomEffect::StopOld => flags.old_stopped = true,
                OomEffect::ProjectOld => flags.old_projected = true,
                OomEffect::ProjectDecision => flags.decision_projected = true,
                OomEffect::InsertRetry => flags.retry_inserted = true,
                OomEffect::StartRetry => flags.retry_started = true,
                OomEffect::ProjectRetry => flags.retry_projected = true,
            }
        }
        assert_eq!(next_oom_effect(true, flags), None);
        assert!(!oom_effects_pending(true, flags));
    }

    #[test]
    fn next_oom_effect_excluding_skips_a_failed_projection_and_still_finds_the_next_one() {
        // A projection that already failed during this drain must not hide the other
        // still-pending ones -- `old_projected` failed and stays unset, but
        // `decision_projected` is still reachable.
        let flags = OomEffectFlags {
            old_marked: true,
            old_stopped: true,
            retry_inserted: true,
            retry_started: true,
            ..OomEffectFlags::default()
        };
        assert_eq!(
            next_oom_effect_excluding(true, flags, &[OomEffect::ProjectOld]),
            Some(OomEffect::ProjectDecision)
        );
    }

    #[test]
    fn oom_effect_is_projection_distinguishes_critical_from_best_effort() {
        assert!(OomEffect::ProjectOld.is_projection());
        assert!(OomEffect::ProjectDecision.is_projection());
        assert!(OomEffect::ProjectRetry.is_projection());
        assert!(!OomEffect::MarkOldTerminal.is_projection());
        assert!(!OomEffect::StopOld.is_projection());
        assert!(!OomEffect::InsertRetry.is_projection());
        assert!(!OomEffect::StartRetry.is_projection());
    }

    #[test]
    fn next_oom_effect_stops_after_the_decision_projection_for_a_failed_at_max_outcome() {
        // `is_retry = false`: there is no retry to insert/start/project, so draining a
        // `FailedAtMax` decision must terminate right after `ProjectDecision`, never asking
        // for any of the three retry-only effects.
        let flags = OomEffectFlags {
            old_marked: true,
            old_stopped: true,
            old_projected: true,
            decision_projected: true,
            ..OomEffectFlags::default()
        };
        assert_eq!(next_oom_effect(false, flags), None);
        assert!(!oom_effects_pending(false, flags));
    }

    #[test]
    fn next_oom_effect_resumes_exactly_at_the_first_unset_flag() {
        // A crash between inserting the retry row and starting its
        // container leaves `retry_inserted = true` but `retry_started = false` -- draining
        // must resume at `StartRetry`, never re-run `InsertRetry` or skip ahead.
        let flags = OomEffectFlags {
            old_marked: true,
            old_stopped: true,
            old_projected: true,
            decision_projected: true,
            retry_inserted: true,
            ..OomEffectFlags::default()
        };
        assert_eq!(next_oom_effect(true, flags), Some(OomEffect::StartRetry));
    }
}
