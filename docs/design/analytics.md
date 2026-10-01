# Analytics & Autoscaling

Status: Proposed

> The thresholds below (1.3x memory headroom, 85% CPU ceiling, three-night hysteresis, rollup
> cadence) and the cost figures are proposed starting points and dated research notes, not
> requirements; tune them against real data before relying on them.

## Summary

Every `managed` run (executed on a cloud-ci `Executor`, Cloudflare Containers by default — see
[ADR 0010](../adr/0010-pluggable-executors.md)) and every `external` run (bring-your-own CI,
ingested via the API — see [./byo-ci.md](./byo-ci.md)) emits timing and resource-usage data. This
document specifies what is collected, how it is stored (raw high-cardinality events in Workers
Analytics Engine; small, fast D1 summaries and rolling aggregates for dashboard/PR-comment reads;
full per-run parsed results in R2), what insights are derived from it (slow/flaky/regressing
spots, and improvements), how it is surfaced (dashboard views, the full run report, PR comment
sections), how run cost is estimated, and the rightsizing algorithm that powers `runner: "auto"`
— picking an instance size per node from historical p95 peak memory and CPU saturation, with
bounds, OOM-retry-one-size-up, hysteresis, and an explainable decision recorded for the dashboard.

Analytics treats `managed` and `external` runs identically: both produce the same `Report` and
resource-sample shapes, so a BYO-CI job split across a GitHub Actions matrix shows up in the same
dashboards and PR comment sections as a node we scheduled ourselves. The one exception is
rightsizing: cost/resource recommendations only make sense for nodes we schedule, so
`runner: "auto"` and OOM-retry are no-ops for `external` runs (no container to size).

## Goals

- Collect, per run/job/step/shard: queue time, duration, critical path contribution, cache
  hit/miss, test durations and pass/fail/flaky state, and (for `managed` jobs) cgroup v2 CPU and
  memory samples from the agent.
- Store raw samples cheaply and at high cardinality (Analytics Engine), keep D1 small and
  SQL-joinable (summaries and rolling per-test aggregates updated in place, not one row per test
  case per run), and keep full per-run parsed results in R2.
- Derive durable insights: slowest tests/steps, flaky tests, duration/queue-time regressions vs. a
  baseline, and cache-efficiency and sharding-balance improvements.
- Provide dashboard views: run detail (DAG + critical path), repo trends, flaky test tracker,
  cost/runner report.
- Estimate run cost from measured CPU-active-time and memory/disk-provisioned-time against the
  active executor's billing rates.
- Implement `runner: "auto"`: pick the smallest instance size that comfortably fits a node's
  historical resource profile, within configured bounds, with explicit hysteresis and OOM
  recovery, and record *why* a size was chosen so it can be rendered in the full run report (see
  [./pr-comment.md](./pr-comment.md)).

## Non-goals

- Rightsizing of Durable Objects or Worker CPU time — those are Cloudflare-billed separately and
  not job-shaped.
- Cross-repo or cross-account benchmarking — all analytics are scoped to the single tenant
  deployment.
- Real-time (sub-second) streaming dashboards — rollups are cron-driven, on the order of minutes.
- Suggesting *what* to change to fix a slow test — that is [./ai.md](./ai.md)'s job; this doc only
  identifies *that* something is slow, flaky, or regressed, and classifies it as a bad or good
  spot.
- Billing passthrough or chargeback — cost estimation here is informational (dashboard/run
  report), not an invoice.

## User experience

### Script

Resource bounds and sizing hints live on the node, as `ci.container`'s `runner` option
([dynamic-pipelines](./dynamic-pipelines.md)):

```ts
await ci.container("test", {
  runner: "auto",                     // shorthand for settings.yml's runners.auto defaults
  run: "cargo test --workspace",
});

await ci.container("build-wasm", {
  runner: { auto: true, min: "basic", max: "standard-3", initial: "standard-1" },
  run: "cargo build --release --target wasm32-unknown-unknown",
});

await ci.container("lint", {
  runner: "standard-1",               // fixed size: no sizing engine involvement
  run: "cargo clippy --workspace",
});
```

`runner: "auto"` with no fields is shorthand for the deployment-wide
`runners.auto: { min: "basic", max: "standard-4", initial: "standard-2" }` in settings.yml (field
reference: [./settings.md](./settings.md)). Analytics only reads `min`/`max`/`initial` and writes
back a chosen size per node to `sizing_decisions`; the instance-size ladder itself (vCPU/memory per
size) is this doc's concern, below.

### CLI

```
$ cloud-ci agent --job test --runner standard-2 ...
# agent samples /sys/fs/cgroup/{memory.current,memory.peak,cpu.stat} every 2s,
# uploads samples as part of the job's Report on exit.

$ cloud-ci upload --report junit:./target/junit.xml --report timing:./target/step-timings.json
# BYO-CI path (same binary, no agent/cgroup sampling since there's no container we control).
```

### Dashboard

- **Run detail**: DAG view, each node colored by duration; critical path highlighted; click a node
  for its step timeline and resource-sample chart (CPU% and memory vs. its instance size's limit).
- **Repo trends**: p50/p95 run duration and queue time over time, cache hit rate, cost per
  day/week.
- **Flaky tests**: table of tests ranked by `test_stats.flakiness_score`, with recent pass/fail
  history sparkline (`test_stats.recent_outcomes`).
- **Runner sizing**: table of `runner: "auto"` nodes, current chosen size, p95 peak memory, p95 CPU
  saturation, last resize date and reason.

### PR comment and full report

Per [dynamic-pipelines](./dynamic-pipelines.md)'s GitHub status checks model and
[./pr-comment.md](./pr-comment.md)'s template design, the
default sticky PR comment is short: it does **not** include a per-test status table, a critical
path breakdown, or the Runner sizing section — those restate what the checks already show, or are
secondary detail. This doc's rollups feed two surfaces instead:

- **The sticky PR comment** (short, default template): a one-line test summary from
  `report_summaries` (total/passed/failed/skipped), and flaky-test labels on failing tests, from
  `test_stats`.
- **The full run report** (dashboard page, linked from the comment): the Performance section
  (regressions/improvements vs. the default-branch baseline, total critical path) and the Runner
  sizing section — for each `runner: "auto"` node, chosen size, previous size (if changed this
  run), and a one-line reason, e.g.
  `standard-2 → standard-3: p95 working set 5.1 GiB × 1.3 headroom = 6.6 GiB > standard-2's 6 GiB`.

A repo can opt a template into showing these sections inline (see
[./pr-comment.md](./pr-comment.md)'s template model); the default does not.

## Design

### Data flow

```mermaid
flowchart LR
    subgraph Container
      Agent[cloud-ci agent] -->|cgroup v2 samples every 2s| Agent
      Agent -->|Report: timings + samples + test results| Worker
    end
    CLI[cloud-ci upload BYO-CI] -->|Report: timings + test results, no samples| Worker
    Worker -->|writeDataPoint per step/sample/test| AE[(Analytics Engine)]
    Worker -->|job/run summary row| D1Live[(D1: runs/jobs/steps)]
    Worker -->|report summary + failed/flaky rows + test_stats upsert| D1Test[(D1: report_summaries, test_failures, test_stats)]
    Worker -->|full parsed report, compressed| R2[(R2)]
    Cron[Cron: */15 rollup] -->|SQL API query| AE
    Cron -->|upsert rollups + fresh insights| D1Roll[(D1: run_rollups, insights)]
    D1Live --> Dashboard
    D1Roll --> Dashboard
    D1Test --> Dashboard
    D1Roll --> FullReport[Full run report / PR comment generator]
    D1Test --> FullReport
    Cron2[Cron: nightly rightsizing + regressions] -->|read p95 samples| AE
    Cron2 -->|write chosen instance size + insights| D1Roll
```

RunCoordinator (per-run Durable Object, see [../architecture.md](../architecture.md)) writes
job/step start and end timestamps directly to D1 as the run progresses (`runs`, `jobs`, `steps`
tables — "live" tables, small, always current). High-cardinality and high-frequency data (every
resource sample, every test case result, every step's fine-grained duration) goes to Analytics
Engine via `writeDataPoint`, because D1 is not suited to write volumes of that shape and because
Analytics Engine's 3-month retention and sampling-at-scale are a better fit for raw time series
(verified 2026-09-30, developers.cloudflare.com/analytics/analytics-engine/limits/: 3-month
retention, up to 20 blobs/20 doubles/1 index per data point, 16 KB blob budget per data point, 250
`writeDataPoint` calls per Worker invocation). The same ingest path also writes a per-report
summary row, upserts the touched tests' rolling aggregates, and writes the full parsed report to
R2 — see D1 rollup tables and R2, below.

### What is collected

| Metric | Granularity | Source | Collected for |
| --- | --- | --- | --- |
| Queue time (dispatch requested → container running) | per job/shard | RunCoordinator timestamps | managed only |
| Step duration | per step | agent (managed) / `cloud-ci upload` (external) | both |
| Job/shard duration | per job/shard | agent / upload | both |
| Critical path | per run (derived) | computed from job DAG + durations | both |
| Cache hit/miss + bytes restored | per job | agent (cache restore step) | managed; external if the pipeline reports it |
| Test duration, pass/fail/skip | per test case | Report (JUnit/Vitest/Playwright JSON) | both |
| Flakiness | per test (derived) | computed incrementally from `test_stats.recent_outcomes` | both |
| CPU sample (`usage_usec` delta) | every 2s | agent reads `/sys/fs/cgroup/cpu.stat` | managed only |
| Memory sample (`memory.current`, `memory.peak`) | every 2s | agent reads `/sys/fs/cgroup/memory.current`, `memory.peak` | managed only |
| OOM event | per job | agent reads `memory.events` `oom_kill` counter | managed only |

cgroup v2 fields used: `cpu.stat` exposes `usage_usec` (cumulative CPU time) which the agent diffs
between samples to compute instantaneous CPU; `memory.current` is live resident usage;
`memory.peak` is the high-water mark since the cgroup was created (reset not required — the agent
reads it once at job end instead of tracking its own max, since the container's cgroup is created
fresh per job); `memory.events`' `oom_kill` field increments when the kernel OOM-killer fires
inside the cgroup, which is how the agent (or, if the agent itself was killed, the Worker's
"container exited non-zero with no final Report" fallback) detects an OOM to trigger
retry-one-size-up [source: docs.kernel.org cgroup-v2.rst, `cpu.stat`/`memory.peak`/`memory.events`
sections; exact kernel doc version not pinned, cross-checked 2026-09-30].

The agent samples every 2 seconds and keeps all samples in memory for the duration of the job (a
1-hour job at 2s intervals is 1800 samples × 2 doubles × 8 bytes ≈ 29 KB, well inside per-job
memory), then emits them as part of the job's end-of-run Report rather than streaming each sample
individually — this keeps the Worker's `writeDataPoint` call count low (one batch write per job
via `writeDataPoints()`, not one call per 2s tick) and avoids the 250-calls-per-invocation
Analytics Engine limit on busy runs with many concurrent jobs reporting to the same Worker
invocation.

### Analytics Engine schema

One dataset, `cloud_ci_metrics`, indexed by `repo_id` (the index field — see "why `repo_id`"
below). Event kind lives in `blob1` so a single dataset can hold several row shapes without
needing one dataset per metric (Analytics Engine datasets are provisioned per Worker binding, and
one binding is simplest to operate for a single-tenant deployment):

| blob1 (kind) | blob2 | blob3 | blob4 | double1 | double2 | double3 | index1 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `step` | run_id | job_id | step_name | duration_ms | queue_ms | — | repo_id |
| `sample` | run_id | job_id | instance_type | cpu_usage_usec_delta | memory_current_bytes | memory_peak_bytes | repo_id |
| `test` | run_id | job_id | test_id | duration_ms | pass(1)/fail(0)/skip(-1) | — | repo_id |
| `cache` | run_id | job_id | cache_key_prefix | hit(1)/miss(0) | bytes_restored | — | repo_id |

`repo_id` is the index because the brief's deployment model is single-tenant-per-org-but-many-repos:
repos are the natural "customer" subgroup for Analytics Engine's equitable sampling (see Sampling,
below), and nearly every dashboard view and query is scoped to one repo at a time.

Querying for rollups uses the SQL API (`POST
https://api.cloudflare.com/client/v4/accounts/<account_id>/analytics_engine/sql`, bearer token
with *Account Analytics Read* permission — verified 2026-09-30,
developers.cloudflare.com/analytics/analytics-engine/sql-api/). The cron job holds this token as a
Worker secret; it is never exposed to the dashboard or to end users.

Because Analytics Engine applies equitable, adaptive sampling per index value at high write volume
(verified 2026-09-30, developers.cloudflare.com/analytics/analytics-engine/sampling/), every
rollup query that aggregates counts or sums MUST weight by `_sample_interval` (e.g.
`SUM(_sample_interval)` instead of `COUNT()`, and `quantileExactWeighted(0.95)(double1,
_sample_interval)` instead of a plain quantile). For a single-tenant deployment driven by one
org's CI traffic, sustained sampling is expected to be rare except for very high-volume
monorepos, but the rollup cron treats it as the common case rather than special-casing it, since
getting this wrong silently understates p95s for the busiest repos.

### D1 rollup tables

The storage-growth constraint (per [ADR 0010](../adr/0010-pluggable-executors.md)'s accompanying
decision) is that **D1 must never hold one row per test case per run** — that scales with total
test executions across all history, not with anything bounded. D1 instead holds three kinds of
rows, all either bounded by a count that does not grow with run volume, or swept by retention:

```sql
-- One row per report upload (run_id, job_name, report_type) — a summary, not per-test data.
CREATE TABLE report_summaries (
  run_id        INTEGER NOT NULL,
  job_name      TEXT NOT NULL,
  report_type   TEXT NOT NULL,       -- junit | playwright-json | vitest-json | coverage | ...
  total         INTEGER NOT NULL,
  passed        INTEGER NOT NULL,
  failed        INTEGER NOT NULL,
  skipped       INTEGER NOT NULL,
  duration_ms   INTEGER NOT NULL,
  r2_key        TEXT NOT NULL,       -- full parsed report, compressed (see R2, below)
  PRIMARY KEY (run_id, job_name, report_type)
);

-- One row per failing or flaky test *occurrence* — not every passing test. Retention-swept.
CREATE TABLE test_failures (
  repo_id      INTEGER NOT NULL,
  test_id      TEXT NOT NULL,        -- hex(sha256(file_path || 0x1f || full_test_name))[0:16]
  run_id       INTEGER NOT NULL,
  job_name     TEXT NOT NULL,
  sha          TEXT NOT NULL,
  branch       TEXT NOT NULL,
  status       TEXT NOT NULL,        -- fail | flaky
  duration_ms  INTEGER NOT NULL,
  message      TEXT,                 -- truncated assertion/failure message, for drill-down
  occurred_at  INTEGER NOT NULL,
  PRIMARY KEY (repo_id, test_id, run_id)
);
CREATE INDEX idx_test_failures_recent ON test_failures (repo_id, occurred_at);

-- One row per (repo_id, test_id), updated in place on every default-branch run. Never grows
-- with run count — bounded by the number of distinct tests a repo has.
CREATE TABLE test_stats (
  repo_id           INTEGER NOT NULL,
  test_id           TEXT NOT NULL,
  file_path         TEXT NOT NULL,
  test_name         TEXT NOT NULL,
  runs              INTEGER NOT NULL DEFAULT 0,
  duration_ewma_ms  REAL NOT NULL,    -- exponential moving average, alpha = 0.2
  last_duration_ms  INTEGER NOT NULL,
  last_status       TEXT NOT NULL,    -- pass | fail | skip
  recent_outcomes   TEXT NOT NULL,    -- last 20 outcomes, one char each ('P'/'F'/'S'), newest last
  flakiness_score   REAL NOT NULL DEFAULT 0,  -- flips / (len(recent_outcomes) - 1)
  last_run_id       INTEGER NOT NULL,
  last_sha          TEXT NOT NULL,
  updated_at        INTEGER NOT NULL,
  PRIMARY KEY (repo_id, test_id)
);
CREATE INDEX idx_test_stats_flaky ON test_stats (repo_id, flakiness_score);

CREATE TABLE run_rollups      (run_id PK, repo_id, head_sha, started_at, duration_ms, queue_ms, critical_path_ms, cache_hit_rate, cost_usd_estimate, status);
CREATE TABLE insights         (repo_id, kind, subject_id, severity, detail_json, created_at, PRIMARY KEY (repo_id, kind, subject_id, created_at));
CREATE TABLE sizing_decisions (repo_id, job_name PK, current_instance_type, p95_memory_bytes, p95_cpu_frac, last_resized_at, reason, PRIMARY KEY (repo_id, job_name));
```

`report_summaries`, `test_failures`, and `test_stats` are populated by the same ingest path (not
the cron): the `post-run-analysis` Queue consumer that parses an uploaded report (via
`cloud-ci-reports`) writes all three in one pass, per report. `run_rollups`, `insights`, and
`sizing_decisions` remain populated by the `*/15` and nightly crons, same as before.

**Upsert pattern for `test_stats`.** A report with thousands of test cases must not become
thousands of D1 queries (D1's per-Worker-invocation cap is 1000 queries on Workers Paid, verified
2026-09-30, developers.cloudflare.com/d1/platform/limits/). The consumer instead builds one JSON
array of `{test_id, file_path, test_name, duration_ms, status}` per report and issues a single
batched upsert using `json_each`, binding the whole array as one parameter (so the 100
bound-parameters-per-query limit, same source, never applies regardless of test count):

```sql
INSERT INTO test_stats (repo_id, test_id, file_path, test_name, runs, duration_ewma_ms,
                         last_duration_ms, last_status, recent_outcomes, last_run_id, last_sha, updated_at)
SELECT :repo_id, value->>'test_id', value->>'file_path', value->>'test_name', 1,
       value->>'duration_ms', value->>'duration_ms', value->>'status',
       substr(value->>'status', 1, 1), :run_id, :sha, :now
FROM json_each(:tests)
ON CONFLICT (repo_id, test_id) DO UPDATE SET
  runs             = test_stats.runs + 1,
  duration_ewma_ms = 0.2 * excluded.last_duration_ms + 0.8 * test_stats.duration_ewma_ms,
  last_duration_ms = excluded.last_duration_ms,
  last_status      = excluded.last_status,
  recent_outcomes  = substr(test_stats.recent_outcomes || excluded.last_status, -20),
  last_run_id      = excluded.last_run_id,
  last_sha         = excluded.last_sha,
  updated_at       = excluded.updated_at;
```

`flakiness_score` is deliberately **not** computed inline (it needs a flip-count scan over
`recent_outcomes`, not a constant-time expression); the `*/15` cron recomputes it for every
`test_stats` row touched since its last tick and writes it back in the same batched-upsert style.
This keeps the ingest-path write cheap and moves the slightly heavier string scan to the async
cron, off the request path.

**Idempotency.** A retried Queue message must not double-apply the EWMA update or append the same
outcome twice. The consumer guards on `report_summaries`' primary key
`(run_id, job_name, report_type)`: it writes `report_summaries` and the `test_stats`/
`test_failures` upserts in one D1 `batch()` call, and checks for an existing `report_summaries`
row for that key first — a retry that finds the row already present skips re-applying the
`test_stats`/`test_failures` writes for that report entirely, since `report_summaries`'s presence
*is* the "already processed" marker.

**D1 size budget.** D1's maximum database size is 10 GB on Workers Paid (500 MB on Free; cannot be
increased past 10 GB even on Paid — verified 2026-09-30,
developers.cloudflare.com/d1/platform/limits/). Every table above is bounded by something other
than cumulative run count: `test_stats` by distinct test count (a repo with 100,000 tests at
~250 bytes/row is ~25 MB); `report_summaries` and `run_rollups` by run × job count, swept by the
retention cron below; `test_failures` similarly swept, and inherently low-cardinality relative to
total test executions since only failures/flakes are stored. This is the design property D5 was
written to guarantee: no table here scales with *(runs × tests per run)*, which is the dimension
that would otherwise threaten the 10 GB ceiling for an active monorepo.

**Retention.** `report_summaries`, `test_failures`, and `run_rollups` are pruned by the retention
cron mentioned in the brief (age-based, deployment-configurable, default 90 days — exact knob
deferred to the migration). `test_stats` is never pruned by age (it is a bounded aggregate, not an
event log) — a row is only removed when its test hasn't appeared in any report for a configurable
number of days (default 180), as a signal the test was deleted/renamed.

`test_stats` is read by [./parallelization.md](./parallelization.md)'s `timing` split strategy and
`auto` shard-count resolution; that doc never defines or writes this table.

Full column lists (types, FKs to `runs`/`jobs`/`repos`) beyond what's shown above are an
implementation detail deferred to the migration that creates these tables; the names and primary
keys above are the contract other docs can rely on.

### R2

The full per-run parsed result — every test case's id, name, duration, and status, not just
failures — is stored in R2, not D1, as a compressed file per report:

```
runs/{run_id}/jobs/{job_name}/parsed/{report_type}.json.zst   # cloud-ci-reports' internal schema, all test cases
```

This is what `cloud-ci-reports` (the report-parsing/merging package, see
[ADR 0010](../adr/0010-pluggable-executors.md)) produces when it parses an uploaded report; it is
the source `report_summaries` and `test_stats`/`test_failures` are derived from. A dashboard view
that needs the full per-test list for one run (not just failures or the rolling aggregate) reads
this object directly rather than querying D1. The merged, framework-native report files
(`runs/{run_id}/jobs/{job_name}/merged/report.{ext}`) are [./parallelization.md](./parallelization.md)'s
R2 keys, distinct from this parsed representation — one is the merged original-format file, the
other is cloud-ci's own structured view used for analytics. Raw logs and the merged HTML site
artifact are [./assets.md](./assets.md)'s domain.

### Derived insights

Computed by the nightly cron (regressions, from Analytics Engine quantile queries) and the `*/15`
cron (fresh-run bad/good spots, plus `flaky_test` from `test_stats.flakiness_score`), written to
`insights`:

| kind | Trigger | Severity | Example `detail_json` |
| --- | --- | --- | --- |
| `slow_test` | test p95 (7-day window, from Analytics Engine `test` events) > 2× its own 30-day p50, min 10 runs | warn | `{"test_id": "...", "p95_ms": 4200, "baseline_p50_ms": 1800}` |
| `flaky_test` | `test_stats.flakiness_score` ≥ 0.15 (score = flips / (len(recent_outcomes)-1), where a flip is pass→fail or fail→pass between consecutive default-branch runs) | warn/critical (≥0.3) | `{"test_id": "...", "score": 0.22}` |
| `duration_regression` | run or job duration p50 (this PR's branch, last 5 runs) > baseline (default branch, last 20 runs) p50 × 1.25 | warn | `{"job_name": "test", "branch_p50_ms": 61000, "base_p50_ms": 45000}` |
| `queue_regression` | queue time p95 (repo, 1h window) > repo's 7-day p95 × 1.5 | info | `{"p95_ms": 9000, "baseline_p95_ms": 5500}` |
| `cache_degraded` | cache hit rate (repo, 1h window) < 7-day average − 20 points | info | `{"hit_rate": 0.41, "baseline": 0.78}` |
| `duration_improvement` | run or job duration p50 (branch) < baseline p50 × 0.85 | good | `{"job_name": "build-wasm", "branch_p50_ms": 30000, "base_p50_ms": 42000}` |
| `sizing_improvement` | rightsizing cron reduces a node's instance size and cost drops | good | `{"job_name": "lint", "from": "standard-2", "to": "basic", "est_savings_usd_month": 4.10}` |

"Bad spots" (`slow_test`, `flaky_test`, `duration_regression`, `queue_regression`,
`cache_degraded`) surface as warnings in the dashboard and the full run report's Performance
section. "Good spots" (`duration_improvement`, `sizing_improvement`) surface as positive callouts
in the same section, so the report is not purely a list of problems — a PR that measurably speeds
up CI should say so.

### Cost estimation

Cost per run is estimated, not billed-exact, from measured active CPU time and provisioned
memory/disk time against Cloudflare Containers' published rates (verified 2026-09-30,
developers.cloudflare.com/containers/platform/pricing/): CPU $0.000020/vCPU-second beyond 375
included vCPU-minutes/month, memory $0.0000025/GiB-second beyond 25 included GiB-hours/month, disk
$0.00000007/GB-second beyond 200 included GB-hours/month, all under the $5/month Workers Paid
plan. Per-job estimate:

```
cpu_cost   = job_duration_s * instance_vcpu * $0.000020      (CPU billed on active usage)
mem_cost   = job_duration_s * instance_memory_gib * $0.0000025  (memory billed on provisioned size)
disk_cost  = job_duration_s * instance_disk_gb * $0.00000007
job_cost_usd_estimate = cpu_cost + mem_cost + disk_cost
```

This intentionally ignores the monthly included-usage tiers (25 GiB-hours, 375 vCPU-minutes, 200
GB-hours) when estimating a single run's cost — the included tier is an account-level monthly
allowance, not something attributable to one run, so per-run estimates are "gross" marginal cost
and the dashboard's monthly rollup separately nets out the included allowance once at the account
level. `run_rollups.cost_usd_estimate` sums the job estimates for the run. The dashboard's
repo-trends cost view sums `run_rollups` over the selected period and subtracts the included-tier
value once, labeled "estimated, before Workers Paid plan". Worker request/CPU-time and Durable
Object cost (billed separately per developers.cloudflare.com/containers/platform/pricing/,
"Workers and Durable Objects Pricing" section) are out of scope for this estimate and are called
out as excluded rather than silently omitted. Egress pricing ($0.025–$0.05/GB depending on region,
verified 2026-09-30 same source) is excluded from the per-run estimate since the agent does not
currently measure network bytes; this is listed as an open question below. Non-default executors
(ADR 0010) bill differently (e.g. AWS EC2 on-demand/spot rates); their cost estimate is out of
scope until an executor beyond Cloudflare Containers ships.

### Rightsizing algorithm (`runner: "auto"`)

Instance sizes and their resources, from the brief (verified 2026-09-30,
developers.cloudflare.com/containers/platform/limits/): `lite` (1/16 vCPU, 256 MiB, 2 GB disk),
`basic` (1/4 vCPU, 1 GiB, 4 GB), `standard-1` (1/2 vCPU, 4 GiB, 8 GB), `standard-2` (1 vCPU, 6 GiB,
12 GB), `standard-3` (2 vCPU, 8 GiB, 16 GB), `standard-4` (4 vCPU, 12 GiB, 20 GB), ordered smallest
to largest by this table's row order. This is Cloudflare Containers' ladder specifically — the
default executor (ADR 0010); a non-default `Executor` exposes its own `sizes` list through its
capability descriptor, and rightsizing/OOM-retry walk whichever ladder belongs to the node's
resolved executor. The algorithm below is executor-agnostic; the concrete sizes are not.

**Inputs** (per `(repo_id, job_name)`, i.e. per named node across its run history — shards of the
same `ci.shard` group share one sizing decision since they run the same steps): the last 20
completed runs' resource samples for that node, read from Analytics Engine.

**Per-run reduction** (nightly cron, before cross-run aggregation):
- `peak_memory = MAX(memory_peak_bytes)` across that run's samples for the node.
- `cpu_saturation = MAX(cpu_usage_usec_delta / sample_interval_usec / instance_vcpu)` — the
  highest fraction of the instance's allotted vCPU actually used in any one sample window, capped
  at 1.0.

**Cross-run aggregation**: `p95_peak_memory = quantileExactWeighted(0.95)` of the 20 runs' peak
memory; `p95_cpu_saturation` likewise over CPU saturation. Using p95 rather than max avoids one
anomalous run (e.g. a one-off large test fixture) permanently pinning a node to an oversized
instance; using p95 rather than p50 avoids routinely OOM-killing the typical "slightly larger than
usual" run.

**Instance selection**:
1. `target_memory = p95_peak_memory * 1.3` (30% headroom — chosen because `memory.peak` is a
   high-water mark over the whole job, not an instant sample, so a tighter margin risks the *next*
   run's peak exceeding it even with no real growth).
2. `target_cpu_vcpu = p95_cpu_saturation * current_instance_vcpu` if `p95_cpu_saturation` is being
   measured against the *current* instance (saturation is relative to whatever vCPU count was
   active when the samples were taken); recompute each candidate's implied saturation as
   `target_cpu_vcpu / candidate_vcpu`.
3. Choose the smallest instance size, within `[min, max]` from the node's config, where
   `candidate.memory_bytes >= target_memory` AND `target_cpu_vcpu / candidate.vcpu <= 0.85` (85%
   ceiling leaves headroom for scheduler jitter and avoids flapping right at 100%).
4. If no candidate within `[min, max]` satisfies both, clamp to `max` and record the shortfall in
   `reason` (the node will keep running, just under-provisioned — this is visible in the full run
   report and dashboard rather than silently capped).
5. If fewer than 5 completed runs exist for the node, use `initial` from the config and do not
   resize (not enough signal).

**Hysteresis**: a resize (up or down) only takes effect if the newly selected size differs from
`sizing_decisions.current_instance_type` for **3 consecutive nightly recalculations** (i.e. the
recommendation must be stable across 3 nights, each evaluated against the latest 20-run window) —
this prevents a node that oscillates near a boundary (e.g. p95 memory bouncing just above/below
the `basic`/`standard-1` line) from resizing every night. The one exception is OOM-retry (below),
which bypasses hysteresis entirely because it is reacting to an observed failure, not a trend.

**OOM retry**: if a node's container is OOM-killed (`memory.events.oom_kill` increments, or the
Worker observes the container process exit without a final Report after a memory-pressure
signal), `RunCoordinator` immediately retries that node once on the next size up from the one it
just used (per the active executor's size ladder) — bypassing hysteresis and the p95 computation —
and records `reason: "oom-retry: <from> -> <to>"` in `sizing_decisions`. This retry-sized instance
becomes the new `current_instance_type` immediately (not just for the one retry) since an actual
OOM is stronger evidence than any number of p95 samples that stayed under threshold. If the node
OOMs again at `max`, it fails with a message naming the configured `max` and the measured peak,
rather than retrying indefinitely.

**Explainability**: every change to `sizing_decisions.current_instance_type` writes a
human-readable `reason` string, e.g. `"standard-1 -> standard-2: p95 peak memory 4.9 GiB x1.3 =
6.4 GiB > standard-1's 4 GiB (stable 3/3 nights)"` or `"standard-2 -> standard-3: oom-retry"`.
This string, plus the before/after size, is what the full run report's Runner sizing section
renders for any node whose size changed on this run or in the last 7 days — surfacing a resize
even on PRs that didn't trigger it, since the first run after a resize is the one most likely to
confuse a reader if unexplained.

```mermaid
flowchart TD
    A[Nightly cron: per repo, per job_name] --> B{>= 5 completed runs?}
    B -- no --> C[use configured 'initial', no resize]
    B -- yes --> D[compute p95 peak_memory, p95 cpu_saturation over last 20 runs]
    D --> E[select smallest size in min..max fitting memory*1.3 and cpu<=85%]
    E --> F{differs from current_instance_type?}
    F -- no --> G[no change]
    F -- yes, 1st or 2nd night --> H[record candidate, wait for confirmation]
    F -- yes, 3rd consecutive night --> I[apply resize, write reason]
    J[Node OOM-killed at runtime] --> K[retry once at next size up, bypass hysteresis]
    K --> L{OOM again at max?}
    L -- yes --> M[fail job, report configured max + measured peak]
    L -- no --> N[succeeds; new size becomes current_instance_type immediately]
```

## Data model

### D1 tables (see also "D1 rollup tables" above for the analytics-owned set)

- `report_summaries`, `test_failures`, `test_stats`, `run_rollups`, `insights`,
  `sizing_decisions` — owned by this doc; schemas above.
- `runs`, `jobs`, `steps` (live, written by RunCoordinator as a run progresses) — owned by
  [../architecture.md](../architecture.md); analytics reads `jobs.instance_type` and
  `jobs.duration_ms` as rollup inputs but does not define these tables.
- [./parallelization.md](./parallelization.md) reads `test_stats.duration_ewma_ms` for
  `cloud-ci split`'s timing strategy and `auto` shard-count resolution, but does not define or
  write this table.

### Analytics Engine dataset

- `cloud_ci_metrics` — schema in "Analytics Engine schema" above. One dataset for the whole
  deployment (single Worker binding), discriminated by `blob1` kind and indexed by `repo_id`.

### R2

- `runs/{run_id}/jobs/{job_name}/parsed/{report_type}.json.zst` — full per-run parsed test results
  (every test case), this doc's own key, defined in "R2" above.
- Merged framework-native reports and the merged HTML site artifact are
  [./parallelization.md](./parallelization.md) and [./assets.md](./assets.md)'s keys respectively;
  this doc reads neither directly.

## Security considerations

- The Analytics Engine SQL API bearer token (Account Analytics Read scope) is a Worker secret,
  used only by the cron job; it is never returned to the dashboard, the PR comment/full-report
  generator, or any end-user-facing API. Dashboard and report reads always go through D1 rollups,
  which are already access-controlled by the deployment's own role model (viewer/operator/admin,
  derived from GitHub repo permission — see [./auth.md](./auth.md)), not the raw Analytics Engine
  data.
- Resource samples and timing data are not secrets, but `test_id` and `step_name` values can embed
  repo-specific strings (file paths, env var names echoed in a step name); these are stored in
  D1/Analytics Engine/R2, which are already scoped to the single tenant's own Cloudflare account,
  so no additional redaction is applied beyond what the agent already avoids echoing (raw secret
  *values* are never part of a Report; that is a job-execution concern, not an analytics one).
- Cost estimates and sizing decisions are read-only derived data; a compromised dashboard viewer
  role can see them but cannot act on them (resizing is cron-driven, not triggerable via any read
  API).

## Failure modes

| Failure | Behavior |
| --- | --- |
| Agent crashes before emitting a Report (e.g. OOM-killed) | RunCoordinator detects the container exited without a final Report; if `memory.events.oom_kill` was observed (via a last-gasp sample or the Worker's own container-exit-code inspection), treat as OOM and trigger OOM-retry; otherwise mark the job failed with no resource data for that run (it is simply excluded from the next p95 window, not treated as a zero). |
| Analytics Engine `writeDataPoints` call fails (e.g. transient 5xx) | Logged and dropped; one run's worth of samples/tests missing from Analytics Engine does not block the run from completing or from showing live durations (sourced from D1 `runs`/`jobs`/`steps`), since the dashboard's live run view does not depend on Analytics Engine at all. |
| `post-run-analysis` consumer retries after partially writing `report_summaries`/`test_stats`/`test_failures` | The three writes happen in one D1 `batch()` call guarded by `report_summaries`' primary key (see "Idempotency" above), so a retry either sees the row already present and skips re-applying, or the whole batch failed atomically and retries cleanly — never a partial EWMA/outcome-string double-application. |
| Rollup cron fails mid-batch | Upserts are idempotent per `(repo_id, period)`/`(run_id, job_id)` key, so a retried cron run (next scheduled tick) safely reprocesses the same window; no partial-rollup corruption, just a delay until the next successful tick. |
| Analytics Engine query sampling kicks in for a very high-volume repo | p95/flakiness computations already weight by `_sample_interval`; accuracy degrades gracefully rather than silently, and extremely low event-count index values (new or quiet repos) are never sampled per Analytics Engine's equitable-sampling design. |
| `runner: "auto"` has fewer than 5 historical runs (new node) | Uses configured `initial` size, does not attempt sizing, so a brand-new node never gets undersized on its first few runs based on no data. |
| Node OOMs even at `max` | Fails with an explicit message naming `max` and the measured peak; does not retry past `max` or silently fall back to a smaller size. |
| Node's actual resource profile is fundamentally bursty / bimodal (e.g. integration tests only present sometimes) | p95 over a 20-run window will reflect the heavier mode often enough to size for it; hysteresis (3 consecutive nights) prevents flapping if the mix of runs varies night to night, at the cost of slower reaction to a genuine, sustained shift. |

## Open questions

- Network egress is not currently sampled by the agent (no per-job byte counter), so cost
  estimates exclude the egress pricing tier entirely; whether to add a `/sys/class/net`
  byte-counter sample is open.
- Whether `cpu.pressure` (PSI) should supplement `cpu.stat`'s `usage_usec` for saturation — PSI
  reflects stall time rather than raw usage and might catch CPU-starved-but-not-100%-busy jobs
  that the current `usage_usec`-based saturation metric would miss. Deferred as `[unverified]`
  whether Cloudflare Containers' kernel/cgroup configuration exposes `cpu.pressure` to the
  container.
- Whether workers-rs can read container cgroup stats directly or whether the agent (running inside
  the container, which it already does for step execution) remains the sole source of samples —
  this is the same open question the brief raises about whether workers-rs can drive containers
  directly at all; this doc assumes the agent-samples-and-uploads approach regardless of that
  outcome, since the agent is already resident in the container for the job's steps.
- Exact hysteresis window (3 nights) and headroom multipliers (1.3× memory, 0.85 CPU ceiling) are
  initial proposals, not tuned against real workload data; expect revision once the system has
  running deployments to observe.
- Whether `insights` rows should ever be deleted/expired, or kept forever as a history — currently
  unspecified; likely candidate for the retention-sweep cron mentioned in the brief but not
  designed here.
- Whether a non-default executor's cost model and size ladder need their own rate table per
  executor type, or whether ADR 0010's capability descriptor should carry pricing hints directly
  — open until a second executor ships.

## Alternatives considered

- **D1-only, no Analytics Engine**: rejected — per-2-second resource samples and per-test-case
  results across every run would make `jobs`/`steps`-adjacent D1 tables extremely large and slow
  for a relational store sized for metadata, and D1 has no built-in time-series rollup or sampling
  behavior; Analytics Engine is purpose-built for this write volume and already included in the
  Workers platform.
- **Analytics Engine only, no D1 rollups**: rejected — every dashboard page load and every PR
  comment/full-report render would need a live SQL API round-trip (network call to a separate
  Cloudflare product, subject to ABR sampling trade-offs on long time ranges), adding latency and
  an external dependency to paths that should be fast and simple joins against other D1 metadata
  (repo settings, PR state).
- **One D1 row per test case per run** (the original design): rejected — this is exactly the
  unbounded-growth pattern the storage-growth decision rules out; row count would scale with
  *(runs × tests per run)*, threatening the 10 GB per-database ceiling for any sufficiently active
  repo. A rolling aggregate (`test_stats`, one row per test, updated in place) plus full parsed
  results in R2 gives both splitting and analytics what they need without that growth.
- **Max instead of p95 for rightsizing**: rejected — a single anomalous run would permanently
  inflate the chosen instance size; p95 balances responsiveness to real growth against single-run
  noise.
- **Resize immediately on any change (no hysteresis)**: rejected — nodes whose resource usage sits
  near a size boundary would resize every night, producing a noisy Runner sizing section on
  unrelated PRs and no stable cost signal.
- **Separate Analytics Engine dataset per metric kind** (`cloud_ci_steps`, `cloud_ci_samples`,
  etc.): rejected for v1 — one dataset with a `blob1` kind discriminator is simpler to provision
  and bind, and dataset-per-kind can be revisited later if the mixed-schema table becomes awkward
  to query.
- **Bill exact Cloudflare cost via the Billing API instead of estimating**: rejected — Cloudflare's
  billing is account-level and would not attribute cost back to an individual job/run without the
  same measured-active-usage approach this doc already takes; using the published per-resource
  rates directly against measured usage gives a per-run number without needing billing-API access
  or waiting for a billing cycle to close.
