//! Shared domain logic used by both `cloud-ci-worker` (managed runs) and
//! `cloud-ci-cli` (BYO CI), per `docs/design/parallelization.md`'s
//! "`cloud-ci split` (also usable from BYO CI)": "the same binary and the
//! same split algorithm (`cloud_ci_core::split`, a library target inside
//! `cloud-ci-core`... that `cloud-ci-worker` also links against for managed
//! runs) used in both places, so a BYO CI matrix and a cloud-ci-managed
//! shard group produce byte-identical assignments for the same inputs."
//!
//! Currently houses [`split`] only; the doc also names this crate as the
//! future home of the rightsizer and other shared domain logic
//! ([ADR 0010](../../../docs/adr/0010-pluggable-executors.md)) — not added
//! here since nothing in this round of work needs it.

pub mod split;
