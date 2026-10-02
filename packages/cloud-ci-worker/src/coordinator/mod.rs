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

#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    error: String,
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
}

#[derive(Debug, Clone, Deserialize)]
struct MaxSeqRow {
    m: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct SummaryRow {
    summary: Option<String>,
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

#[durable_object(fetch)]
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
            (Method::Post, "/close-run") => self.handle_close_run(&sql).await,
            _ => error_response(404, "unknown RunCoordinator route"),
        }
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
                )?;
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

        let (content_sha256, upload_id, bytes) = match &req.source {
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
                (upload.sha256.clone(), Some(upload.id.clone()), bytes)
            }
            Some(submit_report_request::Source::InlineData(data)) => {
                (hex_sha256(data), None, data.clone())
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
            self.handle_close_run(sql).await?;
        }
        Ok(())
    }

    /// Closes the run: for each job, marks every shard that never
    /// uploaded `missing` and concludes the job (failing it if it has any
    /// missing shard), then rolls the jobs' conclusions up into the run's
    /// own and moves the run to a terminal `RunState`
    /// (docs/design/byo-ci.md's "Completion semantics": "the run's
    /// conclusion is the worst job conclusion"). Two callers trigger this
    /// round: `lib.rs::handle_workflow_run_event`, once it has correlated
    /// an incoming `workflow_run` `completed` webhook to this run (see
    /// the `workflow_run` module docs for the correlation strategy), and
    /// [`Self::maybe_close_for_expect_jobs`], once every job named in a
    /// run's declared `expect_jobs` has started and concluded. The
    /// timeout alarm (byo-ci.md's third close trigger) is not built
    /// either way — it needs a Durable Object alarm, separate work.
    ///
    /// Idempotent: a run already in a terminal state is a no-op ack,
    /// covering GitHub's at-least-once webhook redelivery without
    /// re-running (and potentially re-emitting side effects from) the
    /// close logic a second time.
    async fn handle_close_run(&self, sql: &SqlStorage) -> worker::Result<Response> {
        let Some(run_row) = read_run(sql)? else {
            return error_response(404, "run not found");
        };
        if run_state_of(&run_row)?.is_terminal() {
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
        let next_state = logic::run_state_from_conclusion(run_conclusion);
        update_run_status(sql, &run_row.id, next_state)?;
        let run_row = require_run(sql)?;
        self.project_run_to_d1(&run_row).await?;
        self.finalize_check_runs(sql, &run_row).await?;

        let status = run_status_of(&run_row)?;
        Response::from_json(&CloseRunOutcome {
            run_id: run_row.id,
            status,
        })
    }

    async fn project_run_to_d1(&self, row: &RunRow) -> worker::Result<()> {
        let db = self.env.d1("DB")?;
        db.prepare(
            "INSERT INTO runs (id, repo_id, sha, run_key, attempt, status, expect_jobs, trigger, external_url, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
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
            "INSERT INTO reports (id, job_id, shard_index, kind, name, scope, content_sha256, upload_id, accepted_seq, created_at, is_canonical, parsed, summary) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
            created_at INTEGER NOT NULL \
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
            summary TEXT \
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
    Ok(())
}

fn read_run(sql: &SqlStorage) -> worker::Result<Option<RunRow>> {
    let rows: Vec<RunRow> = sql
        .exec(
            "SELECT id, repo_id, sha, run_key, attempt, status, expect_jobs, trigger, external_url, created_at FROM run LIMIT 1",
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
) -> worker::Result<()> {
    let expect_jobs_json = expect_jobs
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| worker::Error::RustError(format!("cannot encode expect_jobs: {e}")))?;
    let repo_id_value = SqlStorageValue::try_from_i64(repo_id as i64)?;
    sql.exec(
        "INSERT INTO run (id, repo_id, sha, run_key, attempt, status, expect_jobs, trigger, external_url, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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

const REPORT_COLUMNS: &str = "id, job_id, shard_index, kind, name, scope, content_sha256, upload_id, accepted_seq, created_at, is_canonical, parsed, summary";

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
) -> worker::Result<()> {
    sql.exec(
        &format!(
            "INSERT INTO report ({REPORT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12)"
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
    Internal(String),
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "run not found"),
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
                let detail = response
                    .json::<ErrorBody>()
                    .await
                    .map(|b| b.error)
                    .unwrap_or_else(|_| "shard_total or expect_jobs conflict".to_string());
                Err(CoordinatorError::Conflict(detail))
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
