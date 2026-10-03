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
/// do other in-flight nodes get cancelled too, same as
/// [`nodes_to_cancel`]?) without that caller would be exactly the kind
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

/// Which of `nodes`' ids a run cancellation should mark `Cancelled` —
/// "Coordinator stops containers, marks nodes `cancelled`...". Only
/// non-terminal nodes are affected: a node that already reached
/// `Succeeded`/`Failed`/`Skipped`/`TimedOut` keeps that real outcome,
/// never gets relabeled `Cancelled` after the fact. Order-preserving.
pub fn nodes_to_cancel(nodes: &[(String, NodeState)]) -> Vec<String> {
    nodes
        .iter()
        .filter(|(_, status)| !status.is_terminal())
        .map(|(id, _)| id.clone())
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
// strategies per report type" table is entirely out of scope here) and it
// does **not** itself stop any container: [`evaluate_barrier`]'s
// `FailFastTriggered` arm only returns the *set* of shard indices a caller
// should cancel. `coordinator::mod`'s `handle_shard_terminal` (this
// round's only caller) records that decision but does not wire it to
// `stop_node_container`/`handle_cancel_run` — shards are not yet modeled
// as `node` rows this round (no RPC registers "shard N started running"),
// so there is no real container handle to stop yet; that wiring is a
// separate, later round once shards are dispatched as real nodes.
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
        let present: std::collections::BTreeSet<u32> =
            latest_terminal.iter().map(|row| row.idx).collect();
        let cancel_idxs = (0..config.expected_total)
            .filter(|idx| !present.contains(idx))
            .collect();
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
    fn nodes_to_cancel_only_affects_non_terminal_nodes() {
        let nodes = vec![
            ("pending-node".to_string(), NodeState::Pending),
            ("running-node".to_string(), NodeState::Running),
            ("done-node".to_string(), NodeState::Succeeded),
            ("failed-node".to_string(), NodeState::Failed),
            ("already-cancelled".to_string(), NodeState::Cancelled),
        ];
        assert_eq!(
            nodes_to_cancel(&nodes),
            vec!["pending-node".to_string(), "running-node".to_string()]
        );
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
}
