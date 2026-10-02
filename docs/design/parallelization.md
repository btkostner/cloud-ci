# Parallelization

Status: Proposed

## Summary

A `ci.shard(id, opts)` call in a pipeline script ([dynamic-pipelines](./dynamic-pipelines.md))
splits a test suite into N `Shard`s that each run as an independent container, then merges their
`Report`s back into one result. Shard count is either a fixed integer or an auto-sizing spec
(`{ min, max, target }`, sized to hit a target wall time). The split can use historical
per-test/per-file timing pulled from D1, a flat file-count round-robin, or a flat test-count
round-robin. The same splitting logic is exposed as
`cloud-ci split` so bring-your-own-CI (BYO CI) pipelines (e.g. a GitHub Actions matrix) get
identical, deterministic shard assignment without running inside cloud-ci's own containers.
`RunCoordinator` (the per-run Durable Object, see [../architecture.md](../architecture.md)) tracks
shard completion and acts as the merge barrier; once every shard in a shard group reaches a
terminal state, it triggers the merge step appropriate to the report type.

## Goals

- Deterministic shard assignment: given the same file list, timing snapshot, strategy, and shard
  count, the computed assignment is identical every time, on both the worker (managed runs) and
  the CLI (BYO CI runs).
- An auto-sizing `count` spec (`{ min, max, target }`) that targets a configured wall-clock
  duration instead of requiring a fixed number of machines.
- Timing-aware splitting using real historical data, with a safe fallback when no history exists
  (new repo, new test files).
- Native (no-container) merging for structured text reports (JUnit XML, lcov/cobertura coverage);
  container-based merging for opaque framework blob formats (Playwright blob, Vitest blob) via the
  framework's own merge CLI.
- `cloud-ci split` works identically whether invoked by the cloud-ci agent inside a managed
  container or by a user's GitHub Actions matrix job.
- Clear, bounded retry semantics for a single failed shard that never perturb the other shards'
  assignments.

## Non-goals

- Dynamic work-stealing or mid-run rebalancing between shards (shard assignment is frozen at
  dispatch time).
- Splitting across repos/pipelines (a shard group is scoped to one `ci.shard` call in one run).
- Choosing the test framework's own parallelism inside a shard (e.g. Playwright's `fullyParallel`
  worker count) — that is the framework's concern, orthogonal to cloud-ci's shard-level splitting.
- Analytics on top of timing data (regressions, flaky-test detection, critical path) — see
  [./analytics.md](./analytics.md), which owns the `test_stats` rolling per-test aggregate this
  doc reads for timing-aware splits and which is applied once per run at `RunCoordinator`
  finalization, not by this doc's merge step.

## User experience

### Script

```ts
// .cloud-ci/pipelines/ci.ts
const test = ci.check("ci/test", { required: true });

await ci.shard("test", {
  split: "timing",                // required: timing | file | count
  count: { min: 2, max: 16, target: "8m" },  // required: int 1-64, or { min, max, target } to auto-size
  runner: "standard-2",
  check: test,
  files: "tests/**/*.spec.ts",
  failFast: false,                // cancel remaining shards on first failure
  run: ({ shard, shards }) => `npx playwright test --shard=${shard}/${shards} --reporter=blob`,
  reports: [{ type: "playwright-blob", path: "blob-report/", merge: "html" }],
  merge: { runner: "basic" },     // instance type for the generated merge node; default basic
});
```

`ci.shard` options:

| Option | Type | Default | Notes |
| --- | --- | --- | --- |
| `split` | `"timing"` \| `"file"` \| `"count"` | required | strategy `cloud-ci split` uses |
| `count` | int `1..64`, or `{ min, max, target }` | required | fixed shard count, or an auto-sizing spec (`min`/`max` bounds, `target` wall-time duration); no global default, every call sets its own |
| `files` | glob or glob[] | required when `split` is `"file"` or `"timing"` | universe of files to divide; each shard's `run` function receives its share as `files` |
| `failFast` | bool | `false` | cancel the remaining shards in the group on the first shard failure |
| `runner` | same shapes as `ci.container`'s `runner` ([dynamic-pipelines](./dynamic-pipelines.md), [ADR 0010](../adr/0010-pluggable-executors.md)) | settings.yml `runners.default` | runner for each shard container |
| `check` | a `ci.check(...)` result, or `null` | `null` | check every shard attaches to; `null` means no check run ([dynamic-pipelines](./dynamic-pipelines.md)'s GitHub status checks model: checks are opt-in, not always-on) |
| `merge.runner` | instance type | `basic` | instance type for the generated `<id>/merge` node |
| `merge.setup` | step[] | `[]` | steps run in the merge node before the merge command |
| `merge.command` | string | generated | overrides the generated merge command entirely |

The engine always computes the split behind the scenes — a managed shard's `run` command never
calls `cloud-ci split` or shells out to assemble its own file list. Instead, `run` is a function
`({ shard, shards, files }) => string` that the engine calls once per shard: `shard` is the
1-based index, `shards` is the total, and `files` is this shard's assigned file list. The job env
still applies to the command it returns. A plain string is also accepted when the command does
not depend on the shard.

For frameworks with native sharding (Playwright, Vitest), `split: "file"` with
`--shard=${shard}/${shards}` is equivalent to cloud-ci computing a round-robin file assignment —
the engine still computes `files` for the shard, but the command can ignore it and use the native
flag instead.

`split: "timing"` and `split: "count"` matter most for frameworks without native sharding
(`go test`, `cargo test`, `mocha`) or when a framework's native sharding doesn't account for
historical duration. In that case the shard's `run` function uses `files` directly:

```ts
await ci.shard("unit", {
  split: "timing",
  count: { min: 2, max: 8, target: "5m" },
  files: ["**/*_test.go"],
  run: ({ files }) => `cargo nextest run ${files.join(" ")}`,
});
```

### `cloud-ci split` (also usable from BYO CI)

```
cloud-ci split --strategy timing|file|count \
  --shards <N> --index <1..N> \
  --files <glob> [--granularity file|test] \
  [--token <scoped-token> | relies on GitHub Actions OIDC]
```

Prints a newline-separated list (files, or `file::test-name` pairs with `--granularity test`) to
stdout for the given `--index`. It is the same binary and the same split algorithm
(`cloud_ci_core::split`, a library target inside `cloud-ci-core` — the package also housing the
rightsizer and other shared domain logic, see [ADR 0010](../adr/0010-pluggable-executors.md) — that
`cloud-ci-worker` also links against for managed runs) used in both places, so a BYO CI matrix and
a cloud-ci-managed shard group produce byte-identical assignments for the same inputs.

GitHub Actions matrix example (BYO CI, no cloud-ci containers involved):

```yaml
jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        shard: [1, 2, 3, 4]
    steps:
      - uses: actions/checkout@v4
      - run: npm ci
      - name: Resolve shard file list
        run: |
          cloud-ci split --strategy timing --shards 4 --index ${{ matrix.shard }} \
            --files "tests/**/*.spec.ts" > shard-files.txt
      - run: npx playwright test $(cat shard-files.txt) --reporter=blob
      - uses: actions/upload-artifact@v4
        with: { name: blob-report-${{ matrix.shard }}, path: blob-report }
      - run: cloud-ci upload playwright-blob 'blob-report/*.zip' --job e2e --shard ${{ matrix.shard }}/4
```

`cloud-ci split` authenticates with the same machine credentials as `cloud-ci upload` (GitHub
Actions OIDC token when `ACTIONS_ID_TOKEN_REQUEST_URL` is present, or a scoped API token via
`CLOUD_CI_TOKEN`) to read historical timing for the target repo — see [./byo-ci.md](./byo-ci.md)
and [./auth.md](./auth.md) for the credential exchange. With four matrix legs calling
`cloud-ci split` independently and concurrently, each leg performs a read-only point lookup of
`test_stats` (analytics.md) for each matched item — no time-window parameter is needed, since the
aggregate itself is already time-smoothed — so all four legs resolve the same per-item durations
consistently as long as no default-branch run's rollup lands mid-batch (see Failure modes).

GitHub Actions job matrices are capped at 256 jobs per workflow run (verified 2026-09-30,
docs.github.com/en/actions/reference/limits), which is well above cloud-ci's own `max: 64` shard
clamp.

## Design

### Shard count resolution

| `count` value | Resolution |
| --- | --- |
| integer `N` (1-64) | Fixed; used as-is. |
| `{ min, max, target }` object | `shard_count = clamp(ceil(historical_total_duration / target), min, max)`, where `historical_total_duration` sums the matched items' `duration_ewma_ms` from `test_stats`; falls back to `min` when there is no history for any matched item — this doc supplies the per-item imputation detail below for the common case of *partial* history. |

### Split strategies

| Strategy | Item granularity | Data source | Algorithm |
| --- | --- | --- | --- |
| `timing` (default) | per-test if the report type exposes it (JUnit `testcase`, Playwright/Vitest JSON), else per-file | `test_stats` D1 table (analytics.md), one row per `(repo_id, test_id)`, scoped to default-branch runs | Longest-Processing-Time (LPT) greedy bin-packing: sort items by duration descending, tie-break by item name ascending; assign each item to the currently least-loaded shard, tie-break by lowest shard index. |
| `file` | whole file | none (or framework's native `--shard`) | Round-robin: files sorted by path ascending, assigned `index = position % shard_count`. |
| `count` | whole test (if discoverable) or whole file | none | Round-robin by position, same as `file` but at test granularity when the runner can enumerate individual tests without running them. |

LPT bin-packing is deterministic because both the sort key and the tie-break for "least-loaded
shard" are total orders — no randomness, no wall-clock dependence, no reliance on map/hash
iteration order.

### Fallback when no history exists

A brand-new repo, or a job whose `files` glob matches files never seen before, has no rows in
`test_stats`. In that case:
- Items *with* history keep their real duration (`duration_ewma_ms`).
- Items *without* history are assigned the **median** duration of items that do have history (not
  zero) — this prevents one unmeasured, possibly-huge test file from being silently treated as
  free and stacked onto an already-full shard.
- If **no** item in the matched set has any history (first run ever for this `files` glob),
  `timing` silently degrades to `file` behavior (round-robin by path) for that run. The first
  successful default-branch run then populates `test_stats`, so the very next run already
  benefits from real timing.

### Deterministic assignment, end to end

1. The shard's `files` glob is expanded against the checked-out worktree at dispatch time, sorted
   by path, and hashed into a run manifest (so retries reuse the exact same input list even if the
   repo state could theoretically differ by the time a retry fires).
2. For managed runs, `RunCoordinator` computes the assignment once (vocabulary: RunCoordinator
   owns "shard assignment" per [../architecture.md](../architecture.md)) when `ci.shard`'s
   `step.do("split:" + id)` runs, and stores it as `shard_plan` rows before dispatching any shard
   container.
3. For BYO CI, each `cloud-ci split --index I` invocation recomputes the *same* plan independently
   (same algorithm, same `test_stats` point reads), reading the item list from `--files` directly
   rather than a server-side manifest — there's no RunCoordinator in play for an `external` run.
4. A retried shard (OOM only — see Failed-shard retry semantics) always reuses the stored
   `shard_plan` entry for its index; a retry never re-runs the split algorithm and never happens
   for a non-OOM failure.

### Check status and sealing (`ci.shard`)

A shard group's `check` option attaches one member per shard to the check — the full member count
for *this* `ci.shard` call becomes known the moment `RunCoordinator` persists `shard_plan` (end to
end, step 2 above). That event fixes this call's contribution, but it does not by itself seal the
check: the same `check` may still receive more attachments from other steps later in the same
pipeline script (another `ci.container`, another `ci.shard`), so the check's overall member set
isn't settled until sealing happens.

A check stays `queued`/`in_progress` and never concludes until it is sealed — automatically, when
the pipeline script's run function finishes scheduling (every step has been scheduled, so no
further attachment is possible), or explicitly via `check.seal()`
([dynamic-pipelines](./dynamic-pipelines.md)). `ci.shard` itself never seals a check; it only ever
attaches members to one. Attaching a member to an already-sealed check is a script error.

Without this rule, an observer polling GitHub's Checks API could see a check go green as soon as
its first attached shard passes, then flip back to `in_progress` when a later step in the same
script attaches another member.

Timeline, before sealing existed (the bug this fixes):

1. `t0` — shard 1 of (eventually) 4 starts and attaches to `check`; with no sealing, the check has
   only this member, so it goes `in_progress` -> `success` as soon as shard 1 passes.
2. `t1` — shard 2 attaches to the same `check`; GitHub now reopens a check that already reported
   `success`, a confusing signal for branch protection, reviewers, or status badges that already
   saw green.

Timeline, with sealing:

1. `t0` — the split step runs, resolves `shard_count = 4`, and persists `shard_plan`; all 4 shard
   members are now attached to `check`, but `check` is **not** sealed yet — the script may still
   attach more members to it later.
2. `t1`..`t4` — shards 1-4 start and run; `check` stays `in_progress` no matter how many of them
   finish, because it isn't sealed yet.
3. `t5` — the pipeline script's run function finishes scheduling (no more steps will ever attach to
   `check`); `RunCoordinator` seals `check` with `expected_total = 4`.
4. `t6` — once all 4 shards are terminal, the merge barrier (below) is satisfied
   (`count(terminal) == expected_total`), and only then does `RunCoordinator` conclude `check` —
   which is also always after `t5`, so GitHub never observes a conclusion before the member set is
   both complete and final.

### Merge barrier (RunCoordinator)

`RunCoordinator`'s SQLite-backed state (one DO instance per run) holds, per shard group:

```
shard_state(job_name TEXT, idx INTEGER, attempt INTEGER, status TEXT, report_key TEXT, duration_ms INTEGER, finished_at INTEGER)
job_group(job_name TEXT PRIMARY KEY, expected_total INTEGER, fail_fast INTEGER, merge_on_failure TEXT, merge_job_id INTEGER)
```

Each shard's terminal ingest call (worker API, authenticated with the shard's per-node short-lived
token) RPCs into `RunCoordinator`, which updates `shard_state` and checks:
- If `fail_fast = true` and a shard just transitioned to terminal `failed` (immediately for a
  non-OOM failure, or after its one automatic OOM retry also fails — see Failed-shard retry
  semantics), `RunCoordinator` immediately cancels remaining `running`/`queued` shards for that
  group (containers stopped via the same lifecycle path used for cancel-superseded in `RepoState`)
  and marks the shard group `failed`.
- Otherwise, when `count(terminal) == expected_total`, the barrier is satisfied and
  `RunCoordinator` decides whether to merge: `merge_on_failure` is `if_any_passed` (default),
  `always`, or `never`. `if_any_passed` runs the merge using only the successful shards' reports so
  the dashboard still shows partial results; the parent node's own status is still `failed` if any
  shard failed.
- On satisfaction, `RunCoordinator` enqueues the merge step via the `job-dispatch` Queue, same as
  any other node — merge dispatch still respects `RepoState`'s per-repo concurrency cap, so it may
  wait briefly behind other running containers.

### Merge strategies per report type

| Report type | Where merged | Default command |
| --- | --- | --- |
| `junit` | Worker / `post-run-analysis` Queue consumer (no container) | Parse each shard's JUnit XML from R2, concatenate `testsuite` elements into one `testsuites` document, write merged XML to R2. Does not touch `test_stats`: rolling per-test aggregates (`test_stats`/`test_failures`) are applied once per run at `RunCoordinator` finalization from each shard's canonical upload ([./analytics.md](./analytics.md)). |
| `coverage` (lcov, cobertura) | Worker / `post-run-analysis` Queue consumer (no container) | Parse and sum per-line/per-branch hit counts across shards; write merged file to R2. |
| `playwright-blob` | Generated `<id>/merge` container node | `npx playwright merge-reports --reporter <merge> <dir>` (verified 2026-09-30, playwright.dev/docs/test-sharding) |
| `vitest-blob` | Generated `<id>/merge` container node | `npx vitest --merge-reports <dir>` (verified 2026-09-30, vitest.dev/guide/reporters; reads the default `.vitest/blob/` directory when `<dir>` is omitted) |

JUnit and coverage formats are plain structured text — cheap enough to merge inline in a Queue
consumer (via `cloud-ci-reports`, the report-parsing package, see
[ADR 0010](../adr/0010-pluggable-executors.md)), so no container is spun up for them. Playwright
and Vitest blob reporters are opaque, framework-versioned binary/zip formats (Playwright:
`report-<hash>-<shard>.zip`; Vitest: files under `.vitest/blob/`) that only the framework's own
Node.js tooling can merge; `workers-rs`/wasm32 has no Node runtime, so these are merged by a
**generated** `<id>/merge` node that runs in a Container exactly like a user-authored one. Its
instance type, pre-merge steps, and command come from `ci.shard`'s own `merge.{runner,setup,command}`
option (default `merge.runner` is `"basic"`); the reporter(s)
passed to `--reporter` come from the triggering report's own `merge` field (`reports[].merge`,
e.g. `merge: "html"`), kept separate from `merge`'s execution config.

```ts
// synthesized internally when ci.shard's report type is playwright-blob or vitest-blob;
// shown here as the equivalent script code, never written by a user script directly.
await ci.container(`${id}/merge`, {
  runner: opts.merge?.runner ?? "basic",     // ci.shard's merge.runner, default basic
  needs: [id],                                // depends on every shard in this group
  run: [
    `cloud-ci agent download-shards --job ${id} --out ./blobs`,
    ...(opts.merge?.setup ?? []),              // merge.setup steps, if any, run before the merge command
    `npx playwright merge-reports --reporter html ./blobs`,  // --reporter value from reports[].merge
    `cloud-ci upload --site playwright-report=playwright-report --job ${id}/merge`,
  ],
});
```

By default this uploads only the merged `site` Artifact (the dashboard-facing HTML report),
which lands at the ordinary site artifact key (`runs/{run_id}/artifacts/{name}/site.tar` +
`site.index.json`, per [./assets.md](./assets.md) and [./byo-ci.md](./byo-ci.md)) —
matching the generated node's default behavior for every blob type. It does **not**, by itself,
contribute to `test_stats`: a blob zip's internal format is an unversioned, undocumented
Playwright/Vitest implementation detail that cloud-ci does not parse directly [unverified whether
a stable public schema exists for either]. If a shard group's `timing` split should benefit from
per-test history on its *next* run, `reports[].merge` must additionally request a
machine-readable reporter (e.g. `merge: "html,json"` for Playwright, since
`merge-reports --reporter` accepts a comma-separated list per
playwright.dev/docs/test-reporters), and `merge.command` is overridden to also run
`cloud-ci upload playwright merge-reports.json --job <id>/merge` — that upload
goes through the same native ingest path as a non-sharded `playwright-json` report, so it is
applied to `test_stats` at run finalization ([./analytics.md](./analytics.md)) identically
regardless of whether the original report type was native
or blob. The Vitest merge node follows the same shape: blobs download into `.vitest/blob/`,
`npx vitest --merge-reports` produces the configured reporters' output, and only an
explicitly-configured JSON/JUnit reporter output is re-uploaded for history.

### Failed-shard retry semantics

- Shards get exactly the same automatic retry behavior as any other node
  ([./analytics.md](./analytics.md)'s rightsizing algorithm) — there is no shard-specific retry
  count to configure. A shard whose container is OOM-killed is retried once, immediately, on the
  next-larger size in the runner's executor capability descriptor (sizes, per
  [ADR 0010](../adr/0010-pluggable-executors.md)), bypassing hysteresis; this is the *only*
  automatic retry path. A shard that fails for any other reason (non-zero exit, assertion failure,
  timeout) is terminal immediately — cloud-ci does not retry flaky test failures on its own.
- An OOM retry reuses the exact same `shard_plan` entry for that index (same file/test list) and
  the same `shard`/`shards` arguments to `run` — only the instance size changes.
  `shard_state.attempt` becomes `2` to record that it happened.
- If the retried shard OOMs again at the new size, it fails for good, reporting the configured
  `max` and measured peak, exactly as a non-sharded `runner: "auto"` node would
  ([./analytics.md](./analytics.md)).
- A shard is "terminal" once it reaches a final `passed`/`failed` status, after at most one OOM
  retry. The merge barrier only counts terminal shards.
- `failFast: true` short-circuits the group as soon as one shard goes terminal-failed, canceling
  the rest; `failFast: false` (default) lets every shard run to its own terminal state so the
  dashboard reports the full picture (and the merge includes every passing shard's results) even
  on partial failure.
- The generated merge node is a node like any other and inherits the identical rule: an OOM during
  `npx playwright merge-reports`/`npx vitest --merge-reports` retries once at the next instance
  size up; a non-OOM crash (bad input, corrupt blob) is terminal immediately. Either way, a failed
  merge node marks the run `merge_failed`, distinct from `failed` (tests themselves failed), so
  operators can tell test failures apart from merge-infrastructure failures.

## Data model

### D1 tables

```sql
CREATE TABLE shard_plan (
  run_id        INTEGER NOT NULL,
  job_name      TEXT NOT NULL,
  idx           INTEGER NOT NULL,   -- 0-based
  total         INTEGER NOT NULL,
  items         TEXT NOT NULL,      -- JSON array of file paths or "file::test" ids
  estimated_ms  INTEGER,
  PRIMARY KEY (run_id, job_name, idx)
);

CREATE TABLE shard_state (
  run_id       INTEGER NOT NULL,
  job_name     TEXT NOT NULL,
  idx          INTEGER NOT NULL,
  attempt      INTEGER NOT NULL DEFAULT 1, -- 1, or 2 after the single automatic OOM retry
  status       TEXT NOT NULL,       -- queued | running | passed | failed | retrying
  report_key   TEXT,
  duration_ms  INTEGER,
  finished_at  INTEGER,
  PRIMARY KEY (run_id, job_name, idx)
);
```

The historical duration source for `timing` splits and `auto` shard-count resolution is
`test_stats`, owned and schema-defined by [./analytics.md](./analytics.md) (one row per
`(repo_id, test_id)`, updated in place once per finalized default-branch run — see that doc's D1
rollup tables for the schema and the upsert pattern). This doc only reads
`test_stats.duration_ewma_ms`; it never defines or writes that table directly — neither does the
merge step: `RunCoordinator` applies each run's canonical reports to `test_stats` once, at run
finalization, via `cloud-ci-reports` ([./analytics.md](./analytics.md)).

`shard_plan`/`shard_state` are mirrored (not duplicated — same tables, DO-local view) inside
`RunCoordinator`'s own SQLite storage for the lifetime of the run; the D1 copy is the durable
record the dashboard and `cloud-ci split`'s historical lookups read after the run completes.

### R2 keys

```
runs/{run_id}/jobs/{job_name}/shards/{shard_index}/reports/{report_kind}/{name}  # per-shard report, native or blob (publication alias, see ./byo-ci.md)
runs/{run_id}/jobs/{job_name}/shards/{idx}/log.txt
runs/{run_id}/jobs/{job_name}/merged/report.{ext}               # merged native report (written by the Worker inline merge)
```

## Security considerations

- Each shard container is minted a short-lived, per-node token (machine auth type 3,
  [./auth.md](./auth.md)) scoped to write only under
  `runs/{run_id}/jobs/{job_name}/shards/{idx}/...` — one shard cannot overwrite another shard's
  report or another run's data.
- The generated merge node's token is scoped to **read** every `shards/*/` prefix under its own
  `job_name` and to upload only as its own `<id>/merge` node (its site artifact lands at the
  [./assets.md](./assets.md) site key); it has no access to other jobs or other runs.
- `cloud-ci split`'s read of `test_stats` from BYO CI is scoped by `repo_id` derived from the
  caller's credential (GitHub Actions OIDC repo claim, or the scoped API token's bound repo) — a
  token for repo A can never read repo B's timing history, consistent with the
  single-tenant-per-deployment model but still enforced per-repo since one deployment serves many
  repos.
- The merge node runs arbitrary framework code (`npx playwright merge-reports`,
  `npx vitest --merge-reports`) inside a Container with the same isolation as any other node — it
  is not given elevated privileges relative to a normal node despite being system-generated.

## Failure modes

| Failure | Handling |
| --- | --- |
| All shards fail | Merge runs only if `merge_on_failure: always`; default `if_any_passed` skips merge entirely (nothing to merge); run marked `failed`. |
| One shard OOMs | Retried once on the next-larger size in the runner's executor capability descriptor (same shard index/assignment), per [./analytics.md](./analytics.md)'s rightsizing retry. |
| One shard fails for a non-OOM reason (test failure, nonzero exit, timeout) | Terminal immediately — no automatic retry; counted toward the merge barrier as-is. |
| Merge node OOMs | Retried once on the next-larger size, identical to any other node's OOM retry. |
| Merge node crashes for a non-OOM reason (bad/corrupt blob input, framework CLI error) | Terminal immediately; run marked `merge_failed` (distinct from `failed`). |
| A shard's blob file missing/corrupt in R2 at merge time | Merge node's download step fails fast with an explicit "shard N report missing" error rather than producing a silently incomplete merged report; surfaces as `merge_failed`. |
| Two BYO CI matrix legs' `cloud-ci split` calls race with a default-branch run's `test_stats` upsert landing between calls | Low risk: each read is a single-row point lookup per test, not a window scan, so a leg sees either the pre- or post-upsert value, never a partial aggregate; documented as a known limitation rather than solved with distributed locking. |
| Repo too new for history (`timing` with zero historical rows) | Falls back to `file` round-robin for that run only. |
| An auto-sized `count` (`{ min, max, target }`) would resolve above `max`, or exceed account container concurrency | Clamped to `max`; `RunCoordinator` drip-feeds shard dispatch through `RepoState`'s existing concurrency limiter if the account-wide concurrent-container cap is hit [unverified exact Cloudflare Containers per-account concurrency ceiling]. |

## Open questions

- Exact Cloudflare Containers concurrent-instance ceiling per account/Durable Object, which in
  practice bounds how large `max` is useful to set [unverified].
- Whether every report parser (coverage especially) can expose a stable per-test identity for
  `test_id`, or whether coverage-only jobs are permanently limited to file-granularity `timing`
  splits.
- Should `--granularity test` be the default for `timing` when the underlying report type supports
  it, trading finer balance for a longer item list (and a longer `shard_plan.items` JSON blob)?
- Whether the generated merge node should default to also requesting a machine-readable reporter
  (not just the `reports[].merge` display reporter) so blob-type shard groups populate
  `test_stats` without the user needing to manually add a second reporter to `merge:` — currently
  left to the user per Merge strategies above.

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| Always require users to hand-pick a fixed shard count | Loses the main benefit of historical timing; `count: { min, max, target }` (auto-sizing within bounds) adapts as a suite grows without a script edit. |
| Run `npx playwright merge-reports` inside the Worker itself | `workers-rs`/wasm32 has no Node.js/npm runtime available; merging must happen in a Container. |
| Merge every report type (including junit/coverage) through a generated container node for consistency | Wasteful: junit/coverage are small structured text files, cheap to parse inline in a Queue consumer; spinning up a container for every node purely to concatenate XML adds latency and cost for no benefit. |
| Dynamic rebalancing: on a failed shard, redistribute its remaining items across the other shards on retry | Rejected for determinism and debuggability — a shard's assignment must be reproducible and stable so a retry's logs/report map 1:1 back to the original plan; also doesn't fit a single-container "shard" model where "remaining items" isn't well defined once a test run has partially executed. |
| Let `cloud-ci split` require network access and server state for every strategy | `file` and `count` strategies work with zero network calls (pure deterministic math over the matched file list), which matters as a fallback when the deployment is unreachable or for first-ever runs with no history. |
| Parse the Playwright/Vitest blob zip format directly instead of shelling out to the framework's merge CLI | Rejected: the blob format is an internal, unversioned implementation detail of each framework (not a documented stable schema), so parsing it ourselves would break silently across framework version bumps; running the framework's own `merge-reports`/`--merge-reports` command is the only forward-compatible option. |
| Keep per-run per-test rows in D1 (the original `test_timings` design) | Rejected: one D1 row per test case per run does not scale — see [./analytics.md](./analytics.md)'s storage-growth rationale. A rolling aggregate updated in place, plus full parsed results in R2, serves both splitting and analytics without the row-count growth. |

## Sequence diagram

```mermaid
sequenceDiagram
    participant GH as GitHub (webhook/App)
    participant Worker as cloud-ci-worker
    participant RC as RunCoordinator (DO)
    participant S1 as Shard 1 (Container)
    participant S2 as Shard 2 (Container)
    participant MJ as Merge node (Container)
    participant R2 as R2
    participant D1 as D1

    GH->>Worker: push / pull_request event
    Worker->>RC: create run, resolve DAG
    RC->>D1: read test_stats aggregates for matched items
    RC->>RC: compute shard_plan (LPT bin-pack or fallback)
    RC->>D1: persist shard_plan, shard_state (queued)
    par Shard 1
        RC->>S1: dispatch (shard=1, run({ shard, shards, files }), token scoped to shards/1/*)
        S1->>R2: upload shards/1/report (blob or native)
        S1->>Worker: ingest terminal status
        Worker->>RC: shard 1 terminal
    and Shard 2
        RC->>S2: dispatch (shard=2, run({ shard, shards, files }), token scoped to shards/2/*)
        S2->>R2: upload shards/2/report
        S2->>Worker: ingest terminal status
        Worker->>RC: shard 2 terminal
    end
    RC->>RC: barrier satisfied (count(terminal) == expected_total)
    alt report type is native (junit/coverage)
        RC->>Worker: enqueue post-run-analysis (Queue)
        Worker->>R2: read shards/*/reports/*, merge inline
        Worker->>R2: write merged/report
    else report type is blob (playwright/vitest)
        RC->>MJ: dispatch generated merge node
        MJ->>R2: download shards/*/reports/{blob kind}/*
        MJ->>MJ: npx playwright merge-reports / vitest --merge-reports
        MJ->>Worker: cloud-ci upload --site (site artifact; + json upload only if reports[].merge includes json)
        Worker->>R2: write runs/{run_id}/artifacts/{name}/site.tar + site.index.json
    end
    RC->>RC: run terminal → finalization
    RC->>D1: apply canonical reports to test_stats/test_failures once per run (see ./analytics.md)
    RC->>Worker: shard group + merge complete
    Worker->>GH: update Check Run / PR comment (see ./pr-comment.md)
```

## Related docs

- [../architecture.md](../architecture.md) — RunCoordinator/RepoState responsibilities, DAG model.
- [./dynamic-pipelines.md](./dynamic-pipelines.md) — `ci.container`/`ci.check` execution model that
  `ci.shard` builds on.
- [./settings.md](./settings.md) — `runners.*` defaults this doc's `runner` option falls back to.
- [./byo-ci.md](./byo-ci.md) — ingest API, `cloud-ci upload`, OIDC credential exchange used by
  `cloud-ci split`.
- [./analytics.md](./analytics.md) — `test_stats` rolling per-test aggregate (duration +
  flakiness) this doc reads for timing splits, applied once per run at `RunCoordinator`
  finalization (not by the merge step); rightsizing retry-one-size-up reused for OOM'd shards.
- [./pr-comment.md](./pr-comment.md) — how shard group / merge status surfaces in the sticky PR
  comment.
- [./auth.md](./auth.md) — token scoping model referenced in Security considerations.
- [./assets.md](./assets.md) — merged HTML site artifact hosting.
- [ADR 0010](../adr/0010-pluggable-executors.md) — executor capability descriptor (sizes) that OOM
  retry and `cloud-ci-core`'s split library target are scoped by.
