# Roadmap

Ordered by risk, not appeal: each phase retires the biggest unknown left before building on it.
Every phase ends with something deployable.

## Phase 0 — Spikes (prove the platform)

| Spike | Question it answers | Exit criterion |
| --- | --- | --- |
| Containers from Rust | Can workers-rs start/stop a container with a runtime instance size, or do we need a TS shim DO? | A Worker starts `standard-1` and `basic` containers on demand and reads their exit status (bindings-completeness gap, not an architectural blocker — contrast Dynamic Workflows row: `default`-policy containers start and run to exit 100% from Rust, proven 2026-10-02 via local Docker-backed `wrangler dev`; `durable_object`-policy per-call instance sizing and exit-status reading are not reachable from the vendored `worker`/`worker-sys` crates today, since `ContainerStartupOptions` has no `image`/`instance` field and `Container` has no `exec()`; the criterion's own wording also conflates two scheduling policies' vocabularies — `basic` is a `default`-policy-only, Wrangler-config instance type, never valid for `durable_object`-policy's runtime-selectable set (lite/standard-1..4), so it needs rewording) |
| buffa on wasm32 | Do generated bindings compile and stay small on `wasm32-unknown-unknown`? | Hand-routed Connect unary call round-trips JSON and binary |
| Cold start | How long from webhook to first step output? | Measured p50/p95 recorded in docs |
| Dynamic Workflows | Can a host Worker run a PR-supplied script as a Dynamic Workflow with egress blocked, start containers from steps, and resume after an isolate recycle? | A script runs 3 dependent containers, survives a forced recycle, and cannot reach the network (architecture + egress isolation confirmed 2026-10-02 via a sibling TS host Worker reached by service binding; containers-from-steps and forced-recycle survival remain unverified, pending the Containers from Rust spike and `@cloudflare/dynamic-workflows` wiring) |
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
