# Roadmap

Ordered by risk, not appeal: each phase retires the biggest unknown left before building on it.
Every phase ends with something deployable.

## Phase 0 — Spikes (prove the platform)

| Spike | Question it answers | Exit criterion |
| --- | --- | --- |
| Containers from Rust | Can workers-rs start/stop a container with a runtime instance size, or do we need a TS shim DO? | A Worker starts `standard-1` and `basic` containers on demand and reads their exit status (criterion's own wording conflates two scheduling policies — `basic` is `default`-policy-only Wrangler config, never a valid `durable_object`-policy runtime `instance` value: confirmed 2026-10-02 against a real local Docker-backed `wrangler dev`, which rejects `instance: "basic"` under `durable_object` with `TypeError: Invalid container instance type.`; demonstrated instead as `basic` under `default` policy and `standard-1` under `durable_object` policy, both from Rust, in the same session — bindings-completeness gap closed 2026-10-02: `worker`/`worker-sys`'s `Container`/`ContainerStartupOptions` patched via [ADR 0011](./adr/0011-patching-third-party-crates.md) to add `image`/`instance`/`containerSnapshot`/`labels` and `exec()`/`ExecProcess`/`ExecOutput`; a `durable_object`-policy container started per-call with `image: "cloudflare/debian-trixie"`, `instance: "standard-1"`, and a `default`-policy `basic` container both ran `exec(["sh", "-c", "exit N"])` and read the exact exit code back through Rust in local Docker-backed `wrangler dev`. Not yet done: wiring this into `cloud-ci-worker`'s own `RunCoordinator`/execution path — that integration is the next round, see [0010](./adr/0010-pluggable-executors.md)) |
| buffa on wasm32 | Do generated bindings compile and stay small on `wasm32-unknown-unknown`? | Hand-routed Connect unary call round-trips JSON and binary (confirmed 2026-10-02: `mise run //packages/cloud-ci-worker:build` (`worker-build --release`, pinned in `packages/cloud-ci-worker/mise.toml`) compiles `cloud-ci-worker` — including `cloud-ci-proto-rust`'s generated `cloud_ci.ingest.v1` bindings, confirmed present as monomorphized `cloud_ci_proto::ingest::v1::{BeginRunRequest,BeginRunResponse}` symbols via `strings` on the output — cleanly for the `wasm32-unknown-unknown` target (`target/wasm32-unknown-unknown`), emitting `packages/cloud-ci-worker/build/index_bg.wasm` at 1,933,354 bytes (1.84 MiB) after `wasm-opt`, reported here as a fact for later comparison, not judged against a baseline. The hand-routed Connect dispatch (`packages/cloud-ci-worker/src/lib.rs`'s `route`, matching ADR 0002) decodes/encodes every procedure through `connect.rs`'s `Codec`, whose `round_trips_both_codecs` test (`packages/cloud-ci-worker/src/connect.rs:168-176`) already proves both `Codec::Proto` and `Codec::Json` round-trip a real generated `BeginRunRequest` byte-for-byte. That test never touches HTTP, and the only existing HTTP-backed round trip — `cloud-ci-cli`'s `full_upload_sequence_executes_against_a_real_http_fixture` (`packages/cloud-ci-cli/src/upload.rs:638-698`), a real-socket fixture standing in for this Worker's `IngestService` — only ever drives `Codec::Json` (`cloud-ci upload`'s fixed production choice). Closed the binary-over-HTTP gap 2026-10-02 with a new test, `client_round_trips_a_successful_binary_response_over_http` (`packages/cloud-ci-cli/src/connect_client.rs:301-329`): a real socket replies with a binary-encoded `BeginRunResponse` under `Content-Type: application/proto`, and `Client::call::<_, _>("BeginRun", …)` with `Codec::Proto` decodes it byte-for-byte. Unverified: no test drives the Worker's own `route`/`fetch` dispatch end to end against a live `Env` (D1/DO) for either codec — `worker`'s `Env` isn't constructible outside a Workers runtime, so that would need a Miniflare/`wrangler dev`-backed integration test this repo doesn't yet have infrastructure for.) |
| Cold start | How long from webhook to first step output? | Measured p50/p95 recorded in docs (partially answered 2026-10-02: no webhook in this codebase yet drives `BeginRun`→`start_node` as one chain — `installation`/`workflow_run`/`pull_request` webhooks only write D1/`RepoState`/`PullRequestState`/close an already-running run, and `RunCoordinator::handle_start_node`'s own module docs say its real container-starting RPCs have "no Dynamic Workflow integration ... yet" — so two real chains were measured separately against a local Docker-backed `wrangler dev` instead, full methodology and raw per-iteration numbers in `/tmp/cold-start-spike-findings.txt`: **Chain A**, a real HMAC-verified `pull_request` webhook POST to a cold (fresh `repo_id`/`pr_number` per call) `PullRequestState` Durable Object, n=20, p50=41.3ms/p95=42.8ms (min 40.3ms, max 43.1ms); **Chain B**, a real `RunCoordinator::handle_start_node` dispatch to a cold `NodeContainer` Durable Object starting a genuinely new local Docker container and reporting its real captured stdout back (`{"exit_code":0,"stdout":"hi-ka\n"}` confirmed via a direct D1 read), n=20, all succeeded, p50=528.3ms/p95=548.7ms (min 476.4ms, max 560.4ms; resolution caveat: each poll round-trip measured ~75ms, so up to one poll-width of overshoot is baked into these numbers). **These are local-`wrangler dev`-only measurements and are explicitly NOT representative of a real Cloudflare deployment's cold-start latency** — no real multi-region edge, no real Cloudflare Containers scheduling/bin-packing/warm pools, no cold image pull (a cached local image was used), measured entirely against one machine's local Docker daemon; do not treat these numbers as a production SLA.)
| Dynamic Workflows | Can a host Worker run a PR-supplied script as a Dynamic Workflow with egress blocked, start containers from steps, and resume after an isolate recycle? | A script runs 3 dependent containers, survives a forced recycle, and cannot reach the network (architecture + egress isolation confirmed 2026-10-02 via a sibling TS host Worker reached by service binding; **containers-from-steps confirmed 2026-10-02** — a real `cloud-ci-dynamic-workflows-host` package's Workflow step reaches a container via a Rust-hosted `ContainerProbe` Durable Object (no direct `ctx.container` access from a Workflow step — DO-only, see `packages/cloud-ci-dynamic-workflows-host/README.md`) over a service binding to `cloud-ci-worker`, using the now-patched `worker` crate's real `Container::exec()`; a control run with no forced reload completed all 3 sequential steps in 31s with real container stdout. **"3 dependent containers" DAG criterion confirmed 2026-10-02** — a second fixture (`test/fixtures/pipeline-script-dag.js`) runs a genuine fan-out/fan-in DAG (node A, then B and C both depending on A, then a join step), each node backed by its own `ContainerProbe` Durable Object instance (`cloud-ci-worker`'s `handle_container_probe_exec` now addresses the DO by an optional `?probe_id=` query param instead of a hardcoded singleton, `[[containers]] max_instances` raised from 1 to 3); against real `wrangler dev` + Docker, B and C's `step.do` calls were both gated on A's completion (`startedAt` strictly after A's `finishedAt` in every run) and then genuinely ran concurrently — measured, not assumed, from each node's own timestamps: ~99.6%/99.8% wall-clock overlap across two runs, not serialized by the local Workflows engine — see `/tmp/dynamic-workflows-dag-findings.txt`. **Forced-recycle survival partially confirmed, partially open** 2026-10-02: a `wrangler dev` hot-reload mid-sleep preserved an already-completed step's recorded result unchanged and un-re-executed across the reload (the actual idempotency claim), but the in-progress `step.sleep()` never resumed afterward in local dev — stuck 3+ minutes against a 30s target, a documented `wrangler dev` 4.145.0 limitation, not a code defect; full run-to-completion recycle survival stays unverified locally — see `/tmp/dynamic-workflows-host-findings.txt`. **Re-checked against wrangler 4.147.0, the latest available release, 2026-10-02**: neither 4.146.0 nor 4.147.0's changelog (github.com/cloudflare/workers-sdk/releases) mentions a Workflows sleep/timer/hot-reload fix, and the identical repro reproduced the same failure signature on 4.147.0 — step1's result again survived the reload, but the sleep step again stuck 3+ minutes past its 30s target while an unreloaded control run on the same 4.147.0 session completed normally in 30s; the version pin was left at 4.145.0 since the newer release fixes nothing here. Full run-to-completion recycle survival stays unverified locally) |
| Container snapshots | Can a run's task containers start from a snapshot taken after `setup`? | Restore time measured against cold `pnpm install` (**confirmed 2026-10-02**, with one real crate-level gap found and worked around rather than hidden: ADR 0011's patched `worker` crate binds `ContainerStartupOptions.container_snapshot` to *restore* a snapshot, but — confirmed by reading the pinned fork commit's actual source at `github.com/btkostner/workers-rs@df96700` — never binds `snapshotContainer()`, the real Cloudflare method that *creates* one (developers.cloudflare.com/containers/guides/snapshots/, checked 2026-10-02); worked around by calling that real JS method directly via `js_sys::Reflect`/`js_sys::Function` reflection on the underlying `Container` object instead of stopping at "the typed wrapper is incomplete" — not a reimplementation or Docker-level substitute. A new `packages/cloud-ci-worker/container_snapshot_spike/` fixture (real, non-trivial `package.json`: `express`/`axios`/`lodash`/`dayjs`/`zod`/`uuid`) and standalone `SnapshotSpike` Durable Object (`src/container_snapshot_spike.rs`) proved the full real loop against a local Docker-backed `wrangler dev`: a fresh container execs a real `pnpm install`, calls the real `snapshotContainer()`, and a *second*, separate container instance restores from that exact snapshot handle and confirms `node_modules` already exists via a live `test -d` exit code — never inferred. Full methodology, the exact crate-gap evidence, and raw per-trial numbers in `/tmp/container-snapshot-spike-findings.txt`: cold `pnpm install` (n=4, one earlier run excluded for a since-fixed `enable_internet` config dead end, not silently dropped) mean 1580.0ms (1480–1652ms); warm snapshot-restore-then-check (n=4) mean 437.5ms (407–453ms), `node_modules` present on every trial; **3.61x** real measured speedup. Local-`wrangler-dev`-only, one fixture/dependency tree, already-Docker-cached image (no cold image pull) — not a production SLA, same caveat class as the Cold start row's numbers; no `RunCoordinator`/`NodeContainer` pipeline wiring this round, and the missing `snapshotContainer()` binding was not upstreamed to the fork, only worked around locally.) |
| GitHub App JWT | RS256 signing via WebCrypto from Rust | Installation token fetched from a deployed Worker (**blocked, documented 2026-10-02, not closed**: RS256/WebCrypto signing itself is built and proven — `packages/cloud-ci-worker/src/github_app.rs`'s `build_claims`/`signing_input` (lines 138–167) construct GitHub's documented claim shape and signing input, unit-tested by `github_app::tests::claims_have_60s_clock_drift_tolerance_and_10min_ttl`/`signing_input_*` (14 tests total in that module, all passing under `mise run //packages/cloud-ci-worker:test`); `sign_rs256` (lines 201–242) calls the real Workers-runtime `SubtleCrypto.sign()` via `web-sys`, smoke-tested live under `wrangler dev` per the module's own doc comment (lines 13–16); `fetch_installation_token` (lines 405–441) correctly builds `POST /app/installations/{id}/access_tokens` and parses `InstallationToken`, unit-tested (`installation_token_url_interpolates_installation_id`, `installation_token_response_parses_documented_shape`) — but the module's own doc comment (lines 27–31, 400–404) already states this call is "**not** live-verified against GitHub: no real GitHub App installation exists in this environment to call it against," and that boundary still holds this round: `.dev.vars` in this environment carries `GITHUB_APP_ID=999999` (fake, per `.dev.vars.example`'s own comment: "does not need to match a real GitHub App") and a throwaway `openssl`-generated RSA key, not a registered GitHub App's real credentials. The literal exit criterion — "a deployed Worker," not a local one — is also unmet independent of credentials: this environment's `wrangler` has no Cloudflare session (`wrangler whoami` → "You are not authenticated") and no `CF_API_TOKEN`, so no `wrangler deploy` can run; this repo's own usage of "deployed" elsewhere (docs/design/auth.md: "Until that first `wrangler deploy` from setup lands," contrasted explicitly with a loopback address as "not the deployed Worker's hostname") means a real edge deployment, never `wrangler dev` — every other roadmap row that only ran `wrangler dev` says so explicitly rather than calling it "deployed" (rows above), so local dev cannot be credited as satisfying this row either, even setting the credentials problem aside. Both gaps trace to the same root cause as the "Dynamic Workflows" row's and the `cloud-ci setup github-app` CLI's (`packages/cloud-ci-cli/src/setup_github_app.rs`, module doc "Three layers" § 2–3) own already-documented blocker: no real GitHub App is registered in this environment, and registering one needs a real browser plus a real GitHub account to drive GitHub's manifest-flow redirect — unavailable here. Closing this row for real requires, in order: (1) a human operator runs `cloud-ci setup github-app` (or registers a GitHub App manually) against a real GitHub account, producing a real App ID/private key/installation; (2) `wrangler login` or a `CF_API_TOKEN` and `wrangler deploy` to put a Worker on Cloudflare's edge with those real secrets bound; (3) a real `fetch_installation_token` call from that deployment observed to return a real, usable installation token. None of the three is achievable from this environment today.) |

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

Status (2026-10-03), from reading the code, not from a live run:

- **Exists:** the `SubmitResourceSamples` RPC; Analytics Engine `test` and `sample` data-point
  writes from `RunCoordinator`; `rollup.rs` and the `*/15` rollup cron (`scheduled` in `lib.rs`),
  which writes `run_rollups` and a `run_test_failures` insight from `test` rows only;
  `test_stats` with an inline `flakiness_score` (`finalize_test_stats`/`refresh_flakiness_scores`).
- **Not built:** the dashboard (no `cloud-ci-web` package); the `step` and `cache` Analytics
  Engine events, so `queue_ms`, `critical_path_ms` and `cache_hit_rate` stay `NULL`; the nightly
  rightsizing cron that feeds real Analytics Engine data into the pure recommendation functions
  (`reduce_run`, `recommend_naive` and `apply_hysteresis` have no callers outside
  `rightsizing.rs`; only the unwired OOM logic calls `rightsizing::oom_retry` and
  `ladder_range`).
- **Not wired:** the OOM-retry decision logic in `coordinator/logic.rs` is pure and unit-tested,
  but nothing calls it, so no OOM retry happens at runtime.

## Phase 5 — AI

- Failure summaries in the PR comment, performance suggestions, opt-in autofix.

## Phase 6 — Executors beyond Containers

- Additional `Executor` implementations per [ADR 0010](./adr/0010-pluggable-executors.md): AWS
  EC2, AWS Lambda, Kubernetes Jobs, self-hosted machines. **Trait boundary proven 2026-10-02**:
  the `Executor` trait, `CapabilityDescriptor`, and pull/callback data shapes are real
  (`packages/cloud-ci-worker/src/executor.rs`), with two conformers — `ContainersExecutor`
  (delegates to `node_container.rs`'s existing real Cloudflare Containers logic, never
  duplicates it) and `FakeExecutor` (a pure in-memory conformer, unit-tested lifecycle, proving
  the trait is implementable by something other than Containers). **Still not built, blocked on
  real cloud credentials this environment does not have:** real AWS EC2/Lambda/Kubernetes
  Jobs/self-hosted conformers (every one still "Possible" in ADR 0010's own table), and the real
  bootstrap-token-issuance path + `cloud-ci agent` pull-loop wiring those conformers (and a real
  `RunCoordinator`-to-`ContainersExecutor` rewiring) would need.

## Phase 7 — Distribution

- Deploy-to-Cloudflare button, setup wizard, upgrade/migration story, public docs site.

Status (2026-10-03): partly built. `packages/cloud-ci-docs` (a VitePress docs site, not
deployed anywhere) and the `cloud-ci setup allowed-orgs` and `cloud-ci setup github-app`
subcommands exist. Not built: the one-step setup wizard, the upgrade/migration story, and the
button, which is blocked by Cloudflare's isolated-subdirectory rule, see
[deployment](./design/deployment.md#deploy-to-cloudflare).

## Next steps

Ordered by dependency; nothing below is done. Each item unblocks the ones after it.

1. **Proto and contract work first** (backward compatible, `buf breaking` against `main`):
   - A `node_id` on `SubmitResourceSamplesRequest`, so a sample is tied to a node, not inferred.
   - A public RPC to register a shard group's merge settings, which unblocks
     `ShardOptions.reports`.
   - An agent-side way to carry the shard attempt.
2. **OOM recovery wiring**, once the contract exists: a multi-size executor ladder,
   lineage- and attempt-aware shard-terminal resolution, and a live smoke test. The pure logic
   already exists; only the wiring and the live proof are missing.
3. **NodeContainer isolation live proof.** Run-scoped addressing is unit-tested only; proving it
   needs `wrangler dev` with a `CLOUDFLARE_API_TOKEN`, which this environment lacks.
4. **Phase 7.** The Deploy-to-Cloudflare button is blocked by Cloudflare's isolated-subdirectory
   rule: the worker has path dependencies outside its directory
   (developers.cloudflare.com/workers/platform/deploy-buttons/, "Last updated Jul 22, 2026",
   as cited in [deployment](./design/deployment.md#deploy-to-cloudflare); not re-fetched for
   this entry). Partial setup subcommands already exist (`cloud-ci setup github-app` and
   `cloud-ci setup allowed-orgs`); the wizard needs an orchestrating subcommand on top of them.
5. **Then:** the `step` and `cache` Analytics Engine events, the nightly rightsizing cron that
   calls the pure rightsizing functions with real Analytics Engine data, the Phase 5 AI context
   builder, and the dashboard.
6. **Blocked on credentials or tooling:** real AWS and Kubernetes executors, the forced-recycle
   test, the live cold-start measurement, and a live GitHub SHA lookup for the frozen settings
   SHA. Until then these are `[unverified]` at runtime.
