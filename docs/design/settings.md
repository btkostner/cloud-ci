# Repo settings and pipeline discovery (`.cloud-ci/settings.yml`)

Status: Proposed

Related: [../architecture.md](../architecture.md), [./dynamic-pipelines.md](./dynamic-pipelines.md),
[./parallelization.md](./parallelization.md), [./analytics.md](./analytics.md),
[./pr-comment.md](./pr-comment.md), [./ai.md](./ai.md), [./auth.md](./auth.md),
[./assets.md](./assets.md), [./byo-ci.md](./byo-ci.md),
[ADR 0009](../adr/0009-typescript-pipeline-workflows.md),
[ADR 0010](../adr/0010-pluggable-executors.md)

## Summary

A repository's pipelines are TypeScript files in `.cloud-ci/pipelines/*.ts`, one file per
pipeline (`ci.ts`, `deploy.ts`, `maintenance.ts`, ...). Each file declares its own triggers and
runs as its own Dynamic Workflow — see [dynamic-pipelines](./dynamic-pipelines.md). This document
covers the two things that are not pipeline code:

1. **Discovery**: how `cloud-ci-worker` finds which `.ts` files exist and which events each one
   cares about, without running any of them.
2. **`.cloud-ci/settings.yml`**: a single static YAML file holding repo config that must be
   readable without evaluating code — check naming, PR comment behavior, AI narrowing,
   concurrency and runner bound defaults, cache/retention preferences, which secrets each
   pipeline may request, and slash-command role minimums.

Dashboard (admin) settings remain the authority for anything that spends money or grants
secrets — runner size ceilings, concurrency ceilings, retention/cache maximums, AI autofix
enablement, and secret grants. `settings.yml` can only narrow inside those ceilings; it is
repo-owned, versioned with the code, and reviewable in PRs, so it must never be able to widen
what an admin has not already allowed.

## Goals

- A documented, cacheable way to discover pipeline files and their triggers from a commit sha,
  with no execution of job code.
- One static file for repo-wide knobs that apply across all pipelines: check naming, PR comment
  behavior, AI narrowing, concurrency/runner defaults, cache/retention preferences, secret-request
  allow-lists, slash-command roles.
- Errors are collected, carry a location, and are visible on the commit — never a silent skip.
- Secure defaults: a PR cannot widen its own fork-PR exposure, autofix mode, concurrency ceiling,
  or secret access by editing `settings.yml` in that same PR.

## Non-goals

- Describing job DAGs, steps, images, or runner selection for a specific run — that is pipeline
  code (`ci.container`, `ci.check`, `ci.shard`, ...), documented in
  [dynamic-pipelines](./dynamic-pipelines.md) and [parallelization](./parallelization.md).
- A YAML pipeline-definition format. Pipelines are TypeScript only; there is no declarative,
  engine-interpreted alternative.
- Per-pipeline overrides of these settings. `settings.yml` is repo-wide; a pipeline that needs a
  different runner or secret list does so in its own code, within the allow-lists here.

## User experience

### Annotated full example

```yaml
version: 1                         # required; only 1 is accepted

checks:
  aggregate:
    enabled: true                  # the recommended branch-protection target
    name: cloud-ci                 # check-run name for the aggregate check
  name_template: "{pipeline} / {check}"   # applied to every ci.check(name, ...) call whose name isn't already slash-qualified

pr_comment:
  enabled: true                    # narrows the dashboard master switch; cannot turn it on if the dashboard has it off
  template: .cloud-ci/templates/pr-comment.md   # falls back to the built-in template if the file is missing
  actions: [rerun_failed, autofix]              # checkbox actions offered in the comment
  coverage_report: coverage                     # report name diffed against the PR base in the comment

ai:
  summaries: true                  # Workers AI failure summaries in the PR comment
  autofix: suggest                 # off | suggest | pull_request ; capped by the dashboard's autofix ceiling
  exclude_paths: ["vendor/**", "**/*.lock"]      # never summarized or autofixed
  max_failures_summarized: 20

commands:
  roles:
    rerun: operator                # operator | admin; raise-only above the operator default
    cancel: operator
    autofix: operator

runners:
  default: auto                    # fallback when a ci.container() call omits runner
  auto: { min: basic, max: standard-3, initial: basic }   # must fit inside the admin ceiling
  pools:
    gpu-ci:                        # named pool a script selects with runner: { pool: "gpu-ci" }
      executor: aws-ec2
      type: g5.xlarge

concurrency:
  repo: 40                         # max containers across all runs of this repo, at once
  run: 12                          # max containers for a single run
  runs: 4                          # max concurrent runs for this repo
  cancel_superseded: { pull_request: true, push: false }

shard:
  split: timing                    # timing | file | count ; default strategy for ci.shard(...)
  min: 2
  max: 16
  target: 10m                      # auto shard count aims for ~10m per shard, by history

retention:
  artifacts_days: 14
  reports_days: 30
  sites_days: 14
  logs_days: 30
  snapshots_days: 7

cache:
  max_size_per_repo: 10GiB         # must fit inside the admin ceiling

secrets:
  ci: [E2E_LOGIN_PASSWORD]         # pipeline file name (no .ts) -> secret names it may request
  deploy: [CLOUDFLARE_API_TOKEN]
```

### Minimal example

```yaml
version: 1
```

Every section is optional. Omitted sections use deployment defaults: aggregate check named
`cloud-ci`, PR comments on with the built-in template, AI off, `operator` for every command,
`runners.default: auto` at the deployment-wide ladder, deployment concurrency/retention/cache
defaults, and no pipeline allowed to request any secret (secure default — an empty `secrets` map
denies every request, it does not grant every secret).

## Design

### Pipeline discovery

```mermaid
sequenceDiagram
  participant GH as GitHub
  participant W as cloud-ci-worker (webhook)
  participant Q as Queue (webhooks)
  participant C as Queue consumer
  participant SB as Dynamic Worker (no egress)
  participant RS as RepoState DO
  GH->>W: push / pull_request / issue_comment
  W->>Q: enqueue (after HMAC verify)
  Q->>C: deliver
  C->>GH: GET /repos/{o}/{r}/contents/.cloud-ci/pipelines?ref={sha}
  alt directory missing (404)
    C->>C: no managed pipelines; external runs still work
  else listed
    loop each *.ts entry
      C->>GH: GET /repos/{o}/{r}/git/blobs/{entry.sha}
      C->>SB: load module, read `export default workflow({ on, run })` without calling run()
      SB-->>C: on (JSON) or an error
    end
    C->>C: match on against the event; cache each file's `on` by blob sha
    alt any file matches
      C->>RS: admit run per matching file (concurrency, cancel-superseded)
    end
    C->>RS: upsert this file's `on.schedule` entries (see Schedules below)
  end
```

- **Listing**: `GET /repos/{owner}/{repo}/contents/.cloud-ci/pipelines?ref={sha}` with an
  installation token (`contents: read`). A directory response is an array of entries, each with
  its own blob `sha`; the endpoint caps at 1,000 entries per directory (verified 2026-09-30,
  https://docs.github.com/en/rest/repos/contents), far above the 100-pipeline-per-repo limit
  below. Entries are filtered to `type: file` and a `.ts` name; `404` means no managed pipelines
  and no run is created.
- **Fetching each file**: `GET /repos/{owner}/{repo}/git/blobs/{entry.sha}` using the blob sha
  from the listing, so content and cache key come from one extra round trip per file, not two.
  Files over 256 KiB fail discovery for that file only, with an annotation; the others still run.
- **Reading triggers without running the pipeline**: the fetched source is transpiled (types
  stripped, same toolchain [dynamic-pipelines](./dynamic-pipelines.md) uses for execution) and
  loaded into a Dynamic Worker with `globalOutbound: null`, which blocks `fetch`/`connect`
  entirely (developers.cloudflare.com/dynamic-workers/usage/egress-control, checked 2026-10-01).
  The consumer imports the module and reads `export default`'s `on` field — the object literal
  passed to `workflow({ on, run })` — without invoking `run`. `on` must be plain,
  JSON-serializable data (no closures, no calls to `ci.*`); a value that is not is a discovery
  error for that file, annotated on the commit. This sandbox is the same one
  [dynamic-pipelines](./dynamic-pipelines.md) uses to execute `run`, so a file that is safe to
  discover is already proven loadable for execution.
- **Cache**: D1 `pipeline_manifest` keyed by `(repo_id, file_name, blob_sha)` stores the extracted
  `on` JSON or the error. Identical files across commits parse once.
- **Matching**: trigger shapes (`push`, `pull_request`, `manual`, `schedule`) and their filters are
  pipeline-code concerns documented in [dynamic-pipelines](./dynamic-pipelines.md); this doc only
  covers how the `on` value for each file is obtained and cached.
- **Reruns**: every admitted run stores the pipeline source it executed at
  `runs/{run_id}/pipeline.js` in R2 (bundled, post-transpile). Reruns (`check_run.rerequested`,
  `/cloud-ci rerun`, dashboard) reuse it and never refetch or re-evaluate `on`.
- **Config-validation check**: discovery and `settings.yml` parse errors are posted as a single
  infra-created check run (fixed name `cloud-ci / config`, not configurable, not disableable —
  distinct from the per-job checks pipeline code creates, since it exists before any pipeline
  code runs). It is created only when discovery is attempted for a commit that has
  `.cloud-ci/pipelines/` or `.cloud-ci/settings.yml`; it concludes `failure` with annotations on
  error, `success` otherwise. This is the one signal that exists independent of any pipeline, so
  a typo cannot fail silently with zero checks on the commit.

### Schedules

Cron entries live in each pipeline file's `on.schedule` (declared in code, not `settings.yml`),
but scheduling mechanics are unchanged from the static-config design: schedules do not map to
Worker Cron Triggers, because an account allows 5 (Free) or 250 (Paid) Cron Triggers in total
(verified 2026-09-30, https://developers.cloudflare.com/workers/platform/limits/). Instead, when a
push to the default branch changes a pipeline file's blob sha, the consumer sends that file's
parsed `schedule` list to the repo's `RepoState`. `RepoState` persists it per `(repo_id, file_name)`
and sets one DO alarm for the earliest next fire time across all pipelines. A deployment-wide
Cron Trigger (`*/15 * * * *`) re-arms any `RepoState` whose alarm was lost. When an alarm fires,
`RepoState` re-fetches that pipeline file at the schedule's `branch` head (same discovery path
above) before starting the run, so a schedule always runs the current file content. If the branch
no longer exists, the occurrence is skipped and logged.

### Fetching `settings.yml`

- Request: `GET /repos/{owner}/{repo}/contents/.cloud-ci/settings.yml?ref={sha}` with an
  installation token (`contents: read`). Files of 1 MB or smaller support all features of this
  endpoint; we cap the file at 64 KiB, so the JSON form always works (verified 2026-09-30,
  https://docs.github.com/en/rest/repos/contents).
- `ref` is always a commit sha, never a branch name, so a push landing between webhook and fetch
  cannot swap settings mid-run. For `pull_request`, the sha is `pull_request.head.sha` — **except
  for fork PRs**, where the entire file is instead read from the base repo's default branch head.
  A fork PR cannot change its own check names, comment template, AI mode, concurrency, runner
  bounds, retention, cache limit, command roles, or secret allow-list by editing `settings.yml` in
  the PR; it always runs under the base branch's settings. Fetching a fork head sha through the
  base repo's contents API would not be needed for this reason, but is also `[unverified]` for
  the same-repo pipeline-discovery fetches above.
- Parse cache: D1 `repo_settings` keyed by `(repo_id, blob_sha)` stores the normalized JSON or the
  error list. Identical files across commits parse once.
- `404`: deployment defaults apply, as in the minimal example above. No config-validation failure.
- Every run stores the resolved settings it used at `runs/{run_id}/settings.json` in R2. Reruns
  reuse it and never refetch.

### Precedence against admin (dashboard) settings

Admin settings live in D1, are set by repo/deployment admins in the dashboard (not in git), and
are the ceiling for anything that spends money or grants secrets. `settings.yml` values are
clamped to the admin ceiling, with a warning annotation on the config-validation check, not a hard
failure — a wider value never takes effect, but it also never blocks an otherwise-valid run.

| `settings.yml` key | Admin ceiling | Rule |
| --- | --- | --- |
| `checks.*` | none | Free — check naming has no spend or secret implication |
| `pr_comment.enabled` | dashboard PR-comment switch | Can only turn further off; cannot enable if the dashboard has it off |
| `ai.summaries`, `.exclude_paths`, `.max_failures_summarized` | dashboard AI opt-in | Only take effect if AI is enabled for the repo |
| `ai.autofix` | per-repo admin autofix ceiling (`off` \| `suggest` \| `pull_request`) | Clamped down to the ceiling; `push`-to-branch autofix is never settable from `settings.yml` at all — admin-only, see [ai.md](./ai.md) |
| `commands.roles.*` | `operator` floor | Can only raise toward `admin`, never lower below `operator` |
| `runners.auto.{min,max}`, `runners.pools.*.type` | deployment instance-size ladder / plan limits | Out-of-bound values clamped to the nearest in-bound size |
| `concurrency.repo`, `.run`, `.runs` | deployment container cap | Clamped to the admin max |
| `retention.*_days` | deployment max retention | Clamped |
| `cache.max_size_per_repo` | deployment max | Clamped |
| `secrets.<pipeline>` | admin secret grants (`repo_secrets`, `secret_grants`, see [auth.md](./auth.md)) | A listed name is still denied at job start if the admin has not granted it; `settings.yml` can only narrow which granted names a given pipeline file may request |

### Field reference

| Path | Type | Default | Constraint |
| --- | --- | --- | --- |
| `version` | int | required | `1` |
| `checks.aggregate.enabled` | bool | `true` | |
| `checks.aggregate.name` | string | `cloud-ci` | `^[a-z0-9][a-z0-9 ./_-]{0,63}$` |
| `checks.name_template` | string | `"{pipeline} / {check}"` | vars: `{pipeline}`, `{check}` |
| `pr_comment.enabled` | bool | `true` | narrow-only, see Precedence |
| `pr_comment.template` | string | `.cloud-ci/templates/pr-comment.md` | path in-repo; missing file falls back to the built-in template |
| `pr_comment.actions` | string[] | `[rerun_failed]` | subset of `rerun_failed, rerun_all, autofix, explain` |
| `pr_comment.coverage_report` | string | none | a report name produced by a pipeline |
| `ai.summaries` | bool | `true` | |
| `ai.autofix` | enum | `off` | `off, suggest, pull_request`; narrow-only, see [ai.md](./ai.md) |
| `ai.exclude_paths` | string[] | `[]` | glob |
| `ai.max_failures_summarized` | int | `20` | 1..100 |
| `commands.roles.<rerun\|cancel\|autofix>` | enum | `operator` | `operator, admin`; raise-only |
| `runners.default` | string | `auto` | fixed type, `auto`, or a `runners.pools` name |
| `runners.auto.{min,max,initial}` | string | deployment default | instance-type ladder, see [parallelization.md](./parallelization.md) |
| `runners.pools.<name>.executor` | string | | `cloudflare-containers, aws-ec2, aws-lambda, kubernetes, self-hosted` (see [ADR 0010](../adr/0010-pluggable-executors.md)) |
| `runners.pools.<name>.type` | string | | executor-specific instance/shape identifier |
| `concurrency.repo` | int | deployment default | 1..200 |
| `concurrency.run` | int | deployment default | 1..100 |
| `concurrency.runs` | int | deployment default | 1..50 |
| `concurrency.cancel_superseded.pull_request` | bool | `true` | |
| `concurrency.cancel_superseded.push` | bool | `false` | |
| `shard.split` | enum | `timing` | `timing, file, count` |
| `shard.min` / `.max` | int | `2` / `16` | lower/upper bound for `auto` shard counts |
| `shard.target` | duration | `10m` | `ci.shard(...)` aims for this long per shard, by history |
| `retention.artifacts_days` | int | `30` | 1..deployment max |
| `retention.reports_days` | int | `30` | 1..deployment max |
| `retention.sites_days` | int | `14` | 1..deployment max |
| `retention.logs_days` | int | `30` | 1..deployment max |
| `retention.snapshots_days` | int | `14` | 1..deployment max |
| `cache.max_size_per_repo` | size | `5GiB` | 1..deployment max |
| `secrets.<pipeline_file_name>` | string[] | `[]` | pipeline must exist in `.cloud-ci/pipelines/`; names `^[A-Z][A-Z0-9_]{0,63}$` |

### Validation

Validation runs in two passes, collecting all errors (up to 50) instead of stopping at the first.
Each error carries `line:col` and is posted as an annotation on the `cloud-ci / config` check.

1. YAML 1.2 core schema, so `on`/`yes`/`no` parse as strings. Duplicate map keys are errors.
   Unknown keys are errors, not warnings — each suggests the nearest known key, so a misspelled
   key cannot silently pass.
2. Semantic: enum values, name patterns, numeric bounds, and the `secrets.<pipeline>` key must
   name a file that exists in `.cloud-ci/pipelines/` at the same sha (an allow-list for a pipeline
   that does not exist is a typo, not a no-op). Admin-ceiling clamping (see Precedence) runs after
   this pass and only ever produces warnings, never errors.

Limits: 64 KiB file, 100 pipeline files per repo, 50 secret names per pipeline entry. The parser
is a pure module with no I/O, shared by the Worker and the CLI (`cloud-ci lint`).

## Data model

```sql
CREATE TABLE repo_settings (repo_id INTEGER, blob_sha TEXT, settings_json TEXT, errors_json TEXT,
  parsed_at INTEGER, PRIMARY KEY (repo_id, blob_sha));
CREATE TABLE pipeline_manifest (repo_id INTEGER, file_name TEXT, blob_sha TEXT, on_json TEXT,
  error TEXT, parsed_at INTEGER, PRIMARY KEY (repo_id, file_name, blob_sha));
CREATE TABLE repo_schedules (repo_id INTEGER, file_name TEXT, idx INTEGER, cron TEXT, branch TEXT,
  config_blob_sha TEXT, next_fire_at INTEGER, PRIMARY KEY (repo_id, file_name, idx));
```

R2 keys: `runs/{run_id}/pipeline.js` (bundled pipeline source, immutable per run) and
`runs/{run_id}/settings.json` (resolved settings, immutable per run). `RepoState` holds the
authoritative schedule and concurrency state; D1 `repo_schedules` is a mirror for the dashboard.
`repo_secrets`, `secret_grants`, and `cache_entries` are defined in [auth.md](./auth.md) and
[assets.md](./assets.md) respectively — `secrets.<pipeline>` in `settings.yml` narrows which
granted names a pipeline file may request, it does not store values.

## Security considerations

- Both `.ts` pipeline sources and `settings.yml` are untrusted input from anyone who can open a
  PR. Discovery evaluates only a static `on` literal, in a network-isolated sandbox, never the
  `run` function, so reading triggers cannot itself start a container or call out.
- `ai.autofix`, `concurrency.*`, `runners.*`, `retention.*`, `cache.*`, and `commands.roles.*` are
  all clamped to an admin ceiling that only the dashboard can raise, so a PR cannot widen its own
  spend or secret exposure by editing `settings.yml`.
- For fork PRs, the entire `settings.yml` is read from the base branch, so a fork PR cannot change
  check names, comment templates, command roles, or its own secret allow-list.
- `secrets.<pipeline>` only narrows which already-granted names a pipeline may request; it can
  never grant a name the admin has not already placed in `repo_secrets` or `secret_grants`.

## Failure modes

| Failure | Behavior |
| --- | --- |
| Contents/blob API 5xx / rate limited | Queue retry with backoff (max 5); then `cloud-ci / config` = `failure` with "could not fetch config" |
| `settings.yml` invalid | `cloud-ci / config` = `failure` with annotations; deployment defaults used for that commit so pipelines can still run |
| A pipeline file's `on` export invalid or not statically evaluable | `cloud-ci / config` = `failure` with an annotation naming the file; other pipeline files still discovered and run |
| No `.cloud-ci/pipelines/` directory | No managed pipelines; external runs still work |
| No `settings.yml` | Deployment defaults apply; no config-validation failure |
| Schedule alarm lost | Re-armed by the 15-minute safety Cron Trigger; at most one occurrence delayed, never double-fired |
| Schedule `branch` missing when the alarm fires | Occurrence skipped and logged |

## Open questions

1. Shared parser/sandbox location: a module inside `cloud-ci-worker`, or a new crate shared with
   the CLI for `cloud-ci lint`? Same open question as the execution-side bundler in
   [dynamic-pipelines](./dynamic-pipelines.md) — likely one answer for both.
2. Should the config-validation check (`cloud-ci / config`) be omittable from the aggregate check
   so an admin-caused clamp warning cannot block branch protection, or should clamp warnings never
   appear there at all (dashboard-only)?
3. `secrets.<pipeline>` currently requires the pipeline file to exist at validation time. Does a
   rename (`ci.ts` -> `verify.ts`) in the same PR as a `settings.yml` update race against which
   sha each is read at? Both are read at the same commit sha today, which should make this moot,
   but it is worth a Phase 0 check.

## Alternatives considered

| Alternative | Decision | Reason |
| --- | --- | --- |
| Keep per-pipeline settings (triggers, concurrency, retention) inline in each `.ts` file | Rejected | Repo-wide policy that a PR must not be able to widen (concurrency ceilings, retention, secret allow-lists) cannot live in code a PR controls; it needs a file read the same way regardless of which pipeline is running, and in the fork-PR case, read from a different ref than the code being tested |
| Discover pipelines by convention (any `.ts` file under `.cloud-ci/`) without an explicit directory listing call | Rejected | Would require fetching the whole tree recursively even when only one file changed; a flat `pipelines/` directory keeps discovery to one listing call plus one blob fetch per file |
| Run each pipeline's full `run` function during discovery and discard side effects to learn `on` | Rejected | Side effects (`ci.container`, `ci.check`) are durable steps against `RunCoordinator`; there is no safe way to "discard" them, and it would mean starting containers just to decide whether to start containers |
| Config stored in the dashboard (D1) instead of the repo | Rejected | `settings.yml` must be versioned with the code and reviewable in PRs, same reasoning as pipeline code; policy that must not be PR-editable lives in admin settings instead, as the ceiling, not by moving the whole file out of git |
| Map each schedule to a Worker Cron Trigger | Rejected | The 250-per-account cap and redeploy-to-change; `RepoState` alarms scale per repo |
