//! `pull_request` GitHub webhook handling — closes the gap
//! `pull_request_state`'s module doc comment names as this round's explicit
//! scope boundary ("no webhook wiring yet"). Wires `pull_request.opened`
//! and `pull_request.synchronize` as callers of
//! [`crate::pull_request_state::PullRequestStateStore::notify_dirty`]/
//! [`crate::pull_request_state::PullRequestStateStore::update_head_sha`],
//! per docs/design/pr-comment.md's "`PullRequestState`: one writer per PR"
//! section: "Tracks the PR's current `head_sha` in its own DO storage,
//! seeded from the `pull_request` webhook" and "On
//! `pull_request.synchronize` (new head sha), the same `PullRequestState`
//! instance updates its tracked `head_sha` in place."
//!
//! This is a **separate** seeding path from the one pr-comment.md
//! describes for `RunCoordinator` ("the same call fires when the first
//! run ... for a head sha is created, so the placeholder comment goes up
//! before any job has reported anything") — that path needs a real
//! `RunCoordinator` caller, not built yet. This module only wires the
//! `pull_request` webhook itself.
//!
//! Same layering as `workflow_run.rs`: payload parsing and the
//! action-dispatch decision are pure, unit-tested with plain
//! `cargo test`; the DO-touching parts need the Workers runtime and are
//! only exercised by the live smoke test
//! (`mise run //packages/cloud-ci-worker:dev`).
//!
//! `lib.rs`'s `/webhooks/github` route is the only caller: it verifies
//! `webhook::verify_signature` first, then parses the raw body into
//! [`PullRequestEvent`] and dispatches here.
//!
//! # `closed` action
//!
//! pr-comment.md never specifies what happens to a closed PR's sticky
//! comment or debounce state, so this round treats `closed` as a no-op
//! for `PullRequestState`: there is no real rendered comment yet to stop
//! updating (the DO's `alarm` flush is still an explicit stub — see
//! `pull_request_state` module docs), and no caller after this round would
//! `notify_dirty` a closed PR's instance anyway once the real
//! `RunCoordinator` wiring lands (no more runs get created against a
//! closed PR). Explicitly deferred, not a gap: if a later round needs
//! closed-PR cleanup (e.g. a final "merged"/"closed" comment edit), that
//! is new, undocumented behavior pr-comment.md would need to specify
//! first — this round does not invent it.
//!
//! # Idempotency
//!
//! GitHub redelivers webhooks at-least-once
//! (docs.github.com/en/webhooks/using-webhooks/handling-webhook-deliveries,
//! accessed 2026-10-02). A redelivered `opened` for a PR whose `head_sha`
//! is already tracked is **not** a double-seed:
//! [`crate::pull_request_state::logic::classify_notify_dirty`] compares
//! the incoming `head_sha` against what the instance already has tracked
//! and returns `Fold`, not `Seed`, the moment `tracked_head_sha ==
//! Some(incoming)` — which is exactly the redelivered-same-sha case. The
//! DO's own `handle_notify_dirty` additionally only grants the one-time
//! "initial placeholder" 0s-bypass flush when `comment_id` and
//! `first_dirty_at` are both still unset, so a redelivered `opened` folds
//! into the existing burst with normal quiet-window timing (or, if the
//! first flush already happened, does not re-flush at all) instead of
//! re-triggering the placeholder bypass. No fix was needed here — the DO
//! built last round already handles this correctly; this module just
//! confirms it by construction (same `notify_dirty` call site, same
//! `reason`, regardless of redelivery).

use serde::Deserialize;
use worker::Env;

use crate::pull_request_state::{
    NotifyDirtyRequest, PullRequestStateStore, UpdateHeadShaRequest, do_name,
};

/// `pull_request.repository` per
/// docs.github.com/en/webhooks/webhook-events-and-payloads#pull_request
/// (accessed 2026-10-02) — only the numeric id `do_name`'s `repo_id`
/// needs; `full_name`/`private`/etc. are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestEventRepository {
    pub id: u64,
}

/// `pull_request.pull_request.head` per the same doc.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestHead {
    pub sha: String,
}

/// `pull_request.pull_request` per the same doc — only `head.sha` is
/// needed; the top-level `number` field (not this nested object's own
/// `number`) is what GitHub calls the PR number.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestPayload {
    pub head: PullRequestHead,
}

/// Top-level `pull_request` event body. `action` is one of
/// `opened`/`synchronize`/`closed`/`reopened`/`edited`/`labeled`/etc
/// (GitHub may add actions over time); see [`decide_action`] for which
/// ones this deployment acts on.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestEvent {
    pub action: String,
    pub number: u64,
    pub repository: PullRequestEventRepository,
    pub pull_request: PullRequestPayload,
}

/// What `lib.rs::handle_pull_request_event` should do for a given
/// `action`, per pr-comment.md's `pull_request`-webhook rows (see module
/// docs). Kept separate from the DO call so it is unit-testable with
/// plain `cargo test`, same layering as `workflow_run::decide_close`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestAction {
    /// A brand new PR (or the first webhook this deployment has ever
    /// seen for it): seed `head_sha` via `notify_dirty`.
    Seed,
    /// New commits pushed to an existing PR: update `head_sha` in place.
    UpdateHeadSha,
    /// The PR closed: no-op this round (see module docs' "`closed`
    /// action" section for why).
    ClosedNoOp,
    /// Any other action (`reopened`, `edited`, `labeled`, etc): no-op,
    /// same "GitHub may add actions over time" reasoning
    /// `installations.rs`/`workflow_run.rs` already use for their own
    /// unhandled actions.
    Unhandled,
}

/// Pure action-dispatch decision — see [`PullRequestAction`] for what
/// each outcome means.
pub fn decide_action(action: &str) -> PullRequestAction {
    match action {
        "opened" => PullRequestAction::Seed,
        "synchronize" => PullRequestAction::UpdateHeadSha,
        "closed" => PullRequestAction::ClosedNoOp,
        _ => PullRequestAction::Unhandled,
    }
}

/// The reason string passed to `notify_dirty` for a `pull_request.opened`
/// seed call — not one of pr-comment.md's typed dirty reasons (those are
/// all post-run-creation events); this module's own name for "a PR just
/// opened", mirroring `pull_request_state::logic`'s own
/// `"run_terminal"` placeholder-naming precedent for a reason the doc
/// doesn't give a wire shape to yet.
pub const OPENED_REASON: &str = "pull_request_opened";

/// Handles one `pull_request` webhook delivery: parses the raw body,
/// dispatches on `action` via [`decide_action`], and calls the matching
/// `PullRequestState` RPC. Returns `Err` only for a malformed body or a
/// DO-call failure — `lib.rs` turns either into an error response; every
/// other outcome (including `ClosedNoOp`/`Unhandled`) is a silent
/// success.
pub async fn handle_pull_request_event(
    env: &Env,
    raw_body: &[u8],
) -> Result<(), PullRequestWebhookError> {
    let event: PullRequestEvent = serde_json::from_slice(raw_body)
        .map_err(|e| PullRequestWebhookError::MalformedPayload(e.to_string()))?;

    match decide_action(&event.action) {
        PullRequestAction::Seed => {
            let store = store_for(env, event.repository.id, event.number)?;
            store
                .notify_dirty(&NotifyDirtyRequest {
                    head_sha: event.pull_request.head.sha,
                    reason: OPENED_REASON.to_string(),
                })
                .await
                .map_err(|e| PullRequestWebhookError::DoCall(e.to_string()))?;
        }
        PullRequestAction::UpdateHeadSha => {
            let store = store_for(env, event.repository.id, event.number)?;
            store
                .update_head_sha(&UpdateHeadShaRequest {
                    new_sha: event.pull_request.head.sha,
                })
                .await
                .map_err(|e| PullRequestWebhookError::DoCall(e.to_string()))?;
        }
        PullRequestAction::ClosedNoOp | PullRequestAction::Unhandled => {}
    }
    Ok(())
}

fn store_for(
    env: &Env,
    repo_id: u64,
    pr_number: u64,
) -> Result<PullRequestStateStore, PullRequestWebhookError> {
    PullRequestStateStore::new(env, &do_name(repo_id, pr_number))
        .map_err(|e| PullRequestWebhookError::DoCall(e.to_string()))
}

#[derive(Debug, Clone)]
pub enum PullRequestWebhookError {
    MalformedPayload(String),
    DoCall(String),
}

impl std::fmt::Display for PullRequestWebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedPayload(msg) => write!(f, "malformed pull_request payload: {msg}"),
            Self::DoCall(msg) => write!(f, "pull request state call failed: {msg}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opened_seeds() {
        assert_eq!(decide_action("opened"), PullRequestAction::Seed);
    }

    #[test]
    fn synchronize_updates_head_sha() {
        assert_eq!(
            decide_action("synchronize"),
            PullRequestAction::UpdateHeadSha
        );
    }

    #[test]
    fn closed_is_a_documented_no_op() {
        assert_eq!(decide_action("closed"), PullRequestAction::ClosedNoOp);
    }

    #[test]
    fn other_actions_are_unhandled_not_errors() {
        for action in [
            "reopened",
            "edited",
            "labeled",
            "assigned",
            "ready_for_review",
        ] {
            assert_eq!(
                decide_action(action),
                PullRequestAction::Unhandled,
                "{action}"
            );
        }
    }

    #[test]
    fn pull_request_payload_parses_only_the_fields_this_module_needs()
    -> Result<(), serde_json::Error> {
        let body = r#"{
            "action": "opened",
            "number": 42,
            "repository": {"id": 1296269, "full_name": "acme/widgets"},
            "pull_request": {
                "number": 42,
                "head": {"sha": "abc123", "ref": "feature-branch"},
                "title": "Add feature"
            }
        }"#;
        let event: PullRequestEvent = serde_json::from_str(body)?;
        assert_eq!(event.action, "opened");
        assert_eq!(event.number, 42);
        assert_eq!(event.repository.id, 1_296_269);
        assert_eq!(event.pull_request.head.sha, "abc123");
        Ok(())
    }

    /// A redelivered `opened` with the same `head_sha` the instance
    /// already has tracked classifies as `Fold`, not `Seed` — confirming
    /// `pull_request_state::logic::classify_notify_dirty`'s existing
    /// behavior is already idempotent for this caller (see module docs'
    /// "Idempotency" section). This module always dispatches `opened`
    /// through the same `notify_dirty` call regardless of redelivery; the
    /// DO itself is what decides seed vs. fold.
    #[test]
    fn redelivered_opened_with_same_head_sha_is_classified_as_fold_not_seed() {
        use crate::pull_request_state::logic::{DirtyPath, classify_notify_dirty};

        // First delivery: nothing tracked yet.
        assert_eq!(classify_notify_dirty(None, "abc123"), DirtyPath::Seed);
        // Redelivery of the same `opened` webhook: `abc123` already
        // tracked from the first delivery.
        assert_eq!(
            classify_notify_dirty(Some("abc123"), "abc123"),
            DirtyPath::Fold
        );
    }
}
