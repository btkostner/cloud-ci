# AGENTS.md

## Commands

| Task | Command |
| --- | --- |
| Install tools and hooks | `mise install` |
| Check everything | `mise run check` |
| Fix formatting | `mise run fix` |
| One package | `mise run //packages/<name>:<task>` |
| Regenerate bindings | `mise run //packages/cloud-ci-proto:generate` |

Pin every tool a task invokes in the owning package's `mise.toml` `[tools]`. Never invoke an
unpinned tool: local and CI runs will disagree.

## Invariants

- **Only a run's `RunCoordinator` writes run state.** Writing run/job status to D1 from a
  webhook handler or upload path creates races that duplicate deliveries will hit.
- **Only a PR's `PullRequestState` edits its sticky comment.** Multiple writers reorder edits and
  create duplicate comments.
- **Inputs enqueue, coordinators decide.** If a handler acts on an event's payload instead of
  re-reading state, duplicated or reordered webhooks produce wrong results.
- **Uploads are idempotent** on (run, job/shard, kind or path, content hash). A non-idempotent
  upload double-counts tests whenever a CLI retries.
- **Hosted report HTML is never served from the dashboard origin.** Doing so gives repo-authored
  JavaScript the viewer's session.
- **The agent uses the public ingest path** ([ADR 0007](docs/adr/0007-one-upload-path.md)). A
  private shortcut lets BYO CI rot unnoticed.
- **The proto contract is backward compatible.** Old CLIs in third-party CI keep calling new
  deployments; `buf breaking` must pass against `main`.
- **D1 migrations are forward-only and safe on a live deployment.** Deployers upgrade on their
  own schedule ([ADR 0003](docs/adr/0003-single-tenant-deployment.md)).
- **No `unsafe`, no `unwrap`/`expect`/`panic`/`todo` in Rust packages.** A panic in the Worker
  fails a request with no typed error; in the coordinator it can strand a run.
- **Generated `generated/` directories are never edited.** They are wiped on every `generate`.
- **Facts about Cloudflare/GitHub in docs are dated and sourced, or marked `[unverified]`.**

## Working with multiple agents

- **Commit verified work promptly, in small commits, on a branch.** An uncommitted pile is not
  a backup.
- **Each agent works in its own worktree and branch:**
  `git worktree add ../cloud-ci-<name> -b <name>`. Never edit another agent's worktree or the
  main checkout.
- **One designated integrator** merges or rebases finished branches into `main` and runs
  `mise run check` after each merge.
- **Allowed git writes:** `add`, `commit`, `switch`, and `worktree` operations on your own
  branch.
- **Forbidden:** `reset --hard`, `checkout -- <path>`, `restore`, `clean`, `stash drop`,
  force-push, and rewriting a branch someone else owns. A blanket checkout once wiped another
  agent's uncommitted work.
