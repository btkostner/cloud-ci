//! `workflow_run` GitHub webhook handling (docs/design/byo-ci.md §
//! "Completion semantics": "completion comes from the shard counts the
//! uploads declare, the external CI's completion webhook, or a
//! timeout" — this module is the completion-webhook signal. `--expect-jobs`
//! counting and the timeout alarm are separate, later work: counting is a
//! simpler per-job-completion counter already partially expressible from
//! existing `CompleteShard` state, while the timeout needs a Durable
//! Object alarm, a different mechanism entirely — neither is built here).
//!
//! Same layering as `installations.rs`: payload parsing and the
//! found/not-found/already-terminal decision are pure, unit-tested with
//! plain `cargo test`; the D1 correlation lookup needs the Workers
//! runtime and is only exercised by the live smoke test
//! (`mise run //packages/cloud-ci-worker:dev`).
//!
//! `lib.rs`'s `/webhooks/github` route is the only caller: it verifies
//! `webhook::verify_signature` first, then parses the raw body into
//! [`WorkflowRunEvent`] and dispatches here.
//!
//! ## Correlation
//!
//! A `workflow_run` webhook carries `repository.id` (the numeric repo id)
//! and `workflow_run.id` (the numeric GitHub Actions run id —
//! `GITHUB_RUN_ID`), neither of which `RunCoordinator`'s own
//! `(repo_id, sha, run_key, attempt)` identity
//! ([`crate::coordinator::do_name`]) can use directly.
//!
//! One tempting shortcut: `run_key`'s GitHub Actions default is
//! `gha/$GITHUB_RUN_ID` (byo-ci.md's "Run identity" table), so
//! `repo_id = workflow_run.repository.id` and
//! `run_key = "gha/{workflow_run.id}"` would reconstruct a matching
//! identity *for runs that used that default*. It silently stops working
//! the moment a caller passes an explicit `--run-key` — a real,
//! documented knob, not a hypothetical — at which point this webhook
//! would either match the wrong run (if some other run happened to reuse
//! the reconstructed key) or, far more likely, just never match at all,
//! permanently stranding that run non-terminal.
//!
//! This module instead correlates on `(repo_id, external_url)`:
//! `external_url` is `BeginRun`'s own field — caller-supplied, or
//! CLI-auto-populated from `GITHUB_RUN_ID`/`GITHUB_SERVER_URL`/etc to a
//! GitHub Actions run's `html_url` — already stored verbatim on the
//! `runs` D1 projection and queryable by value
//! ([`find_run_by_external_url`]). It carries no assumption about how
//! `run_key` was derived, so it keeps matching regardless of whether the
//! caller used the default `run_key` or an explicit one. The tradeoff:
//! a run whose `external_url` was never set (a managed run, or an
//! external run that didn't populate it) simply never correlates to any
//! `workflow_run` delivery — which is correct, not a gap, since nothing
//! else identifies it as "this GitHub Actions run" either.
//!
//! GitHub's own `workflow_run.conclusion` is deliberately never read here:
//! byo-ci.md says "the run's conclusion is the worst job conclusion" —
//! cloud-ci computes its own conclusion from its own job/shard data. Only
//! the webhook's *arrival* with `action: "completed"` is the signal this
//! module acts on.

use serde::Deserialize;
use worker::Env;
use worker::wasm_bindgen::JsValue;

/// `workflow_run.repository` per
/// docs.github.com/en/webhooks/webhook-events-and-payloads#workflow_run
/// (accessed 2026-10-02) — only the numeric id `runs.repo_id` correlates
/// on is needed; `full_name`/`private`/etc. are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRunRepository {
    pub id: u64,
}

/// `workflow_run` per the same doc. `html_url` is what `external_url`
/// would have been set to for a run that auto-populated it from
/// `GITHUB_RUN_ID` (see module docs for the correlation this backs).
/// `conclusion` is intentionally not a field here — see module docs for
/// why GitHub's own verdict is never read.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRunPayload {
    pub id: u64,
    pub html_url: String,
}

/// Top-level `workflow_run` event body. `action` is one of
/// `requested`/`in_progress`/`completed`; only `completed` is acted on
/// (GitHub may add actions over time — any other value is simply a
/// no-op here, not an error, same reasoning as `installation`'s handling
/// of actions this deployment doesn't act on).
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRunEvent {
    pub action: String,
    pub repository: WorkflowRunRepository,
    pub workflow_run: WorkflowRunPayload,
}

/// What `lib.rs::handle_workflow_run_event` should do once (if at all) D1
/// has been queried for `(repo_id, external_url)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// Not a `completed` action; ack without doing anything.
    ActionNotHandled,
    /// No `runs` row matches `(repo_id, external_url)` — not a
    /// cloud-ci-tracked run, or a run whose `external_url` was never set
    /// to this value.
    NoMatch,
    /// The matched run already closed — this same webhook redelivered,
    /// or a different signal got there first; ack without doing anything.
    AlreadyTerminal,
    /// Close this run via `RunCoordinator`.
    Close,
}

/// Pure "found it, and does it still need closing" decision, given the
/// matched run's current [`crate::coordinator::logic::RunState`] if a
/// `(repo_id, external_url)` row was found at all. Kept separate from the
/// D1 lookup so it is unit-testable with plain `cargo test`, same
/// layering as `installations::is_allowed_org`.
pub fn decide_close(
    action: &str,
    matched_run_state: Option<crate::coordinator::logic::RunState>,
) -> CloseDecision {
    if action != "completed" {
        return CloseDecision::ActionNotHandled;
    }
    match matched_run_state {
        None => CloseDecision::NoMatch,
        Some(state) if state.is_terminal() => CloseDecision::AlreadyTerminal,
        Some(_) => CloseDecision::Close,
    }
}

// ---------------------------------------------------------------------------
// D1 read — needs the Workers runtime, not covered by `cargo test` (see
// module docs).
// ---------------------------------------------------------------------------

/// The `runs` row identity + current status `handle_workflow_run_event`
/// needs: `(sha, run_key, attempt)` to derive the Durable Object name
/// ([`crate::coordinator::do_name`]), `status` to answer
/// [`decide_close`]'s "is it already terminal" question without a DO
/// round trip for the common redelivery case.
#[derive(Debug, Clone, Deserialize)]
pub struct CorrelatedRun {
    pub sha: String,
    pub run_key: String,
    pub attempt: i64,
    pub status: String,
}

/// Looks up the `runs` row matching `(repo_id, external_url)` — see
/// module docs for why this is the correlation key instead of
/// reconstructing `run_key`'s default convention.
pub async fn find_run_by_external_url(
    env: &Env,
    repo_id: u64,
    external_url: &str,
) -> worker::Result<Option<CorrelatedRun>> {
    let db = env.d1("DB")?;
    db.prepare(
        "SELECT sha, run_key, attempt, status FROM runs WHERE repo_id = ?1 AND external_url = ?2",
    )
    .bind(&[
        JsValue::from_f64(repo_id as f64),
        JsValue::from_str(external_url),
    ])?
    .first(None)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::logic::RunState;

    #[test]
    fn non_completed_actions_are_not_handled_regardless_of_match() {
        assert_eq!(
            decide_close("requested", None),
            CloseDecision::ActionNotHandled
        );
        assert_eq!(
            decide_close("in_progress", Some(RunState::Running)),
            CloseDecision::ActionNotHandled
        );
    }

    #[test]
    fn completed_with_no_matching_run_is_a_no_op() {
        assert_eq!(decide_close("completed", None), CloseDecision::NoMatch);
    }

    #[test]
    fn completed_for_an_already_terminal_run_is_idempotent() {
        for state in [
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Abandoned,
        ] {
            assert_eq!(
                decide_close("completed", Some(state)),
                CloseDecision::AlreadyTerminal,
                "{state:?}"
            );
        }
    }

    #[test]
    fn completed_for_a_non_terminal_run_closes_it() {
        for state in [RunState::Queued, RunState::Running, RunState::Merging] {
            assert_eq!(
                decide_close("completed", Some(state)),
                CloseDecision::Close,
                "{state:?}"
            );
        }
    }

    #[test]
    fn workflow_run_payload_parses_only_the_fields_this_module_needs()
    -> Result<(), serde_json::Error> {
        let body = r#"{
            "action": "completed",
            "repository": {"id": 1296269, "full_name": "acme/widgets"},
            "workflow_run": {
                "id": 42,
                "html_url": "https://github.com/acme/widgets/actions/runs/42",
                "conclusion": "success"
            }
        }"#;
        let event: WorkflowRunEvent = serde_json::from_str(body)?;
        assert_eq!(event.action, "completed");
        assert_eq!(event.repository.id, 1_296_269);
        assert_eq!(event.workflow_run.id, 42);
        assert_eq!(
            event.workflow_run.html_url,
            "https://github.com/acme/widgets/actions/runs/42"
        );
        Ok(())
    }
}
