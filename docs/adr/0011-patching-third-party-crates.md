# 0011: Patching third-party crates via a `[patch.crates-io]` fork, not a vendored copy

- Status: Accepted
- Date: 2026-10-02

## Context

The Containers from Rust spike ([roadmap](../roadmap.md), [0010](./0010-pluggable-executors.md))
found that the vendored `worker`/`worker-sys` 0.8.7 crates (from `cloudflare/workers-rs`) are
missing `image`/`instance`/`containerSnapshot`/`labels` on `Container::start` and have no
`exec()`/exit-code-reading method, even though Cloudflare's underlying
`ctx.container` JS API supports all of these
(developers.cloudflare.com/containers/api/durable-object-container/,
developers.cloudflare.com/containers/configuration/scheduling-policy/, checked 2026-10-02). The
spike characterized the gap as mechanical `wasm-bindgen` glue, the same shape as every other
method already in the file — not an architectural blocker, but something this repo has to carry
until upstream catches up. This repo had no prior precedent for patching a third-party
dependency: no `[patch]` table in any `Cargo.toml`, and nothing in `AGENTS.md` or any ADR 0001–0010
addresses forking or patching a vendored crate.

Two self-contained options exist: (1) fork `cloudflare/workers-rs` on a public host and pull it
via a Cargo `git` dependency under `[patch.crates-io]`, or (2) vendor a patched copy of the crate
source into this repo under `vendor/` and point `[patch.crates-io]` at that local path.

## Decision

Fork `cloudflare/workers-rs`, branch from the exact `v0.8.7` tag the crate was published from,
and patch `cloud-ci-worker`'s `Cargo.toml` with a `git` dependency pinned to a commit on that
branch:

```toml
[patch.crates-io]
worker = { git = "https://github.com/btkostner/workers-rs", branch = "container-exec-and-durable-object-sizing", rev = "0a6f661a61f420bcce1530cec471a207367b5e8f" }
```

(`worker-sys` does not need its own patch entry: inside the fork's workspace, `worker`'s
`worker-sys.workspace = true` dependency resolves to the fork's own `worker-sys` by path, so
pulling `worker` via `git` pulls a consistent, matching `worker-sys` automatically.)

The fork branch's diff against the `v0.8.7` tag is exactly what would be contributed upstream: it
only touches `worker-sys/src/types/durable_object/container.rs` and `worker/src/container.rs`,
adds a changeset under `.changeset/` per `cloudflare/workers-rs`'s own `changesets` convention,
and passes that repo's own CI gates run locally (`cargo fmt --all -- --check`,
`cargo clippy --features d1,queue --all-targets --workspace -- -D warnings`,
`cargo check --locked`). Opening the actual upstream PR is still worth doing — this diff is
written so it can be sent as-is — but it is not a prerequisite for unblocking this repo, since the
`git` dependency already gives every consumer (CI included) a buildable, reviewable, pinned
source.

Rejected: vendoring a patched copy of the crate into `vendor/` in this repo. A fork is
preferable here because:

- **The diff stays the real diff.** A fork branch's history is `git diff v0.8.7..branch`, the
  literal upstream-mergeable change. A vendored copy's "diff" is only as trustworthy as whatever
  comment or file the vendoring step adds next to it; nothing stops it from drifting from what's
  actually applied.
- **This repo already has GitHub push access with `repo` scope**, so "host a fork somewhere
  reachable" is not a hypothetical — `btkostner/workers-rs` exists and the branch is pushed. The
  caveat the task raised against option 1 (an external fork's availability) is real for a
  contributor without push access, but doesn't apply here.
- **A vendored copy obligates someone to manually re-diff against every future upstream release**
  to decide whether the vendored tree still reflects intent; a fork branch rebases or
  cherry-picks the same two-file diff forward with ordinary `git` tooling, and `git diff
  v0.8.8..branch` after a rebase is the manual-re-diff step, not a from-scratch read of two
  multi-hundred-line files.
- A vendored crate adds ~20 more files (the parts of `worker`/`worker-sys` untouched by this
  change) to this repo's own `git log`/`git blame` surface for no reason; the `git` dependency
  keeps the untouched 99% of the crate out of this repo entirely.

## Consequences

- `cargo build`/`cargo check` for `cloud-ci-worker` fetch `worker` from
  `github.com/btkostner/workers-rs` instead of crates.io until the upstream PR merges and the
  patch is removed. CI needs network access to GitHub (already required for the repo's own
  checkout) in addition to the crates.io registry index.
- `Cargo.lock` for `cloud-ci-worker` pins the exact fork commit; bumping the pin is a normal
  `cargo update -p worker` against the patched source, same as any other dependency bump.
- Removing this patch (once `cloudflare/workers-rs` ships the equivalent, or the maintainers
  reject/reshape the contribution and this repo settles on a different binding) is a one-line
  `Cargo.toml` edit plus a version bump; no vendored tree to delete.
- The fork branch is this repo's record of exactly what was changed and why; it is not a
  secret or a workaround, and the commit message and changeset entry say so.

## What would reverse this

If GitHub (or network egress to it) becomes unavailable to a build environment this project must
support — self-hosted CI without GitHub access, for instance — `git` dependencies stop working
and the vendored-copy approach becomes the only self-contained option left.
