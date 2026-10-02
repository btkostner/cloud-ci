# Roadmap

Ordered by risk, not appeal: each phase retires the biggest unknown left before building on it.
Every phase ends with something deployable.

## Phase 0 — Spikes (prove the platform)

| Spike | Question it answers | Exit criterion |
| --- | --- | --- |
| Containers from Rust | Can workers-rs start/stop a container with a runtime instance size, or do we need a TS shim DO? | A Worker starts `standard-1` and `basic` containers on demand and reads their exit status (criterion's own wording conflates two scheduling policies — `basic` is `default`-policy-only Wrangler config, never a valid `durable_object`-policy runtime `instance` value: confirmed 2026-10-02 against a real local Docker-backed `wrangler dev`, which rejects `instance: "basic"` under `durable_object` with `TypeError: Invalid container instance type.`; demonstrated instead as `basic` under `default` policy and `standard-1` under `durable_object` policy, both from Rust, in the same session — bindings-completeness gap closed 2026-10-02: `worker`/`worker-sys`'s `Container`/`ContainerStartupOptions` patched via [ADR 0011](./adr/0011-patching-third-party-crates.md) to add `image`/`instance`/`containerSnapshot`/`labels` and `exec()`/`ExecProcess`/`ExecOutput`; a `durable_object`-policy container started per-call with `image: "cloudflare/debian-trixie"`, `instance: "standard-1"`, and a `default`-policy `basic` container both ran `exec(["sh", "-c", "exit N"])` and read the exact exit code back through Rust in local Docker-backed `wrangler dev`. Not yet done: wiring this into `cloud-ci-worker`'s own `RunCoordinator`/execution path — that integration is the next round, see [0010](./adr/0010-pluggable-executors.md)) |
| buffa on wasm32 | Do generated bindings compile and stay small on `wasm32-unknown-unknown`? | Hand-routed Connect unary call round-trips JSON and binary |
| Cold start | How long from webhook to first step output? | Measured p50/p95 recorded in docs |
| Dynamic Workflows | Can a host Worker run a PR-supplied script as a Dynamic Workflow with egress blocked, start containers from steps, and resume after an isolate recycle? | A script runs 3 dependent containers, survives a forced recycle, and cannot reach the network (architecture + egress isolation confirmed 2026-10-02 via a sibling TS host Worker reached by service binding; **containers-from-steps confirmed 2026-10-02** — a real `cloud-ci-dynamic-workflows-host` package's Workflow step reaches a container via a Rust-hosted `ContainerProbe` Durable Object (no direct `ctx.container` access from a Workflow step — DO-only, see `packages/cloud-ci-dynamic-workflows-host/README.md`) over a service binding to `cloud-ci-worker`, using the now-patched `worker` crate's real `Container::exec()`; a control run with no forced reload completed all 3 sequential steps in 31s with real container stdout. **"3 dependent containers" DAG criterion confirmed 2026-10-02** — a second fixture (`test/fixtures/pipeline-script-dag.js`) runs a genuine fan-out/fan-in DAG (node A, then B and C both depending on A, then a join step), each node backed by its own `ContainerProbe` Durable Object instance (`cloud-ci-worker`'s `handle_container_probe_exec` now addresses the DO by an optional `?probe_id=` query param instead of a hardcoded singleton, `[[containers]] max_instances` raised from 1 to 3); against real `wrangler dev` + Docker, B and C's `step.do` calls were both gated on A's completion (`startedAt` strictly after A's `finishedAt` in every run) and then genuinely ran concurrently — measured, not assumed, from each node's own timestamps: ~99.6%/99.8% wall-clock overlap across two runs, not serialized by the local Workflows engine — see `/tmp/dynamic-workflows-dag-findings.txt`. **Forced-recycle survival partially confirmed, partially open** 2026-10-02: a `wrangler dev` hot-reload mid-sleep preserved an already-completed step's recorded result unchanged and un-re-executed across the reload (the actual idempotency claim), but the in-progress `step.sleep()` never resumed afterward in local dev — stuck 3+ minutes against a 30s target, a documented `wrangler dev` 4.145.0 limitation, not a code defect; full run-to-completion recycle survival stays unverified locally — see `/tmp/dynamic-workflows-host-findings.txt`) |
| Container snapshots | Can a run's task containers start from a snapshot taken after `setup`? | Restore time measured against cold `pnpm install` |
| GitHub App JWT | RS256 signing via WebCrypto from Rust | Installation token fetched from a deployed Worker |

## Phase 1 — Contract and BYO CI

Packages: `cloud-ci-proto`, `cloud-ci-proto-rust`, `cloud-ci-core`, `cloud-ci-reports`,
`cloud-ci-worker`, `cloud-ci-cli`.

- Ingest RPCs, external runs, JUnit/Vitest/Playwright/lcov parsing (`cloud-ci-reports`).
- GitHub App + optional Check Runs + optional single PR comment.
- Auth: GitHub Actions OIDC and API tokens; GitHub OAuth for humans.
- Artifact upload and isolated HTML site hosting.

BYO CI first because it delivers value with zero execution risk and builds the pipeline every
later feature consumes ([ADR 0007](./adr/0007-one-upload-path.md)).

## Phase 2 — Managed runs

Packages: `cloud-ci-runner-image`, `cloud-ci-proto-typescript`, `cloud-ci-pipeline-sdk`.

- `.cloud-ci/settings.yml` parsing, `.cloud-ci/pipelines/*.ts` scripts as Dynamic Workflows,
  `turbo` helpers, named checks created from scripts, `RepoState`/`RunCoordinator`, Containers
  execution (default `Executor`), log streaming.
- Caches, secrets, cancel-superseded.

## Phase 3 — Parallelization

- `mise` helpers, `ci.group`, turbo remote cache, sidecars.
- `parallel:`, `cloud-ci split` (count → file → timing), merge barriers, native junit/coverage
  merges, Playwright/Vitest blob merge jobs.

## Phase 4 — Analytics and rightsizing

Packages: `cloud-ci-web`.

- Resource sampling, rollups, dashboard, flaky detection, `runner: auto`.

## Phase 5 — AI

- Failure summaries in the PR comment, performance suggestions, opt-in autofix.

## Phase 6 — Executors beyond Containers

- Additional `Executor` implementations per [ADR 0010](./adr/0010-pluggable-executors.md): AWS
  EC2, AWS Lambda, Kubernetes Jobs, self-hosted machines.

## Phase 7 — Distribution

- Deploy-to-Cloudflare button, setup wizard, upgrade/migration story, public docs site.
