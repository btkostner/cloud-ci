//! The per-run `RunCoordinator` Durable Object (ADR 0004, docs/design/byo-ci.md).
//!
//! One instance per run, addressed by [`do_name`] — deterministically derived
//! from `(repo_id, sha, run_key, attempt)` — so a redelivered or reordered
//! `BeginRun` always lands on the same instance. The instance owns minting
//! the run's ULID, holding `status`/`expect_jobs`/job rows in its own SQLite
//! storage (authoritative), and projecting that state into the D1
//! `runs`/`jobs` tables it also writes (ADR 0004: "D1 rows are its
//! projection"). Nothing outside this module talks to a raw [`Stub`]; use
//! [`RunCoordinatorStore`].
//!
//! Decision logic (idempotency rules, state transitions) lives in
//! [`logic`], which has no Durable Object dependency and is unit-tested
//! directly. This module is the thin, storage-wired shell around it —
//! exercised only by the live smoke test (`mise run //packages/cloud-ci-worker:dev`),
//! since `cargo test` cannot run a real Durable Object.
//!
//! ## Check Runs
//!
//! `StartJob.check_names`/`CompleteShard`/run close wire into GitHub
//! Check Runs (docs/design/byo-ci.md's "Checks and scopes",
//! docs/design/pr-comment.md's "Check Runs" table), via
//! [`crate::github_checks`]'s create/update calls:
//!
//! - **Storage.** The DO's own SQLite (`check_run` table, see
//!   [`ensure_schema`]) is authoritative, keyed by `check_name` — unique
//!   per run, since a `RunCoordinator` instance *is* one run, matching
//!   byo-ci.md's "(run, check_name)" dedupe key exactly. A D1 table
//!   (`check_runs`, migration 0008) projects it, same
//!   authoritative-DO/projected-D1 split as `runs`/`jobs`/`job_shards`.
//!   Which jobs attach to a check is derived, not stored separately:
//!   each job's existing `check_names` JSON column is scanned
//!   ([`check_summary_rows`]) rather than maintaining a second join
//!   table.
//! - **Idempotency.** A check-run row is only inserted after its
//!   `POST /check-runs` call actually succeeds, so a redelivered
//!   `StartJob` naming an already-created check sees the row and makes
//!   no second create call ([`RunCoordinator::create_check_runs_for_job`],
//!   via [`logic::new_check_names`]). A redelivered `CompleteShard`/close
//!   recomputes the same shard-table summary from the DO's own state and
//!   re-`PATCH`es it — idempotent by construction (same inputs, same
//!   output), not by skipping the call outright. `finalize_check_runs`
//!   additionally skips a check already `status = "completed"`, so a
//!   redelivered close signal cannot re-finalize one.
//! - **Scope boundary.** The Check Run `output.summary` built here is
//!   its own independent, simpler shard-table markdown
//!   ([`logic::render_check_summary`]: job name, shard N/total,
//!   conclusion) — it is deliberately **not** routed through
//!   `pr_comment::render_pr_report`'s template/context, even though
//!   pr-comment.md's "Check Runs" section describes the summary as
//!   eventually being "built from the same template context". That
//!   cross-module wiring is bigger later work, once a real run's
//!   aggregate data exists in the shape `pr_comment.rs`'s `PrReport`
//!   needs; see `github_checks.rs`'s module docs for the matching note
//!   on its caller side.
//! - **GitHub auth.** Every call resolves an installation token via
//!   [`crate::roles::lookup_repo_owner`] +
//!   [`crate::roles::installation_token_for_repo`] — the same lookup
//!   chain `roles.rs`'s role resolution already uses, not reinvented
//!   here ([`RunCoordinator::check_run_auth`]). Failure anywhere in that
//!   chain (repo not registered, secrets missing, GitHub API error)
//!   logs and is swallowed rather than failing the whole
//!   `StartJob`/`CompleteShard`/close-run call — a Check Run is a
//!   best-effort side channel, the same degrade-and-log posture
//!   `reconcile.rs`'s uninstall-of-disallowed-org already uses.

//! ## Nodes (node identity/idempotency, real container execution)
//!
//! docs/design/dynamic-pipelines.md's "### Execution model" describes
//! `ci.container`'s two-step protocol — `step.do("start:" + id)` then
//! `step.waitForEvent("done:" + id)` — and the idempotency table a
//! Dynamic Workflow's retried steps need `RunCoordinator` to enforce
//! around `(run_id, node_id)`. An earlier round built that table as
//! `RunCoordinator` RPCs — [`RunCoordinator::handle_start_node`],
//! [`RunCoordinator::handle_complete_node`],
//! [`RunCoordinator::handle_ack_node`],
//! [`RunCoordinator::handle_cancel_run`] — without starting any real
//! container (`startNode` only created/returned a `node` row). This
//! round closes exactly that gap:
//! [`RunCoordinator::handle_start_node`] now starts a real,
//! per-`node_id` container via a DO-to-DO call into `NodeContainer`
//! (`crate::node_container`'s own doc comment covers that DO's
//! synchronous-start/asynchronous-`exec()` split and why it is safe
//! under `step.do`'s "returns quickly" contract),
//! [`RunCoordinator::handle_complete_node`] now also receives that real
//! exit code as `NodeContainer`'s own completion callback (the same
//! wire shape a caller-supplied completion already used, so no change
//! to that handler was needed), and
//! [`RunCoordinator::handle_cancel_run`] now actually stops that
//! container rather than only flipping a DB flag. **Still not built:**
//!
//! - Any Dynamic Workflow integration, Cloudflare Workflows binding, or
//!   `step.do`/`step.waitForEvent` wiring. That is a separate, much
//!   larger round; these RPCs are the foundation a future caller would
//!   use, same "foundation ahead of its full caller" pattern as
//!   `repo_state`'s and `pull_request_state`'s rounds.
//! - A redelivery timer. The doc's "the coordinator tracks delivery per
//!   `(run_id, node_id)` and keeps re-sending until the script
//!   acknowledges it" needs a DO alarm, similar to this DO's existing
//!   timeout alarm. This round only tracks the `acked` bit correctly
//!   ([`RunCoordinator::handle_ack_node`]) — it does not build the
//!   on-a-timer re-send mechanism that bit would eventually drive.
//!
//! **Who fails the run on a nondeterministic replay.** The doc says a
//! `startNode` retry with a different spec hash is "rejected as
//! nondeterministic; run fails". [`logic::resolve_start_node`] returns a
//! typed [`logic::NondeterministicReplay`] error; this round's RPC
//! surfaces it as a `409` with `code: "nondeterministic_replay"|("" +
//! String)` (see [`CoordinatorError::NondeterministicReplay`]) rather
//! than itself transitioning the whole run to a failed terminal state —
//! see [`logic::resolve_start_node`]'s doc comment for why that is
//! deliberately deferred to the future Workflow-integration caller.
//!
//! **Storage.** A `node` table (see [`ensure_schema`]) is authoritative,
//! keyed by `node_id` alone — this DO instance *is* one run, the same
//! "run_id is implicit, not a column" pattern `job`/`job_shard` already
//! use. A `nodes` D1 table (migration 0010) projects it, keyed by
//! `(run_id, node_id)` since D1 is shared across runs, same
//! authoritative-DO/projected-D1 split as every other table here.
//!
//! **Run cancellation.** No RPC in this DO set a run to `RunState::Cancelled`
//! before this round — only the enum variant existed. `handle_cancel_run`
//! is the minimal hook this round adds: it moves the run to `Cancelled`
//! and marks every non-terminal node `Cancelled`
//! ([`logic::nodes_to_cancel`]). A `completeNode` call for an already-
//! `Cancelled` node afterward is a documented no-op
//! ([`logic::resolve_complete_node`]'s `DroppedCancelled` arm) — "late
//! completion events for cancelled nodes are dropped" — rather than an
//! error or a relabel.

//! ## Shard groups / merge barrier
//!
//! docs/design/parallelization.md's "### Merge barrier (RunCoordinator)"
//! and "### Failed-shard retry semantics" describe `job_group`/
//! `shard_state` and the barrier a shard group's terminal shard-ingest
//! calls evaluate. This round builds the **state machine only** —
//! [`RunCoordinator::handle_register_shard_group`] and
//! [`RunCoordinator::handle_shard_terminal`] (pure decisions in
//! [`logic`]'s "Shard groups / merge barrier" section) — never real merge
//! execution or real container cancellation:
//!
//! - **No merge execution.** No JUnit/coverage parsing
//!   (`cloud-ci-reports`), no generated `<id>/merge` container node, no
//!   `post-run-analysis` Queue consumer. `handle_shard_terminal` returns
//!   [`ShardBarrierDecision::Satisfied`] (`merge`/`included_idxs`) as a
//!   pure decision; nothing dispatches it. That is a separate, later
//!   round, same "foundation ahead of its full caller" pattern as the
//!   Nodes section above.
//! - **No real cancellation wiring.** `fail_fast`'s
//!   [`ShardBarrierDecision::FailFastTriggered`] only returns the *set*
//!   of shard indices that should be cancelled
//!   ([`logic::evaluate_barrier`]'s doc comment explains why: shards are
//!   not yet modeled as `node` rows this round — there is no RPC to
//!   register "shard N started running" — so there is no real container
//!   handle for `handle_shard_terminal` to hand to
//!   [`RunCoordinator::stop_node_container`]/[`RunCoordinator::handle_cancel_run`]
//!   yet). A future round that dispatches shards as real nodes can wire
//!   this decision straight into that existing cancellation path.
//! - **Idempotency** matches [`logic::resolve_complete_node`]'s exact
//!   discipline, applied to shards: [`logic::resolve_shard_terminal`]
//!   treats a redelivered call with the same terminal status as a no-op
//!   that never re-evaluates the barrier, and a redelivered call with a
//!   *different* terminal status as a 409 conflict. A late-finishing
//!   shard after its group already reached a terminal decision
//!   (`job_group.status` no longer `running`) resolves to
//!   [`ShardBarrierDecision::GroupAlreadyTerminal`] instead of
//!   re-evaluating the barrier a second time.
//! - **Storage.** `job_group`/`shard_state` (see [`ensure_schema`]) are
//!   DO-local only, keyed by `job_name` and `(job_name, idx, attempt)`
//!   respectively — this DO instance *is* one run, same "run_id
//!   implicit" pattern as every other table here. `shard_state` has a D1
//!   projection (`shard_states`, migration 0014); `job_group` does not —
//!   migration 0014's own comment explains why (no reader yet, same
//!   reasoning migration 0011 used to exclude `report_summaries`/
//!   `test_failures`).

pub mod logic;

use crate::github_checks;
use buffa::Enumeration;
use cloud_ci_proto::ingest::v1::{
    BeginRunRequest, CompleteShardRequest, CompleteUploadRequest, Conclusion, CreateUploadRequest,
    JobState, RunStatus, StartJobRequest, SubmitReportRequest, Trigger, UploadKind,
    submit_report_request,
};
use logic::RunState;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::wasm_bindgen::JsValue;
use worker::{
    DurableObject, Env, Method, Request, RequestInit, Response, SqlStorage, SqlStorageValue, State,
    Stub, durable_object,
};

/// Durable Object binding name; must match `durable_objects.bindings[].name`
/// in `wrangler.toml`.
pub const RUN_COORDINATOR_BINDING: &str = "RUN_COORDINATOR";

/// Stub URLs need an absolute form. Durable Object routing ignores the host,
/// so this is a label, not a hostname anyone resolves.
const STUB_ORIGIN: &str = "https://run-coordinator.cloud-ci.internal";

/// Deterministic Durable Object name for a run, derived from the same tuple
/// that uniquely identifies it everywhere else (architecture.md's "Run
/// identity"). Not the run's own ULID: `StartJob`/`GetRun` sometimes only
/// have one or the other, and this is the one every call can always derive.
///
/// `sha` and `run_key` are free-form, caller-supplied text for external
/// (BYO CI) runs (docs/design/byo-ci.md's "Run identity" table), so a plain
/// colon-joined string is not injective: `(1, "a", "b:1", 2)` and
/// `(1, "a:b", "1", 2)` would both join to `"1:a:b:1:2"`, letting two
/// unrelated callers collide on one `RunCoordinator` instance. Hashing each
/// field length-prefixed (so the hashed byte sequence is unambiguous
/// regardless of what bytes `sha`/`run_key` contain, unlike escaping, which
/// is easy to get wrong — e.g. forgetting to also escape the escape
/// character) makes this injective by construction. Reuses `sha2::Sha256`
/// (already a dependency for ingest-token signing) rather than adding one.
pub fn do_name(repo_id: u64, sha: &str, run_key: &str, attempt: u32) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(repo_id.to_be_bytes());
    for field in [sha, run_key] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    hasher.update(attempt.to_be_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // `write!` to a `String` cannot fail; `let _ =` discards the `Result`
        // without `unwrap`/`expect`, per this package's lint rules.
        let _ = write!(out, "{b:02x}");
    }
    out
}

// ---------------------------------------------------------------------------
// Wire types (internal JSON between Worker and DO)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeginRunOutcome {
    pub run_id: String,
    pub status: RunStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartJobOutcome {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRunOutcome {
    pub run_id: String,
    pub status: RunStatus,
    pub jobs: Vec<JobState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUploadOutcome {
    pub upload_id: String,
    pub part_count: u32,
    pub part_size_bytes: u64,
    pub already_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteUploadOutcome {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitReportOutcome {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteShardOutcome {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseRunOutcome {
    pub run_id: String,
    pub status: RunStatus,
}

/// `startNode(run_id, node_id, spec_hash, image, command)`'s request
/// (module docs' Nodes section). `check_name` is carried through to the
/// `node` row for the future caller's own bookkeeping — this round
/// never creates a Check Run from it.
///
/// **`image`/`command`.** Every node this round is a real container
/// node (module docs' Nodes section): `image` is the
/// `durable_object`-scheduling-policy image reference `NodeContainer`
/// passes to `ContainerStartupOptions::set_image` (`node_container.rs`),
/// and `command` is the executable followed by its arguments, matching
/// `Container::exec`'s own `cmd: &[&str]` shape exactly — no shell is
/// started, so shell syntax is never interpreted.
///
/// **`spec_hash` stays caller-supplied, not derived from `image`/
/// `command`.** `resolve_start_node`'s nondeterministic-replay check
/// exists to catch a retried `step.do("start:" + id)` whose *entire*
/// spec changed underneath it (dynamic-pipelines.md: "the SDK rejects a
/// duplicate id at the call site" for the easy case; this is the harder
/// "script took a different branch on replay" case) — the eventual
/// `@cloud-ci/pipeline-sdk` caller's own spec can include fields this
/// round's RPC does not carry at all yet (secrets requested, runner
/// size, sidecars). Deriving `spec_hash` here from only `image`/
/// `command` would silently narrow that check to a subset of what
/// "spec" means, missing a real replay divergence in any field this
/// struct doesn't carry. The caller computing and supplying its own
/// hash over its full spec is the semantics `resolve_start_node`'s doc
/// comment already assumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartNodeRequest {
    pub node_id: String,
    pub spec_hash: String,
    pub image: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub check_name: Option<String>,
}

/// `started: true` only on the fresh-row path
/// ([`logic::StartNodeDecision::Started`]); `false` on the idempotent
/// replay path. `status` is always the node's current status after this
/// call, same either way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartNodeOutcome {
    pub node_id: String,
    pub started: bool,
    pub status: String,
}

/// `completeNode(run_id, node_id, status, result)`'s request. `status`
/// must already be one of [`logic::NodeState::is_terminal`]'s terminal
/// db-string values; `result` is the opaque, caller-supplied result
/// payload (module docs: "a plain result object"), stored but never
/// interpreted by this round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteNodeRequest {
    pub node_id: String,
    pub status: String,
    #[serde(default)]
    pub result: Option<String>,
}

/// `dropped: true` on [`logic::CompleteNodeDecision::DroppedCancelled`]
/// — `status` then reports the node's actual (unchanged, `Cancelled`)
/// status, not the status the completion event claimed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteNodeOutcome {
    pub node_id: String,
    pub status: String,
    pub dropped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckNodeRequest {
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckNodeOutcome {
    pub node_id: String,
    pub acked: bool,
}

/// `handle_cancel_run`'s minimal hook (module docs' "Run cancellation").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRunOutcome {
    pub run_id: String,
    pub cancelled_nodes: Vec<String>,
}

/// `registerShardGroup(job_name, expected_total, fail_fast,
/// merge_on_failure)` — module docs' "Shard groups / merge barrier"
/// section. `merge_on_failure` must be one of
/// [`logic::MergeOnFailure::from_db_str`]'s three values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterShardGroupRequest {
    pub job_name: String,
    pub expected_total: u32,
    pub fail_fast: bool,
    pub merge_on_failure: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterShardGroupOutcome {
    pub job_name: String,
}

/// `shardTerminal(job_name, idx, attempt, status, report_key,
/// duration_ms)` — one shard's terminal ingest call (module docs' "Shard
/// groups / merge barrier" section). `status` must be one of
/// [`logic::ShardTerminalStatus::from_db_str`]'s two terminal values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardTerminalRequest {
    pub job_name: String,
    pub idx: u32,
    pub attempt: u32,
    pub status: String,
    pub report_key: Option<String>,
    pub duration_ms: Option<i64>,
}

/// The barrier decision produced by a `shardTerminal` call, per
/// [`logic::BarrierOutcome`] plus the duplicate-redelivery and
/// already-terminal-group cases that never reach `evaluate_barrier` at
/// all. **Decision only** — see `coordinator` module docs' "Shard groups
/// / merge barrier" section for exactly what is and is not wired from
/// here: `FailFastTriggered.cancel_idxs` is not stopped as a real
/// container, and `Satisfied` does not dispatch any real merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShardBarrierDecision {
    /// This exact `(job_name, idx, attempt)` terminal status was already
    /// recorded by an earlier call — the barrier was not re-evaluated.
    Duplicate,
    /// The group already reached a terminal decision
    /// (`FailFastTriggered`/`Satisfied`) before this call arrived — a
    /// late-finishing shard after the group already decided. The shard's
    /// own row is still recorded, but the barrier is not re-evaluated.
    GroupAlreadyTerminal,
    Waiting,
    FailFastTriggered {
        cancel_idxs: Vec<u32>,
    },
    Satisfied {
        merge: bool,
        included_idxs: Vec<u32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardTerminalOutcome {
    pub job_name: String,
    pub idx: u32,
    pub attempt: u32,
    pub decision: ShardBarrierDecision,
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    error: String,
    /// Set only for errors a caller must distinguish by kind, not just
    /// HTTP status — currently just `"nondeterministic_replay"` (see
    /// [`CoordinatorError::NondeterministicReplay`]). `#[serde(default)]`
    /// so every other `error_response` call (which never sets it) still
    /// deserializes cleanly on the client side.
    #[serde(default)]
    code: Option<String>,
}

// ---------------------------------------------------------------------------
// Durable Object storage rows
// ---------------------------------------------------------------------------

/// The DO's single `run` row, including the identity columns D1's
/// projection needs. There is exactly one of these per DO instance.
#[derive(Debug, Clone, Deserialize)]
struct RunRow {
    id: String,
    repo_id: i64,
    sha: String,
    run_key: String,
    attempt: i64,
    status: String,
    expect_jobs: Option<String>,
    trigger: String,
    external_url: String,
    created_at: i64,
    timeout_s: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct JobRow {
    id: String,
    job_name: String,
    shard_total: i64,
    runner_label: String,
    check_names: String,
    state: String,
    conclusion: Option<String>,
    conclusion_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct JobShardRow {
    job_id: String,
    shard_index: i64,
    state: String,
    conclusion: Option<String>,
    external_url: String,
    completed_at: Option<i64>,
}

/// One Check Run this run has created (`check_run` table — see
/// `coordinator` module docs' storage design). Keyed by `check_name`,
/// unique within this DO instance (one run): `(run, check_name)`
/// first-seen-wins per byo-ci.md's "Checks and scopes".
#[derive(Debug, Clone, Deserialize)]
struct CheckRunRow {
    check_name: String,
    github_check_run_id: i64,
    status: String,
    conclusion: Option<String>,
    created_at: i64,
}

/// One node this run's (future) Dynamic Workflow has started, keyed by
/// `node_id` alone — this DO instance *is* one run, same "run_id
/// implicit" pattern as `JobRow`/`JobShardRow` (module docs' Nodes
/// section).
#[derive(Debug, Clone, Deserialize)]
struct NodeRow {
    node_id: String,
    spec_hash: String,
    status: String,
    check_name: Option<String>,
    result: Option<String>,
    started_at: i64,
    completed_at: Option<i64>,
    acked: i64,
    image: String,
    /// JSON-encoded `Vec<String>` (`StartNodeRequest.command`'s on-disk
    /// form, matching `check_names`' existing JSON-column convention).
    command: String,
}

/// One shard group's `job_group` row, keyed by `job_name` alone — this DO
/// instance *is* one run, same "run_id implicit" pattern as every other
/// DO-local table (module docs' "Shard groups / merge barrier" section).
/// `status` is this implementation's own addition beyond
/// parallelization.md's literal `job_group` columns (`job_name`,
/// `expected_total`, `fail_fast`, `merge_on_failure`, `merge_job_id`): it
/// is what makes a redelivered/late shard-terminal call after the group
/// already decided (`failed` via fail-fast, or `satisfied` via the normal
/// barrier) resolve to [`ShardBarrierDecision::GroupAlreadyTerminal`]
/// instead of re-evaluating the barrier a second time — the same role
/// `node.status`/`run.status` already play for their own idempotency
/// guards elsewhere in this file.
#[derive(Debug, Clone, Deserialize)]
struct ShardGroupRow {
    /// Only used to populate the `SELECT` column binding; the caller
    /// already knows the `job_name` it queried by.
    #[allow(dead_code)]
    job_name: String,
    expected_total: i64,
    fail_fast: i64,
    merge_on_failure: String,
    /// Reserved for the later merge-execution round that actually
    /// dispatches the generated `<id>/merge` node or Queue consumer job
    /// (module docs' scope boundary) — never set or read this round.
    #[allow(dead_code)]
    merge_job_id: Option<String>,
    /// `running | failed | satisfied`.
    status: String,
}

/// One shard's terminal row, keyed by `(job_name, idx, attempt)` — one
/// row per terminal ingest call, never for a shard still
/// `queued`/`running` (this round has no RPC to register those; see
/// module docs' scope boundary).
#[derive(Debug, Clone, Deserialize)]
struct ShardStateDbRow {
    idx: i64,
    attempt: i64,
    status: String,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadRow {
    id: String,
    job_id: String,
    shard_index: i64,
    kind: String,
    name: String,
    scope: String,
    sha256: String,
    size_bytes: i64,
    content_type: String,
    state: String,
    r2_key: String,
    accepted_seq: i64,
    created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct ReportRow {
    id: String,
    job_id: String,
    shard_index: i64,
    kind: String,
    name: String,
    scope: String,
    content_sha256: String,
    upload_id: Option<String>,
    accepted_seq: i64,
    created_at: i64,
    is_canonical: i64,
    parsed: i64,
    summary: Option<String>,
    /// Full parsed report's R2 location (migration 0012's `reports.r2_key`
    /// column; see `handle_submit_report`'s module docs for why every
    /// report, not just upload-backed ones, needs this).
    r2_key: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MaxSeqRow {
    m: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct SummaryRow {
    summary: Option<String>,
}

/// `refresh_flakiness_scores`' per-row D1 read shape.
#[derive(Debug, Clone, Deserialize)]
struct RecentOutcomesRow {
    recent_outcomes: String,
}

fn trigger_db_name(trigger: Trigger) -> &'static str {
    trigger.proto_name()
}

fn upload_kind_db_name(kind: UploadKind) -> &'static str {
    kind.proto_name()
}

fn conclusion_db_name(c: Conclusion) -> &'static str {
    c.proto_name()
}

fn conclusion_from_db_str(s: &str) -> Option<Conclusion> {
    Conclusion::from_proto_name(s)
}

fn shard_state_of(row: &JobShardRow) -> worker::Result<logic::ShardState> {
    match row.state.as_str() {
        "pending" => Ok(logic::ShardState::Pending),
        "uploaded" => Ok(logic::ShardState::Uploaded),
        "missing" => Ok(logic::ShardState::Missing),
        other => Err(worker::Error::RustError(format!(
            "unknown job_shard state {other}"
        ))),
    }
}

fn node_state_of(row: &NodeRow) -> worker::Result<logic::NodeState> {
    logic::NodeState::from_db_str(&row.status)
        .ok_or_else(|| worker::Error::RustError(format!("unknown node state {}", row.status)))
}

fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut out = String::with_capacity(64);
    for b in hasher.finalize() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

// ---------------------------------------------------------------------------
// The Durable Object
// ---------------------------------------------------------------------------

#[durable_object(alarm)]
pub struct RunCoordinator {
    state: State,
    env: Env,
}

impl DurableObject for RunCoordinator {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> worker::Result<Response> {
        let sql = self.state.storage().sql();
        ensure_schema(&sql)?;

        match (req.method(), req.path().as_str()) {
            (Method::Post, "/begin-run") => {
                let body: BeginRunRequest = req.json().await?;
                self.handle_begin_run(&sql, body).await
            }
            (Method::Post, "/start-job") => {
                let body: StartJobRequest = req.json().await?;
                self.handle_start_job(&sql, body).await
            }
            (Method::Get, "/get-run") => self.handle_get_run(&sql).await,
            (Method::Post, "/create-upload") => {
                let body: CreateUploadRequest = req.json().await?;
                self.handle_create_upload(&sql, body).await
            }
            (Method::Post, "/complete-upload") => {
                let body: CompleteUploadRequest = req.json().await?;
                self.handle_complete_upload(&sql, body).await
            }
            (Method::Post, "/submit-report") => {
                let body: SubmitReportRequest = req.json().await?;
                self.handle_submit_report(&sql, body).await
            }
            (Method::Post, "/complete-shard") => {
                let body: CompleteShardRequest = req.json().await?;
                self.handle_complete_shard(&sql, body).await
            }
            (Method::Post, "/close-run") => self.handle_close_run(&sql, false).await,
            (Method::Post, "/start-node") => {
                let body: StartNodeRequest = req.json().await?;
                self.handle_start_node(&sql, body).await
            }
            (Method::Post, "/complete-node") => {
                let body: CompleteNodeRequest = req.json().await?;
                self.handle_complete_node(&sql, body).await
            }
            (Method::Post, "/ack-node") => {
                let body: AckNodeRequest = req.json().await?;
                self.handle_ack_node(&sql, body).await
            }
            (Method::Post, "/cancel-run") => self.handle_cancel_run(&sql).await,
            (Method::Post, "/register-shard-group") => {
                let body: RegisterShardGroupRequest = req.json().await?;
                self.handle_register_shard_group(&sql, body).await
            }
            (Method::Post, "/shard-terminal") => {
                let body: ShardTerminalRequest = req.json().await?;
                self.handle_shard_terminal(&sql, body).await
            }
            _ => error_response(404, "unknown RunCoordinator route"),
        }
    }

    /// Fires when the DO alarm set by `handle_begin_run` (`now +
    /// timeout_s`) reaches its scheduled time — byo-ci.md's third
    /// completion trigger (§ Completion semantics' "Timeout" bullet).
    /// Reuses `handle_close_run`'s own terminal-state no-op guard for
    /// idempotency: if the run already closed via the webhook or
    /// `--expect-jobs` in the meantime (including microseconds before
    /// this alarm fired), this is a clean no-op, not a race. Passes
    /// `by_timeout: true`, which forces the run's state to `Abandoned`
    /// unconditionally rather than computing it from the jobs'
    /// conclusions (see `logic::run_state_for_close`'s docs).
    async fn alarm(&self) -> worker::Result<Response> {
        let sql = self.state.storage().sql();
        ensure_schema(&sql)?;
        self.handle_close_run(&sql, true).await
    }
}

impl RunCoordinator {
    async fn handle_begin_run(
        &self,
        sql: &SqlStorage,
        req: BeginRunRequest,
    ) -> worker::Result<Response> {
        let trigger_name = trigger_db_name(req.trigger.as_known().unwrap_or_default());
        let existing = read_run(sql)?;

        let run_row = match existing {
            None => {
                let now_ms = worker::Date::now().as_millis();
                let run_id = crate::ulid::generate(now_ms).map_err(|e| {
                    worker::Error::RustError(format!("ulid generation failed: {e}"))
                })?;
                let expect_jobs = if req.expect_jobs.is_empty() {
                    None
                } else {
                    Some(req.expect_jobs.clone())
                };
                let timeout_s = logic::resolve_timeout_seconds(req.timeout.seconds);
                insert_run(
                    sql,
                    &run_id,
                    req.key.repo_id,
                    &req.key.sha,
                    &req.key.run_key,
                    req.key.attempt,
                    RunState::Queued,
                    expect_jobs.as_deref(),
                    trigger_name,
                    &req.external_url,
                    now_ms as i64,
                    timeout_s,
                )?;
                // DO alarm for `now + timeout_s`, byo-ci.md's third close
                // trigger (§ Completion semantics' "Timeout" bullet). Only
                // set the first time a run is created — this `None` arm
                // runs once per `(repo_id, sha, run_key, attempt)`, since a
                // redelivered/retried `BeginRun` for the same run always
                // lands in the `Some(row)` arm below (`do_name` is
                // deterministic), so a retry never pushes the timeout out
                // or sets a second alarm.
                self.state
                    .storage()
                    .set_alarm(timeout_s.saturating_mul(1000))
                    .await?;
                require_run(sql)?
            }
            Some(row) => {
                let current_expect_jobs = row
                    .expect_jobs
                    .as_deref()
                    .map(decode_string_list)
                    .transpose()?;
                match logic::resolve_expect_jobs(current_expect_jobs.as_deref(), &req.expect_jobs) {
                    Ok(Some(new_list)) => update_expect_jobs(sql, &row.id, &new_list)?,
                    Ok(None) => {}
                    Err(_) => {
                        return error_response(
                            409,
                            "expect_jobs conflicts with the value already set for this run",
                        );
                    }
                }
                update_trigger_and_url(sql, &row.id, trigger_name, &req.external_url)?;
                require_run(sql)?
            }
        };

        self.project_run_to_d1(&run_row).await?;

        let status = run_status_of(&run_row)?;
        Response::from_json(&BeginRunOutcome {
            run_id: run_row.id,
            status,
        })
    }

    async fn handle_start_job(
        &self,
        sql: &SqlStorage,
        req: StartJobRequest,
    ) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };

        let existing_job = read_job(sql, &req.job_name)?;
        let resolved_total = match logic::resolve_shard_total(
            existing_job.as_ref().map(|j| j.shard_total as u32),
            req.shard_total,
        ) {
            Ok(total) => total,
            Err(_) => {
                return error_response(
                    409,
                    "shard_total conflicts with the value already set for this job",
                );
            }
        };

        let check_names_json = serde_json::to_string(&req.check_names)
            .map_err(|e| worker::Error::RustError(format!("cannot encode check_names: {e}")))?;

        let job_id = match &existing_job {
            Some(job) => {
                update_job(
                    sql,
                    &job.id,
                    resolved_total,
                    &req.runner_label,
                    &check_names_json,
                )?;
                job.id.clone()
            }
            None => {
                let now_ms = worker::Date::now().as_millis();
                let job_id = crate::ulid::generate(now_ms).map_err(|e| {
                    worker::Error::RustError(format!("ulid generation failed: {e}"))
                })?;
                insert_job(
                    sql,
                    &job_id,
                    &req.job_name,
                    resolved_total,
                    &req.runner_label,
                    &check_names_json,
                )?;
                job_id
            }
        };

        let current_state = run_state_of(&run_row)?;
        let next_state = logic::run_state_after_start_job(current_state);
        if next_state != current_state {
            update_run_status(sql, &run_row.id, next_state)?;
        }

        let job_row = read_job(sql, &req.job_name)?
            .ok_or_else(|| worker::Error::RustError("job row missing after upsert".into()))?;
        let run_row = require_run(sql)?;

        self.project_run_to_d1(&run_row).await?;
        self.project_job_to_d1(&run_row.id, &job_row).await?;
        self.create_check_runs_for_job(sql, &run_row, &req.check_names)
            .await?;
        self.maybe_close_for_expect_jobs(sql, &run_row).await?;

        Response::from_json(&StartJobOutcome { job_id })
    }

    async fn handle_get_run(&self, sql: &SqlStorage) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        let job_rows = read_all_jobs(sql)?;
        let status = run_status_of(&run_row)?;
        let jobs = job_rows
            .into_iter()
            .map(|j| JobState {
                job_id: j.id,
                job_name: j.job_name,
                shard_total: j.shard_total as u32,
                // No shard has ever been marked complete this round:
                // `CompleteShard` is out of scope (see module docs), so this
                // is always empty rather than a guess.
                completed_shards: Vec::new(),
                ..Default::default()
            })
            .collect();
        Response::from_json(&GetRunOutcome {
            run_id: run_row.id,
            status,
            jobs,
        })
    }

    async fn handle_create_upload(
        &self,
        sql: &SqlStorage,
        req: CreateUploadRequest,
    ) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        let run_terminal = run_state_of(&run_row)?.is_terminal();

        let shard_state = match read_job_shard(sql, &req.job_id, req.shard_index)? {
            Some(row) => shard_state_of(&row)?,
            None => logic::ShardState::Pending,
        };
        if !logic::upload_allowed_for_shard(run_terminal, shard_state) {
            return error_response(409, "shard is missing, or the run is already terminal");
        }

        let kind_name = req
            .kind
            .as_known()
            .map(upload_kind_db_name)
            .unwrap_or("UPLOAD_KIND_UNSPECIFIED");
        let existing = read_upload_by_identity(
            sql,
            &req.job_id,
            req.shard_index,
            kind_name,
            &req.name,
            &req.sha256,
        )?;
        let existing_for_logic = existing.as_ref().map(|u| logic::ExistingUpload {
            upload_id: u.id.clone(),
            state: if u.state == "complete" {
                logic::UploadState::Complete
            } else {
                logic::UploadState::Pending
            },
            scope: u.scope.clone(),
        });

        match logic::resolve_create_upload(req.size_bytes, &req.scope, existing_for_logic.as_ref())
        {
            Err(logic::CreateUploadError::TooLarge(_)) => error_response(
                409,
                "upload exceeds the 32 MiB single-part limit; multipart is out of scope",
            ),
            Err(logic::CreateUploadError::ScopeConflict) => error_response(
                409,
                "this content was already accepted under a different scope",
            ),
            Ok(logic::CreateUploadDecision::ReturnExisting {
                upload_id,
                already_complete,
            }) => Response::from_json(&CreateUploadOutcome {
                upload_id,
                part_count: 0,
                part_size_bytes: logic::MAX_SINGLE_PART_BYTES,
                already_complete,
            }),
            Ok(logic::CreateUploadDecision::CreateNew) => {
                let now_ms = worker::Date::now().as_millis();
                let upload_id = crate::ulid::generate(now_ms).map_err(|e| {
                    worker::Error::RustError(format!("ulid generation failed: {e}"))
                })?;
                let accepted_seq = next_accepted_seq(sql)?;
                // Immutable, content-addressed location keyed by this
                // upload's own identity (byo-ci.md's Idempotency section).
                // The documented "publication alias" that gets rewritten as
                // canonical content advances is a deferred nice-to-have —
                // see module docs — since nothing reads it yet.
                let r2_key = format!("runs/{}/uploads/{upload_id}/{}", run_row.id, req.sha256);
                insert_upload(
                    sql,
                    &upload_id,
                    &req.job_id,
                    req.shard_index,
                    kind_name,
                    &req.name,
                    &req.scope,
                    &req.sha256,
                    req.size_bytes as i64,
                    &req.content_type,
                    &r2_key,
                    accepted_seq,
                    now_ms as i64,
                )?;
                self.project_upload_to_d1(&require_upload(sql, &upload_id)?)
                    .await?;
                Response::from_json(&CreateUploadOutcome {
                    upload_id,
                    part_count: 1,
                    part_size_bytes: logic::MAX_SINGLE_PART_BYTES,
                    already_complete: false,
                })
            }
        }
    }

    async fn handle_complete_upload(
        &self,
        sql: &SqlStorage,
        req: CompleteUploadRequest,
    ) -> worker::Result<Response> {
        let Some(upload) = read_upload(sql, &req.upload_id)? else {
            return error_response(404, "upload not found");
        };
        let part_numbers: Vec<u32> = req.parts.iter().map(|p| p.number).collect();
        if logic::validate_complete_upload_parts(&part_numbers).is_err() {
            return error_response(
                409,
                "parts do not match the single part CreateUpload reserved",
            );
        }
        let bucket = self.env.bucket("ASSETS")?;
        if bucket.head(&upload.r2_key).await?.is_none() {
            return error_response(409, "no part was uploaded for this upload_id");
        }
        if upload.state != "complete" {
            update_upload_state(sql, &upload.id, "complete")?;
            self.project_upload_state_to_d1(&upload.id, "complete")
                .await?;
        }
        Response::from_json(&CompleteUploadOutcome {})
    }

    async fn handle_submit_report(
        &self,
        sql: &SqlStorage,
        req: SubmitReportRequest,
    ) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        let run_terminal = run_state_of(&run_row)?.is_terminal();
        let shard_state = match read_job_shard(sql, &req.job_id, req.shard_index)? {
            Some(row) => shard_state_of(&row)?,
            None => logic::ShardState::Pending,
        };
        if !logic::upload_allowed_for_shard(run_terminal, shard_state) {
            return error_response(409, "shard is missing, or the run is already terminal");
        }

        let (content_sha256, upload_id, bytes, report_r2_key) = match &req.source {
            Some(submit_report_request::Source::UploadId(id)) => {
                let Some(upload) = read_upload(sql, id)? else {
                    return error_response(404, "upload not found");
                };
                let bucket = self.env.bucket("ASSETS")?;
                let Some(object) = bucket.get(&upload.r2_key).execute().await? else {
                    return Err(worker::Error::RustError(format!(
                        "upload {id} has no object at its own r2_key"
                    )));
                };
                let Some(body) = object.body() else {
                    return Err(worker::Error::RustError(format!(
                        "upload {id}'s R2 object has no body"
                    )));
                };
                let bytes = body.bytes().await?;
                if hex_sha256(&bytes) != upload.sha256 {
                    return error_response(
                        500,
                        "stored upload bytes do not match their declared sha256",
                    );
                }
                // Upload-backed: the bytes already live in R2 at the
                // upload's own key (migration 0002); reuse it rather than
                // writing a second copy (`test_stats` finalization's R2
                // decision — see migration 0012).
                let r2_key = upload.r2_key.clone();
                (
                    upload.sha256.clone(),
                    Some(upload.id.clone()),
                    bytes,
                    r2_key,
                )
            }
            Some(submit_report_request::Source::InlineData(data)) => {
                let content_sha256 = hex_sha256(data);
                // Inline reports never otherwise touch R2 — without this
                // write, the bytes only ever exist in this request's
                // memory and `test_stats` finalization would have nothing
                // to re-parse at run close (migration 0012's module
                // comment). Content-addressed, mirroring uploads' own
                // `runs/<run_id>/uploads/<upload_id>/<sha256>` key shape.
                let r2_key = format!(
                    "runs/{}/reports/{}/{}/{}",
                    run_row.id, req.job_id, req.shard_index, content_sha256
                );
                let bucket = self.env.bucket("ASSETS")?;
                bucket.put(&r2_key, data.clone()).execute().await?;
                (content_sha256, None, data.clone(), r2_key)
            }
            None => {
                return error_response(400, "SubmitReport requires inline_data or upload_id");
            }
        };

        if read_report_by_identity(
            sql,
            &req.job_id,
            req.shard_index,
            &req.report_kind,
            &req.name,
            &content_sha256,
        )?
        .is_some()
        {
            // Identical content already accepted for this slot (a retried
            // call) — a no-op, per the Idempotency section.
            return Response::from_json(&SubmitReportOutcome {});
        }

        let (parsed, summary) = parse_report(&req.report_kind, &bytes);
        let now_ms = worker::Date::now().as_millis();
        let id = crate::ulid::generate(now_ms)
            .map_err(|e| worker::Error::RustError(format!("ulid generation failed: {e}")))?;
        let accepted_seq = next_accepted_seq(sql)?;
        insert_report(
            sql,
            &id,
            &req.job_id,
            req.shard_index,
            &req.report_kind,
            &req.name,
            &req.scope,
            &content_sha256,
            upload_id.as_deref(),
            accepted_seq,
            now_ms as i64,
            parsed,
            summary.as_deref(),
            &report_r2_key,
        )?;
        unset_other_canonical_reports(
            sql,
            &req.job_id,
            req.shard_index,
            &req.report_kind,
            &req.name,
            &id,
        )?;

        self.project_report_to_d1(&require_report(sql, &id)?)
            .await?;
        // The new row flipped any previously canonical row for this slot to
        // non-canonical in the DO's own storage above; mirror that into D1
        // too, since `project_report_to_d1` only inserts the new row.
        self.unset_other_canonical_reports_in_d1(
            &req.job_id,
            req.shard_index,
            &req.report_kind,
            &req.name,
            &id,
        )
        .await?;

        Response::from_json(&SubmitReportOutcome {})
    }

    async fn handle_complete_shard(
        &self,
        sql: &SqlStorage,
        req: CompleteShardRequest,
    ) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        let run_terminal = run_state_of(&run_row)?.is_terminal();
        let existing_shard = read_job_shard(sql, &req.job_id, req.shard_index)?;
        let shard_state = match &existing_shard {
            Some(row) => shard_state_of(row)?,
            None => logic::ShardState::Pending,
        };
        let existing_conclusion = match &existing_shard {
            Some(row) => match &row.conclusion {
                Some(c) => Some(conclusion_from_db_str(c).ok_or_else(|| {
                    worker::Error::RustError(format!("unknown stored conclusion {c}"))
                })?),
                None => None,
            },
            None => None,
        };

        let declared = req
            .conclusion
            .as_known()
            .unwrap_or(Conclusion::CONCLUSION_UNSPECIFIED);
        // `--conclusion` is optional on the CLI; an unspecified value is
        // inferred from the shard's accepted reports (byo-ci.md's
        // `cloud-ci upload` section: "failure if a report has a failed test
        // or an error-level diagnostic, else success").
        let incoming = if declared == Conclusion::CONCLUSION_UNSPECIFIED {
            infer_shard_conclusion(sql, &req.job_id, req.shard_index)?
        } else {
            declared
        };

        let resolved = match logic::resolve_complete_shard(
            run_terminal,
            shard_state,
            existing_conclusion,
            incoming,
        ) {
            Ok(c) => c,
            Err(logic::CompleteShardError::RunTerminal) => {
                return error_response(409, "run is already terminal");
            }
            Err(logic::CompleteShardError::ShardMissing) => {
                return error_response(409, "shard was already marked missing");
            }
            Err(logic::CompleteShardError::ConflictingConclusion) => {
                return error_response(409, "shard already concluded with a different conclusion");
            }
        };

        let now_ms = worker::Date::now().as_millis();
        upsert_job_shard(
            sql,
            &req.job_id,
            req.shard_index,
            "uploaded",
            conclusion_db_name(resolved),
            &req.external_url,
            now_ms as i64,
        )?;
        self.project_job_shard_to_d1(&require_job_shard(sql, &req.job_id, req.shard_index)?)
            .await?;

        // Conclude the job once every declared shard has uploaded. This
        // never advances the run's own status — run-level closing only
        // happens via `handle_close_run` below (the `workflow_run`
        // webhook signal this round; `--expect-jobs` counting and the
        // timeout alarm are separate, later signals, see the
        // `workflow_run` module docs).
        let Some(job_row) = read_job_by_id(sql, &req.job_id)? else {
            return error_response(404, "job not found");
        };
        let uploaded_shards = count_uploaded_shards(sql, &req.job_id)?;
        if uploaded_shards >= job_row.shard_total {
            let shard_conclusions = read_all_shard_conclusions(sql, &req.job_id)?;
            if let Some(job_conclusion) = logic::job_conclusion_from_shards(&shard_conclusions) {
                update_job_conclusion(
                    sql,
                    &req.job_id,
                    "concluded",
                    conclusion_db_name(job_conclusion),
                    None,
                )?;
                self.project_job_to_d1(&run_row.id, &require_job_by_id(sql, &req.job_id)?)
                    .await?;
            }
        }

        let check_names = decode_string_list(&job_row.check_names)?;
        self.update_check_runs_for_job(sql, &run_row, &check_names)
            .await?;
        self.maybe_close_for_expect_jobs(sql, &run_row).await?;

        Response::from_json(&CompleteShardOutcome {})
    }

    /// After `StartJob`/`CompleteShard` change a job's started/concluded
    /// state, checks whether the run's declared `expect_jobs` (named job
    /// list from `BeginRun`, see `logic::expect_jobs_satisfied`) are now
    /// all satisfied, and if so triggers the same close-run operation
    /// the `workflow_run.completed` webhook triggers
    /// ([`Self::handle_close_run`]) — a second, independent caller of
    /// that one close implementation, not a parallel computation of it.
    /// A run with no `expect_jobs` set is unaffected (never closes via
    /// this path, per `logic::expect_jobs_satisfied`'s docs).
    ///
    /// Idempotent for the same reason every other caller of
    /// `handle_close_run` is: that function's own terminal-state guard
    /// makes a redelivered/retried `StartJob`/`CompleteShard` call that
    /// re-observes "satisfied" a no-op, not a second close attempt.
    async fn maybe_close_for_expect_jobs(
        &self,
        sql: &SqlStorage,
        run_row: &RunRow,
    ) -> worker::Result<()> {
        if run_state_of(run_row)?.is_terminal() {
            return Ok(());
        }
        let expect_jobs = run_row
            .expect_jobs
            .as_deref()
            .map(decode_string_list)
            .transpose()?;
        if expect_jobs.as_deref().is_none_or(<[String]>::is_empty) {
            return Ok(());
        }
        let started: Vec<(String, bool)> = read_all_jobs(sql)?
            .into_iter()
            .map(|j| (j.job_name, j.state == "concluded"))
            .collect();
        if logic::expect_jobs_satisfied(expect_jobs.as_deref(), &started) {
            self.handle_close_run(sql, false).await?;
        }
        Ok(())
    }

    /// Closes the run: for each job, marks every shard that never
    /// uploaded `missing` and concludes the job (failing it if it has any
    /// missing shard), then rolls the jobs' conclusions up into the run's
    /// own and moves the run to a terminal `RunState`
    /// (docs/design/byo-ci.md's "Completion semantics": "the run's
    /// conclusion is the worst job conclusion"). Three callers trigger
    /// this: `lib.rs::handle_workflow_run_event`, once it has correlated
    /// an incoming `workflow_run` `completed` webhook to this run (see
    /// the `workflow_run` module docs for the correlation strategy);
    /// [`Self::maybe_close_for_expect_jobs`], once every job named in a
    /// run's declared `expect_jobs` has started and concluded; and
    /// [`DurableObject::alarm`], once the DO alarm `handle_begin_run` set
    /// fires. The first two pass `by_timeout: false` and let
    /// [`logic::run_state_for_close`] compute `succeeded`/`failed` from
    /// the worst job conclusion; the alarm path passes `by_timeout:
    /// true`, which forces `Abandoned` unconditionally, per byo-ci.md's
    /// Failure modes row for a never-uploaded shard: "otherwise the
    /// timeout marks it `missing` and the run moves to `abandoned`" —
    /// not a computed conclusion.
    ///
    /// The run/job/shard mutation block (marking shards missing,
    /// recomputing job/run conclusions, re-transitioning state, Check Run
    /// finalization) is idempotent by never re-running once the run is
    /// terminal: a redelivered webhook, a redelivered `--expect-jobs`
    /// satisfaction, or a DO alarm firing after the run already closed
    /// microseconds earlier all short-circuit that block.
    ///
    /// `finalize_test_stats` does NOT share that short-circuit, by
    /// design — see the terminal-state branch below. It has its own,
    /// separate D1-level idempotency gate (`test_stats_applications`'s
    /// PRIMARY KEY), cheap and safe to hit redundantly, so every call to
    /// this function — terminal or not — attempts it. Without this, a
    /// `finalize_test_stats` failure (a transient R2 read error, a
    /// transient D1 outage — anything other than the expected
    /// "already applied" PK-conflict no-op) occurring *after*
    /// `update_run_status`/`project_run_to_d1` already committed the
    /// run's terminal state would be unrecoverable: every subsequent
    /// close-trigger would hit the terminal guard and never attempt
    /// `finalize_test_stats` again, silently and permanently losing that
    /// run's `test_stats` contribution. Retrying it on every
    /// already-terminal call instead gives every redelivery/refire a
    /// free retry of exactly the failed work, at the cost of one cheap,
    /// no-op-shaped D1 round trip when it already succeeded.
    async fn handle_close_run(
        &self,
        sql: &SqlStorage,
        by_timeout: bool,
    ) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        if run_state_of(&run_row)?.is_terminal() {
            // Run/job/shard mutation is skipped (already applied, see
            // doc comment above) but `finalize_test_stats` still runs —
            // its own `test_stats_applications` marker makes this an
            // instant no-op if finalization already succeeded, and does
            // the previously-failed work if it didn't.
            self.finalize_test_stats(sql, &run_row).await?;
            let status = run_status_of(&run_row)?;
            return Response::from_json(&CloseRunOutcome {
                run_id: run_row.id,
                status,
            });
        }

        let now_ms = worker::Date::now().as_millis() as i64;
        let jobs = read_all_jobs(sql)?;
        let mut job_conclusions = Vec::with_capacity(jobs.len());
        for job in &jobs {
            let shard_rows = read_all_job_shards(sql, &job.id)?;
            let shards: Vec<logic::ShardRecord> = shard_rows
                .iter()
                .map(|r| {
                    let conclusion = match &r.conclusion {
                        Some(c) => Some(conclusion_from_db_str(c).ok_or_else(|| {
                            worker::Error::RustError(format!("unknown stored conclusion {c}"))
                        })?),
                        None => None,
                    };
                    Ok(logic::ShardRecord {
                        shard_index: r.shard_index as u32,
                        state: shard_state_of(r)?,
                        conclusion,
                    })
                })
                .collect::<worker::Result<Vec<_>>>()?;

            let decision = logic::close_job(job.shard_total as u32, &shards);

            for idx in &decision.newly_missing {
                mark_job_shard_missing(sql, &job.id, *idx, now_ms)?;
                self.project_job_shard_to_d1(&require_job_shard(sql, &job.id, *idx)?)
                    .await?;
            }

            update_job_conclusion(
                sql,
                &job.id,
                "concluded",
                conclusion_db_name(decision.conclusion),
                decision.summary.as_deref(),
            )?;
            self.project_job_to_d1(&run_row.id, &require_job_by_id(sql, &job.id)?)
                .await?;

            job_conclusions.push(decision.conclusion);
        }

        let run_conclusion = logic::run_conclusion_from_jobs(&job_conclusions);
        let next_state = logic::run_state_for_close(by_timeout, run_conclusion);
        update_run_status(sql, &run_row.id, next_state)?;
        // Cancels a still-pending timeout alarm once the run closes via
        // the webhook or `--expect-jobs`, so a completed run never has a
        // stale alarm fire later and no-op against an already-terminal
        // run. Harmless when `by_timeout` is true (the alarm that just
        // fired is already consumed) or when no alarm was ever set.
        self.state.storage().delete_alarm().await?;
        let run_row = require_run(sql)?;
        self.project_run_to_d1(&run_row).await?;
        self.finalize_check_runs(sql, &run_row).await?;
        self.finalize_test_stats(sql, &run_row).await?;

        let status = run_status_of(&run_row)?;
        Response::from_json(&CloseRunOutcome {
            run_id: run_row.id,
            status,
        })
    }

    /// Applies every canonical, parsed report's test outcomes to
    /// `test_stats`, exactly once per run (analytics.md's "D1 rollup
    /// tables" § "Once per run" and "Idempotency"). `handle_close_run`'s
    /// own terminal-state guard at its top is this call's only caller
    /// path, and that guard already makes a redelivered/retried close
    /// signal short-circuit before ever reaching this function again —
    /// but the `test_stats_applications` D1 marker below is still the
    /// authoritative gate, not that guard, per the doc's own framing
    /// ("D1's own `test_stats_applications` table remains as a backstop
    /// against ... replaying the same finalization `batch()`"): a future
    /// Queue-based redelivery path (this round does not build a queue —
    /// see module docs' scope boundary) would call this function directly
    /// without going through `handle_close_run`'s guard at all, and this
    /// function is safe under that case too.
    ///
    /// Default-branch scoping (parallelization.md: "`test_stats` ...
    /// scoped to default-branch runs") is deliberately **not** enforced
    /// here — see migration 0011's module comment for why (no branch
    /// tracking exists on `run` yet; filtering which runs call this is
    /// left to a future caller).
    async fn finalize_test_stats(&self, sql: &SqlStorage, run_row: &RunRow) -> worker::Result<()> {
        let reports = read_canonical_parsed_reports(sql)?;
        if reports.is_empty() {
            return Ok(());
        }

        let mut job_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // (job_name, report_type, report_name, scope, r2_key) per
        // canonical+parsed report — analytics.md's "frozen set": "for
        // every (job_name, report_type, report_name, scope) group any job
        // in the run produced, the current canonical content for that
        // group". `is_canonical = 1` already guarantees at most one row
        // per `(job_id, shard_index, kind, name)` slot; grouping by
        // `job_name` (not `job_id`) here matches the doc's own group key,
        // which does not include `shard_index`.
        let mut groups = Vec::with_capacity(reports.len());
        for report in &reports {
            let job_name = match job_names.get(&report.job_id) {
                Some(name) => name.clone(),
                None => {
                    let job = require_job_by_id(sql, &report.job_id)?;
                    job_names.insert(report.job_id.clone(), job.job_name.clone());
                    job.job_name
                }
            };
            groups.push((
                job_name,
                report.kind.clone(),
                report.name.clone(),
                report.scope.clone(),
                report.r2_key.clone(),
            ));
        }

        let bucket = self.env.bucket("ASSETS")?;
        let mut all_rows: Vec<logic::TestOutcomeRow> = Vec::new();
        for (_, kind, _, _, r2_key) in &groups {
            let Some(object) = bucket.get(r2_key).execute().await? else {
                return Err(worker::Error::RustError(format!(
                    "finalize_test_stats: report has no R2 object at its own r2_key {r2_key}"
                )));
            };
            let Some(body) = object.body() else {
                return Err(worker::Error::RustError(format!(
                    "finalize_test_stats: report R2 object {r2_key} has no body"
                )));
            };
            let bytes = body.bytes().await?;
            if let Some(rows) = parse_test_outcomes(kind, &bytes) {
                all_rows.extend(rows);
            }
        }
        let deduped = logic::dedupe_test_outcomes(all_rows);

        let now_ms = worker::Date::now().as_millis() as i64;
        let db = self.env.d1("DB")?;
        let mut statements = Vec::with_capacity(groups.len() + 1);
        for (job_name, kind, name, scope, _) in &groups {
            statements.push(
                db.prepare(
                    "INSERT INTO test_stats_applications \
                     (run_id, job_name, report_type, report_name, scope, applied_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .bind(&[
                    JsValue::from_str(&run_row.id),
                    JsValue::from_str(job_name),
                    JsValue::from_str(kind),
                    JsValue::from_str(name),
                    JsValue::from_str(scope),
                    JsValue::from_f64(now_ms as f64),
                ])?,
            );
        }

        if !deduped.is_empty() {
            let tests_json = serde_json::to_string(
                &deduped
                    .iter()
                    .map(|row| {
                        serde_json::json!({
                            "test_id": row.test_id,
                            "file_path": row.file_path,
                            "test_name": row.test_name,
                            "duration_ms": row.duration_ms,
                            "status": row.outcome.as_status_word(),
                            "outcome_char": row.outcome.as_char().to_string(),
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .map_err(|e| worker::Error::RustError(format!("encoding test_stats rows: {e}")))?;

            // Same `json_each`/`ON CONFLICT` upsert shape as analytics.md's
            // own documented SQL (one batched statement regardless of test
            // count, the `WHERE true` required for the same SQLite parser
            // reason the doc explains). This round's own, deliberate
            // deviation: `recent_outcomes`' per-run char and
            // `last_status`'s word are both carried pre-computed from Rust
            // (`outcome_char`/`status`) rather than derived in SQL from a
            // single `status` column — `TestOutcomeKind` already produces
            // both shapes from one parsed outcome, so there is nothing to
            // derive twice.
            statements.push(
                db.prepare(
                    "INSERT INTO test_stats (repo_id, test_id, file_path, test_name, runs, duration_ewma_ms, \
                     last_duration_ms, last_status, recent_outcomes, flakiness_score, last_run_id, last_sha, updated_at) \
                     SELECT :repo_id, value->>'test_id', value->>'file_path', value->>'test_name', 1, \
                            value->>'duration_ms', value->>'duration_ms', value->>'status', \
                            value->>'outcome_char', 0, :run_id, :sha, :now \
                     FROM json_each(:tests) \
                     WHERE true \
                     ON CONFLICT (repo_id, test_id) DO UPDATE SET \
                       runs             = test_stats.runs + 1, \
                       duration_ewma_ms = 0.2 * excluded.last_duration_ms + 0.8 * test_stats.duration_ewma_ms, \
                       last_duration_ms = excluded.last_duration_ms, \
                       last_status      = excluded.last_status, \
                       recent_outcomes  = substr(test_stats.recent_outcomes || excluded.recent_outcomes, -20), \
                       last_run_id      = excluded.last_run_id, \
                       last_sha         = excluded.last_sha, \
                       updated_at       = excluded.updated_at",
                )
                .bind(&[
                    JsValue::from_f64(run_row.repo_id as f64),
                    JsValue::from_str(&run_row.id),
                    JsValue::from_str(&run_row.sha),
                    JsValue::from_f64(now_ms as f64),
                    JsValue::from_str(&tests_json),
                ])?,
            );
        }

        if let Err(e) = db.batch(statements).await {
            let msg = e.to_string();
            // analytics.md's Idempotency: a redelivered finalization batch
            // hits `test_stats_applications`' PRIMARY KEY, D1 rolls back
            // the whole transaction, and "a caller that gets this failure
            // treats the whole batch as 'already applied,' not an error to
            // retry".
            if msg.contains("UNIQUE constraint failed")
                || msg.contains("PRIMARY KEY constraint failed")
            {
                return Ok(());
            }
            return Err(e);
        }

        self.refresh_flakiness_scores(run_row.repo_id, &deduped)
            .await
    }

    /// Recomputes `flakiness_score` for every `test_id` this run's
    /// finalization just touched. Deliberately **not** part of the
    /// idempotency-gated `batch()` above: analytics.md says `flakiness_score`
    /// "is deliberately not computed inline ... the `*/15` cron
    /// recomputes it ... off the request path" — this round builds no
    /// cron (module docs' scope boundary), so until one exists this is a
    /// best-effort inline refresh rather than a permanently-zero column.
    /// It is safe to run redundantly or to fail silently-ish (logged, not
    /// propagated) because it is a pure function of `test_stats`'
    /// already-durable `recent_outcomes`, the same "recompute from current
    /// state, always converges" property the doc uses to justify
    /// `report_summaries`' own recompute-wholesale semantics — recomputing
    /// it twice, or a run apart from when `recent_outcomes` changed,
    /// always yields the same answer as the current string, never a
    /// double-count the way `runs`/`duration_ewma_ms` would double-count
    /// under a non-gated retry.
    async fn refresh_flakiness_scores(
        &self,
        repo_id: i64,
        touched: &[logic::TestOutcomeRow],
    ) -> worker::Result<()> {
        if touched.is_empty() {
            return Ok(());
        }
        let db = self.env.d1("DB")?;
        let mut updates = Vec::with_capacity(touched.len());
        for row in touched {
            let Some(current) = db
                .prepare(
                    "SELECT recent_outcomes FROM test_stats WHERE repo_id = ?1 AND test_id = ?2",
                )
                .bind(&[
                    JsValue::from_f64(repo_id as f64),
                    JsValue::from_str(&row.test_id),
                ])?
                .first::<RecentOutcomesRow>(None)
                .await?
            else {
                continue;
            };
            let score = logic::flakiness_score(&current.recent_outcomes);
            updates.push(
                db.prepare("UPDATE test_stats SET flakiness_score = ?1 WHERE repo_id = ?2 AND test_id = ?3")
                    .bind(&[
                        JsValue::from_f64(score),
                        JsValue::from_f64(repo_id as f64),
                        JsValue::from_str(&row.test_id),
                    ])?,
            );
        }
        if updates.is_empty() {
            return Ok(());
        }
        db.batch(updates).await?;
        Ok(())
    }

    /// `startNode(run_id, node_id, spec_hash, image, command)` — module
    /// docs' Nodes section, dynamic-pipelines.md's idempotency table's
    /// first two rows. The node row is created first (so a crash after
    /// the row write but before the container call still leaves a
    /// `running` row a redelivered `startNode` can find via
    /// `StartNodeDecision::AlreadyStarted` — that guard is what stops a
    /// redelivery from ever calling [`Self::start_node_container`] a
    /// second time, not a check here), then the real container is
    /// started via a DO-to-DO call into `NodeContainer`
    /// (`node_container.rs`). A synchronous container-start failure (bad
    /// image, scheduling rejection — see that module's `handle_start`)
    /// is mapped straight to this node's own `failed` status via the
    /// same [`update_node_status`] `handle_complete_node` itself uses,
    /// never surfaced as an unhandled 500: the caller's `startNode`
    /// still returns 200, with `status: "failed"` and a `result`
    /// explaining why, exactly like any other terminal node outcome.
    async fn handle_start_node(
        &self,
        sql: &SqlStorage,
        req: StartNodeRequest,
    ) -> worker::Result<Response> {
        if read_run(sql)?.is_none() {
            return error_response(404, "run not found");
        }
        let existing = read_node(sql, &req.node_id)?;
        let existing_for_logic = match &existing {
            Some(row) => Some(logic::ExistingNode {
                spec_hash: row.spec_hash.clone(),
                status: node_state_of(row)?,
            }),
            None => None,
        };

        match logic::resolve_start_node(existing_for_logic.as_ref(), &req.spec_hash) {
            Err(logic::NondeterministicReplay) => error_response_with_code(
                409,
                "start_node spec hash differs from the one already recorded for this node id",
                "nondeterministic_replay",
            ),
            Ok(logic::StartNodeDecision::AlreadyStarted { status }) => {
                // Redelivery: never calls `start_node_container` again
                // (module docs' "Who fails..." note above) — the
                // existing row and whatever container `NodeContainer`
                // already started (or already finished) for this
                // `node_id` are untouched.
                Response::from_json(&StartNodeOutcome {
                    node_id: req.node_id,
                    started: false,
                    status: status.as_db_str().to_string(),
                })
            }
            Ok(logic::StartNodeDecision::Started) => {
                let now_ms = worker::Date::now().as_millis() as i64;
                let command_json = serde_json::to_string(&req.command)
                    .map_err(|e| worker::Error::RustError(format!("cannot encode command: {e}")))?;
                insert_node(
                    sql,
                    &req.node_id,
                    &req.spec_hash,
                    req.check_name.as_deref(),
                    &req.image,
                    &command_json,
                    now_ms,
                )?;
                let run_row = require_run(sql)?;

                if let Err(start_err) = self
                    .start_node_container(&run_row, &req.node_id, &req.image, &req.command)
                    .await
                {
                    let now_ms = worker::Date::now().as_millis() as i64;
                    let result = serde_json::json!({ "error": start_err }).to_string();
                    update_node_status(
                        sql,
                        &req.node_id,
                        logic::NodeState::Failed.as_db_str(),
                        Some(&result),
                        now_ms,
                    )?;
                }
                let row = require_node(sql, &req.node_id)?;
                self.project_node_to_d1(&run_row.id, &row).await?;
                Response::from_json(&StartNodeOutcome {
                    node_id: req.node_id,
                    started: true,
                    status: row.status,
                })
            }
        }
    }

    /// `completeNode(run_id, node_id, status, result)` — module docs'
    /// Nodes section, dynamic-pipelines.md's "Completion event
    /// delivered twice"/"Run cancelled" rows. Does not itself
    /// re-send the completion event to any script (no real Workflow
    /// caller exists yet — see module docs' scope boundary): this is
    /// purely the state-recording half.
    async fn handle_complete_node(
        &self,
        sql: &SqlStorage,
        req: CompleteNodeRequest,
    ) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        let Some(existing) = read_node(sql, &req.node_id)? else {
            return error_response(404, "node not found");
        };
        let Some(incoming_status) = logic::NodeState::from_db_str(&req.status) else {
            return error_response(400, "unknown node status");
        };
        if !incoming_status.is_terminal() {
            return error_response(400, "complete_node requires a terminal status");
        }
        let current_status = node_state_of(&existing)?;

        match logic::resolve_complete_node(current_status, incoming_status) {
            Err(logic::CompleteNodeError::ConflictingStatus) => {
                error_response(409, "node already concluded with a different status")
            }
            Ok(logic::CompleteNodeDecision::DroppedCancelled) => {
                Response::from_json(&CompleteNodeOutcome {
                    node_id: req.node_id,
                    status: current_status.as_db_str().to_string(),
                    dropped: true,
                })
            }
            Ok(logic::CompleteNodeDecision::Recorded) => {
                let now_ms = worker::Date::now().as_millis() as i64;
                update_node_status(
                    sql,
                    &req.node_id,
                    incoming_status.as_db_str(),
                    req.result.as_deref(),
                    now_ms,
                )?;
                let row = require_node(sql, &req.node_id)?;
                self.project_node_to_d1(&run_row.id, &row).await?;
                Response::from_json(&CompleteNodeOutcome {
                    node_id: req.node_id,
                    status: row.status,
                    dropped: false,
                })
            }
        }
    }

    /// `ackNode(run_id, node_id)` — module docs' Nodes section: tracks
    /// the ack bit only, per the scope boundary (no redelivery timer
    /// this round).
    async fn handle_ack_node(
        &self,
        sql: &SqlStorage,
        req: AckNodeRequest,
    ) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        let Some(existing) = read_node(sql, &req.node_id)? else {
            return error_response(404, "node not found");
        };
        let needs_write = logic::resolve_ack_node(existing.acked != 0);
        if needs_write {
            update_node_ack(sql, &req.node_id)?;
            let row = require_node(sql, &req.node_id)?;
            self.project_node_to_d1(&run_row.id, &row).await?;
        }
        Response::from_json(&AckNodeOutcome {
            node_id: req.node_id,
            acked: true,
        })
    }

    /// Minimal run-cancellation hook (module docs' "Run cancellation"):
    /// moves the run to `RunState::Cancelled`, marks every non-terminal
    /// node `Cancelled`, and actually stops that node's real container
    /// via [`Self::stop_node_container`] — "Coordinator stops
    /// containers, marks nodes `cancelled`...", not just a DB flag flip
    /// while a real container keeps running unsupervised.
    async fn handle_cancel_run(&self, sql: &SqlStorage) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        if run_state_of(&run_row)?.is_terminal() {
            // Idempotent no-op: an already-terminal run (including one
            // already `Cancelled` by an earlier, redelivered call)
            // never gets its terminal state overwritten.
            return Response::from_json(&CancelRunOutcome {
                run_id: run_row.id,
                cancelled_nodes: Vec::new(),
            });
        }

        let nodes: Vec<(String, logic::NodeState)> = read_all_nodes(sql)?
            .iter()
            .map(|row| Ok((row.node_id.clone(), node_state_of(row)?)))
            .collect::<worker::Result<Vec<_>>>()?;
        let to_cancel = logic::nodes_to_cancel(&nodes);

        let now_ms = worker::Date::now().as_millis() as i64;
        for node_id in &to_cancel {
            self.stop_node_container(node_id).await;
            mark_node_cancelled(sql, node_id, now_ms)?;
            let row = require_node(sql, node_id)?;
            self.project_node_to_d1(&run_row.id, &row).await?;
        }

        update_run_status(sql, &run_row.id, RunState::Cancelled)?;
        self.state.storage().delete_alarm().await?;
        let run_row = require_run(sql)?;
        self.project_run_to_d1(&run_row).await?;

        Response::from_json(&CancelRunOutcome {
            run_id: run_row.id,
            cancelled_nodes: to_cancel,
        })
    }

    /// Starts `node_id`'s real container via a DO-to-DO call into
    /// `NodeContainer` (`node_container.rs`), addressed by `node_id` so
    /// a redelivered start naturally lands on the same container-backed
    /// instance (defense in depth alongside this DO's own
    /// `StartNodeDecision::AlreadyStarted` guard, which already stops a
    /// second call from ever reaching this function). `NodeContainer`'s
    /// own `/start` only *starts* the container and returns; the real
    /// `exec()` and its exit code are reported back asynchronously to
    /// `/complete-node` (see that module's doc comment for why this is
    /// a background task, not a synchronous wait, matching
    /// dynamic-pipelines.md's "step.do ... returns quickly" contract).
    /// `Err` here means the container itself failed to *start* (bad
    /// image, Docker/runtime error) — a real failure
    /// [`Self::handle_start_node`] maps straight to the node's own
    /// `failed` status, never an unhandled 500.
    async fn start_node_container(
        &self,
        run_row: &RunRow,
        node_id: &str,
        image: &str,
        command: &[String],
    ) -> Result<(), String> {
        let run_do_name = do_name(
            run_row.repo_id as u64,
            &run_row.sha,
            &run_row.run_key,
            run_row.attempt as u32,
        );
        let namespace = self
            .env
            .durable_object(crate::node_container::NODE_CONTAINER_BINDING)
            .map_err(|e| format!("node container namespace unavailable: {e}"))?;
        let id = namespace
            .id_from_name(node_id)
            .map_err(|e| format!("node container id error: {e}"))?;
        let stub = id
            .get_stub()
            .map_err(|e| format!("node container stub error: {e}"))?;

        let encoded = serde_json::to_string(&serde_json::json!({
            "run_do_name": run_do_name,
            "node_id": node_id,
            "image": image,
            "command": command,
        }))
        .map_err(|e| format!("cannot encode node-container start request: {e}"))?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(JsValue::from_str(&encoded)));
        let request =
            Request::new_with_init("https://node-container.cloud-ci.internal/start", &init)
                .map_err(|e| format!("cannot build node-container start request: {e}"))?;
        let mut response = stub
            .fetch_with_request(request)
            .await
            .map_err(|e| format!("node container fetch failed: {e}"))?;
        match response.status_code() {
            200..=299 => Ok(()),
            status => {
                let detail = response.text().await.unwrap_or_default();
                Err(format!("node container rejected start: {status} {detail}"))
            }
        }
    }

    /// Stops `node_id`'s real container via a DO-to-DO call into
    /// `NodeContainer`'s `/stop` — [`Self::handle_cancel_run`]'s only
    /// caller. Best-effort, degrade-and-log on any failure (unreachable
    /// DO, container already gone): blocking the whole run's
    /// cancellation on one node's container failing to stop would leave
    /// the run stuck cancelling forever, the same degrade-and-log
    /// posture `check_run_auth`'s callers already use for GitHub API
    /// failures.
    async fn stop_node_container(&self, node_id: &str) {
        let result: worker::Result<()> = async {
            let namespace = self
                .env
                .durable_object(crate::node_container::NODE_CONTAINER_BINDING)?;
            let id = namespace.id_from_name(node_id)?;
            let stub = id.get_stub()?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post);
            let request =
                Request::new_with_init("https://node-container.cloud-ci.internal/stop", &init)?;
            stub.fetch_with_request(request).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            worker::console_log!("cancel_run: stop container for node {node_id} failed: {e}");
        }
    }

    async fn project_run_to_d1(&self, row: &RunRow) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "INSERT INTO runs (id, repo_id, sha, run_key, attempt, status, expect_jobs, trigger, external_url, created_at, timeout_s) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT (id) DO UPDATE SET \
               status = excluded.status, \
               expect_jobs = excluded.expect_jobs, \
               trigger = excluded.trigger, \
               external_url = excluded.external_url",
        )
        .bind(&[
            JsValue::from_str(&row.id),
            JsValue::from_f64(row.repo_id as f64),
            JsValue::from_str(&row.sha),
            JsValue::from_str(&row.run_key),
            JsValue::from_f64(row.attempt as f64),
            JsValue::from_str(&row.status),
            row.expect_jobs
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
            JsValue::from_str(&row.trigger),
            JsValue::from_str(&row.external_url),
            JsValue::from_f64(row.created_at as f64),
            JsValue::from_f64(row.timeout_s as f64),
        ])?
        .run()
        .await?;
        Ok(())
    }

    async fn project_job_to_d1(&self, run_id: &str, row: &JobRow) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "INSERT INTO jobs (id, run_id, job_name, shard_total, runner_label, check_names, state, conclusion, conclusion_message) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT (id) DO UPDATE SET \
               shard_total = excluded.shard_total, \
               runner_label = excluded.runner_label, \
               check_names = excluded.check_names, \
               state = excluded.state, \
               conclusion = excluded.conclusion, \
               conclusion_message = excluded.conclusion_message",
        )
        .bind(&[
            JsValue::from_str(&row.id),
            JsValue::from_str(run_id),
            JsValue::from_str(&row.job_name),
            JsValue::from_f64(row.shard_total as f64),
            JsValue::from_str(&row.runner_label),
            JsValue::from_str(&row.check_names),
            JsValue::from_str(&row.state),
            row.conclusion
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
            row.conclusion_message
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
        ])?
        .run()
        .await?;
        Ok(())
    }

    async fn project_upload_to_d1(&self, row: &UploadRow) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "INSERT INTO uploads (id, job_id, shard_index, kind, name, scope, sha256, size_bytes, content_type, state, r2_key, accepted_seq, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
             ON CONFLICT (id) DO UPDATE SET state = excluded.state",
        )
        .bind(&[
            JsValue::from_str(&row.id),
            JsValue::from_str(&row.job_id),
            JsValue::from_f64(row.shard_index as f64),
            JsValue::from_str(&row.kind),
            JsValue::from_str(&row.name),
            JsValue::from_str(&row.scope),
            JsValue::from_str(&row.sha256),
            JsValue::from_f64(row.size_bytes as f64),
            JsValue::from_str(&row.content_type),
            JsValue::from_str(&row.state),
            JsValue::from_str(&row.r2_key),
            JsValue::from_f64(row.accepted_seq as f64),
            JsValue::from_f64(row.created_at as f64),
        ])?
        .run()
        .await?;
        Ok(())
    }

    async fn project_upload_state_to_d1(&self, upload_id: &str, state: &str) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare("UPDATE uploads SET state = ?1 WHERE id = ?2")
            .bind(&[JsValue::from_str(state), JsValue::from_str(upload_id)])?
            .run()
            .await?;
        Ok(())
    }

    async fn project_report_to_d1(&self, row: &ReportRow) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "INSERT INTO reports (id, job_id, shard_index, kind, name, scope, content_sha256, upload_id, accepted_seq, created_at, is_canonical, parsed, summary, r2_key) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )
        .bind(&[
            JsValue::from_str(&row.id),
            JsValue::from_str(&row.job_id),
            JsValue::from_f64(row.shard_index as f64),
            JsValue::from_str(&row.kind),
            JsValue::from_str(&row.name),
            JsValue::from_str(&row.scope),
            JsValue::from_str(&row.content_sha256),
            row.upload_id
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
            JsValue::from_f64(row.accepted_seq as f64),
            JsValue::from_f64(row.created_at as f64),
            JsValue::from_f64(row.is_canonical as f64),
            JsValue::from_f64(row.parsed as f64),
            row.summary
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
            JsValue::from_str(&row.r2_key),
        ])?
        .run()
        .await?;
        Ok(())
    }

    async fn unset_other_canonical_reports_in_d1(
        &self,
        job_id: &str,
        shard_index: u32,
        kind: &str,
        name: &str,
        keep_id: &str,
    ) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "UPDATE reports SET is_canonical = 0 \
             WHERE job_id = ?1 AND shard_index = ?2 AND kind = ?3 AND name = ?4 AND id != ?5",
        )
        .bind(&[
            JsValue::from_str(job_id),
            JsValue::from_f64(shard_index as f64),
            JsValue::from_str(kind),
            JsValue::from_str(name),
            JsValue::from_str(keep_id),
        ])?
        .run()
        .await?;
        Ok(())
    }

    async fn project_job_shard_to_d1(&self, row: &JobShardRow) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "INSERT INTO job_shards (job_id, shard_index, state, conclusion, external_url, completed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (job_id, shard_index) DO UPDATE SET \
               state = excluded.state, \
               conclusion = excluded.conclusion, \
               external_url = excluded.external_url, \
               completed_at = excluded.completed_at",
        )
        .bind(&[
            JsValue::from_str(&row.job_id),
            JsValue::from_f64(row.shard_index as f64),
            JsValue::from_str(&row.state),
            row.conclusion
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
            JsValue::from_str(&row.external_url),
            row.completed_at
                .map_or(JsValue::NULL, |v| JsValue::from_f64(v as f64)),
        ])?
        .run()
        .await?;
        Ok(())
    }

    async fn project_check_run_to_d1(&self, run_id: &str, row: &CheckRunRow) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        let now_ms = worker::Date::now().as_millis();
        let id = crate::ulid::generate(now_ms)
            .map_err(|e| worker::Error::RustError(format!("ulid generation failed: {e}")))?;
        db.prepare(
            "INSERT INTO check_runs (id, run_id, check_name, github_check_run_id, status, conclusion, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (run_id, check_name) DO UPDATE SET \
               status = excluded.status, \
               conclusion = excluded.conclusion",
        )
        .bind(&[
            JsValue::from_str(&id),
            JsValue::from_str(run_id),
            JsValue::from_str(&row.check_name),
            JsValue::from_f64(row.github_check_run_id as f64),
            JsValue::from_str(&row.status),
            row.conclusion
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
            JsValue::from_f64(row.created_at as f64),
        ])?
        .run()
        .await?;
        Ok(())
    }

    /// Projects one `node` row into D1's `nodes` table (migration
    /// 0010), keyed by `(run_id, node_id)` since D1 is shared across
    /// runs, unlike the DO's own `node_id`-only primary key (module
    /// docs' Nodes section).
    async fn project_node_to_d1(&self, run_id: &str, row: &NodeRow) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "INSERT INTO nodes (run_id, node_id, spec_hash, status, check_name, result, started_at, completed_at, acked, image, command) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT (run_id, node_id) DO UPDATE SET \
               status = excluded.status, \
               result = excluded.result, \
               completed_at = excluded.completed_at, \
               acked = excluded.acked",
        )
        .bind(&[
            JsValue::from_str(run_id),
            JsValue::from_str(&row.node_id),
            JsValue::from_str(&row.spec_hash),
            JsValue::from_str(&row.status),
            row.check_name
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
            row.result
                .clone()
                .map_or(JsValue::NULL, |v| JsValue::from_str(&v)),
            JsValue::from_f64(row.started_at as f64),
            row.completed_at
                .map_or(JsValue::NULL, |v| JsValue::from_f64(v as f64)),
            JsValue::from_f64(row.acked as f64),
            JsValue::from_str(&row.image),
            JsValue::from_str(&row.command),
        ])?
        .run()
        .await?;
        Ok(())
    }

    /// Projects one terminal `shard_state` row into D1's `shard_states`
    /// table (migration 0014), keyed by `(run_id, job_name, idx,
    /// attempt)` since D1 is shared across runs — same split as
    /// [`Self::project_node_to_d1`]. `job_group` is deliberately not
    /// projected (module docs' "Shard groups / merge barrier" section).
    async fn project_shard_state_to_d1(
        &self,
        run_id: &str,
        job_name: &str,
        row: &ShardStateDbRow,
        report_key: Option<&str>,
        duration_ms: Option<i64>,
        finished_at_ms: i64,
    ) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "INSERT INTO shard_states (run_id, job_name, idx, attempt, status, report_key, duration_ms, finished_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (run_id, job_name, idx, attempt) DO UPDATE SET \
               status = excluded.status, \
               report_key = excluded.report_key, \
               duration_ms = excluded.duration_ms, \
               finished_at = excluded.finished_at",
        )
        .bind(&[
            JsValue::from_str(run_id),
            JsValue::from_str(job_name),
            JsValue::from_f64(row.idx as f64),
            JsValue::from_f64(row.attempt as f64),
            JsValue::from_str(&row.status),
            report_key.map_or(JsValue::NULL, JsValue::from_str),
            duration_ms.map_or(JsValue::NULL, |v| JsValue::from_f64(v as f64)),
            JsValue::from_f64(finished_at_ms as f64),
        ])?
        .run()
        .await?;
        Ok(())
    }

    /// `registerShardGroup(job_name, expected_total, fail_fast,
    /// merge_on_failure)` — module docs' "Shard groups / merge barrier"
    /// section, parallelization.md bullet 1. Idempotent: a redelivered
    /// call for an already-registered `job_name` is a clean no-op
    /// (existing config untouched), never a conflict — nothing in
    /// parallelization.md documents a config-mismatch rejection for this
    /// call the way `StartJob`'s `shard_total`/`BeginRun`'s
    /// `expect_jobs` do, so this stays deliberately permissive rather
    /// than inventing an undocumented conflict rule.
    async fn handle_register_shard_group(
        &self,
        sql: &SqlStorage,
        req: RegisterShardGroupRequest,
    ) -> worker::Result<Response> {
        if read_run(sql)?.is_none() {
            return error_response(404, "run not found");
        }
        if logic::MergeOnFailure::from_db_str(&req.merge_on_failure).is_none() {
            return error_response(400, "unknown merge_on_failure value");
        }
        if read_job_group(sql, &req.job_name)?.is_none() {
            insert_job_group(
                sql,
                &req.job_name,
                req.expected_total,
                req.fail_fast,
                &req.merge_on_failure,
            )?;
        }
        Response::from_json(&RegisterShardGroupOutcome {
            job_name: req.job_name,
        })
    }

    /// One shard's terminal ingest call — module docs' "Shard groups /
    /// merge barrier" section, parallelization.md bullets 2-4. Produces
    /// [`ShardBarrierDecision`] only: see this module's doc comment and
    /// [`logic::evaluate_barrier`]'s doc comment for the exact
    /// decision-only scope boundary this round stops at (no real
    /// cancellation, no real merge dispatch).
    async fn handle_shard_terminal(
        &self,
        sql: &SqlStorage,
        req: ShardTerminalRequest,
    ) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        let Some(group) = read_job_group(sql, &req.job_name)? else {
            return error_response(404, "shard group not registered");
        };
        let Some(incoming_status) = logic::ShardTerminalStatus::from_db_str(&req.status) else {
            return error_response(400, "unknown shard terminal status");
        };

        let existing_status =
            match read_shard_state_status(sql, &req.job_name, req.idx, req.attempt)? {
                None => None,
                Some(s) => Some(logic::ShardTerminalStatus::from_db_str(&s).ok_or_else(|| {
                    worker::Error::RustError("unknown shard_state status already stored".into())
                })?),
            };

        let decision = match logic::resolve_shard_terminal(existing_status, incoming_status) {
            Err(logic::ConflictingShardStatus) => {
                return error_response(
                    409,
                    "shard already concluded with a different terminal status",
                );
            }
            Ok(logic::ShardTerminalDecision::AlreadyRecorded) => ShardBarrierDecision::Duplicate,
            Ok(logic::ShardTerminalDecision::Recorded) => {
                let now_ms = worker::Date::now().as_millis() as i64;
                insert_shard_state(
                    sql,
                    &req.job_name,
                    req.idx,
                    req.attempt,
                    incoming_status.as_db_str(),
                    req.report_key.as_deref(),
                    req.duration_ms,
                    now_ms,
                )?;
                self.project_shard_state_to_d1(
                    &run_row.id,
                    &req.job_name,
                    &ShardStateDbRow {
                        idx: req.idx as i64,
                        attempt: req.attempt as i64,
                        status: incoming_status.as_db_str().to_string(),
                    },
                    req.report_key.as_deref(),
                    req.duration_ms,
                    now_ms,
                )
                .await?;

                if group.status != "running" {
                    // Module docs' "Shard groups / merge barrier"
                    // section: a late-finishing shard after the group
                    // already decided (fail-fast or satisfied) is
                    // recorded but does not re-run the barrier.
                    ShardBarrierDecision::GroupAlreadyTerminal
                } else {
                    let Some(merge_on_failure) =
                        logic::MergeOnFailure::from_db_str(&group.merge_on_failure)
                    else {
                        return Err(worker::Error::RustError(format!(
                            "unknown merge_on_failure stored for {}: {}",
                            req.job_name, group.merge_on_failure
                        )));
                    };
                    let config = logic::JobGroupConfig {
                        expected_total: group.expected_total as u32,
                        fail_fast: group.fail_fast != 0,
                        merge_on_failure,
                    };
                    let latest_terminal = logic::latest_attempt_per_shard(
                        &read_all_shard_terminal_rows(sql, &req.job_name)?,
                    );
                    match logic::evaluate_barrier(config, &latest_terminal, incoming_status) {
                        logic::BarrierOutcome::Waiting => ShardBarrierDecision::Waiting,
                        logic::BarrierOutcome::FailFastTriggered { cancel_idxs } => {
                            update_job_group_status(sql, &req.job_name, "failed")?;
                            ShardBarrierDecision::FailFastTriggered { cancel_idxs }
                        }
                        logic::BarrierOutcome::Satisfied {
                            merge,
                            included_idxs,
                        } => {
                            update_job_group_status(sql, &req.job_name, "satisfied")?;
                            ShardBarrierDecision::Satisfied {
                                merge,
                                included_idxs,
                            }
                        }
                    }
                }
            }
        };

        Response::from_json(&ShardTerminalOutcome {
            job_name: req.job_name,
            idx: req.idx,
            attempt: req.attempt,
            decision,
        })
    }

    /// Best-effort resolution of the installation token needed to call
    /// GitHub's Check Run API for `repo_id` — the same
    /// `roles::lookup_repo_owner` + App-JWT-mint + installation-token
    /// lookup chain `roles.rs`'s role resolution already uses (see
    /// `roles::installation_token_for_repo`'s doc comment), reused rather
    /// than reinvented. Returns `None` and logs rather than failing the
    /// whole `StartJob`/`CompleteShard`/close-run call on any failure
    /// (repo not registered, secrets missing, GitHub API error): a Check
    /// Run is a best-effort side channel here, the same degrade-and-log
    /// posture `reconcile.rs`'s uninstall-of-disallowed-org and `lib.rs`'s
    /// own best-effort uninstall calls already use on GitHub API failure.
    async fn check_run_auth(&self, repo_id: u64) -> Option<(String, String, String)> {
        let owner_row = match crate::roles::lookup_repo_owner(&self.env, repo_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                worker::console_log!(
                    "check run: repo {repo_id} is not registered with this deployment"
                );
                return None;
            }
            Err(e) => {
                worker::console_log!("check run: repo owner lookup failed for {repo_id}: {e}");
                return None;
            }
        };
        let now_s = (worker::Date::now().as_millis() / 1000) as i64;
        match crate::roles::installation_token_for_repo(&self.env, &owner_row, now_s).await {
            Ok(token) => Some((token.token, owner_row.owner_login, owner_row.repo_name)),
            Err(e) => {
                worker::console_log!(
                    "check run: installation token exchange failed for repo {repo_id}: {e}"
                );
                None
            }
        }
    }

    /// Creates any name in `check_names` this run has not already
    /// created a Check Run for (byo-ci.md's "Checks and scopes": "the
    /// Worker creates any check name it hasn't seen yet for this run on
    /// the first `StartJob` that names it"). `check_names` empty, or
    /// every named check already created, makes no GitHub API call at
    /// all — the happy "no checks" path stays a no-op (round scope item
    /// 4).
    async fn create_check_runs_for_job(
        &self,
        sql: &SqlStorage,
        run_row: &RunRow,
        check_names: &[String],
    ) -> worker::Result<()> {
        if check_names.is_empty() {
            return Ok(());
        }
        let already_created: Vec<String> = read_all_check_runs(sql)?
            .into_iter()
            .map(|r| r.check_name)
            .collect();
        let new_names = logic::new_check_names(&already_created, check_names);
        if new_names.is_empty() {
            return Ok(());
        }
        let Some((token, owner, repo)) = self.check_run_auth(run_row.repo_id as u64).await else {
            return Ok(());
        };
        for name in new_names {
            let request = github_checks::CreateCheckRunRequest {
                name: name.clone(),
                head_sha: run_row.sha.clone(),
                status: Some(github_checks::CheckRunStatus::Queued),
                conclusion: None,
                details_url: (!run_row.external_url.is_empty())
                    .then(|| run_row.external_url.clone()),
                output: Some(github_checks::CheckRunOutput {
                    title: name.clone(),
                    summary: "Waiting for jobs to report.".to_string(),
                    text: None,
                }),
            };
            match github_checks::create_check_run(&token, &owner, &repo, &request).await {
                Ok(check_run) => {
                    let now_ms = worker::Date::now().as_millis() as i64;
                    insert_check_run(sql, &name, check_run.id, "queued", now_ms)?;
                    if let Some(row) = read_check_run(sql, &name)? {
                        self.project_check_run_to_d1(&run_row.id, &row).await?;
                    }
                }
                Err(e) => {
                    worker::console_log!("check run: create {name} failed: {e}");
                }
            }
        }
        Ok(())
    }

    /// Updates every already-created Check Run named in `check_names`
    /// with a fresh shard-table summary covering every job attached to
    /// it (pr-comment.md's "Check Runs": "if a check name is shared by
    /// several jobs, the summary covers all of them"), reading the
    /// DO's own state, not `pr_comment.rs`'s template (round scope
    /// boundary — see `coordinator` module docs). A name with no
    /// check-run row yet (not created by `StartJob`, which should never
    /// happen since `CompleteShard` only names checks a job already
    /// declared) is silently skipped rather than created here —
    /// creation only ever happens from `StartJob`.
    async fn update_check_runs_for_job(
        &self,
        sql: &SqlStorage,
        run_row: &RunRow,
        check_names: &[String],
    ) -> worker::Result<()> {
        let mut to_update = Vec::new();
        for name in check_names {
            if read_check_run(sql, name)?.is_some() {
                to_update.push(name.clone());
            }
        }
        if to_update.is_empty() {
            return Ok(());
        }
        let Some((token, owner, repo)) = self.check_run_auth(run_row.repo_id as u64).await else {
            return Ok(());
        };
        for name in to_update {
            let Some(check_run_row) = read_check_run(sql, &name)? else {
                continue;
            };
            let rows = check_summary_rows(sql, &name)?;
            let request = github_checks::UpdateCheckRunRequest {
                status: Some(github_checks::CheckRunStatus::InProgress),
                output: Some(github_checks::CheckRunOutput {
                    title: name.clone(),
                    summary: logic::render_check_summary(&rows),
                    text: None,
                }),
                ..Default::default()
            };
            match github_checks::update_check_run(
                &token,
                &owner,
                &repo,
                check_run_row.github_check_run_id as u64,
                &request,
            )
            .await
            {
                Ok(_) => {
                    update_check_run_state(sql, &name, "in_progress", None)?;
                    if let Some(row) = read_check_run(sql, &name)? {
                        self.project_check_run_to_d1(&run_row.id, &row).await?;
                    }
                }
                Err(e) => {
                    worker::console_log!("check run: update {name} failed: {e}");
                }
            }
        }
        Ok(())
    }

    /// Finalizes every Check Run this run created (`handle_close_run`'s
    /// only caller): `status: completed`, `conclusion` scoped to just the
    /// jobs attached to that check (pr-comment.md: "there is no aggregate
    /// check ... a check only reflects the jobs that named it"), computed
    /// via [`logic::run_conclusion_from_jobs`] — the same worst-of logic
    /// `handle_close_run` already uses for the run's own conclusion, not
    /// duplicated here. Idempotent: a check already `completed` (an
    /// earlier close attempt that got this far before a redelivered
    /// close signal arrived) is skipped.
    async fn finalize_check_runs(&self, sql: &SqlStorage, run_row: &RunRow) -> worker::Result<()> {
        let check_runs = read_all_check_runs(sql)?;
        if check_runs.is_empty() {
            return Ok(());
        }
        let Some((token, owner, repo)) = self.check_run_auth(run_row.repo_id as u64).await else {
            return Ok(());
        };
        for check_run_row in check_runs {
            if check_run_row.status == "completed" {
                continue;
            }
            let rows = check_summary_rows(sql, &check_run_row.check_name)?;
            let conclusion = logic::run_conclusion_from_jobs(
                &rows.iter().filter_map(|r| r.conclusion).collect::<Vec<_>>(),
            );
            let request = github_checks::UpdateCheckRunRequest {
                status: Some(github_checks::CheckRunStatus::Completed),
                conclusion: Some(check_run_conclusion_of(conclusion)),
                output: Some(github_checks::CheckRunOutput {
                    title: check_run_row.check_name.clone(),
                    summary: logic::render_check_summary(&rows),
                    text: None,
                }),
                ..Default::default()
            };
            match github_checks::update_check_run(
                &token,
                &owner,
                &repo,
                check_run_row.github_check_run_id as u64,
                &request,
            )
            .await
            {
                Ok(_) => {
                    update_check_run_state(
                        sql,
                        &check_run_row.check_name,
                        "completed",
                        Some(conclusion_db_name(conclusion)),
                    )?;
                    if let Some(row) = read_check_run(sql, &check_run_row.check_name)? {
                        self.project_check_run_to_d1(&run_row.id, &row).await?;
                    }
                }
                Err(e) => {
                    worker::console_log!(
                        "check run: finalize {} failed: {e}",
                        check_run_row.check_name
                    );
                }
            }
        }
        Ok(())
    }
}

/// Every job attached to `check_name` (its `check_names` JSON column
/// contains it), as rows for [`logic::render_check_summary`] — the data
/// `update_check_runs_for_job`/`finalize_check_runs` read, independent
/// of `pr_comment.rs`'s template context (`coordinator` module docs'
/// scope boundary).
fn check_summary_rows(
    sql: &SqlStorage,
    check_name: &str,
) -> worker::Result<Vec<logic::CheckSummaryJobRow>> {
    let mut rows = Vec::new();
    for job in read_all_jobs(sql)? {
        let names = decode_string_list(&job.check_names)?;
        if !names.iter().any(|n| n == check_name) {
            continue;
        }
        let completed_shards = count_uploaded_shards(sql, &job.id)? as u32;
        let conclusion = job
            .conclusion
            .as_deref()
            .map(|c| {
                conclusion_from_db_str(c).ok_or_else(|| {
                    worker::Error::RustError(format!("unknown stored conclusion {c}"))
                })
            })
            .transpose()?;
        rows.push(logic::CheckSummaryJobRow {
            job_name: job.job_name,
            completed_shards,
            shard_total: job.shard_total as u32,
            conclusion,
        });
    }
    Ok(rows)
}

/// Maps the proto [`Conclusion`] `finalize_check_runs` computes (via
/// [`logic::run_conclusion_from_jobs`]) to the GitHub REST
/// [`github_checks::CheckRunConclusion`] value the `PATCH` request
/// carries. `CONCLUSION_UNSPECIFIED` maps to `Neutral` — GitHub's own
/// "none of the above" value — though it is never actually reached here:
/// a check only reaches [`finalize_check_runs`] once every job attached
/// to it has concluded with a real conclusion.
fn check_run_conclusion_of(c: Conclusion) -> github_checks::CheckRunConclusion {
    match c {
        Conclusion::CONCLUSION_SUCCESS => github_checks::CheckRunConclusion::Success,
        Conclusion::CONCLUSION_FAILURE => github_checks::CheckRunConclusion::Failure,
        Conclusion::CONCLUSION_CANCELLED => github_checks::CheckRunConclusion::Cancelled,
        Conclusion::CONCLUSION_SKIPPED => github_checks::CheckRunConclusion::Skipped,
        Conclusion::CONCLUSION_UNSPECIFIED => github_checks::CheckRunConclusion::Neutral,
    }
}

fn run_state_of(row: &RunRow) -> worker::Result<RunState> {
    RunState::from_db_str(&row.status)
        .ok_or_else(|| worker::Error::RustError(format!("unknown run status {}", row.status)))
}

fn run_status_of(row: &RunRow) -> worker::Result<RunStatus> {
    Ok(run_state_of(row)?.to_proto_status())
}

fn ensure_schema(sql: &SqlStorage) -> worker::Result<()> {
    sql.exec(
        "CREATE TABLE IF NOT EXISTS run ( \
            id TEXT PRIMARY KEY, \
            repo_id INTEGER NOT NULL, \
            sha TEXT NOT NULL, \
            run_key TEXT NOT NULL, \
            attempt INTEGER NOT NULL, \
            status TEXT NOT NULL, \
            expect_jobs TEXT, \
            trigger TEXT NOT NULL, \
            external_url TEXT NOT NULL, \
            created_at INTEGER NOT NULL, \
            timeout_s INTEGER NOT NULL DEFAULT 1800 \
        )",
        None,
    )?;
    sql.exec(
        "CREATE TABLE IF NOT EXISTS job ( \
            id TEXT PRIMARY KEY, \
            job_name TEXT NOT NULL UNIQUE, \
            shard_total INTEGER NOT NULL, \
            runner_label TEXT NOT NULL, \
            check_names TEXT NOT NULL, \
            state TEXT NOT NULL DEFAULT 'running', \
            conclusion TEXT, \
            conclusion_message TEXT \
        )",
        None,
    )?;
    sql.exec(
        "CREATE TABLE IF NOT EXISTS job_shard ( \
            job_id TEXT NOT NULL, \
            shard_index INTEGER NOT NULL, \
            state TEXT NOT NULL DEFAULT 'pending', \
            conclusion TEXT, \
            external_url TEXT NOT NULL DEFAULT '', \
            completed_at INTEGER, \
            PRIMARY KEY (job_id, shard_index) \
        )",
        None,
    )?;
    sql.exec(
        "CREATE TABLE IF NOT EXISTS upload ( \
            id TEXT PRIMARY KEY, \
            job_id TEXT NOT NULL, \
            shard_index INTEGER NOT NULL, \
            kind TEXT NOT NULL, \
            name TEXT NOT NULL, \
            scope TEXT NOT NULL DEFAULT '', \
            sha256 TEXT NOT NULL, \
            size_bytes INTEGER NOT NULL, \
            content_type TEXT NOT NULL DEFAULT '', \
            state TEXT NOT NULL DEFAULT 'pending', \
            r2_key TEXT NOT NULL, \
            accepted_seq INTEGER NOT NULL, \
            created_at INTEGER NOT NULL \
        )",
        None,
    )?;
    sql.exec(
        "CREATE TABLE IF NOT EXISTS report ( \
            id TEXT PRIMARY KEY, \
            job_id TEXT NOT NULL, \
            shard_index INTEGER NOT NULL, \
            kind TEXT NOT NULL, \
            name TEXT NOT NULL, \
            scope TEXT NOT NULL DEFAULT '', \
            content_sha256 TEXT NOT NULL, \
            upload_id TEXT, \
            accepted_seq INTEGER NOT NULL, \
            created_at INTEGER NOT NULL, \
            is_canonical INTEGER NOT NULL DEFAULT 1, \
            parsed INTEGER NOT NULL DEFAULT 0, \
            summary TEXT, \
            r2_key TEXT NOT NULL DEFAULT '' \
        )",
        None,
    )?;
    sql.exec(
        "CREATE TABLE IF NOT EXISTS check_run ( \
            check_name TEXT PRIMARY KEY, \
            github_check_run_id INTEGER NOT NULL, \
            status TEXT NOT NULL, \
            conclusion TEXT, \
            created_at INTEGER NOT NULL \
        )",
        None,
    )?;
    sql.exec(
        "CREATE TABLE IF NOT EXISTS node ( \
            node_id TEXT PRIMARY KEY, \
            spec_hash TEXT NOT NULL, \
            status TEXT NOT NULL, \
            check_name TEXT, \
            result TEXT, \
            started_at INTEGER NOT NULL, \
            completed_at INTEGER, \
            acked INTEGER NOT NULL DEFAULT 0, \
            image TEXT NOT NULL DEFAULT '', \
            command TEXT NOT NULL DEFAULT '[]' \
        )",
        None,
    )?;
    sql.exec(
        "CREATE TABLE IF NOT EXISTS job_group ( \
            job_name TEXT PRIMARY KEY, \
            expected_total INTEGER NOT NULL, \
            fail_fast INTEGER NOT NULL, \
            merge_on_failure TEXT NOT NULL, \
            merge_job_id TEXT, \
            status TEXT NOT NULL DEFAULT 'running' \
        )",
        None,
    )?;
    sql.exec(
        "CREATE TABLE IF NOT EXISTS shard_state ( \
            job_name TEXT NOT NULL, \
            idx INTEGER NOT NULL, \
            attempt INTEGER NOT NULL DEFAULT 1, \
            status TEXT NOT NULL, \
            report_key TEXT, \
            duration_ms INTEGER, \
            finished_at INTEGER, \
            PRIMARY KEY (job_name, idx, attempt) \
        )",
        None,
    )?;
    Ok(())
}

fn read_run(sql: &SqlStorage) -> worker::Result<Option<RunRow>> {
    let rows: Vec<RunRow> = sql
        .exec(
            "SELECT id, repo_id, sha, run_key, attempt, status, expect_jobs, trigger, external_url, created_at, timeout_s FROM run LIMIT 1",
            None,
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn require_run(sql: &SqlStorage) -> worker::Result<RunRow> {
    read_run(sql)?.ok_or_else(|| worker::Error::RustError("run row missing after write".into()))
}

const JOB_COLUMNS: &str =
    "id, job_name, shard_total, runner_label, check_names, state, conclusion, conclusion_message";

fn read_job(sql: &SqlStorage, job_name: &str) -> worker::Result<Option<JobRow>> {
    let rows: Vec<JobRow> = sql
        .exec(
            &format!("SELECT {JOB_COLUMNS} FROM job WHERE job_name = ?1"),
            vec![SqlStorageValue::from(job_name)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn read_job_by_id(sql: &SqlStorage, id: &str) -> worker::Result<Option<JobRow>> {
    let rows: Vec<JobRow> = sql
        .exec(
            &format!("SELECT {JOB_COLUMNS} FROM job WHERE id = ?1"),
            vec![SqlStorageValue::from(id)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn require_job_by_id(sql: &SqlStorage, id: &str) -> worker::Result<JobRow> {
    read_job_by_id(sql, id)?
        .ok_or_else(|| worker::Error::RustError("job row missing after write".into()))
}

fn read_all_jobs(sql: &SqlStorage) -> worker::Result<Vec<JobRow>> {
    sql.exec(
        &format!("SELECT {JOB_COLUMNS} FROM job ORDER BY job_name"),
        None,
    )?
    .to_array()
}

/// `message` is the run-close summary ("N of total shards missing") for
/// a job that concluded with missing shards (docs/design/byo-ci.md's
/// Completion semantics); `None` clears it for a job that concluded
/// normally from its own shard uploads.
fn update_job_conclusion(
    sql: &SqlStorage,
    job_id: &str,
    state: &str,
    conclusion: &str,
    message: Option<&str>,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE job SET state = ?1, conclusion = ?2, conclusion_message = ?3 WHERE id = ?4",
        vec![
            SqlStorageValue::from(state),
            SqlStorageValue::from(conclusion),
            SqlStorageValue::from(message.map(str::to_string)),
            SqlStorageValue::from(job_id),
        ],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_run(
    sql: &SqlStorage,
    id: &str,
    repo_id: u64,
    sha: &str,
    run_key: &str,
    attempt: u32,
    status: RunState,
    expect_jobs: Option<&[String]>,
    trigger: &str,
    external_url: &str,
    created_at_ms: i64,
    timeout_s: i64,
) -> worker::Result<()> {
    let expect_jobs_json = expect_jobs
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| worker::Error::RustError(format!("cannot encode expect_jobs: {e}")))?;
    let repo_id_value = SqlStorageValue::try_from_i64(repo_id as i64)?;
    sql.exec(
        "INSERT INTO run (id, repo_id, sha, run_key, attempt, status, expect_jobs, trigger, external_url, created_at, timeout_s) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        vec![
            SqlStorageValue::from(id),
            repo_id_value,
            SqlStorageValue::from(sha),
            SqlStorageValue::from(run_key),
            SqlStorageValue::try_from_i64(i64::from(attempt))?,
            SqlStorageValue::from(status.as_db_str()),
            SqlStorageValue::from(expect_jobs_json),
            SqlStorageValue::from(trigger),
            SqlStorageValue::from(external_url),
            SqlStorageValue::try_from_i64(created_at_ms)?,
            SqlStorageValue::try_from_i64(timeout_s)?,
        ],
    )?;
    Ok(())
}

fn update_expect_jobs(
    sql: &SqlStorage,
    run_id: &str,
    expect_jobs: &[String],
) -> worker::Result<()> {
    let json = serde_json::to_string(expect_jobs)
        .map_err(|e| worker::Error::RustError(format!("cannot encode expect_jobs: {e}")))?;
    sql.exec(
        "UPDATE run SET expect_jobs = ?1 WHERE id = ?2",
        vec![SqlStorageValue::from(json), SqlStorageValue::from(run_id)],
    )?;
    Ok(())
}

fn update_trigger_and_url(
    sql: &SqlStorage,
    run_id: &str,
    trigger: &str,
    external_url: &str,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE run SET trigger = ?1, external_url = ?2 WHERE id = ?3",
        vec![
            SqlStorageValue::from(trigger),
            SqlStorageValue::from(external_url),
            SqlStorageValue::from(run_id),
        ],
    )?;
    Ok(())
}

fn update_run_status(sql: &SqlStorage, run_id: &str, status: RunState) -> worker::Result<()> {
    sql.exec(
        "UPDATE run SET status = ?1 WHERE id = ?2",
        vec![
            SqlStorageValue::from(status.as_db_str()),
            SqlStorageValue::from(run_id),
        ],
    )?;
    Ok(())
}

fn insert_job(
    sql: &SqlStorage,
    id: &str,
    job_name: &str,
    shard_total: u32,
    runner_label: &str,
    check_names_json: &str,
) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO job (id, job_name, shard_total, runner_label, check_names) VALUES (?1, ?2, ?3, ?4, ?5)",
        vec![
            SqlStorageValue::from(id),
            SqlStorageValue::from(job_name),
            SqlStorageValue::try_from_i64(i64::from(shard_total))?,
            SqlStorageValue::from(runner_label),
            SqlStorageValue::from(check_names_json),
        ],
    )?;
    Ok(())
}

fn update_job(
    sql: &SqlStorage,
    id: &str,
    shard_total: u32,
    runner_label: &str,
    check_names_json: &str,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE job SET shard_total = ?1, runner_label = ?2, check_names = ?3 WHERE id = ?4",
        vec![
            SqlStorageValue::try_from_i64(i64::from(shard_total))?,
            SqlStorageValue::from(runner_label),
            SqlStorageValue::from(check_names_json),
            SqlStorageValue::from(id),
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Job shards
// ---------------------------------------------------------------------------

fn read_job_shard(
    sql: &SqlStorage,
    job_id: &str,
    shard_index: u32,
) -> worker::Result<Option<JobShardRow>> {
    let rows: Vec<JobShardRow> = sql
        .exec(
            "SELECT job_id, shard_index, state, conclusion, external_url, completed_at \
             FROM job_shard WHERE job_id = ?1 AND shard_index = ?2",
            vec![
                SqlStorageValue::from(job_id),
                SqlStorageValue::try_from_i64(i64::from(shard_index))?,
            ],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn read_all_job_shards(sql: &SqlStorage, job_id: &str) -> worker::Result<Vec<JobShardRow>> {
    sql.exec(
        "SELECT job_id, shard_index, state, conclusion, external_url, completed_at \
         FROM job_shard WHERE job_id = ?1",
        vec![SqlStorageValue::from(job_id)],
    )?
    .to_array()
}

fn require_job_shard(
    sql: &SqlStorage,
    job_id: &str,
    shard_index: u32,
) -> worker::Result<JobShardRow> {
    read_job_shard(sql, job_id, shard_index)?
        .ok_or_else(|| worker::Error::RustError("job_shard row missing after write".into()))
}

#[allow(clippy::too_many_arguments)]
fn upsert_job_shard(
    sql: &SqlStorage,
    job_id: &str,
    shard_index: u32,
    state: &str,
    conclusion: &str,
    external_url: &str,
    completed_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO job_shard (job_id, shard_index, state, conclusion, external_url, completed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (job_id, shard_index) DO UPDATE SET \
           state = excluded.state, \
           conclusion = excluded.conclusion, \
           external_url = excluded.external_url, \
           completed_at = excluded.completed_at",
        vec![
            SqlStorageValue::from(job_id),
            SqlStorageValue::try_from_i64(i64::from(shard_index))?,
            SqlStorageValue::from(state),
            SqlStorageValue::from(conclusion),
            SqlStorageValue::from(external_url),
            SqlStorageValue::try_from_i64(completed_at_ms)?,
        ],
    )?;
    Ok(())
}

/// Marks one never-uploaded shard `missing` at run-close time
/// (docs/design/byo-ci.md's Completion semantics). Unlike
/// `upsert_job_shard`, there is no real conclusion to record — the shard
/// never completed — so `conclusion` stays/becomes `NULL` rather than
/// being forced to a placeholder value. Idempotent: re-closing a run
/// whose shard is already `missing` from an earlier close attempt only
/// refreshes `completed_at`.
fn mark_job_shard_missing(
    sql: &SqlStorage,
    job_id: &str,
    shard_index: u32,
    now_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO job_shard (job_id, shard_index, state, conclusion, external_url, completed_at) \
         VALUES (?1, ?2, 'missing', NULL, '', ?3) \
         ON CONFLICT (job_id, shard_index) DO UPDATE SET \
           state = 'missing', \
           completed_at = excluded.completed_at",
        vec![
            SqlStorageValue::from(job_id),
            SqlStorageValue::try_from_i64(i64::from(shard_index))?,
            SqlStorageValue::try_from_i64(now_ms)?,
        ],
    )?;
    Ok(())
}

fn count_uploaded_shards(sql: &SqlStorage, job_id: &str) -> worker::Result<i64> {
    let rows: Vec<MaxSeqRow> = sql
        .exec(
            "SELECT COUNT(*) as m FROM job_shard WHERE job_id = ?1 AND state = 'uploaded'",
            vec![SqlStorageValue::from(job_id)],
        )?
        .to_array()?;
    Ok(rows.first().map(|r| r.m).unwrap_or(0))
}

fn read_all_shard_conclusions(sql: &SqlStorage, job_id: &str) -> worker::Result<Vec<Conclusion>> {
    let rows: Vec<JobShardRow> = sql
        .exec(
            "SELECT job_id, shard_index, state, conclusion, external_url, completed_at \
             FROM job_shard WHERE job_id = ?1 AND state = 'uploaded'",
            vec![SqlStorageValue::from(job_id)],
        )?
        .to_array()?;
    rows.into_iter()
        .map(|r| {
            let c = r.conclusion.ok_or_else(|| {
                worker::Error::RustError("uploaded shard has no conclusion".into())
            })?;
            conclusion_from_db_str(&c)
                .ok_or_else(|| worker::Error::RustError(format!("unknown stored conclusion {c}")))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

const UPLOAD_COLUMNS: &str = "id, job_id, shard_index, kind, name, scope, sha256, size_bytes, content_type, state, r2_key, accepted_seq, created_at";

fn read_upload(sql: &SqlStorage, id: &str) -> worker::Result<Option<UploadRow>> {
    let rows: Vec<UploadRow> = sql
        .exec(
            &format!("SELECT {UPLOAD_COLUMNS} FROM upload WHERE id = ?1"),
            vec![SqlStorageValue::from(id)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn require_upload(sql: &SqlStorage, id: &str) -> worker::Result<UploadRow> {
    read_upload(sql, id)?
        .ok_or_else(|| worker::Error::RustError("upload row missing after write".into()))
}

fn read_upload_by_identity(
    sql: &SqlStorage,
    job_id: &str,
    shard_index: u32,
    kind: &str,
    name: &str,
    sha256: &str,
) -> worker::Result<Option<UploadRow>> {
    let rows: Vec<UploadRow> = sql
        .exec(
            &format!(
                "SELECT {UPLOAD_COLUMNS} FROM upload \
                 WHERE job_id = ?1 AND shard_index = ?2 AND kind = ?3 AND name = ?4 AND sha256 = ?5"
            ),
            vec![
                SqlStorageValue::from(job_id),
                SqlStorageValue::try_from_i64(i64::from(shard_index))?,
                SqlStorageValue::from(kind),
                SqlStorageValue::from(name),
                SqlStorageValue::from(sha256),
            ],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

#[allow(clippy::too_many_arguments)]
fn insert_upload(
    sql: &SqlStorage,
    id: &str,
    job_id: &str,
    shard_index: u32,
    kind: &str,
    name: &str,
    scope: &str,
    sha256: &str,
    size_bytes: i64,
    content_type: &str,
    r2_key: &str,
    accepted_seq: i64,
    created_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        &format!(
            "INSERT INTO upload ({UPLOAD_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10, ?11, ?12)"
        ),
        vec![
            SqlStorageValue::from(id),
            SqlStorageValue::from(job_id),
            SqlStorageValue::try_from_i64(i64::from(shard_index))?,
            SqlStorageValue::from(kind),
            SqlStorageValue::from(name),
            SqlStorageValue::from(scope),
            SqlStorageValue::from(sha256),
            SqlStorageValue::try_from_i64(size_bytes)?,
            SqlStorageValue::from(content_type),
            SqlStorageValue::from(r2_key),
            SqlStorageValue::try_from_i64(accepted_seq)?,
            SqlStorageValue::try_from_i64(created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn update_upload_state(sql: &SqlStorage, id: &str, state: &str) -> worker::Result<()> {
    sql.exec(
        "UPDATE upload SET state = ?1 WHERE id = ?2",
        vec![SqlStorageValue::from(state), SqlStorageValue::from(id)],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

const REPORT_COLUMNS: &str = "id, job_id, shard_index, kind, name, scope, content_sha256, upload_id, accepted_seq, created_at, is_canonical, parsed, summary, r2_key";

fn read_report(sql: &SqlStorage, id: &str) -> worker::Result<Option<ReportRow>> {
    let rows: Vec<ReportRow> = sql
        .exec(
            &format!("SELECT {REPORT_COLUMNS} FROM report WHERE id = ?1"),
            vec![SqlStorageValue::from(id)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn require_report(sql: &SqlStorage, id: &str) -> worker::Result<ReportRow> {
    read_report(sql, id)?
        .ok_or_else(|| worker::Error::RustError("report row missing after write".into()))
}

fn read_report_by_identity(
    sql: &SqlStorage,
    job_id: &str,
    shard_index: u32,
    kind: &str,
    name: &str,
    content_sha256: &str,
) -> worker::Result<Option<ReportRow>> {
    let rows: Vec<ReportRow> = sql
        .exec(
            &format!(
                "SELECT {REPORT_COLUMNS} FROM report \
                 WHERE job_id = ?1 AND shard_index = ?2 AND kind = ?3 AND name = ?4 AND content_sha256 = ?5"
            ),
            vec![
                SqlStorageValue::from(job_id),
                SqlStorageValue::try_from_i64(i64::from(shard_index))?,
                SqlStorageValue::from(kind),
                SqlStorageValue::from(name),
                SqlStorageValue::from(content_sha256),
            ],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

/// Every canonical, parsed report this run's DO storage currently has —
/// `finalize_test_stats`'s input. This DO instance *is* one run, so no
/// `run_id` filter is needed (same "run_id implicit" pattern as every
/// other DO-local query in this module). `is_canonical = 1` already
/// resolves to the highest-`accepted_seq` content per slot
/// ([`unset_other_canonical_reports`]'s only caller flips the rest), and
/// `parsed = 1` excludes raw unparsed blobs/coverage reports, which carry
/// no per-test-case data (analytics.md's "Raw, unparsed blob uploads
/// never reach this array").
///
/// **No separate frozen-report-IDs snapshot.** analytics.md's Idempotency
/// section's "This implementation's snapshot" addendum (right after its
/// general "persists the frozen report IDs ... before dispatching"
/// description) explains why this live re-query — not a separately
/// persisted snapshot — is restart-safe here: `handle_submit_report`
/// rejects (409) once the run is terminal
/// (`logic::upload_allowed_for_shard`'s `run_terminal` check), and
/// `finalize_test_stats` only ever runs after that terminal transition,
/// so this `SELECT`'s result set is already frozen by construction
/// before it is ever read.
fn read_canonical_parsed_reports(sql: &SqlStorage) -> worker::Result<Vec<ReportRow>> {
    sql.exec(
        &format!("SELECT {REPORT_COLUMNS} FROM report WHERE is_canonical = 1 AND parsed = 1"),
        None,
    )?
    .to_array()
}

#[allow(clippy::too_many_arguments)]
fn insert_report(
    sql: &SqlStorage,
    id: &str,
    job_id: &str,
    shard_index: u32,
    kind: &str,
    name: &str,
    scope: &str,
    content_sha256: &str,
    upload_id: Option<&str>,
    accepted_seq: i64,
    created_at_ms: i64,
    parsed: i64,
    summary: Option<&str>,
    r2_key: &str,
) -> worker::Result<()> {
    sql.exec(
        &format!(
            "INSERT INTO report ({REPORT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13)"
        ),
        vec![
            SqlStorageValue::from(id),
            SqlStorageValue::from(job_id),
            SqlStorageValue::try_from_i64(i64::from(shard_index))?,
            SqlStorageValue::from(kind),
            SqlStorageValue::from(name),
            SqlStorageValue::from(scope),
            SqlStorageValue::from(content_sha256),
            SqlStorageValue::from(upload_id.map(str::to_string)),
            SqlStorageValue::try_from_i64(accepted_seq)?,
            SqlStorageValue::try_from_i64(created_at_ms)?,
            SqlStorageValue::try_from_i64(parsed)?,
            SqlStorageValue::from(summary.map(str::to_string)),
            SqlStorageValue::from(r2_key),
        ],
    )?;
    Ok(())
}

fn unset_other_canonical_reports(
    sql: &SqlStorage,
    job_id: &str,
    shard_index: u32,
    kind: &str,
    name: &str,
    keep_id: &str,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE report SET is_canonical = 0 \
         WHERE job_id = ?1 AND shard_index = ?2 AND kind = ?3 AND name = ?4 AND id != ?5",
        vec![
            SqlStorageValue::from(job_id),
            SqlStorageValue::try_from_i64(i64::from(shard_index))?,
            SqlStorageValue::from(kind),
            SqlStorageValue::from(name),
            SqlStorageValue::from(keep_id),
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Check Runs
// ---------------------------------------------------------------------------

const CHECK_RUN_COLUMNS: &str = "check_name, github_check_run_id, status, conclusion, created_at";

fn read_check_run(sql: &SqlStorage, check_name: &str) -> worker::Result<Option<CheckRunRow>> {
    let rows: Vec<CheckRunRow> = sql
        .exec(
            &format!("SELECT {CHECK_RUN_COLUMNS} FROM check_run WHERE check_name = ?1"),
            vec![SqlStorageValue::from(check_name)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn read_all_check_runs(sql: &SqlStorage) -> worker::Result<Vec<CheckRunRow>> {
    sql.exec(&format!("SELECT {CHECK_RUN_COLUMNS} FROM check_run"), None)?
        .to_array()
}

fn insert_check_run(
    sql: &SqlStorage,
    check_name: &str,
    github_check_run_id: u64,
    status: &str,
    created_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO check_run (check_name, github_check_run_id, status, conclusion, created_at) \
         VALUES (?1, ?2, ?3, NULL, ?4)",
        vec![
            SqlStorageValue::from(check_name),
            SqlStorageValue::try_from_i64(github_check_run_id as i64)?,
            SqlStorageValue::from(status),
            SqlStorageValue::try_from_i64(created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn update_check_run_state(
    sql: &SqlStorage,
    check_name: &str,
    status: &str,
    conclusion: Option<&str>,
) -> worker::Result<()> {
    match conclusion {
        Some(c) => sql.exec(
            "UPDATE check_run SET status = ?1, conclusion = ?2 WHERE check_name = ?3",
            vec![
                SqlStorageValue::from(status),
                SqlStorageValue::from(c),
                SqlStorageValue::from(check_name),
            ],
        )?,
        None => sql.exec(
            "UPDATE check_run SET status = ?1 WHERE check_name = ?2",
            vec![
                SqlStorageValue::from(status),
                SqlStorageValue::from(check_name),
            ],
        )?,
    };
    Ok(())
}

// ---------------------------------------------------------------------------
// Nodes (`startNode`/completion/`ack`, module docs' Nodes section)
// ---------------------------------------------------------------------------

const NODE_COLUMNS: &str = "node_id, spec_hash, status, check_name, result, started_at, completed_at, acked, image, command";

fn read_node(sql: &SqlStorage, node_id: &str) -> worker::Result<Option<NodeRow>> {
    let rows: Vec<NodeRow> = sql
        .exec(
            &format!("SELECT {NODE_COLUMNS} FROM node WHERE node_id = ?1"),
            vec![SqlStorageValue::from(node_id)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn require_node(sql: &SqlStorage, node_id: &str) -> worker::Result<NodeRow> {
    read_node(sql, node_id)?
        .ok_or_else(|| worker::Error::RustError("node row missing after write".into()))
}

/// Every node this run has ever started, for [`logic::nodes_to_cancel`]'s
/// input — `handle_cancel_run`'s only caller.
fn read_all_nodes(sql: &SqlStorage) -> worker::Result<Vec<NodeRow>> {
    sql.exec(&format!("SELECT {NODE_COLUMNS} FROM node"), None)?
        .to_array()
}

#[allow(clippy::too_many_arguments)]
fn insert_node(
    sql: &SqlStorage,
    node_id: &str,
    spec_hash: &str,
    check_name: Option<&str>,
    image: &str,
    command_json: &str,
    started_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO node (node_id, spec_hash, status, check_name, started_at, image, command) \
         VALUES (?1, ?2, 'running', ?3, ?4, ?5, ?6)",
        vec![
            SqlStorageValue::from(node_id),
            SqlStorageValue::from(spec_hash),
            SqlStorageValue::from(check_name.map(str::to_string)),
            SqlStorageValue::try_from_i64(started_at_ms)?,
            SqlStorageValue::from(image),
            SqlStorageValue::from(command_json),
        ],
    )?;
    Ok(())
}

/// Records a node's terminal status and result
/// ([`logic::CompleteNodeDecision::Recorded`]). Never called for the
/// `DroppedCancelled` decision — that decision writes nothing at all,
/// per "late completion events for cancelled nodes are dropped".
fn update_node_status(
    sql: &SqlStorage,
    node_id: &str,
    status: &str,
    result: Option<&str>,
    completed_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE node SET status = ?1, result = ?2, completed_at = ?3 WHERE node_id = ?4",
        vec![
            SqlStorageValue::from(status),
            SqlStorageValue::from(result.map(str::to_string)),
            SqlStorageValue::try_from_i64(completed_at_ms)?,
            SqlStorageValue::from(node_id),
        ],
    )?;
    Ok(())
}

fn update_node_ack(sql: &SqlStorage, node_id: &str) -> worker::Result<()> {
    sql.exec(
        "UPDATE node SET acked = 1 WHERE node_id = ?1",
        vec![SqlStorageValue::from(node_id)],
    )?;
    Ok(())
}

/// Marks `node_id` `Cancelled` — `handle_cancel_run`'s per-node write,
/// for each id [`logic::nodes_to_cancel`] returned. Unlike
/// `update_node_status`, this never touches `result`: a cancelled node
/// never produced one.
fn mark_node_cancelled(
    sql: &SqlStorage,
    node_id: &str,
    completed_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE node SET status = 'cancelled', completed_at = ?1 WHERE node_id = ?2",
        vec![
            SqlStorageValue::try_from_i64(completed_at_ms)?,
            SqlStorageValue::from(node_id),
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shard groups / merge barrier (`job_group`/`shard_state`, module docs'
// "Shard groups / merge barrier" section).
// ---------------------------------------------------------------------------

const JOB_GROUP_COLUMNS: &str =
    "job_name, expected_total, fail_fast, merge_on_failure, merge_job_id, status";

fn read_job_group(sql: &SqlStorage, job_name: &str) -> worker::Result<Option<ShardGroupRow>> {
    let rows: Vec<ShardGroupRow> = sql
        .exec(
            &format!("SELECT {JOB_GROUP_COLUMNS} FROM job_group WHERE job_name = ?1"),
            vec![SqlStorageValue::from(job_name)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn insert_job_group(
    sql: &SqlStorage,
    job_name: &str,
    expected_total: u32,
    fail_fast: bool,
    merge_on_failure: &str,
) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO job_group (job_name, expected_total, fail_fast, merge_on_failure, merge_job_id, status) \
         VALUES (?1, ?2, ?3, ?4, NULL, 'running')",
        vec![
            SqlStorageValue::from(job_name),
            SqlStorageValue::try_from_i64(i64::from(expected_total))?,
            SqlStorageValue::try_from_i64(i64::from(fail_fast))?,
            SqlStorageValue::from(merge_on_failure),
        ],
    )?;
    Ok(())
}

/// Transitions a group out of `running` once [`logic::evaluate_barrier`]
/// returns a decision — `failed` for `FailFastTriggered`, `satisfied` for
/// `Satisfied`. Never called back to `running`: both are terminal group
/// states (module docs' "Shard groups / merge barrier" section).
fn update_job_group_status(sql: &SqlStorage, job_name: &str, status: &str) -> worker::Result<()> {
    sql.exec(
        "UPDATE job_group SET status = ?1 WHERE job_name = ?2",
        vec![
            SqlStorageValue::from(status),
            SqlStorageValue::from(job_name),
        ],
    )?;
    Ok(())
}

/// The exact `(job_name, idx, attempt)` row's status, if a terminal
/// ingest call already recorded one — [`logic::resolve_shard_terminal`]'s
/// `existing_status` input.
fn read_shard_state_status(
    sql: &SqlStorage,
    job_name: &str,
    idx: u32,
    attempt: u32,
) -> worker::Result<Option<String>> {
    let rows: Vec<ShardStateDbRow> = sql
        .exec(
            "SELECT idx, attempt, status FROM shard_state WHERE job_name = ?1 AND idx = ?2 AND attempt = ?3",
            vec![
                SqlStorageValue::from(job_name),
                SqlStorageValue::try_from_i64(i64::from(idx))?,
                SqlStorageValue::try_from_i64(i64::from(attempt))?,
            ],
        )?
        .to_array()?;
    Ok(rows.into_iter().next().map(|r| r.status))
}

#[allow(clippy::too_many_arguments)]
fn insert_shard_state(
    sql: &SqlStorage,
    job_name: &str,
    idx: u32,
    attempt: u32,
    status: &str,
    report_key: Option<&str>,
    duration_ms: Option<i64>,
    finished_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO shard_state (job_name, idx, attempt, status, report_key, duration_ms, finished_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        vec![
            SqlStorageValue::from(job_name),
            SqlStorageValue::try_from_i64(i64::from(idx))?,
            SqlStorageValue::try_from_i64(i64::from(attempt))?,
            SqlStorageValue::from(status),
            SqlStorageValue::from(report_key.map(str::to_string)),
            SqlStorageValue::from(duration_ms),
            SqlStorageValue::try_from_i64(finished_at_ms)?,
        ],
    )?;
    Ok(())
}

/// Every terminal row recorded so far for `job_name`, across every
/// attempt of every shard — [`logic::latest_attempt_per_shard`]'s input.
/// Every row in `shard_state` is already terminal by construction (this
/// round only ever inserts a row from a terminal ingest call; see
/// `ShardStateDbRow`'s doc comment), so no status filter is needed here.
fn read_all_shard_terminal_rows(
    sql: &SqlStorage,
    job_name: &str,
) -> worker::Result<Vec<logic::ShardStateRow>> {
    let rows: Vec<ShardStateDbRow> = sql
        .exec(
            "SELECT idx, attempt, status FROM shard_state WHERE job_name = ?1",
            vec![SqlStorageValue::from(job_name)],
        )?
        .to_array()?;
    rows.into_iter()
        .map(|row| {
            let status = logic::ShardTerminalStatus::from_db_str(&row.status).ok_or_else(|| {
                worker::Error::RustError(format!("unknown shard_state status: {}", row.status))
            })?;
            Ok(logic::ShardStateRow {
                idx: row.idx as u32,
                attempt: row.attempt as u32,
                status,
            })
        })
        .collect()
}

/// `RunCoordinator`'s own per-run monotonic counter for `accepted_seq`
/// (byo-ci.md's Idempotency section), derived from the DO's own storage
/// rather than a dedicated counter column: one more than the highest
/// `accepted_seq` already assigned to any upload or report this run has
/// accepted.
fn next_accepted_seq(sql: &SqlStorage) -> worker::Result<i64> {
    let upload_max: Vec<MaxSeqRow> = sql
        .exec(
            "SELECT COALESCE(MAX(accepted_seq), 0) as m FROM upload",
            None,
        )?
        .to_array()?;
    let report_max: Vec<MaxSeqRow> = sql
        .exec(
            "SELECT COALESCE(MAX(accepted_seq), 0) as m FROM report",
            None,
        )?
        .to_array()?;
    let u = upload_max.first().map(|r| r.m).unwrap_or(0);
    let r = report_max.first().map(|r| r.m).unwrap_or(0);
    Ok(u.max(r) + 1)
}

/// Infers a shard's conclusion from its accepted, parsed, canonical reports
/// when `CompleteShard` omits `--conclusion` (byo-ci.md's `cloud-ci upload`
/// section: "failure if a report has a failed test or an error-level
/// diagnostic, else success"). Reports with no parser (`parsed = 0`, e.g.
/// `lcov` coverage, which has no pass/fail concept) never contribute a
/// failure signal here.
fn infer_shard_conclusion(
    sql: &SqlStorage,
    job_id: &str,
    shard_index: u32,
) -> worker::Result<Conclusion> {
    let rows: Vec<SummaryRow> = sql
        .exec(
            "SELECT summary FROM report \
             WHERE job_id = ?1 AND shard_index = ?2 AND is_canonical = 1 AND parsed = 1 AND summary IS NOT NULL",
            vec![
                SqlStorageValue::from(job_id),
                SqlStorageValue::try_from_i64(i64::from(shard_index))?,
            ],
        )?
        .to_array()?;
    for row in rows {
        let Some(summary) = row.summary else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(&summary)
            .map_err(|e| worker::Error::RustError(format!("cannot decode report summary: {e}")))?;
        let failed = value.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
        let errored = value.get("errored").and_then(|v| v.as_u64()).unwrap_or(0);
        if failed > 0 || errored > 0 {
            return Ok(Conclusion::CONCLUSION_FAILURE);
        }
    }
    Ok(Conclusion::CONCLUSION_SUCCESS)
}

/// Dispatches to whichever `cloud-ci-reports` parser exists for
/// `report_kind` (case-insensitive, matching the CLI names in
/// byo-ci.md's "Supported report formats"). A kind with no parser yet, or
/// bytes that fail to parse, store the raw bytes unparsed rather than
/// erroring — byo-ci.md's Failure modes: "Raw bytes are still stored in R2
/// ... but no `report_uploads` row is written" (here: `parsed = 0`,
/// `summary = NULL`, but the `report` row itself still exists).
fn parse_report(kind: &str, bytes: &[u8]) -> (i64, Option<String>) {
    let summary = match kind.to_ascii_lowercase().as_str() {
        "junit" => cloud_ci_reports::junit::parse(bytes)
            .ok()
            .map(summarize_test_suites),
        "vitest" => cloud_ci_reports::vitest::parse(bytes)
            .ok()
            .map(summarize_test_suites),
        "playwright" => cloud_ci_reports::playwright::parse(bytes)
            .ok()
            .map(summarize_test_suites),
        "lcov" => cloud_ci_reports::lcov::parse(bytes)
            .ok()
            .map(summarize_lcov),
        _ => None,
    };
    match summary {
        Some(json) => (1, Some(json)),
        None => (0, None),
    }
}

/// Extracts per-test-case outcomes from a report's full bytes, for
/// `finalize_test_stats` — a richer sibling of [`parse_report`] above:
/// that function only keeps aggregate counts and failing-test
/// names/messages (`reports.summary`'s JSON shape), which is not enough
/// for `test_stats`' per-test duration/status/history columns
/// (analytics.md's D1 rollup tables: `report_uploads`/`reports.summary`
/// never retains individual durations). `lcov` (and any other kind with
/// no `TestSuites`-shaped parser) returns `None` — coverage reports have
/// no pass/fail test-case concept to contribute.
fn parse_test_outcomes(kind: &str, bytes: &[u8]) -> Option<Vec<logic::TestOutcomeRow>> {
    let suites = match kind.to_ascii_lowercase().as_str() {
        "junit" => cloud_ci_reports::junit::parse(bytes).ok()?,
        "vitest" => cloud_ci_reports::vitest::parse(bytes).ok()?,
        "playwright" => cloud_ci_reports::playwright::parse(bytes).ok()?,
        _ => return None,
    };
    let mut rows = Vec::new();
    for suite in &suites.suites {
        for tc in &suite.test_cases {
            // `file` is populated by Vitest/Playwright always, and by
            // JUnit writers that follow xUnit's legacy `file` attribute
            // (see `cloud_ci_reports::TestCase::file`'s doc comment); when
            // absent, the enclosing `<testsuite>`'s name is the closest
            // stand-in cloud-ci-reports gives us for "what file/module is
            // this test in".
            let file_path = tc.file.clone().unwrap_or_else(|| suite.name.clone());
            let full_name = logic::full_test_name(tc.classname.as_deref(), &tc.name);
            let test_id = logic::test_id(&file_path, &full_name);
            let duration_ms = (tc.time.unwrap_or(0.0) * 1000.0).round() as i64;
            let outcome = match &tc.outcome {
                cloud_ci_reports::Outcome::Passed => logic::TestOutcomeKind::Passed,
                // `Errored` folds into `Failed` — see `TestOutcomeKind`'s
                // doc comment.
                cloud_ci_reports::Outcome::Failed(_) | cloud_ci_reports::Outcome::Errored(_) => {
                    logic::TestOutcomeKind::Failed
                }
                cloud_ci_reports::Outcome::Skipped(_) => logic::TestOutcomeKind::Skipped,
            };
            rows.push(logic::TestOutcomeRow {
                test_id,
                file_path,
                test_name: tc.name.clone(),
                duration_ms,
                outcome,
            });
        }
    }
    Some(rows)
}

fn summarize_test_suites(suites: cloud_ci_reports::TestSuites) -> String {
    let mut passed = 0u64;
    let mut failed = 0u64;
    let mut skipped = 0u64;
    let mut errored = 0u64;
    let mut failed_tests = Vec::new();
    for suite in &suites.suites {
        for tc in &suite.test_cases {
            match &tc.outcome {
                cloud_ci_reports::Outcome::Passed => passed += 1,
                cloud_ci_reports::Outcome::Skipped(_) => skipped += 1,
                cloud_ci_reports::Outcome::Failed(f) => {
                    failed += 1;
                    failed_tests.push(serde_json::json!({"name": tc.name, "message": f.message}));
                }
                cloud_ci_reports::Outcome::Errored(f) => {
                    errored += 1;
                    failed_tests.push(serde_json::json!({"name": tc.name, "message": f.message}));
                }
            }
        }
    }
    serde_json::json!({
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "errored": errored,
        "failed_tests": failed_tests,
    })
    .to_string()
}

fn summarize_lcov(report: cloud_ci_reports::LcovReport) -> String {
    let mut total_lines = 0u64;
    let mut hit_lines = 0u64;
    for file in &report.source_files {
        for line in &file.lines {
            total_lines += 1;
            if line.hit_count > 0 {
                hit_lines += 1;
            }
        }
    }
    serde_json::json!({
        "total_lines": total_lines,
        "hit_lines": hit_lines,
        "files": report.source_files.len(),
    })
    .to_string()
}

fn decode_string_list(json: &str) -> worker::Result<Vec<String>> {
    serde_json::from_str(json)
        .map_err(|e| worker::Error::RustError(format!("cannot decode expect_jobs: {e}")))
}

fn error_response(status: u16, message: &str) -> worker::Result<Response> {
    Ok(Response::from_json(&ErrorBody {
        error: message.to_string(),
        code: None,
    })?
    .with_status(status))
}

/// Same as [`error_response`], but tags the body with `code` so
/// [`RunCoordinatorStore::call`] can distinguish this error kind from a
/// generic 409 conflict — currently only used for
/// [`CoordinatorError::NondeterministicReplay`].
fn error_response_with_code(status: u16, message: &str, code: &str) -> worker::Result<Response> {
    Ok(Response::from_json(&ErrorBody {
        error: message.to_string(),
        code: Some(code.to_string()),
    })?
    .with_status(status))
}

// ---------------------------------------------------------------------------
// Client wrapper
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum CoordinatorError {
    NotFound,
    Conflict(String),
    /// `startNode` named an id with a spec hash different from the one
    /// already recorded for it — dynamic-pipelines.md's "rejected as
    /// nondeterministic; run fails". Kept distinct from `Conflict` so a
    /// future Workflow-integration caller can tell "ordinary 409" from
    /// "the run itself needs to be failed" without string-matching the
    /// message (see module docs' Nodes section for who actually fails
    /// the run on this).
    NondeterministicReplay,
    Internal(String),
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "run not found"),
            Self::NondeterministicReplay => {
                write!(
                    f,
                    "start_node spec hash differs from a prior start for this node id"
                )
            }
            Self::Conflict(msg) | Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

/// Typed client for one run's Durable Object. The rest of the Worker uses
/// this; raw stubs never leave this module.
pub struct RunCoordinatorStore {
    stub: Stub,
}

impl RunCoordinatorStore {
    pub fn new(env: &Env, do_name: &str) -> Result<Self, CoordinatorError> {
        let stub = env
            .durable_object(RUN_COORDINATOR_BINDING)
            .and_then(|namespace| namespace.id_from_name(do_name)?.get_stub())
            .map_err(|e| CoordinatorError::Internal(format!("run coordinator unavailable: {e}")))?;
        Ok(Self { stub })
    }

    pub async fn begin_run(
        &self,
        req: &BeginRunRequest,
    ) -> Result<BeginRunOutcome, CoordinatorError> {
        self.call(Method::Post, "/begin-run", Some(req)).await
    }

    pub async fn start_job(
        &self,
        req: &StartJobRequest,
    ) -> Result<StartJobOutcome, CoordinatorError> {
        self.call(Method::Post, "/start-job", Some(req)).await
    }

    pub async fn get_run(&self) -> Result<GetRunOutcome, CoordinatorError> {
        self.call::<(), _>(Method::Get, "/get-run", None).await
    }

    pub async fn create_upload(
        &self,
        req: &CreateUploadRequest,
    ) -> Result<CreateUploadOutcome, CoordinatorError> {
        self.call(Method::Post, "/create-upload", Some(req)).await
    }

    pub async fn complete_upload(
        &self,
        req: &CompleteUploadRequest,
    ) -> Result<CompleteUploadOutcome, CoordinatorError> {
        self.call(Method::Post, "/complete-upload", Some(req)).await
    }

    pub async fn submit_report(
        &self,
        req: &SubmitReportRequest,
    ) -> Result<SubmitReportOutcome, CoordinatorError> {
        self.call(Method::Post, "/submit-report", Some(req)).await
    }

    pub async fn complete_shard(
        &self,
        req: &CompleteShardRequest,
    ) -> Result<CompleteShardOutcome, CoordinatorError> {
        self.call(Method::Post, "/complete-shard", Some(req)).await
    }

    pub async fn close_run(&self) -> Result<CloseRunOutcome, CoordinatorError> {
        self.call::<(), _>(Method::Post, "/close-run", None).await
    }

    pub async fn start_node(
        &self,
        req: &StartNodeRequest,
    ) -> Result<StartNodeOutcome, CoordinatorError> {
        self.call(Method::Post, "/start-node", Some(req)).await
    }

    pub async fn complete_node(
        &self,
        req: &CompleteNodeRequest,
    ) -> Result<CompleteNodeOutcome, CoordinatorError> {
        self.call(Method::Post, "/complete-node", Some(req)).await
    }

    pub async fn ack_node(&self, req: &AckNodeRequest) -> Result<AckNodeOutcome, CoordinatorError> {
        self.call(Method::Post, "/ack-node", Some(req)).await
    }

    pub async fn cancel_run(&self) -> Result<CancelRunOutcome, CoordinatorError> {
        self.call::<(), _>(Method::Post, "/cancel-run", None).await
    }

    pub async fn register_shard_group(
        &self,
        req: &RegisterShardGroupRequest,
    ) -> Result<RegisterShardGroupOutcome, CoordinatorError> {
        self.call(Method::Post, "/register-shard-group", Some(req))
            .await
    }

    pub async fn shard_terminal(
        &self,
        req: &ShardTerminalRequest,
    ) -> Result<ShardTerminalOutcome, CoordinatorError> {
        self.call(Method::Post, "/shard-terminal", Some(req)).await
    }

    async fn call<B, R>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<R, CoordinatorError>
    where
        B: Serialize,
        R: DeserializeOwned,
    {
        let mut init = RequestInit::new();
        init.with_method(method);
        if let Some(body) = body {
            let encoded = serde_json::to_string(body)
                .map_err(|e| CoordinatorError::Internal(format!("cannot encode {path}: {e}")))?;
            init.with_body(Some(JsValue::from_str(&encoded)));
        }
        let request = Request::new_with_init(&format!("{STUB_ORIGIN}{path}"), &init)
            .map_err(|e| stub_error(path, e))?;
        let mut response = self
            .stub
            .fetch_with_request(request)
            .await
            .map_err(|e| stub_error(path, e))?;

        match response.status_code() {
            200..=299 => response.json::<R>().await.map_err(|e| stub_error(path, e)),
            404 => Err(CoordinatorError::NotFound),
            409 => {
                let body = response.json::<ErrorBody>().await.ok();
                match body {
                    Some(ErrorBody {
                        code: Some(code), ..
                    }) if code == "nondeterministic_replay" => {
                        Err(CoordinatorError::NondeterministicReplay)
                    }
                    Some(ErrorBody { error, .. }) => Err(CoordinatorError::Conflict(error)),
                    None => Err(CoordinatorError::Conflict(
                        "shard_total or expect_jobs conflict".to_string(),
                    )),
                }
            }
            status => {
                let detail = response.text().await.unwrap_or_default();
                Err(CoordinatorError::Internal(format!(
                    "run coordinator rejected {path}: {status} {detail}"
                )))
            }
        }
    }
}

fn stub_error(path: &str, e: worker::Error) -> CoordinatorError {
    CoordinatorError::Internal(format!("run coordinator error on {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn do_name_is_deterministic_for_the_same_identity() {
        let a = do_name(1, "sha", "run-key", 2);
        let b = do_name(1, "sha", "run-key", 2);
        assert_eq!(a, b);
    }

    #[test]
    fn do_name_does_not_collide_across_a_field_boundary_shift() {
        // A naive colon-join would make these two different identities
        // collide: "1:a:b:1:2" either way. `run_key`/`sha` are free-form,
        // caller-supplied text for external runs (docs/design/byo-ci.md),
        // so this is a real cross-caller collision, not a theoretical one.
        let a = do_name(1, "a", "b:1", 2);
        let b = do_name(1, "a:b", "1", 2);
        assert_ne!(a, b, "different run identities must not share a DO name");
    }

    #[test]
    fn do_name_differs_when_any_field_differs() {
        let base = do_name(1, "sha", "run-key", 1);
        assert_ne!(base, do_name(2, "sha", "run-key", 1));
        assert_ne!(base, do_name(1, "other-sha", "run-key", 1));
        assert_ne!(base, do_name(1, "sha", "other-run-key", 1));
        assert_ne!(base, do_name(1, "sha", "run-key", 2));
    }
}
