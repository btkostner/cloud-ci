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
//! collected" table), and [`rightsizing`], the pure `runner: "auto"`
//! decision math (`docs/design/analytics.md`'s "Rightsizing algorithm"
//! section) — shared-logic, no-I/O-preferring pure domain code, same
//! rationale as [`split`]. `rightsizing` is not actually linked from
//! `cloud-ci-cli` today (rightsizing is a no-op for BYO CI runs), but lives
//! here rather than in `cloud-ci-worker` for the same no-Workers-runtime,
//! plain-`cargo test` reasons as every other module in this crate; see its
//! own module doc for the full placement rationale and this round's scope
//! boundary.

pub mod cgroup;
pub mod rightsizing;
pub mod sampler;
pub mod settings;
pub mod split;
