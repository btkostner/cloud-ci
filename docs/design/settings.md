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
   readable without evaluating code — PR comment behavior, AI settings, concurrency and runner
   bound defaults, cache/retention preferences, which secrets each pipeline may request, and
   slash-command role minimums.

`settings.yml` is the only place these repo settings live — there is no dashboard or admin layer
underneath it that stores a separate copy or a ceiling for any of these values. The file is read
only from the repo's default branch (the HEAD of the default branch at event time), never from a
PR's head or base ref, regardless of whether the PR is same-repo or a fork. This is also the
security property: a PR cannot widen its own autofix mode, concurrency bound, or secret access by
editing `settings.yml` in that same PR, because the edit only takes effect once it lands on the
default branch. Deployment-wide config (wrangler `vars` at deploy time) still sets hard platform
limits that `settings.yml` cannot exceed — for example, the largest runner instance size a
deployment offers — but there is no per-repo admin settings layer between those platform limits
and `settings.yml`.

## Goals

- A documented, cacheable way to discover pipeline files and their triggers from a commit sha,
  with no execution of job code.
- One static file for repo-wide knobs that apply across all pipelines: PR comment behavior, AI
  settings, concurrency/runner defaults, cache/retention preferences, secret-request allow-lists,
  slash-command roles.
- Errors are collected, carry a location, and are visible on the commit — never a silent skip.
- Secure defaults: a PR cannot widen its own autofix mode, concurrency bound, or secret access by
  editing `settings.yml` in that same PR, because the file is always read from the default
  branch, regardless of which ref or PR triggered the run.

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

pr_comment:
  enabled: true                    # master on/off for the sticky comment
  template: .cloud-ci/templates/pr-comment.md   # falls back to the built-in template if the file is missing

ai:
  enabled: true                    # master switch for this repo; off by default
  summaries: true                  # Workers AI failure summaries in the PR comment (off | pr | all)
  flaky_hints: true                # heuristic hints for known-flaky failures
  perf_suggestions: weekly         # off | weekly
  autofix: suggest                 # off | suggest | pull_request
  autofix_allow_push_to_pr_branch: false   # requires autofix != off
  autofix_on_forks: off            # off | suggest
  exclude_paths: ["vendor/**", "**/*.lock"]      # never summarized or autofixed
  max_failures_summarized: 20
  daily_neuron_cap: 20000

commands:
  roles:
    rerun: operator                # operator | admin; raise-only above the operator default
    cancel: operator
    autofix: operator

runners:
  default: auto                    # fallback when a ci.container() call omits runner
  auto: { min: basic, max: standard-3, initial: basic }

concurrency:
  repository: 40                   # max containers across all runs of this repo, at once
  pipelines: 4                     # max concurrent pipeline runs for this repo
  pipeline: 12                     # max containers for a single pipeline run
  # cancel-superseded policy is pipeline code, not settings.yml — see
  # dynamic-pipelines.md#concurrency's `export const concurrency`

retention:
  artifacts_days: 14
  reports_days: 30
  sites_days: 14
  logs_days: 30
  snapshots_days: 7

cache:
  max_size_per_repo: 10GiB

secrets:
  ci: [E2E_LOGIN_PASSWORD]         # pipeline file name (no .ts) -> secret names it may request
  deploy: [CLOUDFLARE_API_TOKEN]
```

### Minimal example

```yaml
version: 1
```

Every section is optional. Omitted sections use deployment defaults: PR comments on with the
built-in template, AI off, `operator` for every command, `runners.default: auto` at the
deployment-wide ladder, deployment concurrency/retention/cache defaults, and no pipeline allowed
to request any secret (secure default — an empty `secrets` map denies every request, it does not
grant every secret).

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
  It is an infrastructure signal, not a gate: docs recommend against requiring it in branch
  protection unless a repo opts in, since a deployment-side fetch failure would otherwise block
  merges.

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
  cannot swap settings mid-run. Regardless of the triggering event — `push` to any branch,
  `pull_request` (same-repo or fork), `schedule`, or a manual run — `ref` is the default branch's
  HEAD sha at the time the event was received, never the PR's own head or base sha, and never the
  sha of a non-default branch a `push` landed on. For a `push` to the default branch itself, that
  sha is already in the webhook payload; for every other event, the consumer resolves the default
  branch's current head sha with one extra API call before fetching `settings.yml`
  `[unverified: exact endpoint, likely GET /repos/{owner}/{repo}/git/refs/heads/{default_branch}]`.
  This is also why `settings.yml` can never be widened by a PR: the PR's own ref is never read,
  for forks or same-repo PRs alike.
- Parse cache: D1 `repo_settings` keyed by `(repo_id, blob_sha)` stores the normalized JSON or the
  error list. Identical files across commits parse once.
- `404`: deployment defaults apply, as in the minimal example above. No config-validation failure.
- Every run stores the resolved settings it used at `runs/{run_id}/settings.json` in R2. Reruns
  reuse it and never refetch.

### Deployment-wide limits

There is no admin dashboard layer between `settings.yml` and the platform. The only upstream
bound is deployment-wide config — wrangler `vars` set at deploy time — which caps a handful of
values that would otherwise let a single repo exhaust shared account resources:

| `settings.yml` key | Deployment-wide bound | Rule |
| --- | --- | --- |
| `runners.auto.{min,max}` | instance-size ladder the deployment makes available | out-of-bound values clamp to the nearest in-bound size |
| `concurrency.repository` / `.pipelines` / `.pipeline` | deployment container cap | clamped to the deployment max |
| `retention.*_days` | deployment max retention | clamped |
| `cache.max_size_per_repo` | deployment max | clamped |
| `ai.autofix_allow_push_to_pr_branch` | whether this installation's GitHub App has `contents: write` | if the installation lacks the permission, autofix stays capped at `suggest` regardless of this key, see [ai.md](./ai.md) |

A value outside its deployment-wide bound is clamped, with a warning annotation on the
`cloud-ci / config` check — never a hard failure, so an out-of-range value never blocks an
otherwise-valid run. `secrets.<pipeline>` has no deployment-wide bound beyond the 50-name limit
in Validation below: the name itself is only useful if a secret of that name exists in the
deployment's Secrets Store, which is checked at job start, not at config-validation time.

### Field reference

| Path | Type | Default | Constraint |
| --- | --- | --- | --- |
| `version` | int | required | `1` |
| `pr_comment.enabled` | bool | `true` | |
| `pr_comment.template` | string | `.cloud-ci/templates/pr-comment.md` | path in-repo; missing file falls back to the built-in template |
| `ai.enabled` | bool | `false` | master switch for this repo |
| `ai.summaries` | enum | `pr` | `off, pr, all` |
| `ai.flaky_hints` | bool | `true` | |
| `ai.perf_suggestions` | enum | `weekly` | `off, weekly` |
| `ai.autofix` | enum | `off` | `off, suggest, pull_request`; see [ai.md](./ai.md) |
| `ai.autofix_allow_push_to_pr_branch` | bool | `false` | requires `ai.autofix != off`; also requires the installation's GitHub App to have `contents: write`, see [Deployment-wide limits](#deployment-wide-limits) |
| `ai.autofix_on_forks` | enum | `off` | `off, suggest` |
| `ai.exclude_paths` | string[] | `[]` | glob |
| `ai.max_failures_summarized` | int | `20` | 1..100 |
| `ai.daily_neuron_cap` | int | `20000` | |
| `ai.model_summary` / `.model_flaky` / `.model_perf` / `.model_autofix` | string | deploy-time default | Workers AI model id starting with `@cf/`; see [ai.md](./ai.md#model-selection) |
| `commands.roles.<rerun\|cancel\|autofix>` | enum | `operator` | `operator, admin`; raise-only |
| `runners.default` | string | `auto` | fixed instance-type name, or `auto` |
| `runners.auto.{min,max,initial}` | string | deployment default | instance-type ladder, see [parallelization.md](./parallelization.md) |
| `concurrency.repository` | int | deployment default | 1..200; max containers across all runs of this repo at once |
| `concurrency.pipelines` | int | deployment default | 1..50; max concurrent pipeline runs for this repo |
| `concurrency.pipeline` | int | deployment default | 1..100; max containers for a single pipeline run |
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
   that does not exist is a typo, not a no-op). Deployment-wide-bound clamping (see
   [Deployment-wide limits](#deployment-wide-limits)) runs after this pass and only ever produces
   warnings, never errors.

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
`cache_entries` is defined in [assets.md](./assets.md). `secrets.<pipeline>` in `settings.yml` is
the grant list controlling which secret names a pipeline file may request; secret values
themselves stay in Cloudflare Secrets Store and are never read into D1.

## Security considerations

- Both `.ts` pipeline sources and `settings.yml` are untrusted input from anyone who can open a
  PR. Discovery reads a static `on` object or calls `on(ctx)` in a network-isolated, CPU-limited
  sandbox, never the `run` function, so reading triggers cannot itself start a container or call
  out.
- `settings.yml` is read only from the repo's default branch, for every event type (push, pull
  request, schedule, manual), never from a PR's own head or base ref. A PR — same-repo or fork —
  cannot widen `ai.autofix`, `concurrency.*`, `runners.*`, `retention.*`, `cache.*`,
  `commands.roles.*`, or its own secret allow-list by editing `settings.yml` in that same PR; the
  edit only takes effect once it is merged to the default branch.
- `secrets.<pipeline>` is the complete grant list for which secret names a pipeline file may
  request; it does not store secret values, which stay in Cloudflare Secrets Store and are only
  resolved at job start.
- Trust boundary (accepted in PR #1 review, 2026-10-01): merging to the default branch is the
  authority. Merged `settings.yml` can grant a pipeline any secret stored in the deployment and
  raise concurrency, retention, and cache up to deploy-time platform limits. There is no
  dashboard or deployer allow-list above it. Guarding those changes is the job of GitHub
  repository rules: branch protection, required reviews, and CODEOWNERS on `.cloud-ci/`.

## Failure modes

| Failure | Behavior |
| --- | --- |
| Contents/blob API 5xx / rate limited | Queue retry with backoff (max 5); then `cloud-ci / config` = `failure` with "could not fetch config" |
| `settings.yml` invalid | `cloud-ci / config` = `failure` with annotations; deployment defaults used for that commit so pipelines can still run |
| A pipeline file's `on` export is invalid, or `on(ctx)` throws, times out, or exceeds its budget | That pipeline does not run for the event; `cloud-ci / config` = `failure` with an annotation naming the file; other pipeline files still discovered and run |
| No `.cloud-ci/pipelines/` directory | No managed pipelines; external runs still work |
| No `settings.yml` | Deployment defaults apply; no config-validation failure |
| Schedule alarm lost | Re-armed by the 15-minute safety Cron Trigger; at most one occurrence delayed, never double-fired |
| Schedule `branch` missing when the alarm fires | Occurrence skipped and logged |

## Open questions

1. `secrets.<pipeline>` currently requires the pipeline file to exist at validation time. Does a
   rename (`ci.ts` -> `verify.ts`) in the same PR as a `settings.yml` update race against which
   sha each is read at? Both are read at the same commit sha today, which should make this moot,
   but it is worth a Phase 0 check.

## Alternatives considered

| Alternative | Decision | Reason |
| --- | --- | --- |
| Keep per-pipeline settings (triggers, concurrency, retention) inline in each `.ts` file | Rejected | Repo-wide policy that a PR must not be able to widen (concurrency ceilings, retention, secret allow-lists) cannot live in code a PR controls; it needs a file read the same way regardless of which pipeline is running, and in the fork-PR case, read from a different ref than the code being tested |
| Discover pipelines by convention (any `.ts` file under `.cloud-ci/`) without an explicit directory listing call | Rejected | Would require fetching the whole tree recursively even when only one file changed; a flat `pipelines/` directory keeps discovery to one listing call plus one blob fetch per file |
| Run each pipeline's full `run` function during discovery and discard side effects to learn `on` | Rejected | Side effects (`ci.container`, `ci.check`) are durable steps against `RunCoordinator`; there is no safe way to "discard" them, and it would mean starting containers just to decide whether to start containers |
| Config stored in the dashboard (D1) instead of the repo | Rejected | `settings.yml` must be versioned with the code and reviewable in PRs, same reasoning as pipeline code; policy that must not be PR-editable is enforced by reading the file only from the default branch, not by moving it out of git or adding a separate admin-settings layer |
| Map each schedule to a Worker Cron Trigger | Rejected | The 250-per-account cap and redeploy-to-change; `RepoState` alarms scale per repo |
