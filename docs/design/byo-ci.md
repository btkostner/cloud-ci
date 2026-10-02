# Bring-your-own CI: ingest API, `cloud-ci upload`, external runs

Status: **Proposed**

## Summary

`cloud-ci` accepts CI results produced by *any* CI system — GitHub Actions, Buildkite, a
laptop — through a Connect RPC ingest API and the `cloud-ci upload` CLI. These are *external
runs*: a run whose jobs executed somewhere other than our own Cloudflare Containers, keyed by
`(repo, sha, run key, attempt)`. An external run still gets a `RunCoordinator` as its single
state writer; the coordinator just never schedules containers for it. Once a run's
first job lands, an external run looks identical to a managed run to every downstream
consumer: the PR comment, Check Runs, analytics, and (optionally) AI insights. This is the
same upload path our own `cloud-ci agent` uses inside containers ([ADR 0007](../adr/0007-one-upload-path.md)),
so it cannot silently regress.

Transport is not settled. "Connect RPC" here means the hand-routed Connect unary shape proposed
in [ADR 0002](../adr/0002-rust-cloudflare-worker.md), which depends on the Phase 0 "buffa on
wasm32" spike in the [roadmap](../roadmap.md). If that spike fails, the control plane keeps the
same method names and message shapes over plain JSON `POST`s; the data plane below is plain HTTP
either way.

This doc covers the ingest API shape, the CLI UX, run identity, auth, supported report
formats, upload mechanics and resumability, idempotency, completion semantics, and how external
runs feed [pr-comment](./pr-comment.md) and [analytics](./analytics.md). Token *format* and
scopes are owned by [auth](./auth.md); this doc only names the scope it needs. Artifact
*serving* (site hosting, caching, retention) is owned by [assets](./assets.md); this doc only
specifies what gets written to R2 and in what shape.

## Goals

- One Connect RPC contract that both `cloud-ci agent` (managed runs) and `cloud-ci upload`
  (external runs) call.
- Resumable upload of large artifacts and reports without requiring the uploader to hold R2 or
  AWS credentials.
- Zero-config auth from GitHub Actions via OIDC; token-based auth from anywhere else.
- No explicit "done" command. A run reaches a terminal state from the shard counts its uploads
  declare, the external CI's completion webhook, or a timeout.
- Parity: an external run contributes to the PR comment, Check Runs, and analytics the same as
  a managed run, minus the signals a managed run can only produce from our own container
  (resource samples, rightsizing).

## Non-goals

- **Live log streaming for external runs.** The CI system that ran the job already has a log
  viewer (the Actions run page, the Buildkite build page); `cloud-ci` does not duplicate it.
  Ingest accepts reports and artifacts, not step-by-step stdout. A `CompleteShard` call may
  attach an `external_url` that the PR comment and dashboard link out to instead.
- **Executing anything.** BYO CI never schedules work; it only records what already happened.
- **OIDC trust for non-GitHub providers at launch.** Buildkite, CircleCI, GitLab CI, etc. all
  have their own OIDC issuers; this doc defines the token-validation *shape* generically
  (§ Auth) but only wires up the GitHub Actions issuer in phase 1. Adding another issuer is
  config, not a new code path, because of that shape.
- **Fork-PR OIDC trust.** GitHub does not grant `id-token: write` to workflows triggered by
  `pull_request` from a fork — the `GITHUB_TOKEN` and every requested permission are
  downgraded to read-only, and OIDC has no read form, so no token is issued (verified
  2026-09-30, [GitHub Docs: events that trigger workflows](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows)).
  Fork contributions must use a scoped API token or `pull_request_target` (§ Security
  considerations); we do not work around GitHub's restriction.

## User experience

### When a pipeline script is and isn't involved

Managed runs are driven by a TypeScript pipeline script (`.cloud-ci/pipelines/*.ts`, see [dynamic-pipelines](./dynamic-pipelines.md)).
External runs have no such file — the uploading CI system owns its own job definitions. An
external run's job list is whatever sequence of `StartJob`/`CompleteShard` calls the uploader
makes; there is no DAG, no `runner:` instance sizing, and no merge-barrier scheduling. (Native
report merging and framework blob merges still apply — see § Supported report formats and
[parallelization](./parallelization.md).)

### `cloud-ci upload`

```
cloud-ci upload <kind> <path-or-glob>... [options]
cloud-ci upload [--report <kind>:<path-or-glob>]...
                [--artifact <name>=<path>]...
                [--site <name>=<dir>]... [options]
cloud-ci upload deployment --name <name> --preview-url <url> [--inspect-url <url>] [options]
cloud-ci upload deployment --name <name> --from <tool> <log-file|-> [options]

options:
  [--job <name>] [--shard <i>/<total>] [--partial]
  [--conclusion success|failure|cancelled]
  [--scope <name> | --scope-from manifest|turbo] [--turbo-task <task>]
  [--check <name>]...
  [--expect-jobs <n>] [--timeout <duration>]
  [--repo <owner>/<name>] [--sha <sha>] [--run-key <key>] [--attempt <n>]
  [--token <api-token> | (auto: GitHub Actions OIDC)]
```

`<kind>` is a report kind's CLI name from § Supported report formats (for example `junit`,
`playwright`, `oxlint`, `vite-build`). The positional form uploads one kind. `--report`,
`--artifact`, and `--site` are repeatable, so one invocation can upload several kinds.

Each invocation does these steps:

1. Opens the run if it does not exist yet (`BeginRun` is an upsert, see § Idempotency).
2. Starts or reuses the job named by `--job` (default: `$GITHUB_JOB` on GitHub Actions, else
   the report kind) and declares the job's shard total from `--shard <i>/<total>` (default
   `1/1`).
3. Uploads the files for shard `i`.
4. Marks shard `i` as uploaded (`CompleteShard`), unless `--partial` is set.

There is no `finalize` command. A job is complete when all `total` shards have uploaded, and a
run is complete when its jobs are complete and a completion signal arrives (§ Completion
semantics). Upload everything a shard produces in one invocation (repeat `--report` for several
kinds). If a shard must upload in several invocations, set `--partial` on every invocation
except the last.

`--conclusion` sets the shard's conclusion. If it is omitted, the Worker infers it from the
parsed reports: `failure` if a report has a failed test or an error-level diagnostic, else
`success`.

`--check <name>` (repeatable) attaches this job's result to one or more named Check Runs — the
same named-check model managed pipelines use via `ci.check` ([dynamic-pipelines](./dynamic-pipelines.md)).
An external run has no script to declare checks up front, so `StartJob` creates any check name it
hasn't seen yet for this run the first time a job names it (§ Checks and scopes). `--scope` and
`--scope-from` set the monorepo scope (package/app name) of each uploaded report, for grouping
in the PR comment and full report ([pr-comment](./pr-comment.md)); see § Globs and scopes.

Run-identity and auth flags are optional on GitHub Actions; the CLI auto-detects them from the
environment (§ Run identity). On any other CI system they are required, or sourced from
`CLOUD_CI_*` environment variables (`CLOUD_CI_REPO`, `CLOUD_CI_SHA`, `CLOUD_CI_RUN_KEY`,
`CLOUD_CI_ATTEMPT`, `CLOUD_CI_TOKEN`) so a shared CI template can set them once.

Example, outside GitHub Actions:

```
cloud-ci upload --job unit-tests \
  --report junit:'reports/*.xml' \
  --report lcov:coverage/lcov.info \
  --expect-jobs 1 \
  --repo acme/widgets --sha "$BUILDKITE_COMMIT" \
  --run-key "buildkite/$BUILDKITE_BUILD_ID" --attempt 1 \
  --token "$CLOUD_CI_TOKEN"
```

`--expect-jobs 1` tells the run how many jobs to wait for. Without it, and without a completion
webhook from the CI system, the run closes when its timeout passes (§ Completion semantics).

### Globs and scopes

Quote globs so that the CLI expands them, not the shell (`**` is supported). Shell-expanded file
lists also work. A glob that matches no files is an error: the CLI exits non-zero before it
calls the API. Each matched file becomes one report, and the CLI sets its scope as follows:

- `--scope <name>`: every file gets this scope.
- `--scope-from manifest` (default): the CLI walks up from the file's directory to the nearest
  package manifest (`package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`) and uses that
  package's name. A manifest at the repository root, or no manifest, gives the unscoped group.
- `--scope-from turbo`: the CLI runs `turbo run <task> --dry=json` (task from `--turbo-task`,
  default the `--job` name) and maps each file to the package whose directory contains it, with
  turbo's package name as the scope. The field names in turbo's dry-run JSON are `[unverified]`
  until implementation. Use this when turbo's package set differs from the manifests on disk.

`--site <name>=<dir-glob>` and `--artifact <name>=<path-glob>` follow the same rules. When the
glob matches more than one path, each match becomes its own site or artifact with its own scope,
named `<name>/<scope>`, so several packages can share one `--site` flag.

A `deployment` upload has no file; its scope comes from `--scope`, else from the nearest
manifest above the current working directory.

Example: a turbo monorepo runs Playwright in GitHub Actions for every app, then uploads all
results with one command:

```yaml
      - name: Run Playwright in every app
        run: npx turbo run playwright

      - name: Upload Playwright results
        if: always()
        run: cloud-ci upload playwright 'apps/*/test-results/*.json' --job playwright
```

Each app's Playwright config writes its JSON reporter output to
`apps/<app>/test-results/results.json`. The CLI expands the glob, and maps
`apps/web/test-results/results.json` to the scope `@acme/web` from `apps/web/package.json`
(and the same for every other app). To use turbo's package graph instead of the manifests on
disk, add `--scope-from turbo`; the task name defaults to the job name `playwright`. The job has
one shard (`1/1`), so this single command completes it. The PR comment then shows one
Playwright section per app.

### Deployments (previews)

A `deployment` report records an external preview: `preview_url` (the deployed preview) and
optional `inspect_url` (the third-party build page, for example a Chromatic build or a Vercel
deployment page). The PR comment shows deployments in its Previews section
([pr-comment](./pr-comment.md)).

Explicit form, which works with any tool:

```
cloud-ci upload deployment --job storybook --name storybook \
  --preview-url "$STORYBOOK_URL" \
  --inspect-url "$CHROMATIC_BUILD_URL"
```

Parser form, which reads the tool's output and extracts the URLs:

```
set -o pipefail
npx wrangler versions upload 2>&1 | tee wrangler.log
cloud-ci upload deployment --job docs --name docs --from wrangler wrangler.log
```

`--from <tool>` reads a log file, or stdin when the path is `-`. Explicit `--preview-url` and
`--inspect-url` override parsed values. If the parser finds no preview URL, the CLI exits
non-zero and uploads nothing. Which tools get a parser (candidates: `chromatic`, `vercel`,
`wrangler`) and their exact output formats are open questions `[unverified]`.

### GitHub Actions snippet

```yaml
name: CI
on: [push, pull_request]

permissions:
  contents: read
  id-token: write   # required for OIDC; no stored secret needed

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install cloud-ci CLI
        run: curl -fsSL https://<your-deployment>/install.sh | sh

      - name: Run tests
        run: npm test -- --reporter=junit --outputFile=reports/junit.xml

      - name: Upload results
        if: always()
        run: |
          cloud-ci upload --job test \
            --report junit:reports/junit.xml \
            --report lcov:coverage/lcov.info \
            --conclusion ${{ job.status }}
```

`${{ job.status }}` gives `success`, `failure`, or `cancelled` `[unverified]`. There is no final
step: the `test` job has one shard, so this upload completes it, and GitHub's `workflow_run`
webhook closes the run (§ Completion semantics).

A matrix or sharded suite calls `cloud-ci upload` once per leg with the same `--job` and its own
`--shard`:

```yaml
  e2e:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        shard: [1, 2, 3, 4]
    steps:
      - uses: actions/checkout@v4
      - run: npx playwright test --shard=${{ matrix.shard }}/4 --reporter=blob
      - if: always()
        run: cloud-ci upload playwright-blob 'blob-report/*.zip' --job e2e --shard ${{ matrix.shard }}/4
```

No job gated on `needs:` all legs is necessary. The `e2e` job is complete when shard 4 of 4 has
uploaded; a shard that never uploads is marked `missing` when the run closes. See
[parallelization](./parallelization.md) for the matrix and split pattern.

A single large runner can also run many Playwright suites at once. In this example a 32-core
self-hosted runner runs `turbo run e2e`, which starts the Playwright suites of 8 packages spread
across `apps/` and `packages/`. One upload sends all 8 results:

```yaml
  e2e:
    runs-on: [self-hosted, linux, x64, 32-core]
    steps:
      - uses: actions/checkout@v4

      - name: Install cloud-ci CLI
        run: curl -fsSL https://<your-deployment>/install.sh | sh

      - run: pnpm install --frozen-lockfile

      - name: Run every package's e2e suite
        run: npx turbo run e2e --concurrency=8 --continue

      - name: Upload e2e results
        if: always()
        run: |
          cloud-ci upload --job e2e \
            --report playwright:'apps/*/test-results/e2e.json' \
            --report playwright:'packages/*/test-results/e2e.json' \
            --site e2e-report='apps/*/playwright-report' \
            --site e2e-report='packages/*/playwright-report' \
            --check e2e \
            --conclusion ${{ job.status }}
```

Each package's `playwright.config.ts` sets its reporters to write a JSON file and an HTML report
into the package directory, and limits its own workers so the 8 suites share the 32 cores:

```ts
export default defineConfig({
  workers: 4,
  reporter: [
    ["json", { outputFile: "test-results/e2e.json" }],
    ["html", { outputFolder: "playwright-report", open: "never" }],
  ],
});
```

What happens:

- `--concurrency=8` with `workers: 4` gives about 32 Playwright workers at once. `--continue`
  makes turbo run every package's suite even after one fails, so the upload has all 8 results.
- The two `--report` globs match 8 JSON files. The CLI gives each file the scope of its nearest
  `package.json` (for example `apps/web/test-results/e2e.json` becomes `@acme/web`). A package
  whose suite did not run, because turbo had a cache hit, has no new file; see the note below.
- Each `--site` glob match becomes one hosted HTML report with the same scope as its JSON file,
  so the PR comment links to the right report for each package
  ([assets](./assets.md)). Sites from a glob are named `<name>/<scope>`, for example
  `e2e-report/@acme/web`.
- `--check e2e` posts one `e2e` Check Run for the job. The PR comment and the full report group
  the tests by package and expand only the packages that failed.
- The job has one shard (`1/1`), so this one command completes it. It runs once, on one machine,
  so it is not sharded even though 8 suites ran in parallel.

Turbo cache hits replay a task's logs but restore its outputs only if `test-results/**` and
`playwright-report/**` are listed in the task's `outputs` in `turbo.json`. List them, so a cache
hit still leaves a result file for the upload to find.

## Design

### Control plane and data plane are different transports

| Plane | Transport | Why |
| --- | --- | --- |
| Control (begin run, start job, create/complete upload, submit small reports, complete shard) | Connect RPC, `POST /cloud_ci.ingest.v1.IngestService/<Method>` | Typed, versioned by the proto contract every language binding shares; small request/response bodies the Worker can buffer and validate as JSON/protobuf. |
| Data (upload part bytes) | Plain `PUT /ingest/v1/uploads/{upload_id}/parts/{n}` | A Connect unary RPC would require base64- or proto-bytes-encoding the payload and buffering the whole message before the handler sees it. A raw `PUT` lets the Worker stream the request body straight into an R2 multipart `uploadPart` call without holding the full part in memory, and keeps the CLI's upload code a plain HTTP client instead of a Connect streaming-RPC client (client-streaming Connect support on `wasm32` is unproven — see [ADR 0002](../adr/0002-rust-cloudflare-worker.md) and the Phase 0 spikes in the [roadmap](../roadmap.md) for the broader pattern of not betting ingest-critical code on unverified wasm32 capability). |

### `IngestService` (sketch)

```protobuf
// cloud_ci.ingest.v1
service IngestService {
  rpc BeginRun(BeginRunRequest) returns (BeginRunResponse);
  rpc StartJob(StartJobRequest) returns (StartJobResponse);
  rpc CreateUpload(CreateUploadRequest) returns (CreateUploadResponse);
  rpc CompleteUpload(CompleteUploadRequest) returns (CompleteUploadResponse);
  rpc SubmitReport(SubmitReportRequest) returns (SubmitReportResponse);
  rpc CompleteShard(CompleteShardRequest) returns (CompleteShardResponse);
  rpc GetRun(GetRunRequest) returns (GetRunResponse);
}
```

| RPC | Key fields | Notes |
| --- | --- | --- |
| `BeginRun` | `repo`, `sha`, `run_key`, `attempt`, `trigger` (push/pull_request/manual), `external_url`, `expect_jobs` (optional), `timeout` (optional, default 30 minutes, clamped to the deployment-wide maximum) | Upsert on `(repo_id, sha, run_key, attempt)`. Returns `run_id` + current state. The first non-empty `expect_jobs` wins; a later different value is rejected (400). |
| `StartJob` | `run_id`, `job_name`, `shard_total` (default 1), `runner_label` (freeform, e.g. `ubuntu-latest`), `check_names` (repeated, optional) | Returns `job_id`. Idempotent on `(run_id, job_name)`. A `shard_total` different from the stored one is rejected (400). Unknown `check_names` are created on first use (§ Checks and scopes). |
| `CreateUpload` | `job_id`, `shard_index`, `kind` (report/artifact/site), `name`, `scope` (optional), `content_type`, `size_bytes`, `sha256` | Size ≤ 32 MiB → single-part, caller `PUT`s once. Larger → multipart; response includes `part_size_bytes` (fixed 32 MiB) and `part_count`. Dedupes on `(job_id, shard_index, kind, name, sha256)` — see § Idempotency. |
| `CompleteUpload` | `upload_id`, `parts: [{number, etag}]` | Calls R2's multipart complete; validates part count/sizes against what `CreateUpload` reserved. |
| `SubmitReport` | `job_id`, `shard_index`, `report_kind`, `name`, `scope` (optional), either `inline_data` (≤ 4 MiB) or `upload_id` | Parses the report via `cloud-ci-reports` into D1 per-report summaries, failed/flaky test rows, and rolling per-test aggregates — never one row per test case (§ Supported report formats). Raw bytes are always kept in R2 regardless of path taken. A `deployment` report is small inline JSON. |
| `CompleteShard` | `job_id`, `shard_index`, `conclusion` (optional, inferred from the shard's reports if omitted), `external_url` | Marks the shard uploaded. When all `shard_total` shards are uploaded, the job concludes with the worst shard conclusion. Triggers the same Check-Run-update and comment-debounce path as a managed job (§ How external runs feed...). |
| `GetRun` | `repo`, `sha`, `run_key`, `attempt` | Lets the CLI resume: look up `run_id` and which jobs/uploads already exist before re-sending anything. |

### Run identity

An external run is keyed by `(repo_id, sha, run_key, attempt)` — the same tuple
[architecture.md](../architecture.md#data-model) uses for every run. `repo_id` is GitHub's
numeric repository id, not `owner/name` (renames happen). The other three fields:

| Field | Meaning | GitHub Actions default | Elsewhere |
| --- | --- | --- | --- |
| `sha` | Commit under test | `GITHUB_EVENT_PATH`'s `pull_request.head.sha` on `pull_request` events (not `GITHUB_SHA`, which is the ephemeral merge commit `refs/pull/N/merge` — verified 2026-09-30, [GitHub Docs](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows)); otherwise `GITHUB_SHA` | Required flag / `CLOUD_CI_SHA` |
| `run_key` | Groups retries of "the same" run | `gha/$GITHUB_RUN_ID` | Required flag / `CLOUD_CI_RUN_KEY` |
| `attempt` | Distinguishes re-runs under one `run_key` | `$GITHUB_RUN_ATTEMPT` | Required flag / `CLOUD_CI_ATTEMPT`, defaults to `1` |

Re-running a GitHub Actions workflow increments `GITHUB_RUN_ATTEMPT` but keeps `GITHUB_RUN_ID`,
so attempts of the same `run_key` are distinct runs in `cloud-ci` (each gets its own row, its
own jobs, its own place in history) but the PR comment and analytics can still group them by
`run_key` when showing "retried" context. This mirrors how GitHub's own Checks UI treats
attempts.

### Auth

`BeginRun` is the only call that authenticates with something other than an ingest token,
because it's the one call made before a run-scoped token exists:

| Credential | Who | Validation |
| --- | --- | --- |
| GitHub Actions OIDC JWT | GitHub Actions jobs with `permissions: {id-token: write}` | `iss` = `https://token.actions.githubusercontent.com`; RS256 signature against the issuer's JWKS (cached, refetched on `kid` miss); `aud` = this deployment's exchange URL, requested explicitly via `actions/core`'s `getIDToken(audience)` (not the default owner-URL audience, so a token minted for `cloud-ci` can't be replayed at another OIDC relying party); `exp`/`nbf` checked; `repository_id` and `repository_owner_id` claims (not the `repository` name string — renames happen) matched against the GitHub App installation that owns the target repo. |
| Scoped API token | Any other CI system | Opaque token, hashed in D1, looked up and checked for the `ingest:write` scope and a repo allowlist containing the target `repo_id` — same token family as [auth](./auth.md)'s machine tokens. |

`claims_supported` for GitHub's OIDC issuer (verified 2026-09-30 against
`https://token.actions.githubusercontent.com/.well-known/openid-configuration`) includes
`repository_id`, `repository_owner_id`, `run_id`, `run_attempt`, `sha`, `ref`, `event_name`,
and `repository_visibility`, among others — enough to validate identity without an extra
GitHub API call. We deliberately validate on `repository_id`/`repository_owner_id` rather than
parsing the `sub` claim: GitHub is rolling out an "immutable" `sub` format
(`repo:OWNER@OWNER-ID/REPO@REPO-ID:ref:...`) for repositories created after 2026-07-15 while
older repositories keep the pre-existing format (verified 2026-09-30, [GitHub Docs: OIDC reference](https://docs.github.com/en/actions/reference/security/oidc#immutable-subject-claims));
matching on the dedicated ID claims sidesteps having to branch on which `sub` shape a given
repo uses.

Either path returns an **ingest token**: the same opaque HMAC-signed token format
`RunCoordinator`-minted job tokens use ([auth](./auth.md)), with `typ: "ingest"`, `scope:
["ingest:write"]`, and `repo_id`/`run_id` claims, 1-hour TTL. The CLI holds this token for the
rest of the run's calls and re-exchanges (re-runs `BeginRun` with a fresh OIDC JWT) if a job
outlives it — GitHub-issued OIDC JWTs are themselves short-lived [unverified exact TTL; not
documented as a fixed duration in GitHub's reference docs], so the exchange, not the original
JWT, is what the rest of the run relies on.

### Checks and scopes

External jobs attach to the same named-check and report-scope model managed pipelines use:

- **Checks.** `StartJob`'s optional `check_names` names the Check Run(s) this job's result rolls
  into — the same model scripts use via `ci.check(name, opts)` ([dynamic-pipelines](./dynamic-pipelines.md)).
  A script declares its checks before any work starts; an external run has no script, so the
  Worker creates any check name it hasn't seen yet for this run on the first `StartJob` that
  names it (`queued`, not required by default — the same semantics as a script-created check). A
  job with no `check_names` reports no check run and only appears in the PR comment and
  dashboard, exactly like a script node with `check: null`. Because a later job can still name
  the same check, an external check is sealed only when the run completes (§ Completion
  semantics); it stays `queued`/`in_progress` until then and concludes from all of its jobs.
  So it can never go green on one job and then flip when another job attaches.
- **Scopes.** Each report carries an optional `scope` (package/app name, matching the `scope` a
  script sets via turbo helpers) so the PR comment and full report group test/coverage/report
  links by scope the same way for external and managed runs ([pr-comment](./pr-comment.md)).
  Scope is per report, not per job, so one upload of a glob can cover many packages (§ Globs
  and scopes). Reports with no `scope` fall into the default (unscoped) group.

### Supported report formats

| Kind | CLI name | Format | Merge strategy |
| --- | --- | --- | --- |
| `JUNIT` | `junit` | JUnit XML | Native: Worker parses and unions test cases across shards |
| `VITEST_JSON` | `vitest` | Vitest's `--reporter=json` output | Native |
| `PLAYWRIGHT_JSON` | `playwright` | Playwright's `--reporter=json` output | Native |
| `LCOV` | `lcov` | lcov tracefile | Native: line-hit union |
| `COBERTURA` | `cobertura` | Cobertura XML | Native: line-hit union |
| `CLOUD_CI_TIMING` | `timing` | Our own `{test, file, duration_ms}` JSON, written by `cloud-ci agent`/`cloud-ci upload` after any of the above is parsed | Feeds [parallelization](./parallelization.md)'s `split: timing` |
| `BENCH` | `bench` | `{name, value, unit}` JSON | Native: latest-wins per name |
| `OXLINT_JSON` | `oxlint` | `oxlint --format json` output `[unverified exact schema]` | Native: union of diagnostics across shards and scopes; diagnostics with file/line become Check Run annotations |
| `OXFMT_JSON` | `oxfmt` | oxfmt JSON output listing unformatted files `[unverified that oxfmt has a JSON output]` | Native: union of unformatted files |
| `VITE_BUILD` | `vite-build` | JSON written by `@cloud-ci/vite-plugin`: build time and output file sizes | Native: latest-wins per (scope, output file); sizes compared with the latest default-branch report for the same scope |
| `DEPLOYMENT` | `deployment` | `{name, preview_url, inspect_url?, tool?}` JSON, built by the CLI from flags or a log parser | Native: latest-wins per (scope, name) |
| `PLAYWRIGHT_BLOB` | `playwright-blob` | Playwright's `--reporter=blob` shard output | Framework merge: `npx playwright merge-reports --reporter=html <dir>` run by a generated merge job (verified command 2026-09-30, [Playwright: Sharding](https://playwright.dev/docs/test-sharding)) |
| `VITEST_BLOB` | `vitest-blob` | Vitest's `--reporter=blob` shard output | Framework merge: `vitest --merge-reports` over the directory of blob files (verified 2026-09-30, [Vitest: CLI](https://vitest.dev/guide/cli); blob file location changed across Vitest versions — pin the version in the generated merge job) |

Native formats are parsed by `cloud-ci-reports` (`cloud-ci-worker` and `cloud-ci-cli` both
depend on it, so parsing is identical on both paths) into per-report summaries, failed/flaky
test rows, and rolling per-test aggregates in D1 — never one row per test case per run; the full
parsed result is stored as a compressed file in R2 keyed by run/report (retention:
[assets](./assets.md)). Blob formats are opaque to us; their merge step runs as an ordinary job
(managed, or externally if the uploader already has a merge step) and its output is submitted
the same way as any other report.

**Vite build report.** Vite has no native JSON stats output `[unverified]`, so a small plugin,
`@cloud-ci/vite-plugin`, writes `.cloud-ci/vite-build.json` under the Vite project root (not
under the build output directory, so it is never deployed). The file holds the Vite version, the
build time in milliseconds, and one entry per output file with its size in bytes and optional
gzip size. Upload it with `cloud-ci upload vite-build 'apps/*/.cloud-ci/vite-build.json'`; the
usual manifest rule gives each app its scope. The Rollup hooks the plugin uses for timing are
`[unverified]` until implementation.

**Deployments.** A `deployment` report has no tests and no conclusion of its own; it only feeds
the PR comment's Previews section and the dashboard (§ Deployments (previews)).

### Upload mechanics and resumability

Fixed part size: **32 MiB**. Chosen as a decisive tradeoff: comfortably under every Cloudflare
plan's Worker request body limit (Free/Pro 100 MB, Business 200 MB, Enterprise 500 MB,
verified 2026-09-30, [Workers limits](https://developers.cloudflare.com/workers/platform/limits/)),
comfortably clears R2's 5 MiB multipart part-size floor (verified 2026-09-30, [R2 limits](https://developers.cloudflare.com/r2/platform/limits/)),
and keeps the retry unit small — a network blip loses at most one part, not the whole
artifact. The Worker never buffers a full part in memory: the `PUT` body streams directly into
the R2 binding's `uploadPart` call, so the 32 MiB figure is about plan portability and retry
granularity, not the Worker's 128 MB isolate memory ceiling.

Resumability:

1. The CLI computes `sha256` of the local file before uploading (streaming hash, not loaded
   whole into memory for large files).
2. `CreateUpload(job_id, kind, name, sha256, size)` — if a *completed* upload already exists
   for that `(job_id, kind, name, sha256)`, the Worker returns it immediately with no new parts
   to send (§ Idempotency). If an *incomplete* multipart upload exists for the same key (CLI
   crashed mid-upload, network died), the Worker returns the existing `upload_id` and the set
   of part numbers already received — tracked in the upload's own D1 row as parts land, not by
   querying R2's `ListParts` [unverified whether the R2 Workers-API binding exposes `ListParts`;
   tracking receipt ourselves avoids depending on it].
3. The CLI `PUT`s only the missing parts, then calls `CompleteUpload`.
4. R2 aborts genuinely abandoned incomplete multipart uploads after 7 days by default (verified
   2026-09-30, [R2: Upload objects](https://developers.cloudflare.com/r2/objects/upload-objects/)) —
   but a run's own idle timeout (§ Completion semantics) fires first, so a cleanup sweep
   ([assets](./assets.md) retention) explicitly aborts incomplete uploads belonging to
   `abandoned` runs instead of waiting on R2's default.

### Idempotency

- **`BeginRun`** is an upsert keyed on `(repo_id, sha, run_key, attempt)`: calling it twice
  (a CI step retried wholesale) returns the same `run_id` and current state, never a conflict.
- **`StartJob`** is idempotent on `(run_id, job_name)`. Every shard of a job calls it with the
  same `shard_total`; a different total is rejected (400), so two legs cannot disagree on the
  shard count.
- **Uploads** are idempotent on `(job_id, shard_index, kind, name, sha256)` — identical bytes
  re-sent after a retry are a no-op. The shard index is part of the key because every shard of a
  job usually uploads files with the same name (for example `results.json`); without it, shard 2's
  report would look like a corrected copy of shard 1's. Different bytes under the same
  `(job_id, shard_index, kind, name)` (a shard re-ran part of its suite and produced a corrected
  report) are *not* overwritten in place: they land as a new row, and the most recently
  completed one is canonical for parsing/merging/dashboard reads, while older rows stay in R2
  for audit. This matches the invariant already stated in
  [architecture.md](../architecture.md#coordination-invariants): "every upload is idempotent on
  `(run, job/shard, report kind | artifact path, content hash)`" — the hash is part of the key,
  so distinct content is a distinct upload, not a conflict.
- **`CompleteShard`** is idempotent: calling it again with the same `conclusion` is a no-op;
  calling it with a *different* conclusion after the job is concluded is rejected (400), since a
  CI system shouldn't be able to flip a result after the fact without going through a new
  attempt. An upload for a shard already marked `missing`, or for a run that is already
  terminal, is also rejected (400).

### Completion semantics

There is no `finalize` command or RPC. An external run has no scheduler to tell
`RunCoordinator` when it is done, so completion comes from the shard counts the uploads declare,
the external CI's completion webhook, or a timeout.

**Job.** Every upload declares `--shard <i>/<total>` (default `1/1`). A job is complete when each
shard `1..total` has a `CompleteShard`. Its conclusion is the worst shard conclusion. A job with
one shard is complete after its single upload.

**Run.** A run closes on the first of these signals:

```mermaid
flowchart TD
    R[running] -->|"workflow_run.completed webhook<br/>matches GITHUB_RUN_ID"| C1[closed: succeeded / failed]
    R -->|"--expect-jobs N: N jobs complete"| C2[closed: succeeded / failed]
    R -->|"timeout, all jobs complete"| C3[closed: succeeded / failed]
    R -->|"timeout, shards missing"| A[abandoned]
```

1. **Completion webhook.** If `BeginRun` was called with `external_url` pointing at a GitHub
   Actions run (or the CLI auto-populated `GITHUB_RUN_ID`), the Worker correlates it against
   incoming `workflow_run` `completed` deliveries — which fire "regardless of whether the
   workflow was successful or unsuccessful" (verified 2026-09-30,
   [GitHub: Webhook events and payloads § workflow_run](https://docs.github.com/en/webhooks/webhook-events-and-payloads#workflow_run);
   requires the GitHub App to hold at least read-level `Actions` permission, same source). This
   is the default path on GitHub Actions and needs no extra step. Equivalent webhooks from other
   CI systems are an open question.
2. **`--expect-jobs N`** on `BeginRun`. Once `N` jobs are complete, the run closes. Useful for CI
   systems with no completion webhook and a fixed, known job count. This is also the way to
   close a run without shards promptly outside GitHub Actions.
3. **Timeout.** `RunCoordinator` sets a DO alarm for `--timeout` (default 30 minutes, clamped to
   the deployment-wide maximum) after the most recent ingest call for the run. If every job is
   complete when it fires, the run closes normally. If any job still has shards that did not
   upload, those shards are marked `missing` and the run moves to `abandoned` — one of
   [architecture.md](../architecture.md#run-states)'s terminal states.

When the run closes by webhook or `--expect-jobs` while a job still has shards that did not
upload, those shards are marked `missing` at once, without waiting for the timeout. A job with a
missing shard concludes `failure` with the summary "N of total shards missing". On close, the
run's conclusion is the worst job conclusion, and every external check is sealed and concludes
from its jobs (§ Checks and scopes). A later ingest call for the same `run_key`/`attempt` after
the run is terminal is rejected (400); the CI system must retry under a new `attempt`.

### How external runs feed PR comment & analytics

**PR comment / Check Runs.** `CompleteShard` and run completion enqueue the same debounced
comment-refresh message `RunCoordinator` emits for a managed job completing
([architecture.md](../architecture.md#core-flows) step 5) — the comment template doesn't
branch on `run.kind`. The one external-specific addition is a small "via GitHub Actions" (or
whatever `external_url`'s host implies) badge linking out, since there's no `cloud-ci`-native
log to link to instead. `deployment` reports fill the template context's `deployments` list,
which the default template renders as a Previews section. See [pr-comment](./pr-comment.md).

**Analytics.** Duration, critical-path, flaky-test, and slowest-test analysis all read from
report-supplied timestamps and per-test durations (`StartJob`/`CompleteShard` times, parsed
`CLOUD_CI_TIMING`/JUnit/Vitest/Playwright durations) — external runs supply all of that, so
they participate fully. Resource-sample analytics (cgroup CPU/memory, cost estimate, `runner:
auto` rightsizing) do not apply: there's no container we control to sample or size.
Analytics Engine rows for external runs are written with `kind=external` and omit the
`resource_*` fields entirely rather than zero-filling them, so rollups can exclude external
runs from averages that would otherwise be skewed. See [analytics](./analytics.md).

## Data model

D1 (extends [architecture.md](../architecture.md#data-model)'s run/job/report tables with
ingest-specific bookkeeping):

| Table | Key columns | Notes |
| --- | --- | --- |
| `runs` | `id`, `repo_id`, `sha`, `run_key`, `attempt`, `kind` (`managed`\|`external`), `external_url`, `state`, `expect_jobs`, `timeout_s` | `UNIQUE(repo_id, sha, run_key, attempt)` backs the `BeginRun` upsert. |
| `jobs` | `id`, `run_id`, `name`, `shard_total`, `runner_label`, `check_names` (JSON array, references the shared `checks` table), `state`, `conclusion` | `UNIQUE(run_id, name)`. |
| `job_shards` | `job_id`, `shard_index`, `state` (`pending`\|`uploaded`\|`missing`), `conclusion`, `external_url`, `completed_at` | `PRIMARY KEY(job_id, shard_index)`; a job is complete when `shard_total` rows are `uploaded`. |
| `uploads` | `id`, `job_id`, `shard_index`, `kind`, `name`, `scope`, `sha256`, `size_bytes`, `state` (`pending`\|`complete`), `received_parts` (bitset/count), `r2_key` | `UNIQUE(job_id, shard_index, kind, name, sha256)` backs upload dedupe; `received_parts` lets `CreateUpload` answer "which parts do you have" without R2 `ListParts`. |
| `reports` | `id`, `job_id`, `shard_index`, `kind`, `name`, `scope`, `upload_id`, `created_at`, `is_canonical` | Newest row per `(job_id, shard_index, kind, name)` flips `is_canonical`; parsed summary and failed/flaky-test rows reference the report row, not the raw upload — the full per-run parsed result is a separate R2 object (§ Supported report formats). |

R2 keys:

| Content | Key |
| --- | --- |
| Report raw bytes | `runs/{run_id}/jobs/{job_name}/shards/{shard_index}/reports/{report_kind}/{name}` |
| Plain artifact (file or non-browsable dir, tarred) | `runs/{run_id}/artifacts/{name}` |
| Site artifact (browsable HTML dir) | `runs/{run_id}/artifacts/{name}/site.tar` + `runs/{run_id}/artifacts/{name}/site.index.json` — uncompressed tar, index maps relative path → `{offset, len, content_type, content_encoding?}` with `offset`/`len` pointing at exact file content (past the file's tar header, before its padding), so [assets](./assets.md) can serve any path as an R2 ranged read with no server-side expansion |

Upload key layout matches [assets](./assets.md) exactly (coordinated during design); this doc
only specifies what `cloud-ci upload` writes, not how it's served.

## Security considerations

- **Token scope.** Ingest tokens (OIDC-exchanged or API-token-derived) carry `scope: ["ingest:write"]`
  and a `repo_id` claim; the Worker rejects any call whose target `repo_id` doesn't match.
  Neither credential can read, cancel, or modify anything outside ingest for that one repo.
- **Audience pinning.** The CLI requests a deployment-specific `aud` for its OIDC JWT rather
  than accepting GitHub's default (the repository owner's URL), so a JWT minted for one
  `cloud-ci` deployment cannot be replayed against a different Connect RPC audience or a
  different cloud provider's OIDC relying party.
- **Identity by ID, not name.** `repository_id`/`repository_owner_id` claims are checked, not
  the `repository` string — a repo rename or transfer can't retroactively authorize uploads
  that were scoped to the old name.
- **Fork PRs get no OIDC.** GitHub does not issue `id-token: write` tokens to workflows
  triggered by `pull_request` from a fork (§ Non-goals). Projects that need external-CI results
  from fork contributions must either issue those contributors nothing (status quo: fork PRs
  simply don't get BYO-CI data, only whatever the base-repo's own managed run produces) or
  adopt `pull_request_target` with an explicit, reviewed checkout of the PR head — a tradeoff
  inherent to `pull_request_target` generally, not specific to `cloud-ci`, and one we do not
  paper over.
- **Upload parts are keyed by an unguessable `upload_id` (ULID)** plus the bearer ingest token;
  a part PUT without a valid token for the owning run's `repo_id` is rejected before touching
  R2.
- **Part size cap enforced before touching R2.** The Worker rejects any part whose
  `Content-Length` exceeds 32 MiB at the HTTP layer, so an oversized or hostile upload never
  reaches the R2 binding.
- **No secrets stored for GitHub Actions callers.** OIDC means no long-lived credential exists
  for the common case; API tokens (non-GitHub CI) are hashed in D1 like every other machine
  token ([auth](./auth.md)).

## Failure modes

| Failure | Behavior |
| --- | --- |
| CLI crashes mid multipart upload | Next invocation recomputes `sha256`, calls `CreateUpload` again, gets the same `upload_id` back with already-received part numbers, uploads only what's missing. |
| Network drop mid-part | CLI retries that one `PUT`; re-uploading a part number before `CompleteUpload` simply overwrites it (standard multipart semantics). |
| A shard never uploads (leg crashed before `cloud-ci upload`, or was cancelled) | The completion webhook or `--expect-jobs` marks it `missing` when the run closes; otherwise the timeout marks it `missing` and the run moves to `abandoned`. The job concludes `failure` ("N of total shards missing"). |
| Two legs of one job declare different totals (`--shard 1/4` and `--shard 2/3`) | `StartJob` rejects the second call (400); the first total wins. |
| Shard uploads after it was marked `missing`, or after the run is terminal | Rejected (400); the CI system retries under a new `attempt`. |
| `workflow_run` webhook arrives for a run already closed by `--expect-jobs` | No-op; the run is already terminal. |
| Upload glob matches no files | CLI exits non-zero before any RPC; nothing is uploaded. |
| Deployment log parser finds no preview URL | CLI exits non-zero; nothing is uploaded. |
| Two jobs in the same run race to `BeginRun` first | Both get the same `run_id` from the upsert; no duplicate run rows. |
| Caller's OIDC JWT `aud` doesn't match deployment | `BeginRun` rejects with 401 before any run is created. |
| Report bytes fail to parse (malformed JUnit XML, etc.) | Raw bytes are still stored in R2 and `reports.is_canonical` is set, but no test-case rows are written; the PR comment shows "report attached, unparsed" rather than silently dropping it. |
| Same `(job_id, kind, name)` uploaded with different content twice in one job (flaky re-run) | Both rows kept; newest is canonical (§ Idempotency) — not a merge, a replacement for display purposes. |

## Open questions

- Exact TTL of GitHub's issued OIDC JWTs is not pinned down in GitHub's reference docs
  [unverified] — affects only how aggressively the CLI needs to re-exchange for very long jobs
  (already handled by re-running `BeginRun`; this just affects how often).
- Whether the R2 Workers-API binding exposes `ListParts` for a multipart upload [unverified] —
  if it does, `uploads.received_parts` bookkeeping in D1 becomes a cross-check rather than the
  source of truth.
- Whether to support provider-specific OIDC issuers (GitLab CI, CircleCI) in a later phase, and
  whether that's config (issuer URL + claim-name mapping) or requires per-provider code beyond
  the generic shape in § Auth.
- Whether `--expect-jobs` should be inferable from a GitHub Actions matrix automatically (e.g.
  via the `workflow_run` payload's job count) instead of requiring the flag.
- Which non-GitHub CI systems have a run-completion webhook we can correlate (for example a
  Buildkite build-finished event `[unverified]`), so they can close runs without `--expect-jobs`.
- Which tools get a deployment log parser for `--from` (candidates: `chromatic`, `vercel`,
  `wrangler versions upload`), and the exact output format each one prints `[unverified]`.
- Whether oxfmt has a JSON output `[unverified]`; if not, the `oxfmt` kind parses its
  `--check` text output instead.
- The exact JSON fields of `turbo run --dry=json` that `--scope-from turbo` reads
  `[unverified]`.

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| Presigned R2 (S3-compatible) URLs, client uploads directly to R2 | Requires minting scoped S3-style credentials server-side (R2's S3 API uses account-wide access keys, so "scoped" means a Cloudflare API call per upload to generate one) for no benefit over proxying: the Worker still has to authenticate every call and track idempotency/dedupe state either way. It would also make `cloud-ci upload` an AWS-SigV4 client instead of a plain HTTPS + Connect-RPC client, for every non-Rust binding we might ever generate. |
| Connect client-streaming RPC for upload bytes | Avoids a second transport, but client-streaming support for `buffa`-generated bindings on `wasm32-unknown-unknown` is unproven (see the `buffa on wasm32` spike in [roadmap.md](../roadmap.md)); betting the ingest hot path on it is the kind of risk [ADR 0007](../adr/0007-one-upload-path.md) exists to avoid. |
| One RPC that accepts an entire report/artifact inline, no separate upload flow | Fine for small JUnit files, breaks for multi-hundred-MB Playwright HTML reports or coverage sites; `SubmitReport`'s inline-vs-upload split keeps the common case (small reports) a single call while large payloads still get chunking. |
| Variable part size (client picks, up to R2's 5 GiB max) | Simpler resumability math and a fixed 32 MiB Worker-side validation rule beat a few fewer round trips on very large files; also keeps part size well inside every Cloudflare plan's request body limit without per-deployment tuning. |
| Treat re-uploaded content under the same name as a hard conflict (reject) | Flaky re-runs within a job are common enough that rejecting them would make `cloud-ci upload` unusable from a retry loop; keeping both rows and picking the newest as canonical costs a little R2 storage for a lot of uploader-side simplicity. |
| Explicit `cloud-ci upload finalize` command and `FinalizeRun` RPC | It needs a final job gated on `needs:` every leg, which every CI system expresses differently and which is easy to forget or to skip when a leg is cancelled. Each leg already knows the shard total, so counting shards, plus the CI's own completion webhook or a timeout, closes the run without an extra step. |
