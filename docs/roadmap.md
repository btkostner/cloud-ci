# Roadmap

Ordered by risk, not appeal: each phase retires the biggest unknown left before building on it.
Every phase ends with something deployable.

## Phase 0 — Spikes (prove the platform)

| Spike | Question it answers | Exit criterion |
| --- | --- | --- |
| Containers from Rust | Can workers-rs start/stop a container with a runtime instance size, or do we need a TS shim DO? | A Worker starts `standard-1` and `basic` containers on demand and reads their exit status |
| buffa on wasm32 | Do generated bindings compile and stay small on `wasm32-unknown-unknown`? | Hand-routed Connect unary call round-trips JSON and binary |
| Cold start | How long from webhook to first step output? | Measured p50/p95 recorded in docs |
| Dynamic Workers | Can the Worker evaluate `pipeline.ts` in a Worker Loader sandbox (from Rust or a TS shim) with egress blocked? | A plan proto returned from a sandboxed program in < 100 ms |
| Container snapshots | Can a run's task containers start from a snapshot taken after `setup`? | Restore time measured against cold `pnpm install` |
| GitHub App JWT | RS256 signing via WebCrypto from Rust | Installation token fetched from a deployed Worker |

## Phase 1 — Contract and BYO CI

Packages: `cloud-ci-proto`, `cloud-ci-proto-rust`, `cloud-ci-core`, `cloud-ci-worker`,
`cloud-ci-cli`.

- Ingest RPCs, external runs, JUnit/Vitest/Playwright/lcov parsing.
- GitHub App + Check Runs + single PR comment.
- Auth: GitHub Actions OIDC and API tokens; GitHub OAuth for humans.
- Artifact upload and isolated HTML site hosting.

BYO CI first because it delivers value with zero execution risk and builds the pipeline every
later feature consumes ([ADR 0007](./adr/0007-one-upload-path.md)).

## Phase 2 — Managed runs

Packages: `cloud-ci-runner-image`, `cloud-ci-proto-typescript`, `cloud-ci-pipeline-sdk`.

- `pipeline.yml` parsing, `pipeline.ts` evaluation, Turborepo adapter and turbo remote cache,
  selector checks, `RepoState`/`RunCoordinator`, container execution, log streaming.
- Caches, secrets, cancel-superseded.

## Phase 3 — Parallelization

- mise and generic graph adapters, `group: auto`.
- `parallel:`, `cloud-ci split` (count → file → timing), merge barriers, native junit/coverage
  merges, Playwright/Vitest blob merge jobs.

## Phase 4 — Analytics and rightsizing

Packages: `cloud-ci-web`.

- Resource sampling, rollups, dashboard, flaky detection, `runner: auto`.

## Phase 5 — AI

- Failure summaries in the PR comment, performance suggestions, opt-in autofix.
- Cloudflare Access auth mode.

## Phase 6 — Distribution

- Deploy-to-Cloudflare button, setup wizard, upgrade/migration story, public docs site.
