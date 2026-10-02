//! The per-PR `PullRequestState` Durable Object (docs/design/pr-comment.md's
//! "`PullRequestState`: one writer per PR", "Debounce and coalescing"),
//! and docs/architecture.md's "Coordination invariants"/data-model table.
//!
//! One instance per `(repo_id, pr_number)`, addressed by [`do_name`] —
//! `idFromName("{repo_id}/{pr_number}")`, pr-comment.md's own example
//! string format. The instance owns the debounce/coalescing alarm and,
//! eventually, every GitHub PATCH/create call for the PR's sticky
//! comment (pr-comment.md: "`PullRequestState` is the sole writer of the
//! PR's sticky comment"). Nothing outside this module talks to a raw
//! [`Stub`]; use [`PullRequestStateStore`].
//!
//! # Scope boundary (this round)
//!
//! This round builds the DO's local storage, naming, and the
//! `notify_dirty`/`update_head_sha` RPCs plus the debounce/coalescing
//! alarm timing — the "given a stream of dirty events with timestamps,
//! what's the resulting flush schedule" piece pr-comment.md's "Debounce
//! and coalescing" table specifies. It deliberately does **not**:
//!
//! - Make any real GitHub API call (comment create/PATCH).
//! - Wire [`crate::pr_comment::render_pr_report`] or build a real
//!   `PrReport` from D1.
//! - Wire any webhook (`pull_request.synchronize`, `notify_dirty` from a
//!   real `RunCoordinator`) as a caller.
//!
//! `notify_dirty`/`update_head_sha` are callable capability routes with
//! **no real caller yet** this round — same pattern as
//! `roles::resolve_role` before its first caller, `github_checks.rs`'s
//! "capability module, no caller yet", and `template_spike.rs`'s "not
//! the real thing, proves the mechanism". The live smoke test (`mise run
//! //packages/cloud-ci-worker:dev`) exercises them directly against a
//! `PullRequestState` instance, same posture as `coordinator::mod`'s own
//! "exercised only by the live smoke test" note — `cargo test` cannot
//! run a real Durable Object.
//!
//! Decision logic (which path a `notify_dirty` takes, the full debounce
//! timing table) lives in [`logic`], which has no Durable Object
//! dependency and is unit-tested directly. This module is the thin,
//! storage-wired shell around it.
//!
//! # Alarm flush stub
//!
//! When the alarm fires, [`PullRequestState::alarm`] does not call
//! GitHub or render anything real — see its own doc comment for exactly
//! what the stub does and why.

pub mod logic;

use logic::{AlarmTarget, DebounceState, DirtyPath};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use worker::wasm_bindgen::JsValue;
use worker::{
    DurableObject, Env, Method, Request, RequestInit, Response, SqlStorage, SqlStorageValue, State,
    Stub, durable_object,
};

/// Durable Object binding name; must match `durable_objects.bindings[].name`
/// in `wrangler.toml`.
pub const PULL_REQUEST_STATE_BINDING: &str = "PULL_REQUEST_STATE";

/// Stub URLs need an absolute form. Durable Object routing ignores the host,
/// so this is a label, not a hostname anyone resolves.
const STUB_ORIGIN: &str = "https://pull-request-state.cloud-ci.internal";

/// Deterministic Durable Object name for a PR, per pr-comment.md's "One
/// sticky PR comment" section: "a dedicated Durable Object,
/// `PullRequestState`, one instance per `(repo_id, pr_number)`, addressed
/// by `idFromName("{repo_id}/{pr_number}")`" — reusing that exact string
/// format rather than inventing a new one.
///
/// Unlike `coordinator::do_name`, this needs no hashing: `repo_id` and
/// `pr_number` are both plain integers, not free-form caller-supplied
/// text, so a `/`-joined decimal pair is already injective by
/// construction — `(1, 23)` and `(12, 3)` join to `"1/23"` and `"12/3"`,
/// which can never collide, since a `/` never appears inside a decimal
/// integer's own digits. No length-prefixing or hashing is needed to
/// disambiguate a boundary that cannot be ambiguous in the first place.
pub fn do_name(repo_id: u64, pr_number: u64) -> String {
    format!("{repo_id}/{pr_number}")
}

// ---------------------------------------------------------------------------
// Wire types (internal JSON between Worker and DO)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyDirtyRequest {
    pub head_sha: String,
    /// One of pr-comment.md's typed dirty reasons (`run_created`,
    /// `job_state`, `report_merged`, `ai_summary_ready`,
    /// `coverage_ready`, `head_changed`, `command_refresh`,
    /// `action_executed`), or this module's own `"run_terminal"`
    /// placeholder — see [`logic::is_terminal_reason`].
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyDirtyOutcome {
    /// Which path the call took — useful for the smoke test/caller to
    /// confirm seed vs. fold vs. ignored without a second read.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateHeadShaRequest {
    pub new_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateHeadShaOutcome {}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    error: String,
}

// ---------------------------------------------------------------------------
// Durable Object storage row
// ---------------------------------------------------------------------------

/// The DO's single `comment_state` row — one per instance, since one
/// instance serves exactly one PR (pr-comment.md: "since each instance
/// serves exactly one PR, there is nothing to multiplex").
///
/// Columns beyond pr-comment.md's own literal `comment_state` schema
/// (`dirty_seq`/`flushed_seq`) are this round's own choice to directly
/// express the debounce table's inputs rather than derive them from a
/// seq-counter diff:
///
/// - `pending_dirty` — whether there is an unflushed burst at all (the
///   seq-counter schema expresses this as `dirty_seq != flushed_seq`;
///   this round uses a plain bool-as-int instead, since there is no real
///   flush to assign a `flushed_seq` to yet).
/// - `first_dirty_at` — start of the current unflushed burst, needed for
///   the "max delay 20 s after first unflushed dirty event" rule
///   ([`logic::next_alarm_target`]).
/// - `last_dirty_reason` — the most recent `reason` passed to
///   `notify_dirty`, needed to detect the terminal-flush fast path
///   ([`logic::is_terminal_reason`]).
#[derive(Debug, Clone, Deserialize)]
struct CommentStateRow {
    head_sha: Option<String>,
    comment_id: Option<i64>,
    #[allow(dead_code)] // read back by a future render-wiring round
    render_seq: i64,
    rendered_hash: Option<String>,
    last_patched_at: Option<i64>,
    #[allow(dead_code)] // not yet branched on outside notify/update handlers
    pending_dirty: i64,
    #[allow(dead_code)] // informational; the alarm itself is the live schedule
    quiet_until: Option<i64>,
    first_dirty_at: Option<i64>,
    #[allow(dead_code)] // read back by a future render-wiring round
    last_dirty_reason: Option<String>,
}

const COMMENT_STATE_COLUMNS: &str = "head_sha, comment_id, render_seq, rendered_hash, last_patched_at, pending_dirty, quiet_until, first_dirty_at, last_dirty_reason";

// ---------------------------------------------------------------------------
// The Durable Object
// ---------------------------------------------------------------------------

#[durable_object(alarm)]
pub struct PullRequestState {
    state: State,
    /// Unused this round (no GitHub call/installation-token lookup is
    /// wired yet — see module docs' scope boundary); kept so the field
    /// is already in place for the flush step's eventual real GitHub
    /// call, same shape as `coordinator::RunCoordinator`'s own `env`.
    #[allow(dead_code)]
    env: Env,
}

impl DurableObject for PullRequestState {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> worker::Result<Response> {
        let sql = self.state.storage().sql();
        ensure_schema(&sql)?;

        match (req.method(), req.path().as_str()) {
            (Method::Post, "/notify-dirty") => {
                let body: NotifyDirtyRequest = req.json().await?;
                self.handle_notify_dirty(&sql, body).await
            }
            (Method::Post, "/update-head-sha") => {
                let body: UpdateHeadShaRequest = req.json().await?;
                self.handle_update_head_sha(&sql, body).await
            }
            (Method::Get, "/get-state") => self.handle_get_state(&sql),
            _ => error_response(404, "unknown PullRequestState route"),
        }
    }

    /// Fires when the debounce/coalescing alarm set by
    /// `handle_notify_dirty`/`handle_update_head_sha` reaches its
    /// scheduled time.
    ///
    /// This round's flush is an **explicit, documented no-op stub**:
    /// real work (building a `PrReport` via
    /// `crate::pr_comment::render_pr_report` from D1, then PATCHing or
    /// creating the GitHub comment) is not wired until a later round
    /// (see module docs' scope boundary). What it *does* do, so the
    /// debounce bookkeeping stays internally consistent for the next
    /// burst:
    ///
    /// - Increments `render_seq` and clears `pending_dirty`/
    ///   `first_dirty_at`/`quiet_until` — "a flush happened".
    /// - Sets `last_patched_at = now` — feeds the min-interval floor for
    ///   the next burst.
    /// - Sets `comment_id` to a sentinel placeholder value (`0`) the
    ///   first time it is `NULL`, rather than a real GitHub comment id.
    ///   This is required for debounce correctness even without a real
    ///   GitHub call: the "initial placeholder" row's 0 s bypass is
    ///   specified as "at most once per head sha (only when no
    ///   `comment_id` exists yet for it)" — without *some* non-null
    ///   marker recorded here, every post-flush dirty event for the same
    ///   head sha would see `comment_id` still `NULL` and incorrectly
    ///   re-trigger the 0 s bypass forever. `rendered_hash` is left
    ///   untouched (still `NULL`/stale) since there is no real rendered
    ///   body to hash yet — see [`logic::should_skip_flush`]'s doc
    ///   comment for why the hash-skip decision itself is still unit
    ///   tested even though this live path never calls it.
    async fn alarm(&self) -> worker::Result<Response> {
        let sql = self.state.storage().sql();
        ensure_schema(&sql)?;
        self.handle_alarm_flush(&sql).await
    }
}

impl PullRequestState {
    /// `POST /notify-dirty` — see [`logic::classify_notify_dirty`] for
    /// the seed/ignore/fold decision and [`logic::next_alarm_target`]
    /// for the resulting alarm schedule.
    async fn handle_notify_dirty(
        &self,
        sql: &SqlStorage,
        req: NotifyDirtyRequest,
    ) -> worker::Result<Response> {
        let existing = read_state(sql)?;
        if existing.is_none() {
            insert_empty_row(sql)?;
        }
        let row = read_state(sql)?.ok_or_else(|| {
            worker::Error::RustError("comment_state row missing after insert".into())
        })?;

        let now_ms = worker::Date::now().as_millis() as i64;

        match logic::classify_notify_dirty(row.head_sha.as_deref(), &req.head_sha) {
            DirtyPath::Ignored => Ok(Response::from_json(&NotifyDirtyOutcome {
                path: "ignored".into(),
            })?),
            DirtyPath::Seed | DirtyPath::Fold => {
                // Seed: no head_sha tracked yet, so this is simultaneously
                // the first-ever dirty event (comment_id is NULL and no
                // burst is in progress) — always qualifies for the
                // initial-placeholder 0s bypass. Fold: same head_sha as
                // already tracked; qualifies for the bypass only if this
                // instance has never had a burst *and* never flushed a
                // placeholder for it yet (module docs' alarm-stub note).
                let is_initial_placeholder =
                    row.comment_id.is_none() && row.first_dirty_at.is_none();
                let first_dirty_at = row.first_dirty_at.unwrap_or(now_ms);
                let is_terminal = logic::is_terminal_reason(&req.reason);
                let target = logic::next_alarm_target(
                    now_ms,
                    is_terminal,
                    is_initial_placeholder,
                    DebounceState {
                        first_dirty_at: Some(first_dirty_at),
                        last_patched_at: row.last_patched_at,
                    },
                );
                let quiet_until = match target {
                    AlarmTarget::Immediate => now_ms,
                    AlarmTarget::At(ms) => ms,
                };
                update_dirty(sql, &req.head_sha, first_dirty_at, &req.reason, quiet_until)?;
                self.schedule_alarm(target, now_ms).await?;
                let path = if is_initial_placeholder {
                    "seed_placeholder"
                } else {
                    "fold"
                };
                Ok(Response::from_json(&NotifyDirtyOutcome {
                    path: path.into(),
                })?)
            }
        }
    }

    /// `POST /update-head-sha` — pr-comment.md's `pull_request.synchronize`
    /// row: "the same `PullRequestState` instance updates its tracked
    /// `head_sha` in place: no claim, no epoch bump, no handoff ...
    /// Flush immediately (`head_changed`, 1 s quiet)".
    ///
    /// `comment_id` is deliberately **not** cleared: the mockup's
    /// "edited in place for the life of the PR's current head sha" plus
    /// the Stale-sha-handling table's "Previous head" footer imply the
    /// *same* comment is reused across a head change, not recreated.
    /// `rendered_hash` *is* reset to `NULL` — "nothing rendered for this
    /// sha yet" — so the (future) hash-skip check never compares a new
    /// sha's render against the old sha's hash.
    async fn handle_update_head_sha(
        &self,
        sql: &SqlStorage,
        req: UpdateHeadShaRequest,
    ) -> worker::Result<Response> {
        let existing = read_state(sql)?;
        if existing.is_none() {
            insert_empty_row(sql)?;
        }
        let row = read_state(sql)?.ok_or_else(|| {
            worker::Error::RustError("comment_state row missing after insert".into())
        })?;

        let now_ms = worker::Date::now().as_millis() as i64;
        // A head change always starts a fresh burst: whatever burst was
        // in flight for the old sha is no longer relevant to the new
        // sha's placeholder/flush timing.
        let target = logic::next_alarm_target(
            now_ms,
            true, // terminal flush: "head_changed, 1s quiet" is unconditional here
            false,
            DebounceState {
                first_dirty_at: Some(now_ms),
                last_patched_at: row.last_patched_at,
            },
        );
        let quiet_until = match target {
            AlarmTarget::Immediate => now_ms,
            AlarmTarget::At(ms) => ms,
        };
        update_head_sha_row(sql, &req.new_sha, now_ms, quiet_until)?;
        self.schedule_alarm(target, now_ms).await?;
        Response::from_json(&UpdateHeadShaOutcome {})
    }

    /// `GET /get-state` — debug/smoke-test introspection only (not part
    /// of pr-comment.md's RPC surface); lets the live smoke test observe
    /// the row without a separate D1/SQLite inspection path.
    fn handle_get_state(&self, sql: &SqlStorage) -> worker::Result<Response> {
        let row = read_state(sql)?;
        Response::from_json(&row.map(|r| {
            serde_json::json!({
                "head_sha": r.head_sha,
                "comment_id": r.comment_id,
                "render_seq": r.render_seq,
                "rendered_hash": r.rendered_hash,
                "last_patched_at": r.last_patched_at,
                "pending_dirty": r.pending_dirty,
                "quiet_until": r.quiet_until,
                "first_dirty_at": r.first_dirty_at,
                "last_dirty_reason": r.last_dirty_reason,
            })
        }))
    }

    /// See [`PullRequestState::alarm`]'s doc comment for exactly what
    /// this stub does and why.
    async fn handle_alarm_flush(&self, sql: &SqlStorage) -> worker::Result<Response> {
        let row = read_state(sql)?;
        let now_ms = worker::Date::now().as_millis() as i64;
        if let Some(row) = row {
            let comment_id = row.comment_id.unwrap_or(0);
            flush_stub(sql, comment_id, row.render_seq + 1, now_ms)?;
        }
        Response::ok("flushed (stub)")
    }

    /// `Durable Object alarms are a single global deadline`: setting one
    /// always overwrites whatever was scheduled before, so this is both
    /// "set" and "extend" depending on what `target` resolves to.
    async fn schedule_alarm(&self, target: AlarmTarget, now_ms: i64) -> worker::Result<()> {
        let delay_ms = match target {
            AlarmTarget::Immediate => 0,
            AlarmTarget::At(ms) => (ms - now_ms).max(0),
        };
        self.state.storage().set_alarm(delay_ms).await
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
    // Single row per instance (`id = 1`), matching pr-comment.md's own
    // `comment_state` table's `CHECK (id = 1)` single-row enforcement —
    // see `CommentStateRow`'s doc comment for why this round's column
    // set otherwise diverges from that doc's literal schema.
    sql.exec(
        "CREATE TABLE IF NOT EXISTS comment_state ( \
            id INTEGER PRIMARY KEY CHECK (id = 1), \
            head_sha TEXT, \
            comment_id INTEGER, \
            render_seq INTEGER NOT NULL DEFAULT 0, \
            rendered_hash TEXT, \
            last_patched_at INTEGER, \
            pending_dirty INTEGER NOT NULL DEFAULT 0, \
            quiet_until INTEGER, \
            first_dirty_at INTEGER, \
            last_dirty_reason TEXT \
        )",
        None,
    )?;
    Ok(())
}

fn read_state(sql: &SqlStorage) -> worker::Result<Option<CommentStateRow>> {
    let rows: Vec<CommentStateRow> = sql
        .exec(
            &format!("SELECT {COMMENT_STATE_COLUMNS} FROM comment_state WHERE id = 1"),
            None,
        )?
        .to_array()?;
    Ok(rows.into_iter().next())
}

fn insert_empty_row(sql: &SqlStorage) -> worker::Result<()> {
    sql.exec(
        "INSERT INTO comment_state (id, render_seq, pending_dirty) VALUES (1, 0, 0)",
        None,
    )?;
    Ok(())
}

fn update_dirty(
    sql: &SqlStorage,
    head_sha: &str,
    first_dirty_at: i64,
    reason: &str,
    quiet_until: i64,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE comment_state SET head_sha = ?1, pending_dirty = 1, first_dirty_at = ?2, \
         last_dirty_reason = ?3, quiet_until = ?4 WHERE id = 1",
        vec![
            SqlStorageValue::from(head_sha),
            SqlStorageValue::try_from_i64(first_dirty_at)?,
            SqlStorageValue::from(reason),
            SqlStorageValue::try_from_i64(quiet_until)?,
        ],
    )?;
    Ok(())
}

fn update_head_sha_row(
    sql: &SqlStorage,
    new_sha: &str,
    first_dirty_at: i64,
    quiet_until: i64,
) -> worker::Result<()> {
    sql.exec(
        "UPDATE comment_state SET head_sha = ?1, rendered_hash = NULL, pending_dirty = 1, \
         first_dirty_at = ?2, last_dirty_reason = 'head_changed', quiet_until = ?3 WHERE id = 1",
        vec![
            SqlStorageValue::from(new_sha),
            SqlStorageValue::try_from_i64(first_dirty_at)?,
            SqlStorageValue::try_from_i64(quiet_until)?,
        ],
    )?;
    Ok(())
}

fn flush_stub(
    sql: &SqlStorage,
    comment_id: i64,
    next_render_seq: i64,
    now_ms: i64,
) -> worker::Result<()> {
    // Would call render_pr_report + GitHub PATCH/create here; not wired
    // until a later round (see `PullRequestState::alarm`'s doc comment).
    sql.exec(
        "UPDATE comment_state SET comment_id = ?1, render_seq = ?2, pending_dirty = 0, \
         first_dirty_at = NULL, quiet_until = NULL, last_patched_at = ?3 WHERE id = 1",
        vec![
            SqlStorageValue::try_from_i64(comment_id)?,
            SqlStorageValue::try_from_i64(next_render_seq)?,
            SqlStorageValue::try_from_i64(now_ms)?,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client wrapper
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum PullRequestStateError {
    Internal(String),
}

impl std::fmt::Display for PullRequestStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

/// Typed client for one PR's Durable Object. Mirrors
/// `coordinator::RunCoordinatorStore` exactly — the rest of the Worker
/// uses this; raw stubs never leave this module. No real caller exists
/// yet this round (module docs' scope boundary); the live smoke test
/// exercises it via a temporary debug route instead.
pub struct PullRequestStateStore {
    stub: Stub,
}

impl PullRequestStateStore {
    pub fn new(env: &Env, do_name: &str) -> Result<Self, PullRequestStateError> {
        let stub = env
            .durable_object(PULL_REQUEST_STATE_BINDING)
            .and_then(|namespace| namespace.id_from_name(do_name)?.get_stub())
            .map_err(|e| {
                PullRequestStateError::Internal(format!("pull request state unavailable: {e}"))
            })?;
        Ok(Self { stub })
    }

    pub async fn notify_dirty(
        &self,
        req: &NotifyDirtyRequest,
    ) -> Result<NotifyDirtyOutcome, PullRequestStateError> {
        self.call(Method::Post, "/notify-dirty", Some(req)).await
    }

    /// `GET /get-state` — see [`PullRequestState::handle_get_state`]'s
    /// doc comment; debug/smoke-test introspection only.
    pub async fn get_state(&self) -> Result<serde_json::Value, PullRequestStateError> {
        self.call(Method::Get, "/get-state", None::<&()>).await
    }

    pub async fn update_head_sha(
        &self,
        req: &UpdateHeadShaRequest,
    ) -> Result<UpdateHeadShaOutcome, PullRequestStateError> {
        self.call(Method::Post, "/update-head-sha", Some(req)).await
    }

    async fn call<B, R>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<R, PullRequestStateError>
    where
        B: Serialize,
        R: DeserializeOwned,
    {
        let mut init = RequestInit::new();
        init.with_method(method);
        if let Some(body) = body {
            let encoded = serde_json::to_string(body).map_err(|e| {
                PullRequestStateError::Internal(format!("cannot encode {path}: {e}"))
            })?;
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
                Err(PullRequestStateError::Internal(format!(
                    "pull request state rejected {path}: {status} {detail}"
                )))
            }
        }
    }
}

fn stub_error(path: &str, e: worker::Error) -> PullRequestStateError {
    PullRequestStateError::Internal(format!("pull request state error on {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn do_name_is_slash_joined_decimal_pair() {
        assert_eq!(do_name(1, 23), "1/23");
        assert_eq!(do_name(12, 3), "12/3");
        assert_ne!(do_name(1, 23), do_name(12, 3));
    }
}
