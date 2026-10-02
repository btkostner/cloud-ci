//! Shared domain logic used by both `cloud-ci-worker` (managed runs) and
//! `cloud-ci-cli` (BYO CI), per `docs/design/parallelization.md`'s
//! "`cloud-ci split` (also usable from BYO CI)": "the same binary and the
//! same split algorithm (`cloud_ci_core::split`, a library target inside
//! `cloud-ci-core`... that `cloud-ci-worker` also links against for managed
//! runs) used in both places, so a BYO CI matrix and a cloud-ci-managed
//! shard group produce byte-identical assignments for the same inputs."
//!
//! Also houses [`cgroup`] and [`sampler`], the cgroup v2 resource-sampling
//! logic `cloud-ci agent` uses (`docs/design/analytics.md`'s "What is
//! collected" table) — shared-logic, no-I/O-preferring pure domain code,
//! same rationale as [`split`]. The future rightsizer and other shared
//! domain logic ([ADR 0010](../../../docs/adr/0010-pluggable-executors.md))
//! are not added here since nothing in this round of work needs them.

pub mod cgroup;
pub mod sampler;
pub mod settings;
pub mod split;
