# Architecture

Status: **Proposed**. Nothing here is implemented yet; this describes the shape we intend to
build. Per-feature mechanics live in [`design/`](./design/), decisions and their reasons in
[`adr/`](./adr/).

## What cloud-ci is

A CI system you deploy into **your own Cloudflare account**. One deployment can serve one or
more GitHub organizations (multiple App installations belonging to one company) across any
number of repositories ([ADR 0003](./adr/0003-single-tenant-deployment.md)). It does three jobs:

1. **Runs CI** on Cloudflare Containers, driven by TypeScript pipeline scripts in
   `.cloud-ci/pipelines/*.ts` (durable workflows that can run a Turborepo or mise graph node by
   node) (*managed runs*).
2. **Accepts CI results from anywhere else** — GitHub Actions, Buildkite, a laptop — through an
   ingest API and the `cloud-ci` CLI (*external runs*).
3. **Turns both into one view**: an optional single PR comment, optional named GitHub Check
   Runs, hosted HTML reports, analytics, runner rightsizing, and AI-assisted failure analysis.

Managed and external runs share one data model. Everything downstream of ingestion (comment,
analytics, AI, asset hosting) does not know or care which kind of run produced the data.

## System shape

```mermaid
flowchart LR
    GH[GitHub<br/>App webhooks + API]
    BYO[External CI<br/>cloud-ci upload]
    U[Browser]

    subgraph CF[Deployer's Cloudflare account]
      W[cloud-ci-worker<br/>Rust Worker]
      Q[(Queues)]
      RC[RunCoordinator DO<br/>one per run]
      RS[RepoState DO<br/>one per repo]
      PR[PullRequestState DO<br/>one per PR]
      EX[Executor<br/>Containers default]
      D1[(D1<br/>metadata + rollups)]
      R2[(R2<br/>logs, artifacts, sites, caches)]
      AE[(Analytics Engine<br/>raw samples)]
      AI[Workers AI]
    end

    GH -->|webhook| W
    BYO -->|ingest RPC| W
    U -->|dashboard / assets, GitHub OAuth| W
    W --> Q --> W
    W --> RS --> RC
    RC -->|start, instance size| EX
    EX -->|cloud-ci agent: logs, reports, artifacts| W
    W --> D1 & R2 & AE
    W --> AI
    RC -->|run events| PR
    PR -->|sticky comment, optional| GH
    W -->|checks, optional; autofix| GH
```

## Packages

Monorepo following the protobuf-contract pattern ([ADR 0001](./adr/0001-monorepo-protobuf-contract.md)).
None exist yet; [roadmap](./roadmap.md) says when each arrives.

| Package | Language | Role |
| --- | --- | --- |
| `cloud-ci-proto` | protobuf | API contract source of truth: run/job/step/test/artifact model, ingest, query |
| `cloud-ci-proto-rust` | Rust | Generated bindings, imported as `cloud_ci_proto` |
| `cloud-ci-proto-typescript` | TypeScript | Generated bindings for the dashboard and pipeline SDK |
| `cloud-ci-pipeline-sdk` | TypeScript | `@cloud-ci/pipeline-sdk`: typed `ci` API, `@cloud-ci/pipeline-sdk/turbo` and `@cloud-ci/pipeline-sdk/mise` helper modules for pipeline scripts |
| `cloud-ci-core` | Rust (no `worker` dep) | Splitter, rightsizer, shared domain logic — used by Worker and CLI, tested natively |
| `cloud-ci-reports` | Rust (no `worker` dep) | Third-party report parsing + merging: JUnit, Vitest, Playwright, lcov, cobertura, bench, timing, oxlint, oxfmt, vite build, deployment — split out from `cloud-ci-core` since this surface is expected to grow; used by Worker and CLI ([ADR 0010](./adr/0010-pluggable-executors.md) context: D5) |
| `cloud-ci-worker` | Rust → wasm32 | HTTP front door, webhooks, ingest, asset serving, queue consumers, Durable Objects, cron |
| `cloud-ci-cli` | Rust (native) | `cloud-ci` binary: `upload`, `split`, `merge`, `login`, `agent` |
| `cloud-ci-runner-image` | Dockerfile | Base container image that runs `cloud-ci agent` |
| `cloud-ci-web` | Svelte | Dashboard, served as Worker static assets |

The `agent` subcommand that executes jobs inside our containers (or any other
[executor](#extension-boundary)) uploads through **the same code path** as `cloud-ci upload` in
someone else's CI. That is deliberate: if BYO-CI ingestion regresses, our own runs regress with
it, so it cannot silently rot.

Repo configuration lives in `.cloud-ci/`: `pipelines/*.ts` (one file per pipeline, each with its
own triggers — see [dynamic-pipelines](./design/dynamic-pipelines.md)) and a single static
`settings.yml` (PR comment, check names, concurrency, runner bounds defaults, cache/retention,
AI narrowing, secret requests, slash-command roles — see [settings](./design/settings.md)).

## Data model

| Concept | Identity | Lives in |
| --- | --- | --- |
| Repo | GitHub repo id (not name — renames happen) | D1 |
| Run | ULID; unique on (repo, sha, run key, attempt) | D1 row + `RunCoordinator` DO while active |
| Job | (run, job name) | D1 + DO |
| Shard | (job, index, total) | D1 + DO |
| Step | (job/shard, ordinal) | D1 |
| Report | (job/shard, kind) — junit, vitest, playwright, lcov, cobertura, timing, bench, oxlint, oxfmt, vite build, deployment | parsed by `cloud-ci-reports`; D1 keeps per-report summaries, failed/flaky test rows, and rolling per-test aggregates (one row per (repo, test_id), updated in place) — never one row per test case per run; full per-run parsed results live in R2, compressed, keyed by run/report (see [analytics](./design/analytics.md)) |
| Artifact / site | (run, name) | R2 under `runs/{run}/artifacts/{name}/…` |
| Log | (job/shard, step) | R2, chunked |
| Metric sample | (job/shard, timestamp) | Analytics Engine; rolled up into D1 by cron |

### Run states

| State | Meaning | Terminal |
| --- | --- | --- |
| `queued` | Accepted, waiting on concurrency limits or container capacity | no |
| `running` | At least one job started | no |
| `merging` | All shards done, merge jobs/native merges in flight | no |
| `succeeded` | Every required job succeeded | yes |
| `failed` | A required job failed | yes |
| `cancelled` | Superseded by a newer push or cancelled by a user | yes |
| `abandoned` | External run's shards never all uploaded before its timeout | yes |

External runs skip `queued`; they go straight to `running` on first upload.

## Core flows

### Managed run

1. GitHub `push`/`pull_request` webhook → Worker verifies signature, enqueues, returns 200
   immediately (GitHub's webhook timeout is short; all real work happens off the request).
2. Queue consumer fetches every `.cloud-ci/pipelines/*.ts` whose `on:` trigger matches the event
   at the commit sha, starts a Dynamic Workflow per matching script (which then requests nodes
   from the run's coordinator), creates a run in D1 for each, and hands them to the repo's
   `RepoState` DO, which applies concurrency rules (cancel-superseded, per-repo limits).
   Multiple pipelines can run for the same commit.
3. `RunCoordinator` DO for the run owns the job DAG. For each ready job it resolves the instance
   size (fixed, or chosen by the rightsizer for `runner: auto`) and executor, computes shard
   assignments, mints a short-lived job token, and starts the job via that `Executor`.
4. The agent (`cloud-ci agent`, running wherever the executor booted it) pulls its job spec,
   runs steps, streams logs, samples resource usage where available, and uploads
   reports/artifacts through the public ingest API.
5. On job completion the coordinator advances the DAG, runs merge barriers for shard groups,
   updates any Check Runs the script created, and notifies the `PullRequestState` of every PR
   whose head is this sha, which debounces and re-renders the sticky comment (if enabled).
6. On run completion: post-run queue → analytics write, AI analysis (if enabled), final comment.

### External run

`cloud-ci upload` authenticates (GitHub Actions OIDC or API token), opens or joins a run keyed by
(repo, sha, run key, attempt), and uploads reports/artifacts, each declaring a shard index and
total. A job completes when all its shards have uploaded; a run closes on the external CI's
completion webhook, an explicit `--expect-jobs` count, or a timeout. From step 5 onward it is
the same path as a managed run. See [byo-ci](./design/byo-ci.md).

## Coordination invariants

- **Webhooks and uploads enqueue; coordinators decide.** Inputs record *that* something happened;
  the `RunCoordinator` re-reads its own state and decides what to do next. Duplicate webhook
  deliveries, retried uploads, and reordered queue messages are therefore harmless.
- **One writer per run, one writer per PR comment.** Only the run's `RunCoordinator` mutates
  run/job state and its Check Runs; D1 is its durable projection. Only the PR's
  `PullRequestState` edits that PR's sticky comment, since the comment aggregates many runs.
- **Every upload is idempotent** on (run, job/shard, report kind | artifact path, content hash).
- **Report HTML never shares an origin with the dashboard.** Hosted sites run arbitrary JS from
  the repo under test; serving them from the dashboard origin would hand that JS the user's
  session. A completed run's own detail page is generated once as a static snapshot and served
  from the same asset origin, for the same reason; the dashboard keeps only what needs live
  data — in-progress runs, run lists, cross-run analytics, auth, actions. See
  [assets](./design/assets.md).
- **GitHub is told, not asked.** We never block a run on a GitHub API call succeeding; Check Run
  and comment updates are retried from the coordinator's state, not from the event that caused
  them.

## Extension boundary

Two traits keep vendor specifics out of core logic:

| Trait | Implementations planned | Isolates |
| --- | --- | --- |
| `Forge` | GitHub | Webhook parsing, config fetch, checks, PR comments, permissions, autofix PRs |
| `Executor` | Cloudflare Containers (default), AWS EC2, AWS Lambda, Kubernetes Jobs, self-hosted — see [ADR 0010](./adr/0010-pluggable-executors.md) | Booting a machine that runs `cloud-ci agent`; everything after boot (job spec, logs, uploads) goes over the public ingest API, so the coordinator never talks to executor-specific APIs beyond start/stop |

GitLab/Gitea support would be a second `Forge`, not a change to the coordinator. A new backend
(bare metal, another cloud) is a new `Executor` with a capability descriptor
([ADR 0010](./adr/0010-pluggable-executors.md)), not a change to the agent, the ingest API, or
anything downstream of it.

## Cross-cutting design docs

| Doc | Covers |
| --- | --- |
| [settings](./design/settings.md) | `.cloud-ci/settings.yml` static repo config |
| [dynamic-pipelines](./design/dynamic-pipelines.md) | TypeScript pipeline scripts as durable workflows, Turborepo/mise helpers, sidecars, named checks |
| [pr-comment](./design/pr-comment.md) | Sticky PR comment, Check Runs, slash commands |
| [byo-ci](./design/byo-ci.md) | Ingest API, `cloud-ci upload`, external runs |
| [parallelization](./design/parallelization.md) | Sharding, test splitting, merging |
| [analytics](./design/analytics.md) | Metrics, insights, rightsizing/autoscaling |
| [ai](./design/ai.md) | Workers AI summaries, suggestions, autofix |
| [auth](./design/auth.md) | GitHub OAuth, roles, machine tokens, GitHub App |
| [assets](./design/assets.md) | R2 artifact and HTML report hosting, caches |
