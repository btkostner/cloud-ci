# Parallelization

Status: Proposed

## Summary

A `parallel:` block on a pipeline `Job` splits its work into N `Shard`s that run as independent containers, then merges their `Report`s back into one result. Shard count can be fixed or `auto` (sized to hit a target wall time). The split can use historical per-test/per-file timing pulled from D1, a flat file-count round-robin, or a flat test-count round-robin. The same splitting logic is exposed as `cloud-ci split` so bring-your-own-CI (BYO CI) pipelines (e.g. a GitHub Actions matrix) get identical, deterministic shard assignment without running inside cloud-ci's own containers. `RunCoordinator` (the per-run Durable Object, see ../architecture.md) tracks shard completion and acts as the merge barrier; once every shard in a shard group reaches a terminal state, it triggers the merge step appropriate to the report type.

## Goals

- Deterministic shard assignment: given the same file list, timing snapshot, strategy, and shard count, the computed assignment is identical every time, on both the worker (managed runs) and the CLI (BYO CI runs).
- `auto` shard count that targets a configured wall-clock duration instead of a fixed number of machines.
- Timing-aware splitting using real historical data, with a safe fallback when no history exists (new repo, new test files).
- Native (no-container) merging for structured text reports (JUnit XML, lcov/cobertura coverage); container-based merging for opaque framework blob formats (Playwright blob, Vitest blob) via the framework's own merge CLI.
- `cloud-ci split` works identically whether invoked by the cloud-ci agent inside a managed container or by a user's GitHub Actions matrix job.
- Clear, bounded retry semantics for a single failed shard that never perturb the other shards' assignments.

## Non-goals

- Dynamic work-stealing or mid-run rebalancing between shards (shard assignment is frozen at dispatch time).
- Splitting across repos/pipelines (a shard group is scoped to one Job in one Run).
- Choosing the test framework's own parallelism inside a shard (e.g. Playwright's `fullyParallel` worker count) — that is the framework's concern, orthogonal to cloud-ci's shard-level splitting.
- Analytics on top of timing data (regressions, flaky-test detection, critical path) — see ./analytics.md, which builds on the `test_timings` table this doc defines.

## User experience

### Pipeline config

```yaml
# .cloud-ci/pipeline.yml
jobs:
  test:
    runner: standard-2
    parallel:
      shards: auto                   # integer 1-64, or auto
      split: timing                  # timing | file | count (default: timing)
      files: "tests/**/*.spec.ts"
      min: 2                         # auto bounds, default 2
      max: 16                        # auto bounds, default 16
      target: 8m                     # auto only: pick a shard count so each shard runs ~target
      fail_fast: false               # cancel remaining shards on first failure
      merge:
        runner: basic                # instance type for the generated test/merge job; default basic
    steps:
      - run: npm ci
      - run: npx playwright test --shard=$CLOUD_CI_SHARD_INDEX/$CLOUD_CI_SHARD_TOTAL --reporter=blob
    reports:
      - { type: playwright-blob, path: blob-report/, merge: html }
```

Full field table, validation rules, and defaults for `parallel`, `reports`, and the generated `<job>/merge` job are owned by [./pipeline-config.md#parallel](./pipeline-config.md#parallel); this doc covers the algorithms and runtime behavior behind those fields.

Every shard container receives `CLOUD_CI_SHARD_INDEX` (1-based) and `CLOUD_CI_SHARD_TOTAL` as environment variables, in addition to the usual job env. For frameworks with native sharding (Playwright, Vitest), that's all a shard needs — `split: file` with native `--shard=$INDEX/$TOTAL` is equivalent to cloud-ci computing a round-robin file assignment, so no call to `cloud-ci split` is required inside a managed job; the worker only uses the computed assignment to decide shard *count*, not to hand the framework a file list.

`split: timing` and `split: count` are useful for frameworks without native sharding (`go test`, `cargo test`, `mocha`) or when a framework's native sharding doesn't account for historical duration. In that case the step calls `cloud-ci split` to get an explicit list:

```yaml
steps:
  - run: npm ci
  - run: cargo nextest run $(cloud-ci split --strategy timing --granularity file)
```

### `cloud-ci split` (also usable from BYO CI)

```
cloud-ci split --strategy timing|file|count \
  --shards <N> --index <1..N> \
  --files <glob> [--granularity file|test] \
  [--since 14d] [--branch main] \
  [--token <scoped-token> | relies on GitHub Actions OIDC]
```

Prints a newline-separated list (files, or `file::test-name` pairs with `--granularity test`) to stdout for the given `--index`. It is the same binary and the same split algorithm (`cloud_ci_cli::split`, a library target inside `cloud-ci-cli` that `cloud-ci-worker` also links against for managed runs) used in both places, so a BYO CI matrix and a cloud-ci-managed shard group produce byte-identical assignments for the same inputs.

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
      - run: cloud-ci upload --type playwright-blob --path blob-report --run-key ${{ github.run_id }}-${{ github.run_attempt }}
```

`cloud-ci split` authenticates with the same machine credentials as `cloud-ci upload` (GitHub Actions OIDC token when `ACTIONS_ID_TOKEN_REQUEST_URL` is present, or a scoped API token via `CLOUD_CI_TOKEN`) to read historical timing for the target repo — see ./byo-ci.md and ./auth.md for the credential exchange. With four matrix legs calling `cloud-ci split` independently and concurrently, each leg performs a read-only query against the same historical snapshot (bounded by `--since`), so all four legs resolve the same shard count's worth of data consistently as long as no new runs land in the few seconds between calls (see Failure modes).

GitHub Actions job matrices are capped at 256 jobs per workflow run (verified 2026-09-30, docs.github.com/en/actions/reference/limits), which is well above cloud-ci's own `max: 64` shard clamp.

## Design

### Shard count resolution

| `shards` value | Resolution |
| --- | --- |
| integer `N` (1-64) | Fixed; used as-is. |
| `auto` | `shard_count = clamp(ceil(historical_total_duration / target), min, max)`, where `historical_total_duration` sums the matched items' durations from `test_timings`; falls back to `min` when there is no history for any matched item (pipeline-config.md's rule — this doc supplies the per-item imputation detail below for the common case of *partial* history). |

### Split strategies

| Strategy | Item granularity | Data source | Algorithm |
| --- | --- | --- | --- |
| `timing` (default) | per-test if the report type exposes it (JUnit `testcase`, Playwright/Vitest JSON), else per-file | `test_timings` D1 table, last `--since` window (default 14d) on the target branch (default: repo default branch) | Longest-Processing-Time (LPT) greedy bin-packing: sort items by duration descending, tie-break by item name ascending; assign each item to the currently least-loaded shard, tie-break by lowest shard index. |
| `file` | whole file | none (or framework's native `--shard`) | Round-robin: files sorted by path ascending, assigned `index = position % shard_count`. |
| `count` | whole test (if discoverable) or whole file | none | Round-robin by position, same as `file` but at test granularity when the runner can enumerate individual tests without running them. |

LPT bin-packing is deterministic because both the sort key and the tie-break for "least-loaded shard" are total orders — no randomness, no wall-clock dependence, no reliance on map/hash iteration order.

### Fallback when no history exists

A brand-new repo, or a job whose `files` glob matches files never seen before, has no rows in `test_timings`. In that case:
- Items *with* history keep their real duration.
- Items *without* history are assigned the **median** duration of items that do have history (not zero) — this prevents one unmeasured, possibly-huge test file from being silently treated as free and stacked onto an already-full shard.
- If **no** item in the matched set has any history (first run ever for this `files` glob), `timing` silently degrades to `file` behavior (round-robin by path) for that run. The first successful run then populates `test_timings`, so the very next run already benefits from real timing.

### Deterministic assignment, end to end

1. The job's `files` glob is expanded against the checked-out worktree at dispatch time, sorted by path, and hashed into a run manifest (so retries reuse the exact same input list even if the repo state could theoretically differ by the time a retry fires).
2. For managed runs, `RunCoordinator` computes the assignment once (vocabulary: RunCoordinator owns "shard assignment" per ../architecture.md) when the job becomes runnable, and stores it as `shard_plan` rows before dispatching any shard container.
3. For BYO CI, each `cloud-ci split --index I` invocation recomputes the *same* plan independently (same algorithm, same D1 query bounded by `--since`), reading the item list from `--files` directly rather than a server-side manifest — there's no RunCoordinator in play for an `external` run.
4. A retried shard (OOM only — see Failed-shard retry semantics) always reuses the stored `shard_plan` entry for its index; a retry never re-runs the split algorithm and never happens for a non-OOM failure.

### Merge barrier (RunCoordinator)

`RunCoordinator`'s SQLite-backed state (one DO instance per run) holds, per shard group:

```
shard_state(job_name TEXT, idx INTEGER, attempt INTEGER, status TEXT, report_key TEXT, duration_ms INTEGER, finished_at INTEGER)
job_group(job_name TEXT PRIMARY KEY, expected_total INTEGER, fail_fast INTEGER, merge_on_failure TEXT, merge_job_id INTEGER)
```

Each shard's terminal ingest call (worker API, authenticated with the shard's per-job short-lived token) RPCs into `RunCoordinator`, which updates `shard_state` and checks:
- If `fail_fast = true` and a shard just transitioned to terminal `failed` (immediately for a non-OOM failure, or after its one automatic OOM retry also fails — see Failed-shard retry semantics), `RunCoordinator` immediately cancels remaining `running`/`queued` shards for that job (containers stopped via the same lifecycle path used for cancel-superseded in `RepoState`) and marks the shard group `failed`.
- Otherwise, when `count(terminal) == expected_total`, the barrier is satisfied and `RunCoordinator` decides whether to merge: `merge_on_failure` is `if_any_passed` (default), `always`, or `never`. `if_any_passed` runs the merge using only the successful shards' reports so the dashboard still shows partial results; the parent job's own status is still `failed` if any shard failed.
- On satisfaction, `RunCoordinator` enqueues the merge step via the `job-dispatch` Queue, same as any other job — merge dispatch still respects `RepoState`'s per-repo concurrency cap, so it may wait briefly behind other running containers.

### Merge strategies per report type

| Report type | Where merged | Default command |
| --- | --- | --- |
| `junit` | Worker / `post-run-analysis` Queue consumer (no container) | Parse each shard's JUnit XML from R2, concatenate `testsuite` elements into one `testsuites` document, write per-test rows into `test_timings`, write merged XML to R2. |
| `coverage` (lcov, cobertura) | Worker / `post-run-analysis` Queue consumer (no container) | Parse and sum per-line/per-branch hit counts across shards; write merged file to R2. |
| `playwright-blob` | Generated `<job>/merge` container job | `npx playwright merge-reports --reporter <merge> <dir>` (verified 2026-09-30, playwright.dev/docs/test-sharding) |
| `vitest-blob` | Generated `<job>/merge` container job | `npx vitest --merge-reports <dir>` (verified 2026-09-30, vitest.dev/guide/reporters; reads the default `.vitest/blob/` directory when `<dir>` is omitted) |

JUnit and coverage formats are plain structured text — cheap enough to merge inline in a Queue consumer, so no container is spun up for them. Playwright and Vitest blob reporters are opaque, framework-versioned binary/zip formats (Playwright: `report-<hash>-<shard>.zip`; Vitest: files under `.vitest/blob/`) that only the framework's own Node.js tooling can merge; `workers-rs`/wasm32 has no Node runtime, so these are merged by a **generated** `<job>/merge` job that runs in a Container exactly like a user-authored job. Its instance type, pre-merge steps, and command come from the triggering job's own `parallel.merge.{runner,setup,command}`; the reporter(s) passed to `--reporter` come from the triggering report's own `merge` field (`reports[].merge`, e.g. `merge: html`), kept separate from `parallel.merge`'s execution config, as pipeline-config.md specifies:

```yaml
# synthesized by RunCoordinator when a shard group's report type is playwright-blob;
# never written by a user into pipeline.yml.
jobs:
  test/merge:
    runner: basic                 # parallel.merge.runner, default basic
    kind: merge                   # internal marker
    needs: [test]                 # depends on every shard in the `test` group
    steps:
      - run: cloud-ci agent download-shards --job test --out ./blobs
      # parallel.merge.setup steps, if any, run here, before the merge command
      - run: npx playwright merge-reports --reporter html ./blobs   # --reporter value from reports[].merge
      - run: cloud-ci upload --type site --path playwright-report --job test/merge
```

By default this uploads only the merged `site` Artifact (the dashboard-facing HTML report) — matching the generated job's default behavior for every blob type. It does **not**, by itself, add rows to `test_timings`: a blob zip's internal format is an unversioned, undocumented Playwright/Vitest implementation detail that cloud-ci does not parse directly [unverified whether a stable public schema exists for either]. If a shard group's `timing` split should benefit from per-test history on its *next* run, `reports[].merge` must additionally request a machine-readable reporter (e.g. `merge: html,json` for Playwright, since `merge-reports --reporter` accepts a comma-separated list per playwright.dev/docs/test-reporters), and `parallel.merge.command` is overridden to also run `cloud-ci upload --type playwright-json --path merge-reports.json --job test/merge` — that upload goes through the same native ingest path as a non-sharded `playwright-json` report, so `test_timings` gets populated identically regardless of whether the original report type was native or blob. The Vitest merge job follows the same shape: blobs download into `.vitest/blob/`, `npx vitest --merge-reports` produces the configured reporters' output, and only an explicitly-configured JSON/JUnit reporter output is re-uploaded for history.

### Failed-shard retry semantics

- Shards get exactly the same automatic retry behavior as any other job (./analytics.md's rightsizing algorithm) — there is no `parallel`-specific retry count to configure. A shard whose container is OOM-killed is retried once, immediately, on the next-larger instance type, bypassing hysteresis; this is the *only* automatic retry path. A shard that fails for any other reason (non-zero exit, assertion failure, timeout) is terminal immediately — cloud-ci does not retry flaky test failures on its own.
- An OOM retry reuses the exact same `shard_plan` entry for that index (same file/test list) and the same `CLOUD_CI_SHARD_INDEX`/`CLOUD_CI_SHARD_TOTAL` — only the instance type changes. `shard_state.attempt` becomes `2` to record that it happened.
- If the retried shard OOMs again at the new size, it fails for good, reporting the configured `max` and measured peak, exactly as a non-sharded `runner: auto` job would (./analytics.md).
- A shard is "terminal" once it reaches a final `passed`/`failed` status, after at most one OOM retry. The merge barrier only counts terminal shards.
- `fail_fast: true` short-circuits the group as soon as one shard goes terminal-failed, canceling the rest; `fail_fast: false` (default) lets every shard run to its own terminal state so the dashboard reports the full picture (and the merge includes every passing shard's results) even on partial failure.
- The generated merge job is a job like any other and inherits the identical rule: an OOM during `npx playwright merge-reports`/`npx vitest --merge-reports` retries once at the next instance size up; a non-OOM crash (bad input, corrupt blob) is terminal immediately. Either way, a failed merge job marks the run `merge_failed`, distinct from `failed` (tests themselves failed), so operators can tell test failures apart from merge-infrastructure failures.

## Data model

### D1 tables

```sql
CREATE TABLE test_timings (
  repo_id      INTEGER NOT NULL,
  test_id      TEXT NOT NULL,      -- hex(sha256(file_path || 0x1f || full_test_name))[0:16]
  file_path    TEXT NOT NULL,
  test_name    TEXT NOT NULL,      -- nested suite titles joined with " > "
  duration_ms  INTEGER NOT NULL,
  status       TEXT NOT NULL,      -- pass | fail | skip
  run_id       INTEGER NOT NULL,
  sha          TEXT NOT NULL,
  branch       TEXT NOT NULL,
  started_at   INTEGER NOT NULL,
  PRIMARY KEY (repo_id, test_id, run_id)
);
CREATE INDEX idx_test_timings_lookup ON test_timings (repo_id, test_id, started_at);

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

`test_id`'s formula is shared verbatim with ./analytics.md, which layers daily rollups (`test_daily`) and `test_flakiness` on top of `test_timings` without redefining the key.

`shard_plan`/`shard_state` are mirrored (not duplicated — same tables, DO-local view) inside `RunCoordinator`'s own SQLite storage for the lifetime of the run; the D1 copy is the durable record the dashboard and `cloud-ci split`'s historical lookups read after the run completes.

### R2 keys

```
runs/{run_id}/jobs/{job_name}/shards/{idx}/report.{ext}        # per-shard native report (junit.xml, lcov.info, ...)
runs/{run_id}/jobs/{job_name}/shards/{idx}/blob/{file}          # per-shard blob reporter output (verbatim framework filenames)
runs/{run_id}/jobs/{job_name}/shards/{idx}/log.txt
runs/{run_id}/jobs/{job_name}/merged/report.{ext}               # merged native report
runs/{run_id}/jobs/{job_name}/merged/html/                      # merged HTML site artifact (served from assets host, see ./assets.md)
```

## Security considerations

- Each shard container is minted a short-lived, per-job token (machine auth type 3, ../design/auth.md) scoped to write only under `runs/{run_id}/jobs/{job_name}/shards/{idx}/...` — one shard cannot overwrite another shard's report or another run's data.
- The generated merge job's token is scoped to **read** every `shards/*/` prefix under its own `job_name` and **write** only under `merged/`; it has no access to other jobs or other runs.
- `cloud-ci split`'s read of `test_timings` from BYO CI is scoped by `repo_id` derived from the caller's credential (GitHub Actions OIDC repo claim, or the scoped API token's bound repo) — a token for repo A can never read repo B's timing history, consistent with the single-tenant-per-deployment model but still enforced per-repo since one deployment serves many repos.
- The merge job runs arbitrary framework code (`npx playwright merge-reports`, `npx vitest --merge-reports`) inside a Container with the same isolation as any other job container — it is not given elevated privileges relative to a normal job despite being system-generated.

## Failure modes

| Failure | Handling |
| --- | --- |
| All shards fail | Merge runs only if `merge_on_failure: always`; default `if_any_passed` skips merge entirely (nothing to merge); run marked `failed`. |
| One shard OOMs | Retried once on next-larger instance size (same shard index/assignment), per ./analytics.md's rightsizing retry. |
| One shard fails for a non-OOM reason (test failure, nonzero exit, timeout) | Terminal immediately — no automatic retry; counted toward the merge barrier as-is. |
| Merge job OOMs | Retried once on next-larger instance size, identical to any other job's OOM retry. |
| Merge job crashes for a non-OOM reason (bad/corrupt blob input, framework CLI error) | Terminal immediately; run marked `merge_failed` (distinct from `failed`). |
| A shard's blob file missing/corrupt in R2 at merge time | Merge job's download step fails fast with an explicit "shard N report missing" error rather than producing a silently incomplete merged report; surfaces as `merge_failed`. |
| Two BYO CI matrix legs' `cloud-ci split` calls race with a new run landing in `test_timings` between calls | Low risk in practice (`--since` windows are days wide, not seconds); documented as a known limitation rather than solved with distributed locking. |
| Repo too new for history (`timing` with zero historical rows) | Falls back to `file` round-robin for that run only. |
| `auto` shard count would exceed `max`/exceed account container concurrency | Clamped to `max`; `RunCoordinator` drip-feeds shard dispatch through `RepoState`'s existing concurrency limiter if the account-wide concurrent-container cap is hit [unverified exact Cloudflare Containers per-account concurrency ceiling]. |

## Open questions

- Exact Cloudflare Containers concurrent-instance ceiling per account/Durable Object, which in practice bounds how large `max` is useful to set [unverified].
- Whether `workers-rs` can enqueue container starts directly from the merge-dispatch path on the Queue consumer, or needs the TypeScript Durable Object shim noted as an open question in ../architecture.md — assumed to be the same dispatch code path as normal jobs; if the shim is required, merge-job dispatch inherits whatever that shim's constraints are.
- Whether every report parser (coverage especially) can expose a stable per-test identity for `test_id`, or whether coverage-only jobs are permanently limited to file-granularity `timing` splits.
- Should `--granularity test` be the default for `timing` when the underlying report type supports it, trading finer balance for a longer item list (and a longer `shard_plan.items` JSON blob)?
- Whether the generated merge job should default to also requesting a machine-readable reporter (not just the `reports[].merge` display reporter) so blob-type shard groups populate `test_timings` without the user needing to manually add a second reporter to `merge:` — currently left to the user per Merge strategies above.

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| Always require users to hand-pick a fixed shard count | Loses the main benefit of historical timing; `auto` with a target wall time adapts as a suite grows without a pipeline.yml edit. |
| Run `npx playwright merge-reports` inside the Worker itself | `workers-rs`/wasm32 has no Node.js/npm runtime available; merging must happen in a Container. |
| Merge every report type (including junit/coverage) through a generated container job for consistency | Wasteful: junit/coverage are small structured text files, cheap to parse inline in a Queue consumer; spinning up a container for every job purely to concatenate XML adds latency and cost for no benefit. |
| Dynamic rebalancing: on a failed shard, redistribute its remaining items across the other shards on retry | Rejected for determinism and debuggability — a shard's assignment must be reproducible and stable so a retry's logs/report map 1:1 back to the original plan; also doesn't fit a single-container "shard" model where "remaining items" isn't well defined once a test run has partially executed. |
| Let `cloud-ci split` require network access and server state for every strategy | `file` and `count` strategies work with zero network calls (pure deterministic math over the matched file list), which matters as a fallback when the deployment is unreachable or for first-ever runs with no history. |
| Parse the Playwright/Vitest blob zip format directly instead of shelling out to the framework's merge CLI | Rejected: the blob format is an internal, unversioned implementation detail of each framework (not a documented stable schema), so parsing it ourselves would break silently across framework version bumps; running the framework's own `merge-reports`/`--merge-reports` command is the only forward-compatible option. |

## Sequence diagram

```mermaid
sequenceDiagram
    participant GH as GitHub (webhook/App)
    participant Worker as cloud-ci-worker
    participant RC as RunCoordinator (DO)
    participant S1 as Shard 1 (Container)
    participant S2 as Shard 2 (Container)
    participant MJ as Merge job (Container)
    participant R2 as R2
    participant D1 as D1

    GH->>Worker: push / pull_request event
    Worker->>RC: create run, resolve DAG
    RC->>D1: query test_timings (--since window)
    RC->>RC: compute shard_plan (LPT bin-pack or fallback)
    RC->>D1: persist shard_plan, shard_state (queued)
    par Shard 1
        RC->>S1: dispatch (CLOUD_CI_SHARD_INDEX=1, token scoped to shards/1/*)
        S1->>R2: upload shards/1/report (blob or native)
        S1->>Worker: ingest terminal status
        Worker->>RC: shard 1 terminal
    and Shard 2
        RC->>S2: dispatch (CLOUD_CI_SHARD_INDEX=2, token scoped to shards/2/*)
        S2->>R2: upload shards/2/report
        S2->>Worker: ingest terminal status
        Worker->>RC: shard 2 terminal
    end
    RC->>RC: barrier satisfied (count(terminal) == expected_total)
    alt report type is native (junit/coverage)
        RC->>Worker: enqueue post-run-analysis (Queue)
        Worker->>R2: read shards/*/report, merge inline
        Worker->>D1: write merged rows into test_timings
        Worker->>R2: write merged/report
    else report type is blob (playwright/vitest)
        RC->>MJ: dispatch generated merge job
        MJ->>R2: download shards/*/blob/*
        MJ->>MJ: npx playwright merge-reports / vitest --merge-reports
        MJ->>Worker: cloud-ci upload (site artifact; + json upload only if reports[].merge includes json)
        Worker->>R2: write merged/html
        Worker->>D1: write merged rows into test_timings (only for the optional json upload)
    end
    RC->>Worker: shard group + merge complete
    Worker->>GH: update Check Run / PR comment (see ./pr-comment.md)
```

## Related docs

- ../architecture.md — RunCoordinator/RepoState responsibilities, DAG model.
- ./pipeline-config.md#parallel — authoritative `parallel`/`reports` field names, defaults, and validation rules.
- ./byo-ci.md — ingest API, `cloud-ci upload`, OIDC credential exchange used by `cloud-ci split`.
- ./analytics.md — `test_daily`/`test_flakiness` built on `test_timings`; rightsizing retry-one-size-up reused for OOM'd shards.
- ./pr-comment.md — how shard group / merge status surfaces in the sticky PR comment.
- ./auth.md — token scoping model referenced in Security considerations.
- ./assets.md — merged HTML site artifact hosting.
