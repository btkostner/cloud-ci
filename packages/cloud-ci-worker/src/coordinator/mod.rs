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

pub mod logic;

use buffa::Enumeration;
use cloud_ci_proto::ingest::v1::{BeginRunRequest, JobState, RunStatus, StartJobRequest, Trigger};
use logic::RunState;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
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
pub fn do_name(repo_id: u64, sha: &str, run_key: &str, attempt: u32) -> String {
    format!("{repo_id}:{sha}:{run_key}:{attempt}")
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

#[derive(Debug, Serialize)]
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
}

fn trigger_db_name(trigger: Trigger) -> &'static str {
    trigger.proto_name()
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
            "INSERT INTO jobs (id, run_id, job_name, shard_total, runner_label, check_names) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (id) DO UPDATE SET \
               shard_total = excluded.shard_total, \
               runner_label = excluded.runner_label, \
               check_names = excluded.check_names",
        )
        .bind(&[
            JsValue::from_str(&row.id),
            JsValue::from_str(run_id),
            JsValue::from_str(&row.job_name),
            JsValue::from_f64(row.shard_total as f64),
            JsValue::from_str(&row.runner_label),
            JsValue::from_str(&row.check_names),
        ])?
        .run()
        .await?;
        Ok(())
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
            check_names TEXT NOT NULL \
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

fn read_job(sql: &SqlStorage, job_name: &str) -> worker::Result<Option<JobRow>> {
    let rows: Vec<JobRow> = sql
        .exec(
            "SELECT id, job_name, shard_total, runner_label, check_names FROM job WHERE job_name = ?1",
            vec![SqlStorageValue::from(job_name)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn read_all_jobs(sql: &SqlStorage) -> worker::Result<Vec<JobRow>> {
    sql.exec(
        "SELECT id, job_name, shard_total, runner_label, check_names FROM job ORDER BY job_name",
        None,
    )?
    .to_array()
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
                let detail = response.text().await.unwrap_or_default();
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
