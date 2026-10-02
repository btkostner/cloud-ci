//! Pure decision logic for `RepoState`'s `admit_run`/`complete_run`:
//! cancel-superseded slot handling, concurrency-cap checks, and
//! idempotency, with no `worker`/Durable-Object dependency, so it is
//! unit-testable with plain `cargo test`. The Durable Object in
//! [`super`] is the only caller; it supplies the current persisted rows
//! (as plain data, via [`AdmissionRow`]) and applies whatever
//! [`decide_admit`]/[`decide_complete`] decide.
//!
//! See `repo_state` module docs for the full scope boundary (pure
//! admission/concurrency state machine only — no pipeline discovery, no
//! Dynamic Worker, no real cancellation of in-flight work).

/// One admission record's lifecycle state. `Active` holds its
/// `(pipeline_file, group)` slot and counts against the concurrency
/// caps; `Cancelled`/`Completed` are both terminal and free the slot —
/// kept as two distinct values (rather than one `Terminal`) only so a
/// caller inspecting history can tell "superseded" from "ran to
/// completion", matching dynamic-pipelines.md's "a newer run cancels
/// the older one in the group" wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionState {
    Active,
    Cancelled,
    Completed,
}

/// The identity a `RepoState` instance uses everywhere: `pipeline_file`
/// and `run_key` are kept as distinct fields per this round's RPC
/// signature, even though architecture.md's "Managed run" flow step 2
/// sets `run_key` to the pipeline file name in practice ("with `run
/// key` set to the pipeline file name") — a future caller that always
/// passes `pipeline_file == run_key` is free to do so; this module does
/// not assume it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity {
    pub pipeline_file: String,
    pub run_key: String,
    pub attempt: u32,
}

/// The subset of one `admission` row this module's decisions need.
#[derive(Debug, Clone)]
pub struct AdmissionRow {
    pub identity: RunIdentity,
    pub group_name: String,
    pub state: AdmissionState,
}

/// `settings.yml`'s `concurrency.*` caps (docs/design/settings.md's
/// "Field reference" table), passed in by the caller as plain
/// parameters this round — see `repo_state` module docs for why this
/// round never fetches `settings.yml` itself.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// `concurrency.repository`: max containers across all runs of this
    /// repo at once. This round has no real containers, so the DO
    /// checks it against *admitted active runs* as a proxy — see
    /// `repo_state` module docs for why.
    pub repository: u32,
    /// `concurrency.pipelines`: max concurrent pipeline runs for this
    /// repo — checked directly, since "active admitted runs" *is* the
    /// real definition of this cap, not a proxy.
    pub pipelines: u32,
    /// `concurrency.pipeline`: max containers for a single pipeline
    /// run. Stored on the admitted row as a declared value; never
    /// enforced this round (no Executor exists to start real
    /// containers against it yet).
    pub pipeline: u32,
}

/// One `admit_run` call's inputs, after the DO has already looked up
/// any existing rows it needs (see [`decide_admit`]'s parameters for
/// what those are).
#[derive(Debug, Clone)]
pub struct AdmitRequest {
    pub identity: RunIdentity,
    pub group_name: String,
    pub cancel_superseded: bool,
    pub caps: Caps,
}

/// Which named cap rejected an admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapName {
    Repository,
    Pipelines,
}

/// [`decide_admit`]'s decision. The DO shell turns each variant into
/// the storage writes (insert/cancel) and the wire response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitDecision {
    /// A row for this exact identity is already `Active`. No-op: the
    /// caller gets back the same admission it already holds, per this
    /// round's redelivery/idempotency requirement. Nothing is written.
    AlreadyAdmitted,
    /// Admitted into `group_name`'s slot. `cancelled` is the identity
    /// of the prior slot holder this admission superseded, if any —
    /// **bookkeeping only**: the DO marks that row `Cancelled` in its
    /// own SQLite and nothing else. It does not call any
    /// `RunCoordinator` to cancel real work; that cross-DO wiring is a
    /// separate future round (see `repo_state` module docs' scope
    /// boundary, loudly).
    Admitted { cancelled: Option<RunIdentity> },
    /// `group_name`'s slot is already held by another active run and
    /// `cancel_superseded` was `false`. dynamic-pipelines.md's
    /// Concurrency section says such runs "serialize" — this round
    /// models that as a typed rejection the caller can choose to queue
    /// or surface, rather than inventing a queueing mechanism here (see
    /// `repo_state` module docs).
    RejectedSlotOccupied { holder: RunIdentity },
    /// The repo is at a concurrency cap. Carries which cap, since
    /// `Repository` and `Pipelines` are reported identically this round
    /// (same "active admitted runs" count checked against two
    /// independently configured limits — see [`Caps`]'s doc comments)
    /// but a caller may still want to know which one to report.
    RejectedAtCap { cap: CapName, limit: u32 },
}

/// Decides one `admit_run` call.
///
/// - `req` is the incoming request.
/// - `existing_by_identity` is the row (any state) already stored under
///   `req.identity`, if any — `None` the first time this identity is
///   ever seen.
/// - `active_slot_holder` is the row currently `Active` in
///   `(req.identity.pipeline_file, req.group_name)`'s slot, if any.
///   Must never be the same row as `existing_by_identity` — the DO
///   shell looks this up only after ruling out the idempotent-no-op
///   case, since a run's own identity can never "occupy" its own slot
///   as someone else's holder.
/// - `active_count` is the number of `Active` rows across the whole
///   repo (all pipeline files, all groups) *before* this decision,
///   including `active_slot_holder` if present.
pub fn decide_admit(
    req: &AdmitRequest,
    existing_by_identity: Option<&AdmissionRow>,
    active_slot_holder: Option<&AdmissionRow>,
    active_count: u32,
) -> AdmitDecision {
    if let Some(existing) = existing_by_identity
        && existing.state == AdmissionState::Active
    {
        return AdmitDecision::AlreadyAdmitted;
    }
    // Terminal (Cancelled/Completed) row under this exact identity, or
    // no row at all: not a redelivery of a live admission, but either
    // a brand-new identity or a fresh admission attempt that happens
    // to reuse an already-settled identity (e.g. a manual rerun with
    // the same attempt number after the prior attempt completed).
    // Falls through to ordinary admission below either way.

    if let Some(holder) = active_slot_holder {
        if !req.cancel_superseded {
            return AdmitDecision::RejectedSlotOccupied {
                holder: holder.identity.clone(),
            };
        }
        // The holder is about to be cancelled (freeing its slot) in the
        // same operation that admits the new run (taking the slot), so
        // the net change in active-run count is zero: one leaves, one
        // enters. `effective_count` reflects that net-neutral swap
        // rather than double-counting the departing holder against the
        // caps.
        let effective_count = active_count.saturating_sub(1);
        if let Some(decision) = reject_if_at_cap(effective_count, req.caps) {
            return decision;
        }
        return AdmitDecision::Admitted {
            cancelled: Some(holder.identity.clone()),
        };
    }

    if let Some(decision) = reject_if_at_cap(active_count, req.caps) {
        return decision;
    }
    AdmitDecision::Admitted { cancelled: None }
}

/// Shared cap check for both the "replacing a slot holder" and "slot
/// free" paths in [`decide_admit`]: `effective_count` is the active-run
/// count the repo would have *before* adding the run being admitted
/// now. Checks `pipelines` before `repository` — both use the same
/// underlying count this round (see [`Caps`]'s doc comments), so the
/// order only affects which `CapName` a simultaneous breach of both
/// reports, and `concurrency.pipelines` is the cap
/// dynamic-pipelines.md's Limits table names first.
fn reject_if_at_cap(effective_count: u32, caps: Caps) -> Option<AdmitDecision> {
    if effective_count >= caps.pipelines {
        return Some(AdmitDecision::RejectedAtCap {
            cap: CapName::Pipelines,
            limit: caps.pipelines,
        });
    }
    if effective_count >= caps.repository {
        return Some(AdmitDecision::RejectedAtCap {
            cap: CapName::Repository,
            limit: caps.repository,
        });
    }
    None
}

/// [`decide_complete`]'s decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteDecision {
    /// The row was `Active` and is now freed. The DO shell marks it
    /// `Completed`.
    Released,
    /// The row exists but was already `Cancelled`/`Completed` —
    /// idempotent no-op, same "redelivered call must not double-free a
    /// slot" requirement `admit_run` has for double-admission.
    AlreadyTerminal,
    /// No row exists for this identity at all.
    NotFound,
}

/// Decides one `complete_run` call, given the row (any state) stored
/// under the identity, if any.
pub fn decide_complete(existing: Option<&AdmissionRow>) -> CompleteDecision {
    match existing {
        None => CompleteDecision::NotFound,
        Some(row) if row.state == AdmissionState::Active => CompleteDecision::Released,
        Some(_) => CompleteDecision::AlreadyTerminal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(pipeline_file: &str, run_key: &str, attempt: u32) -> RunIdentity {
        RunIdentity {
            pipeline_file: pipeline_file.into(),
            run_key: run_key.into(),
            attempt,
        }
    }

    fn row(
        pipeline_file: &str,
        run_key: &str,
        attempt: u32,
        state: AdmissionState,
    ) -> AdmissionRow {
        AdmissionRow {
            identity: identity(pipeline_file, run_key, attempt),
            group_name: "main".into(),
            state,
        }
    }

    fn caps(repository: u32, pipelines: u32, pipeline: u32) -> Caps {
        Caps {
            repository,
            pipelines,
            pipeline,
        }
    }

    fn req(cancel_superseded: bool, caps: Caps) -> AdmitRequest {
        AdmitRequest {
            identity: identity("ci.ts", "ci.ts", 1),
            group_name: "main".into(),
            cancel_superseded,
            caps,
        }
    }

    #[test]
    fn redelivered_identity_already_active_is_idempotent_no_op() {
        let existing = row("ci.ts", "ci.ts", 1, AdmissionState::Active);
        let decision = decide_admit(
            &req(true, caps(40, 4, 12)),
            Some(&existing),
            None,
            1, // the existing row itself
        );
        assert_eq!(decision, AdmitDecision::AlreadyAdmitted);
    }

    #[test]
    fn terminal_row_under_same_identity_falls_through_to_fresh_admission() {
        let existing = row("ci.ts", "ci.ts", 1, AdmissionState::Completed);
        let decision = decide_admit(&req(true, caps(40, 4, 12)), Some(&existing), None, 0);
        assert_eq!(decision, AdmitDecision::Admitted { cancelled: None });
    }

    #[test]
    fn empty_slot_admits_directly() {
        let decision = decide_admit(&req(true, caps(40, 4, 12)), None, None, 0);
        assert_eq!(decision, AdmitDecision::Admitted { cancelled: None });
    }

    #[test]
    fn occupied_slot_with_cancel_superseded_cancels_holder_and_admits() {
        let holder = row("ci.ts", "ci.ts", 0, AdmissionState::Active);
        let decision = decide_admit(&req(true, caps(40, 4, 12)), None, Some(&holder), 1);
        assert_eq!(
            decision,
            AdmitDecision::Admitted {
                cancelled: Some(identity("ci.ts", "ci.ts", 0))
            }
        );
    }

    #[test]
    fn occupied_slot_without_cancel_superseded_is_rejected() {
        let holder = row("ci.ts", "ci.ts", 0, AdmissionState::Active);
        let decision = decide_admit(&req(false, caps(40, 4, 12)), None, Some(&holder), 1);
        assert_eq!(
            decision,
            AdmitDecision::RejectedSlotOccupied {
                holder: identity("ci.ts", "ci.ts", 0)
            }
        );
    }

    #[test]
    fn at_pipelines_cap_is_rejected() {
        let decision = decide_admit(&req(true, caps(40, 4, 12)), None, None, 4);
        assert_eq!(
            decision,
            AdmitDecision::RejectedAtCap {
                cap: CapName::Pipelines,
                limit: 4
            }
        );
    }

    #[test]
    fn below_pipelines_cap_but_at_repository_cap_is_rejected_as_repository() {
        let decision = decide_admit(&req(true, caps(2, 40, 12)), None, None, 2);
        assert_eq!(
            decision,
            AdmitDecision::RejectedAtCap {
                cap: CapName::Repository,
                limit: 2
            }
        );
    }

    #[test]
    fn cancel_superseded_swap_does_not_double_count_departing_holder_against_cap() {
        // active_count = 4 (the cap), but one of those 4 is the slot
        // holder about to be cancelled, so the net count after the swap
        // stays at 4 — must still admit.
        let holder = row("ci.ts", "ci.ts", 0, AdmissionState::Active);
        let decision = decide_admit(&req(true, caps(40, 4, 12)), None, Some(&holder), 4);
        assert_eq!(
            decision,
            AdmitDecision::Admitted {
                cancelled: Some(identity("ci.ts", "ci.ts", 0))
            }
        );
    }

    #[test]
    fn slot_occupied_rejection_skips_cap_check_entirely() {
        // Even over every cap, a plain "serialize" rejection must not be
        // reported as a cap rejection — it is a different failure mode.
        let holder = row("ci.ts", "ci.ts", 0, AdmissionState::Active);
        let decision = decide_admit(&req(false, caps(0, 0, 0)), None, Some(&holder), 99);
        assert_eq!(
            decision,
            AdmitDecision::RejectedSlotOccupied {
                holder: identity("ci.ts", "ci.ts", 0)
            }
        );
    }

    #[test]
    fn complete_active_row_releases() {
        let existing = row("ci.ts", "ci.ts", 1, AdmissionState::Active);
        assert_eq!(decide_complete(Some(&existing)), CompleteDecision::Released);
    }

    #[test]
    fn complete_already_cancelled_row_is_idempotent_no_op() {
        let existing = row("ci.ts", "ci.ts", 1, AdmissionState::Cancelled);
        assert_eq!(
            decide_complete(Some(&existing)),
            CompleteDecision::AlreadyTerminal
        );
    }

    #[test]
    fn complete_already_completed_row_is_idempotent_no_op() {
        let existing = row("ci.ts", "ci.ts", 1, AdmissionState::Completed);
        assert_eq!(
            decide_complete(Some(&existing)),
            CompleteDecision::AlreadyTerminal
        );
    }

    #[test]
    fn complete_unknown_identity_is_not_found() {
        assert_eq!(decide_complete(None), CompleteDecision::NotFound);
    }

    #[test]
    fn admitting_into_a_different_pipeline_files_same_group_name_is_independent() {
        // Groups are scoped per pipeline file (dynamic-pipelines.md's
        // Concurrency section: "the same pipeline file with the same
        // group string serialize") — a holder in ci.ts's "main" group
        // must not block deploy.ts's own "main" group. This is enforced
        // by the DO shell's slot lookup being keyed on
        // (pipeline_file, group_name), not tested again here since this
        // module only sees whatever `active_slot_holder` the shell
        // already scoped correctly; asserting `None` here documents
        // that a correctly-scoped shell passes `None` for an unrelated
        // pipeline file's identically-named group.
        let decision = decide_admit(&req(true, caps(40, 4, 12)), None, None, 0);
        assert_eq!(decision, AdmitDecision::Admitted { cancelled: None });
    }
}
