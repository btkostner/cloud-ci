//! The `cloud-ci-merge` Queue's message type plus the pure decision logic its consumer needs —
//! the real dispatch target for `RunCoordinator::handle_shard_terminal`'s
//! `ShardBarrierDecision::Satisfied { merge: true, included_idxs }` decision
//! (`docs/design/parallelization.md`'s "### Merge barrier (RunCoordinator)": "On satisfaction,
//! `RunCoordinator` decides whether to merge ... On satisfaction, `RunCoordinator` enqueues the
//! merge step"). `coordinator::mod`'s own module doc comment ("Shard groups / merge barrier"
//! section) previously documented this dispatch as entirely unbuilt ("nothing dispatches it
//! yet") — this round closes that gap for the two report types parallelization.md's "### Merge
//! strategies per report type" table marks as natively (no-container) mergeable.
//!
//! # Scope boundary — native `junit`/`coverage` (lcov) merge only
//!
//! parallelization.md's merge-strategy table has four rows. This round builds the first two —
//! `junit` and `coverage` — exactly as the table describes: "Worker / `post-run-analysis` Queue
//! consumer (no container)". This module (plus [`cloud_ci_reports::merge`], which does the real
//! XML/lcov work) and [`crate::handle_shard_merge_requested`][super::handle_shard_merge_requested]
//! (`src/lib.rs`'s Queue consumer glue) together are that consumer. `coverage` here means `lcov`
//! specifically — `cloud-ci-reports` has no `cobertura` parser yet (grepping its `src/` tree
//! finds only `junit`/`lcov`/`playwright`/`vitest`), so a `cobertura`-kind report in
//! `included_idxs` is treated the same as any other [`MergeableKind::Unsupported`] kind: logged
//! and skipped, never force-merged with lossy/incorrect logic.
//!
//! **Not built this round — the `playwright-blob`/`vitest-blob` container-node path.**
//! parallelization.md's table's other two rows need a *generated* `<id>/merge` container node
//! (`npx playwright merge-reports`/`npx vitest --merge-reports`, since neither framework's merge
//! tooling runs under `workers-rs`/wasm32 — see that doc's own "JUnit and coverage formats are
//! plain structured text ... Playwright and Vitest blob reporters are opaque ... only the
//! framework's own Node.js tooling can merge" paragraph). Generating that node is
//! `ci.shard`'s own job (`merge.{runner,setup,command}` options, parallelization.md's `ci.shard`
//! options table), and `ci.shard` does not exist yet in `cloud-ci-pipeline-sdk` — grepping that
//! package's `src/` finds only `ci.check`/`ci.container` entry points, confirmed before this
//! round started. Building the container-node path here would mean inventing `ci.shard`'s
//! generated-node shape speculatively, ahead of the SDK entry point that is supposed to drive
//! it — exactly the kind of "foundation ahead of its caller" this crate's established convention
//! (see `coordinator::mod`'s Nodes section) reserves for a dedicated round once `ci.shard` is
//! real. `MergeableKind::Unsupported` is this round's honest stand-in: a `playwright-blob`/
//! `vitest-blob` report reaching this consumer today is logged and skipped, not merged incorrectly.
//!
//! # Testability
//!
//! [`group_by_kind_and_name`], [`merged_report_r2_key`], and [`mergeable_kind`] are pure (no
//! `worker`/D1/R2 dependency) and unit-tested below with plain `cargo test`. The Queue/D1/R2
//! wiring around them (`src/lib.rs`'s `#[event(queue)]` handler routing, D1 reads, R2 reads/
//! writes) is Workers-runtime-only, same documented convention as `ai_queue.rs`'s own module
//! docs and every other live-infra piece in this crate.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `reports.kind`'s value for a JUnit XML report (`coordinator::mod::parse_report`'s own
/// lowercase kind dispatch).
pub const JUNIT_KIND: &str = "junit";
/// `reports.kind`'s value for an lcov tracefile.
pub const LCOV_KIND: &str = "lcov";

/// The one message `RunCoordinator::handle_shard_terminal` enqueues onto `cloud-ci-merge` on a
/// `ShardBarrierDecision::Satisfied { merge: true, .. }` outcome. Carries only what the consumer
/// needs to re-read the shard group's own canonical reports from D1 — never the reports'
/// payload itself, matching `ai_queue::AnalysisRequested`'s same "re-read authoritative state"
/// discipline (this crate's "Inputs enqueue, coordinators decide" invariant's Queue-consumer
/// sibling).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardMergeRequested {
    pub run_id: String,
    pub job_id: String,
    pub job_name: String,
    /// The shard indices `evaluate_barrier` selected for this merge — every shard when
    /// `merge_on_failure` is `always`, or only the passed shards under `if_any_passed`
    /// (parallelization.md: "`if_any_passed` (default) ... runs the merge using only the
    /// successful shards' reports").
    pub included_idxs: Vec<u32>,
}

/// One canonical report row the consumer reads from D1's `reports` projection, already scoped
/// to this shard group's `job_id` and `included_idxs` — [`group_by_kind_and_name`]'s input.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MergeCandidateReport {
    pub kind: String,
    pub name: String,
    pub shard_index: u32,
    pub r2_key: String,
}

/// Groups `rows` by `(kind, name)` — parallelization.md's merge-strategy table applies per
/// report *name*, not per job as a whole: a job that uploads both a `junit` report named
/// `results.xml` and an `lcov` report named `coverage.info` produces two independent merge
/// outputs, one per `(kind, name)` group, each written to its own R2 key
/// ([`merged_report_r2_key`]). Order-preserving over first-occurrence `(kind, name)` pairs —
/// this crate's established grouping convention
/// (`coordinator::mod::finalize_test_stats`'s own grouping loop, `logic::new_check_names`'s doc
/// comment).
pub fn group_by_kind_and_name(
    rows: Vec<MergeCandidateReport>,
) -> Vec<((String, String), Vec<MergeCandidateReport>)> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), Vec<MergeCandidateReport>> = HashMap::new();
    for row in rows {
        let key = (row.kind.clone(), row.name.clone());
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(row);
    }
    order
        .into_iter()
        .map(|key| {
            let rows = groups.remove(&key).unwrap_or_default();
            (key, rows)
        })
        .collect()
}

/// The two report kinds this round merges natively, and the catch-all for everything else —
/// see this module's doc comment's scope-boundary section for exactly why `Unsupported` covers
/// both the container-node formats (`playwright-blob`/`vitest-blob`) and `cobertura` (no parser
/// yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeableKind {
    Junit,
    Lcov,
    Unsupported,
}

pub fn mergeable_kind(kind: &str) -> MergeableKind {
    match kind {
        JUNIT_KIND => MergeableKind::Junit,
        LCOV_KIND => MergeableKind::Lcov,
        _ => MergeableKind::Unsupported,
    }
}

/// `junit` merges to an XML document; `lcov` merges to a plain-text tracefile — `.info` is the
/// conventional lcov extension (`lcov --capture -o coverage.info`, `geninfo`'s own default
/// output name). An unsupported kind never reaches this function — callers branch on
/// [`mergeable_kind`] before calling it.
fn file_extension_for_kind(kind: MergeableKind) -> &'static str {
    match kind {
        MergeableKind::Junit => ".xml",
        MergeableKind::Lcov => ".info",
        MergeableKind::Unsupported => "",
    }
}

/// The content-addressed R2 key a merged report is written to:
/// `runs/{run_id}/jobs/{job_name}/merged/{report_kind}/{content_sha256}{ext}` — extending this
/// crate's existing `runs/{run_id}/...` R2 key convention
/// (`coordinator::mod::handle_submit_report`'s own `runs/{run_id}/reports/{job_id}/
/// {shard_index}/{content_sha256}` key for a single shard's report; `docs/design/byo-ci.md`'s
/// "Data model" § R2 keys table for the publication-alias/artifact siblings). Content-addressed
/// like `uploads.r2_key`, not slot-addressed like the publication alias — a merged document has
/// no single shard slot to replace in place, and content-addressing means re-merging identical
/// input (a redelivered Queue message) always lands on the same key, which is what makes
/// [`crate::handle_shard_merge_requested`][super::handle_shard_merge_requested]'s R2 write safe
/// to repeat.
pub fn merged_report_r2_key(
    run_id: &str,
    job_name: &str,
    report_kind: MergeableKind,
    kind_str: &str,
    content_sha256: &str,
) -> String {
    format!(
        "runs/{run_id}/jobs/{job_name}/merged/{kind_str}/{content_sha256}{}",
        file_extension_for_kind(report_kind)
    )
}

/// Lowercase hex-encoded SHA-256 digest of `bytes` — the merged document's `content_sha256`,
/// same encoding this crate's other content-addressed R2 keys already use
/// (`coordinator::mod::hex_sha256`'s private sibling; duplicated here rather than exported
/// across modules since that function is `coordinator::mod`-private and this module has no
/// dependency on `coordinator` otherwise).
pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(kind: &str, name: &str, shard_index: u32, r2_key: &str) -> MergeCandidateReport {
        MergeCandidateReport {
            kind: kind.to_string(),
            name: name.to_string(),
            shard_index,
            r2_key: r2_key.to_string(),
        }
    }

    #[test]
    fn group_by_kind_and_name_splits_distinct_report_names_independently() {
        let rows = vec![
            candidate("junit", "results.xml", 1, "r2/1/junit"),
            candidate("lcov", "coverage.info", 1, "r2/1/lcov"),
            candidate("junit", "results.xml", 2, "r2/2/junit"),
            candidate("lcov", "coverage.info", 2, "r2/2/lcov"),
        ];

        let groups = group_by_kind_and_name(rows);

        assert_eq!(groups.len(), 2);
        let (key0, rows0) = &groups[0];
        assert_eq!(key0, &("junit".to_string(), "results.xml".to_string()));
        assert_eq!(rows0.len(), 2);
        assert_eq!(rows0[0].shard_index, 1);
        assert_eq!(rows0[1].shard_index, 2);

        let (key1, rows1) = &groups[1];
        assert_eq!(key1, &("lcov".to_string(), "coverage.info".to_string()));
        assert_eq!(rows1.len(), 2);
    }

    #[test]
    fn group_by_kind_and_name_preserves_first_occurrence_order() {
        let rows = vec![
            candidate("lcov", "coverage.info", 1, "a"),
            candidate("junit", "results.xml", 1, "b"),
            candidate("lcov", "coverage.info", 2, "c"),
        ];

        let groups = group_by_kind_and_name(rows);

        assert_eq!(
            groups.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            vec![
                ("lcov".to_string(), "coverage.info".to_string()),
                ("junit".to_string(), "results.xml".to_string()),
            ]
        );
    }

    #[test]
    fn group_by_kind_and_name_empty_input_produces_no_groups() {
        assert!(group_by_kind_and_name(Vec::new()).is_empty());
    }

    #[test]
    fn mergeable_kind_recognizes_junit_and_lcov_only() {
        assert_eq!(mergeable_kind("junit"), MergeableKind::Junit);
        assert_eq!(mergeable_kind("lcov"), MergeableKind::Lcov);
        assert_eq!(mergeable_kind("cobertura"), MergeableKind::Unsupported);
        assert_eq!(
            mergeable_kind("playwright-blob"),
            MergeableKind::Unsupported
        );
        assert_eq!(mergeable_kind("vitest-blob"), MergeableKind::Unsupported);
    }

    #[test]
    fn merged_report_r2_key_uses_content_addressed_merged_path_with_kind_extension() {
        let key = merged_report_r2_key(
            "01J...RUN",
            "test",
            MergeableKind::Junit,
            "junit",
            "deadbeef",
        );
        assert_eq!(key, "runs/01J...RUN/jobs/test/merged/junit/deadbeef.xml");

        let key =
            merged_report_r2_key("01J...RUN", "test", MergeableKind::Lcov, "lcov", "cafef00d");
        assert_eq!(key, "runs/01J...RUN/jobs/test/merged/lcov/cafef00d.info");
    }

    #[test]
    fn hex_sha256_matches_known_test_vectors() {
        // Empty input's SHA-256 digest is a standard, widely-published test vector.
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
