//! The per-repo `RepoState` Durable Object (docs/architecture.md's
//! "System shape" diagram — `RS[RepoState DO<br/>one per repo]` — and its
//! "Core flows" § "Managed run" step 2: "requests admission from the
//! repo's `RepoState` DO (concurrency rules: cancel-superseded, per-repo
//! limits) using a stable identity — `(repo, sha, run key, attempt)` ...
//! so a redelivered or reordered webhook can never admit a duplicate
//! run").
//!
//! One instance per `repo_id`, addressed by [`do_name`] —
//! `idFromName("{repo_id}")`. `repo_id` is a single plain integer (the
//! GitHub repo id, architecture.md's "Data model" table: "GitHub repo id
//! (not name — renames happen)"), so — same reasoning as
//! `pull_request_state::do_name`'s doc comment — a decimal string is
//! already injective by construction; no hashing or length-prefixing is
//! needed, unlike `coordinator::do_name`, whose tuple includes free-form
//! caller-supplied `sha`/`run_key` text.
//!
//! # Scope boundary (this round)
//!
//! This round builds `RepoState`'s admission/concurrency state machine
//! only: given a stable run identity and `settings.yml`'s concurrency
//! caps (passed in as plain RPC parameters — this round does **not**
//! fetch `settings.yml` itself, see below), decide admit / reject /
//! cancel-and-admit, and track just enough SQLite state to make that
//! decision correctly and idempotently. It deliberately does **not**
//! build:
//!
//! - **Pipeline discovery.** Fetching `.cloud-ci/pipelines/*.ts`, the
//!   Dynamic Worker sandbox that reads each file's `on` trigger, or the
//!   `pipeline_manifest` cache (docs/design/settings.md's "Pipeline
//!   discovery" sequence diagram) — none of it exists yet. This round's
//!   `admit_run` has no caller that discovers pipelines; it is a
//!   capability route exercised directly by the live smoke test, same
//!   "capability module, no real caller yet" posture as
//!   `pull_request_state`'s `notify_dirty`/`update_head_sha` before
//!   their first caller.
//! - **`settings.yml` fetching/parsing.** `concurrency.repository` /
//!   `.pipelines` / `.pipeline` (docs/design/settings.md's "Field
//!   reference": 1..200 / 1..50 / 1..100, deployment defaults when
//!   absent) arrive at this round's `admit_run` as plain `u32`
//!   parameters the (not-yet-existing) caller is expected to resolve
//!   and clamp before calling. `RepoState` trusts whatever caps it is
//!   given; it does not re-validate them against the field reference's
//!   bounds or deployment-wide clamping (docs/design/settings.md's
//!   "Deployment-wide limits" table) — that is the future caller's job,
//!   once it exists.
//! - **`RunCoordinator` initialization.** architecture.md's "Managed
//!   run" step 2 continues "Once admitted, the consumer requests
//!   `RunCoordinator` initialization for that identity" — this round's
//!   `admit_run` returns a decision and nothing more; no
//!   `RunCoordinatorStore::begin_run` call happens from here. That
//!   cross-DO wiring is the next, separate round.
//! - **Real cancellation of in-flight work.** When `cancel_superseded`
//!   causes an older run's slot to be taken, this round's `admit_run`
//!   **only** marks that older run's row `cancelled` in `RepoState`'s
//!   own SQLite (see [`AdmitRunOutcome`]). It does **not** call into any
//!   `RunCoordinator` to actually stop real containers or fail the
//!   older run's checks — said loudly here and again on
//!   [`RepoState::handle_admit_run`], since the gap between "this round
//!   marks it cancelled" and "a real run actually stops" is exactly the
//!   kind of mistake this module's doc comments exist to prevent a
//!   future reader from assuming is already wired.
//! - **Workflow/script execution of any kind.** No Dynamic Workflow, no
//!   container start — `concurrency.pipeline` (max containers for a
//!   single pipeline run) is accepted and stored on the admitted row
//!   ([`AdmissionTableRow::pipeline_cap`]) but never enforced against a
//!   real container start, since no `Executor` exists yet to start one
//!   (see [`logic::Caps`]'s doc comments).
//!
//! Decision logic (idempotency, cancel-superseded, cap checks) lives in
//! [`logic`], which has no Durable Object dependency and is
//! unit-tested directly. This module is the thin, storage-wired shell
//! around it — exercised only by the live smoke test (`mise run
//! //packages/cloud-ci-worker:dev`), same posture as
//! `coordinator::mod`'s and `pull_request_state::mod`'s own
//! "`cargo test` cannot run a real Durable Object" notes.

pub mod logic;

use logic::{
    AdmissionRow, AdmissionState, AdmitDecision, AdmitRequest, CapName, Caps, CompleteDecision,
    RunIdentity,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use worker::wasm_bindgen::JsValue;
use worker::{
    DurableObject, Env, Method, Request, RequestInit, Response, SqlStorage, SqlStorageValue, State,
    Stub, durable_object,
};

/// Durable Object binding name; must match `durable_objects.bindings[].name`
/// in `wrangler.toml`.
pub const REPO_STATE_BINDING: &str = "REPO_STATE";

/// Stub URLs need an absolute form. Durable Object routing ignores the host,
/// so this is a label, not a hostname anyone resolves.
const STUB_ORIGIN: &str = "https://repo-state.cloud-ci.internal";

/// Deterministic Durable Object name for a repo: `idFromName("{repo_id}")`.
/// `repo_id` alone is the whole identity — see module docs for why no
/// hashing is needed here.
pub fn do_name(repo_id: u64) -> String {
    format!("{repo_id}")
}

// ---------------------------------------------------------------------------
// Wire types (internal JSON between Worker and DO)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsWire {
    pub repository: u32,
    pub pipelines: u32,
    pub pipeline: u32,
}

impl From<CapsWire> for Caps {
    fn from(w: CapsWire) -> Self {
        Caps {
            repository: w.repository,
            pipelines: w.pipelines,
            pipeline: w.pipeline,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmitRunRequest {
    pub pipeline_file: String,
    pub run_key: String,
    pub attempt: u32,
    pub group: String,
    pub cancel_superseded: bool,
    pub caps: CapsWire,
}

/// `cancelled`/`rejected_holder` carry `(pipeline_file, run_key,
/// attempt)` triples, never a richer `RunIdentity` struct on the wire,
/// to keep this response shape stable regardless of how [`logic`]'s
/// internal types evolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmitRunOutcome {
    /// `"admitted"`, `"already_admitted"`, `"rejected_slot_occupied"`,
    /// or `"rejected_at_cap"`.
    pub status: String,
    pub admitted: bool,
    /// Set only when `status == "admitted"` and this admission
    /// superseded a prior active run in the same slot. **Bookkeeping
    /// only** — see module docs' scope boundary: no real cancellation
    /// of that run's work happens as a result of this field being set.
    pub cancelled_run: Option<(String, String, u32)>,
    /// Set only when `status == "rejected_slot_occupied"`.
    pub rejected_holder: Option<(String, String, u32)>,
    /// Set only when `status == "rejected_at_cap"`: `"repository"` or
    /// `"pipelines"`.
    pub rejected_cap: Option<String>,
    pub rejected_cap_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRunRequest {
    pub pipeline_file: String,
    pub run_key: String,
    pub attempt: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRunOutcome {
    /// `"released"`, `"already_terminal"`, or `"not_found"`.
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    error: String,
}

// ---------------------------------------------------------------------------
// Durable Object storage row
// ---------------------------------------------------------------------------

/// One row of the DO's `admission` table — see [`ensure_schema`] for the
/// full column list and [`logic::AdmissionRow`] for the subset the pure
/// decision functions actually need (this struct carries the extra
/// storage-only columns: `id`, `pipeline_cap`, timestamps).
#[derive(Debug, Clone, Deserialize)]
struct AdmissionTableRow {
    id: i64,
    pipeline_file: String,
    run_key: String,
    attempt: i64,
    group_name: String,
    state: String,
    /// `concurrency.pipeline`'s declared value at admission time —
    /// stored, never enforced this round (module docs' scope boundary).
    #[allow(dead_code)] // read back via /get-state for smoke-test introspection only
    pipeline_cap: i64,
    created_at: i64,
    #[allow(dead_code)] // informational; no reconciliation job reads this yet
    completed_at: Option<i64>,
}

impl AdmissionTableRow {
    fn admission_state(&self) -> worker::Result<AdmissionState> {
        match self.state.as_str() {
            "active" => Ok(AdmissionState::Active),
            "cancelled" => Ok(AdmissionState::Cancelled),
            "completed" => Ok(AdmissionState::Completed),
            other => Err(worker::Error::RustError(format!(
                "unknown admission state {other:?}"
            ))),
        }
    }

    fn to_logic_row(&self) -> worker::Result<AdmissionRow> {
        Ok(AdmissionRow {
            identity: RunIdentity {
                pipeline_file: self.pipeline_file.clone(),
                run_key: self.run_key.clone(),
                attempt: self.attempt as u32,
            },
            group_name: self.group_name.clone(),
            state: self.admission_state()?,
        })
    }
}

const ADMISSION_COLUMNS: &str = "id, pipeline_file, run_key, attempt, group_name, state, pipeline_cap, created_at, completed_at";

// ---------------------------------------------------------------------------
// The Durable Object
// ---------------------------------------------------------------------------

#[durable_object]
pub struct RepoState {
    state: State,
    /// Unused this round (no cross-DO `RunCoordinator` call exists yet
    /// — module docs' scope boundary); kept so the field is already in
    /// place for that future round, same shape as
    /// `pull_request_state::PullRequestState`'s own unused `env`.
    #[allow(dead_code)]
    env: Env,
}

impl DurableObject for RepoState {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> worker::Result<Response> {
        let sql = self.state.storage().sql();
        ensure_schema(&sql)?;

        match (req.method(), req.path().as_str()) {
            (Method::Post, "/admit-run") => {
                let body: AdmitRunRequest = req.json().await?;
                self.handle_admit_run(&sql, body)
            }
            (Method::Post, "/complete-run") => {
                let body: CompleteRunRequest = req.json().await?;
                self.handle_complete_run(&sql, body)
            }
            (Method::Get, "/get-state") => self.handle_get_state(&sql),
            _ => error_response(404, "unknown RepoState route"),
        }
    }
}

impl RepoState {
    /// `POST /admit-run` — see [`logic::decide_admit`] for the full
    /// decision. This handler's only job is: look up the exact rows
    /// `decide_admit` needs, hand them to it, then translate whichever
    /// [`AdmitDecision`] comes back into storage writes and the wire
    /// response. The cancel-superseded write below is **bookkeeping
    /// only** — see module docs' scope boundary (repeated here
    /// deliberately): it never calls out to a `RunCoordinator` to stop
    /// any real work.
    fn handle_admit_run(&self, sql: &SqlStorage, req: AdmitRunRequest) -> worker::Result<Response> {
        let identity = RunIdentity {
            pipeline_file: req.pipeline_file.clone(),
            run_key: req.run_key.clone(),
            attempt: req.attempt,
        };

        let existing_by_identity = read_by_identity(sql, &identity)?
            .map(|r| r.to_logic_row())
            .transpose()?;

        // Only look up a slot holder once we know this isn't a
        // redelivery of an already-active admission — a run's own
        // identity can never be "someone else's" holder of its own
        // slot (see `decide_admit`'s doc comment on this invariant).
        let active_slot_holder = if matches!(
            existing_by_identity.as_ref().map(|r| r.state),
            Some(AdmissionState::Active)
        ) {
            None
        } else {
            read_active_slot_holder(sql, &req.pipeline_file, &req.group)?
                .map(|r| r.to_logic_row())
                .transpose()?
        };

        let active_count = count_active(sql)? as u32;

        let decision = logic::decide_admit(
            &AdmitRequest {
                identity: identity.clone(),
                group_name: req.group.clone(),
                cancel_superseded: req.cancel_superseded,
                caps: req.caps.clone().into(),
            },
            existing_by_identity.as_ref(),
            active_slot_holder.as_ref(),
            active_count,
        );

        let now_ms = worker::Date::now().as_millis() as i64;

        match decision {
            AdmitDecision::AlreadyAdmitted => Response::from_json(&AdmitRunOutcome {
                status: "already_admitted".into(),
                admitted: true,
                cancelled_run: None,
                rejected_holder: None,
                rejected_cap: None,
                rejected_cap_limit: None,
            }),
            AdmitDecision::Admitted { cancelled } => {
                if let Some(cancelled_identity) = &cancelled {
                    mark_cancelled(sql, cancelled_identity)?;
                }
                insert_admission(sql, &identity, &req.group, req.caps.pipeline as i64, now_ms)?;
                Response::from_json(&AdmitRunOutcome {
                    status: "admitted".into(),
                    admitted: true,
                    cancelled_run: cancelled.map(|c| (c.pipeline_file, c.run_key, c.attempt)),
                    rejected_holder: None,
                    rejected_cap: None,
                    rejected_cap_limit: None,
                })
            }
            AdmitDecision::RejectedSlotOccupied { holder } => {
                Response::from_json(&AdmitRunOutcome {
                    status: "rejected_slot_occupied".into(),
                    admitted: false,
                    cancelled_run: None,
                    rejected_holder: Some((holder.pipeline_file, holder.run_key, holder.attempt)),
                    rejected_cap: None,
                    rejected_cap_limit: None,
                })
            }
            AdmitDecision::RejectedAtCap { cap, limit } => Response::from_json(&AdmitRunOutcome {
                status: "rejected_at_cap".into(),
                admitted: false,
                cancelled_run: None,
                rejected_holder: None,
                rejected_cap: Some(match cap {
                    CapName::Repository => "repository".into(),
                    CapName::Pipelines => "pipelines".into(),
                }),
                rejected_cap_limit: Some(limit),
            }),
        }
    }

    /// `POST /complete-run` — see [`logic::decide_complete`]. No real
    /// caller yet this round (module docs' scope boundary): the future
    /// caller is `RunCoordinator`'s own close-run logic, once the
    /// cross-DO wiring that creates a `RepoState` admission ↔
    /// `RunCoordinator` instance link exists.
    fn handle_complete_run(
        &self,
        sql: &SqlStorage,
        req: CompleteRunRequest,
    ) -> worker::Result<Response> {
        let identity = RunIdentity {
            pipeline_file: req.pipeline_file,
            run_key: req.run_key,
            attempt: req.attempt,
        };
        let existing = read_by_identity(sql, &identity)?
            .map(|r| r.to_logic_row())
            .transpose()?;

        let decision = logic::decide_complete(existing.as_ref());
        let now_ms = worker::Date::now().as_millis() as i64;

        let status = match decision {
            CompleteDecision::Released => {
                mark_completed(sql, &identity, now_ms)?;
                "released"
            }
            CompleteDecision::AlreadyTerminal => "already_terminal",
            CompleteDecision::NotFound => "not_found",
        };

        Response::from_json(&CompleteRunOutcome {
            status: status.into(),
        })
    }

    /// `GET /get-state` — debug/smoke-test introspection only (not part
    /// of this round's documented RPC surface); lets the live smoke
    /// test observe every admission row without a separate SQLite
    /// inspection path, same pattern as
    /// `pull_request_state::PullRequestState::handle_get_state`.
    fn handle_get_state(&self, sql: &SqlStorage) -> worker::Result<Response> {
        let rows = read_all(sql)?;
        Response::from_json(
            &rows
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "pipeline_file": r.pipeline_file,
                        "run_key": r.run_key,
                        "attempt": r.attempt,
                        "group_name": r.group_name,
                        "state": r.state,
                        "pipeline_cap": r.pipeline_cap,
                        "created_at": r.created_at,
                        "completed_at": r.completed_at,
                    })
                })
                .collect::<Vec<_>>(),
        )
    }
}

fn error_response(status: u16, message: &str) -> worker::Result<Response> {
    Ok(Response::from_json(&ErrorBody {
        error: message.into(),
    })?
    .with_status(status))
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

fn ensure_schema(sql: &SqlStorage) -> worker::Result<()> {
    // One row per admitted run attempt; `UNIQUE(pipeline_file, run_key,
    // attempt)` is the stable identity architecture.md's "Managed run"
    // step 2 specifies (repo is implicit — one DO instance per repo).
    // There is deliberately no `UNIQUE(pipeline_file, group_name) WHERE
    // state = 'active'` constraint (SQLite partial-unique-index-on-text
    // comparison across app-level state transitions is easy to get
    // subtly wrong under concurrent alarms/requests); the "at most one
    // active row per slot" invariant is instead enforced entirely by
    // `decide_admit`'s logic — every admission path either checks the
    // slot is empty or cancels the current holder in the same
    // operation.
    sql.exec(
        "CREATE TABLE IF NOT EXISTS admission ( \
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            pipeline_file TEXT NOT NULL, \
            run_key TEXT NOT NULL, \
            attempt INTEGER NOT NULL, \
            group_name TEXT NOT NULL, \
            state TEXT NOT NULL CHECK (state IN ('active', 'cancelled', 'completed')), \
            pipeline_cap INTEGER NOT NULL, \
            created_at INTEGER NOT NULL, \
            completed_at INTEGER, \
            UNIQUE(pipeline_file, run_key, attempt) \
        )",
        None,
    )?;
    Ok(())
}

fn read_by_identity(
    sql: &SqlStorage,
    identity: &RunIdentity,
) -> worker::Result<Option<AdmissionTableRow>> {
    let rows: Vec<AdmissionTableRow> = sql
        .exec(
            &format!(
                "SELECT {ADMISSION_COLUMNS} FROM admission \
                 WHERE pipeline_file = ?1 AND run_key = ?2 AND attempt = ?3"
            ),
            vec![
                SqlStorageValue::from(identity.pipeline_file.as_str()),
                SqlStorageValue::from(identity.run_key.as_str()),
                SqlStorageValue::try_from_i64(i64::from(identity.attempt))?,
            ],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn read_active_slot_holder(
    sql: &SqlStorage,
    pipeline_file: &str,
    group_name: &str,
) -> worker::Result<Option<AdmissionTableRow>> {
    let rows: Vec<AdmissionTableRow> = sql
        .exec(
            &format!(
                "SELECT {ADMISSION_COLUMNS} FROM admission \
                 WHERE pipeline_file = ?1 AND group_name = ?2 AND state = 'active'"
            ),
            vec![
                SqlStorageValue::from(pipeline_file),
                SqlStorageValue::from(group_name),
            ],
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn count_active(sql: &SqlStorage) -> worker::Result<i64> {
    #[derive(Deserialize)]
    struct CountRow {
        c: i64,
    }
    let rows: Vec<CountRow> = sql
        .exec(
            "SELECT COUNT(*) AS c FROM admission WHERE state = 'active'",
            None,
        )?
        .to_array()?;
    Ok(rows.first().map(|r| r.c).unwrap_or(0))
}

fn read_all(sql: &SqlStorage) -> worker::Result<Vec<AdmissionTableRow>> {
    let rows: Vec<AdmissionTableRow> = sql
        .exec(
            &format!("SELECT {ADMISSION_COLUMNS} FROM admission ORDER BY id"),
            None,
        )?
        .to_array()?;
    Ok(rows)
}

fn insert_admission(
    sql: &SqlStorage,
    identity: &RunIdentity,
    group_name: &str,
    pipeline_cap: i64,
    created_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO admission \
         (pipeline_file, run_key, attempt, group_name, state, pipeline_cap, created_at, completed_at) \
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, NULL)",
        vec![
            SqlStorageValue::from(identity.pipeline_file.as_str()),
            SqlStorageValue::from(identity.run_key.as_str()),
            SqlStorageValue::try_from_i64(i64::from(identity.attempt))?,
            SqlStorageValue::from(group_name),
            SqlStorageValue::try_from_i64(pipeline_cap)?,
            SqlStorageValue::try_from_i64(created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn mark_cancelled(sql: &SqlStorage, identity: &RunIdentity) -> worker::Result<()> {
    sql.exec(
        "UPDATE admission SET state = 'cancelled' \
         WHERE pipeline_file = ?1 AND run_key = ?2 AND attempt = ?3",
        vec![
            SqlStorageValue::from(identity.pipeline_file.as_str()),
            SqlStorageValue::from(identity.run_key.as_str()),
            SqlStorageValue::try_from_i64(i64::from(identity.attempt))?,
        ],
    )?;
    Ok(())
}

fn mark_completed(
    sql: &SqlStorage,
    identity: &RunIdentity,
    completed_at_ms: i64,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE admission SET state = 'completed', completed_at = ?4 \
         WHERE pipeline_file = ?1 AND run_key = ?2 AND attempt = ?3",
        vec![
            SqlStorageValue::from(identity.pipeline_file.as_str()),
            SqlStorageValue::from(identity.run_key.as_str()),
            SqlStorageValue::try_from_i64(i64::from(identity.attempt))?,
            SqlStorageValue::try_from_i64(completed_at_ms)?,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client wrapper
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RepoStateError {
    Internal(String),
}

impl std::fmt::Display for RepoStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

/// Typed client for one repo's Durable Object. Mirrors
/// `coordinator::RunCoordinatorStore`/`pull_request_state::PullRequestStateStore`
/// exactly — the rest of the Worker uses this; raw stubs never leave
/// this module. No real caller exists yet this round (module docs'
/// scope boundary); the live smoke test exercises it via a temporary
/// debug route (see repo root report for exact commit removing it).
pub struct RepoStateStore {
    stub: Stub,
}

impl RepoStateStore {
    pub fn new(env: &Env, do_name: &str) -> Result<Self, RepoStateError> {
        let stub = env
            .durable_object(REPO_STATE_BINDING)
            .and_then(|namespace| namespace.id_from_name(do_name)?.get_stub())
            .map_err(|e| RepoStateError::Internal(format!("repo state unavailable: {e}")))?;
        Ok(Self { stub })
    }

    pub async fn admit_run(
        &self,
        req: &AdmitRunRequest,
    ) -> Result<AdmitRunOutcome, RepoStateError> {
        self.call(Method::Post, "/admit-run", Some(req)).await
    }

    pub async fn complete_run(
        &self,
        req: &CompleteRunRequest,
    ) -> Result<CompleteRunOutcome, RepoStateError> {
        self.call(Method::Post, "/complete-run", Some(req)).await
    }

    async fn call<B, R>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<R, RepoStateError>
    where
        B: Serialize,
        R: DeserializeOwned,
    {
        let mut init = RequestInit::new();
        init.with_method(method);
        if let Some(body) = body {
            let encoded = serde_json::to_string(body)
                .map_err(|e| RepoStateError::Internal(format!("cannot encode {path}: {e}")))?;
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
            status => {
                let detail = response.text().await.unwrap_or_default();
                Err(RepoStateError::Internal(format!(
                    "repo state rejected {path}: {status} {detail}"
                )))
            }
        }
    }
}

fn stub_error(path: &str, e: worker::Error) -> RepoStateError {
    RepoStateError::Internal(format!("repo state error on {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn do_name_is_plain_decimal_repo_id() {
        assert_eq!(do_name(1), "1");
        assert_eq!(do_name(999999), "999999");
        assert_ne!(do_name(1), do_name(12));
    }
}
