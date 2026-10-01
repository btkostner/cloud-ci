# PR comment and Check Runs

Status: Proposed

Related: [../architecture.md](../architecture.md), [./byo-ci.md](./byo-ci.md), [./parallelization.md](./parallelization.md), [./analytics.md](./analytics.md), [./ai.md](./ai.md), [./auth.md](./auth.md), [./assets.md](./assets.md), [./pipeline-config.md](./pipeline-config.md)


> Check Runs are now opt-in per node or per named selector, plus the always-on `cloud-ci`
> aggregate check; see [dynamic-pipelines](./dynamic-pipelines.md#github-status-checks). Where
> this doc says "one Check Run per job", read "per enabled node or selector".
## Summary

cloud-ci reports results to GitHub in two ways:

1. **Check Runs (always on).** Each job gets one Check Run on the commit, and each run also gets an aggregate `cloud-ci` check. Check Runs drive branch protection, and they work even with comments disabled.
2. **One sticky PR comment (optional, per repo).** The comment is created once and then edited in place. A hidden HTML marker identifies it. It covers every run, `managed` and `external`, recorded for the PR's current head sha: job status, test summary, failures with AI summaries, coverage delta against base, perf deltas and critical path, runner-sizing decisions, report/site links, and flaky notes. Writes are debounced and coalesced by a dedicated `PullRequestState` Durable Object (one instance per PR), and the rendered body is held under a byte budget below GitHub's comment limit.

`/cloud-ci` slash commands in PR comments let people rerun, cancel, refresh, and toggle the comment. Commands are authorized against the commenter's GitHub repo permission.

## Goals

- One comment per PR, never one per push or per run. Its content always describes the current head sha.
- Managed and external runs look the same in the comment (see [./byo-ci.md](./byo-ci.md)).
- The rendered body can never exceed GitHub's limit. Truncation is deterministic, and the full untruncated report is always one click away.
- A busy PR does not hit GitHub's secondary rate limit. With 40 shards finishing within 10 seconds, the comment gets at most 1–2 PATCHes.
- Check Runs exist for every job, whatever the comment setting, so branch protection can require them.
- Slash commands are idempotent under webhook redelivery and are authorized per [./auth.md](./auth.md).

## Non-goals

- Inline review comments on diff lines. Check Run annotations already cover that, and autofix suggested-changes reviews belong to [./ai.md](./ai.md).
- Commit statuses (`/statuses`). Check Runs replace them.
- Comments on pushes with no PR (branch builds). These get Check Runs only.
- Support for GitHub Enterprise Server or other forges.
- Splitting the report across several comments. When there is too much content, it is truncated and linked, never paginated into extra comments.

## User experience

### Configuration

An admin enables the comment per repo in the dashboard (D1 `repo_settings.pr_comment`, default `off`). The pipeline file tunes it. Full schema: [./pipeline-config.md](./pipeline-config.md).

```yaml
# .cloud-ci/pipeline.yml
github:
  pr_comment:
    sections: [status, tests, failures, coverage, perf, sizing, artifacts, flaky]  # order is fixed; this list filters
    collapse_when_green: true      # all passing -> one-line header + collapsed table
    ai_summary: true               # requires AI enabled at deployment level, see ./ai.md
    coverage:
      report: unit                 # which coverage report name to diff vs base
      fail_under_delta: null       # informational only; gating belongs to checks
    perf:
      threshold_pct: 5             # hide benchmark/duration deltas smaller than this
  checks:
    external: per_run              # per_job | per_run | none (external runs only)
```

Precedence: the dashboard admin switch is the master on/off. For same-repo PRs, `github.pr_comment` is read from the pipeline file at the head sha. For fork PRs it is read from the base branch, so a fork cannot turn on AI summaries (which cost money) or change sections. Repos that only send `external` runs and have no pipeline file use dashboard defaults.

### Comment layout (mockup)

Here is a PR with one managed pipeline (`ci`) and one external run (GitHub Actions `build`), during execution. Status words are plain text; no emoji or images.

````markdown
<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->
### cloud-ci: 2 failed, 14 passed, 1 running on `def5678`

Base `main` @ `abc1234` · updated 18:42:10 UTC · [Dashboard](https://ci.example.com/acme/web/pull/412) · [Full report](https://ci.example.com/acme/web/pull/412/report/def5678)

| Run | Job | Status | Duration | vs base | Runner |
| --- | --- | --- | --- | --- | --- |
| ci | lint | passed | 0:42 | -3s | basic |
| ci | unit (8 shards) | **failed** (1/8 shards) | 3:10 | +22s | standard-2 |
| ci | e2e (4 shards) | running (3/4 done) | 6:02 | | auto: standard-3 |
| ci | merge-reports | waiting | | | lite |
| gha/build (external) | build | passed | 4:11 | +1s | |

**Tests:** 4,812 passed · **2 failed** · 37 skipped · 1 flaky · 11m 04s total across shards

#### Failures (2)

<details open><summary><code>unit</code> · src/cart/total.test.ts › applies discount › rounds half-even</summary>

> **AI summary** (Workers AI, may be wrong): `roundHalfEven` now gets the pre-tax
> subtotal because of the reordering in `src/cart/total.ts:41`; the expected value assumes post-tax.

```text
AssertionError: expected 10.05 to equal 10.04
    at src/cart/total.test.ts:88:21
```
Shard 3/8 · attempt 1 · first failure on this PR · [log](https://ci.example.com/l/r_9f2/unit-3#L210) · [history](https://ci.example.com/acme/web/tests/t_77a)
</details>

<details><summary><code>gha/build</code> · api/handlers_test.go › TestCreateOrder/duplicate_id</summary>
...
</details>

#### Coverage (`unit`)
Lines **84.12%** (+0.31) · Branches **71.02%** (-0.40) · vs `abc1234` on `main`
<details><summary>5 files changed coverage</summary>

| File | Lines | Change |
| --- | --- | --- |
| src/cart/total.ts | 91.3% | -4.2 |
</details>

#### Performance
| Item | Base (median of 10) | Head | Change |
| --- | --- | --- | --- |
| job `e2e` wall time | 5:31 | 6:02 | +9.4% |
| bench `parse_large_lockfile` | 41.2 ms | 47.9 ms | **+16.3%** |

Critical path: `e2e` → `merge-reports` (6:02 of 6:44 total on this attempt).

#### Runner sizing
| Job | Chosen | Previous | Reason |
| --- | --- | --- | --- |
| `e2e` | standard-3 | standard-2 | p95 working set 5.1 GiB × 1.3 = 6.6 GiB > 6 GiB (standard-2) |

#### Reports
[Playwright report (merged)](https://assets.example.com/s/acme/web/pr-412/latest/playwright/) · [Vitest HTML](https://assets.example.com/s/acme/web/pr-412/latest/vitest/) · [Coverage HTML](https://assets.example.com/s/acme/web/pr-412/latest/coverage/) · [12 artifacts](https://ci.example.com/acme/web/runs/r_9f2/artifacts)

#### Flaky
- `e2e` checkout.spec.ts › pays with card: failed attempt 1, passed attempt 2. Flake rate on `main` over 30 days: 4.1%.

<sub>Previous head `9a0c1e2`: 3 failed, 13 passed (superseded). Commands: `/cloud-ci help`.</sub>
````

When every job has passed and `collapse_when_green` is set, the comment becomes the header line (`### cloud-ci: all 17 jobs passed on \`def5678\``) plus the coverage and perf one-liners. The status table moves into a collapsed `<details>`.

Report links go to the separate assets hostname, never the dashboard host (security invariant in [./assets.md](./assets.md)). PR-scoped `latest` URLs stay stable across pushes.

### Check Runs

| Check Run | Created when | Name | Details URL |
| --- | --- | --- | --- |
| Per managed job | Job enters DAG (status `queued`) | `cloud-ci / <pipeline> / <job>` | dashboard job page |
| Per external job (`checks.external: per_job`) | First ingest for job | `cloud-ci / <run_key> / <job>` | dashboard job page |
| Per external run (`per_run`, default) | First ingest for run | `cloud-ci / <run_key>` | dashboard run page |
| Aggregate | First run recorded for sha | `cloud-ci` | dashboard PR/commit page |

External runs default to `per_run` because the external CI (for example GitHub Actions) usually posts its own per-job checks already. A shard group maps to a single Check Run, and the summary holds a shard table. The aggregate `cloud-ci` check is the one meant to be required in branch protection. It turns `completed` once every run known for the sha is terminal. Its conclusion is `failure` if any required job failed, otherwise `success`.

Check Run `output.summary` holds the per-job slice of the comment layout (tests, failures, AI summary). Failures that carry file/line from reports become annotations (`annotation_level: failure`, flaky as `warning`).

### Slash commands

These are recognized in `issue_comment.created` events on PRs (`issue.pull_request` present). A command is any line that begins with `/cloud-ci`. At most 5 commands are processed per comment, and only from the first 20 lines.

| Command | Effect | Min role |
| --- | --- | --- |
| `/cloud-ci help` | Replies with the command list | viewer |
| `/cloud-ci refresh` | Re-renders now. Recreates the comment if it was deleted | viewer |
| `/cloud-ci rerun failed` | Reruns failed jobs/shards of managed runs on head sha | operator |
| `/cloud-ci rerun all` | New attempt of all managed runs on head sha | operator |
| `/cloud-ci rerun <job>` | Reruns one job (plus its dependents) | operator |
| `/cloud-ci cancel` | Cancels in-flight managed runs on head sha | operator |
| `/cloud-ci comment off` / `on` | Per-PR toggle of the sticky comment (Check Runs unaffected) | operator |
| `/cloud-ci explain <job>[ <test>]` | Asks for a longer AI analysis, posted into the comment's failure entry | operator |
| `/cloud-ci autofix` | Delegates to the autofix flow in [./ai.md](./ai.md) | operator, plus repo opt-in |

External runs cannot be rerun or cancelled from cloud-ci. Against them, `rerun`/`cancel` are no-ops, and the reply links the external run URL captured at ingest.

## Design

### Component flow

```mermaid
sequenceDiagram
    participant GH as GitHub
    participant W as Worker (webhook/ingest)
    participant RC as RunCoordinator (run X)
    participant PRS as PullRequestState (repo/pr)
    participant D1
    participant R2
    W->>RC: job/shard/report event (managed agent or ingest)
    RC->>GH: Check Run create/PATCH (per job, throttled)
    RC->>D1: persist job/report state
    RC->>PRS: notify_dirty(head_sha, reason) [fire-and-forget, no ack]
    PRS->>PRS: ignore if head_sha mismatch; else coalesce, set/extend alarm
    Note over PRS: alarm fires
    PRS->>D1: load aggregate (pull_requests, runs, jobs, reports)
    PRS->>PRS: render full + budgeted body; hash
    PRS->>R2: put full report (if hash changed)
    PRS->>GH: PATCH /issues/comments/{id} (or create)
    PRS->>D1: render_seq, rendered_hash, last_patched_at
```

### PullRequestState: one writer per PR

A PR comment aggregates many runs, and each run has its own `RunCoordinator`. Ownership belongs to the PR, not to any one run, so cloud-ci gives each PR a dedicated Durable Object, `PullRequestState`, one instance per `(repo_id, pr_number)`, addressed by `idFromName("{repo_id}/{pr_number}")`. The id is derived from stable identifiers, so any part of the Worker reaches the right instance directly; there is no lookup table, election, or claim step.

`PullRequestState` is the sole writer of the PR's sticky comment. It:
- Owns the debounce/coalescing alarm (see below) and every GitHub PATCH/create call for the comment.
- Tracks the PR's current `head_sha` in its own DO storage, seeded from the `pull_request` webhook and corrected by the ingest path for PRs that only ever see external runs.
- Receives `/cloud-ci refresh`, `/cloud-ci comment on|off`, and `/cloud-ci explain` directly (see Slash command handling).

When a `RunCoordinator` records a job/shard/report event, it looks up every open PR whose head is the run's sha (`pull_requests_head` index; a sha can be the head of several stacked PRs) and sends `notify_dirty(head_sha, reason)` to each PR's `PullRequestState` instance, fire-and-forget: the `RunCoordinator` does not wait for an ack and does not retry. `PullRequestState` ignores a `notify_dirty` whose `head_sha` does not match what it has tracked; otherwise it folds the event into its pending flush.

On `pull_request.synchronize` (new head sha), the same `PullRequestState` instance updates its tracked `head_sha` in place: no claim, no epoch bump, no handoff, because it was already the only writer for this PR across every sha it will ever have. There is nothing to fail over between on DO eviction either; Cloudflare restarts the instance with its storage intact, and it resumes from the alarm.

Check Runs are unaffected by any of this: they stay with the run's own `RunCoordinator`, keyed per (run, job), and never go through `PullRequestState`.

This design still assumes [./byo-ci.md](./byo-ci.md) gives every external run something that can call `notify_dirty` (a `RunCoordinator` or equivalent); see Open questions.

### Debounce and coalescing

`PullRequestState` keeps its state in local DO SQLite (`comment_state`) and drives it off the DO's own alarm directly: since each instance serves exactly one PR, there is nothing to multiplex.

| Parameter | Value | Purpose |
| --- | --- | --- |
| quiet window | 4 s after the last dirty event | Waits out bursts (shards finishing together) |
| max delay | 20 s after the first unflushed dirty event | Bounds staleness under continuous activity |
| min interval | 10 s between PATCHes of one comment | Rate-limit guard |
| terminal flush | quiet window 1 s when the last run on sha turns terminal or `reason = head_changed` | Fast final state |
| hash skip | no PATCH if `sha256(body) == rendered_hash` | Avoids no-op writes (e.g. log-only events) |

Dirty reasons are typed (`job_state`, `report_merged`, `ai_summary_ready`, `coverage_ready`, `head_changed`, `command_refresh`). Log lines and resource samples never mark the comment dirty. A per-repo token bucket in `RepoState` gates all content-generating writes (comments and Check Run PATCHes): 40/min and 400/hour per repo. That stays under GitHub's general secondary limit of "no more than 80 content-generating requests per minute and no more than 500 content-generating requests per hour" (verified 2026-09-30, https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api). That limit applies per installation, not per repo, so many busy repos together can still hit it; see Open questions. When the bucket is empty, flushes are deferred, not dropped. The coalesced state is always the latest, so a deferred flush loses nothing.

### Aggregation

On flush `PullRequestState` builds a `PrReport` (proto message in `cloud-ci-proto`, so the dashboard's full report page uses the same model) from D1:

- **Runs:** every run with `sha = head_sha` that is linked to the PR. Managed runs that test the merge ref record `pr_head_sha` and are matched on that. For each `(run_key, job)` only the latest attempt is shown; an earlier failed attempt that later passed counts as flaky.
- **Jobs:** status, duration, runner/instance type, shard progress (`k/N done`), and `vs base` (the base-branch median of the last 10 successful runs of the same pipeline/job, from analytics rollups, see [./analytics.md](./analytics.md)).
- **Tests:** merged totals per run. While shards are still running, partial totals are shown and labelled `partial`. After the merge barrier ([./parallelization.md](./parallelization.md)), the merged report replaces the partial sums.
- **Failures:** failed test cases ordered by (required job first, first-failure-on-PR first, job DAG order, test name). Each carries message, trimmed stack, shard, attempt, log deep link, and history link. AI summaries are attached when `ai_summaries` has a row for the failure cluster; the AI job reports `ai_summary_ready` asynchronously. The comment never waits on AI and never shows a placeholder.
- **Coverage:** the head report named by `coverage.report` (or the only one) is compared with the base report. The base is the coverage report of the same name from the latest completed run at the PR's merge-base sha, which comes from the compare API's `merge_base_commit` and is cached per (base, head). If no run exists at the merge-base, the latest base-branch run committed before it is used, labelled `(approximate base)`. Per-file deltas are shown only for files the PR touches or whose line coverage changed by at least 0.1 points.
- **Perf:** job wall-time deltas and benchmark report deltas beyond `perf.threshold_pct`, plus the run's critical path (longest job-dependency chain by wall time). Regressions are bold. Metric definitions live in [./analytics.md](./analytics.md).
- **Runner sizing:** for jobs with `runner: auto`, the instance type analytics chose, the previous type, and a one-line reason, read from D1 `insights`/`sizing_decisions` ([./analytics.md](./analytics.md)). Shown only when sizing changed or on its first decision.
- **Reports:** site artifacts (merged Playwright, Vitest HTML, coverage HTML) at PR `latest` URLs, plus the artifact count ([./assets.md](./assets.md)).
- **Flaky:** in-run retry flakes plus tests that analytics marks as known-flaky and that failed in this run.

### Size budget and truncation

GitHub rejects comment bodies over the limit with `body is too long (maximum is 65536 characters)`. The REST docs do not state this limit. The source is the observed API error (verified 2026-09-30, https://github.com/orgs/community/discussions/41331, https://github.com/renovatebot/renovate/issues/15850). Check Run `output.summary` and `output.text` are documented at 65535 characters each (verified 2026-09-30, https://docs.github.com/en/enterprise-server@3.2/rest/checks/runs). However, the API has also reported the limit as a "bytesize" (https://github.com/github/docs/issues/35252). Whether the unit is characters or bytes is [unverified], so cloud-ci budgets in **UTF-8 bytes**: the byte length is at least the character length, so a byte cap is safe under either reading.

| Target | Hard cap (bytes) |
| --- | --- |
| PR comment body | 60,000 |
| Check Run `output.summary` | 60,000 |
| Check Run `output.text` | unused (summary only) |
| Annotation `title` | 255 chars (documented) |
| Annotation `message` | 4,096 (documented max 64 KB; we cap lower) |

The renderer is section-based. Every section renders into blocks, which are complete markdown units: a table row, a `<details>` element, a fenced code block. Truncation only removes whole blocks, so it cannot leave an open fence or an unclosed `<details>`. The header (marker, title line, base/links line) and the footer are reserved first (≤ 2,000 bytes). The remaining budget goes to sections in priority order. A section's unused budget rolls to the next one.

| Priority | Section | Soft budget | Degradation within section |
| --- | --- | --- | --- |
| 1 | status table | 12,000 | >40 jobs: show non-passing rows plus `N jobs passed` rollup row; still over: per-run rollup rows |
| 2 | failures | 28,000 | first 10 full (`<details>`); next 40 one-line (name, job, log link); rest `and N more` link |
| 3 | tests summary | 1,000 | never truncated (fixed size) |
| 4 | coverage | 4,000 | drop per-file table, keep totals |
| 5 | perf | 4,000 | keep top 10 by absolute change |
| 6 | sizing | 1,500 | keep only jobs whose instance type changed this run |
| 7 | reports | 3,000 | keep merged sites; collapse artifacts to count link |
| 8 | flaky | 3,000 | keep top 10 by flake rate |

Per-failure caps: message 1,000 bytes, stack 20 lines / 2,000 bytes, AI summary 800 bytes. A trimmed field ends with `… (truncated, see log)`, and the cut always falls on a UTF-8 character boundary.

If the total is still over the cap after section budgets, whole sections are dropped in this order: flaky, sizing, perf, reports (a single `Reports` link stays), coverage table, then failure stacks (one-line form for all). A final guard handles the case where the body is *still* over 60,000 bytes, which should not happen: the renderer emits the minimal form (header, status counts, `Full report` link) and logs `comment_render_overflow`. The full untruncated render is always written to R2 and shown on the dashboard's full report page, so truncation removes nothing from the record.

### Untrusted content escaping

Test names, messages, stack traces, job names, and AI output come from untrusted code: fork PRs, external CI, and model output. Before any of them enters markdown:

- Inline text is escaped for markdown metacharacters, and `|` is escaped inside tables.
- Text with newlines or backticks goes into a fenced block whose fence is one backtick longer than the longest backtick run in the content.
- `<!--` and `-->` are replaced with `<!-` / `->`, so content can never forge the cloud-ci marker or hide text.
- All raw HTML is escaped; only renderer-generated `<details>`, `<summary>`, `<code>`, `<sub>` are emitted.
- `@` is followed by a zero-width non-joiner outside code, so names like `@org/team` do not notify anyone. Bare `#123` and `GH-123` references are wrapped in code spans to avoid cross-reference backlinks.
- Links are generated only from cloud-ci URLs. URLs found in test output are rendered as text, not links.

### Marker-based upsert

The marker is the first line of the body: `<!-- cloud-ci:pr-comment v1 repo=<repo_id> pr=<number> -->`. D1 `pr_comments.comment_id` is the primary pointer, and the marker is the recovery path.

1. `comment_id` known: `PATCH /repos/{owner}/{repo}/issues/comments/{comment_id}`.
2. PATCH returns 404, meaning a human deleted the comment: set `suppressed_sha = head_sha` and do not recreate for this sha. On the next `head_changed`, or on `/cloud-ci refresh`, clear the suppression and create a new comment.
3. `comment_id` unknown (first render, or D1 lost the row): list `GET /repos/{owner}/{repo}/issues/{pr}/comments?per_page=100` across pages. Adopt the first comment whose body starts with the `cloud-ci:pr-comment` marker and whose `performed_via_github_app.id` equals our App id. Comments carrying our marker from any other author are ignored, since anyone can paste the marker. If none is found, `POST /repos/{owner}/{repo}/issues/{pr}/comments`. If several of ours are found (an earlier race), adopt the oldest and delete the rest.
4. Creation is serialized by `PullRequestState`'s single-threaded execution, so the find-or-create race in step 3 can only arise if the DO's own storage was lost (for example, an evicted `comment_id`) while a comment it created earlier still exists on GitHub. The list-then-adopt logic in step 3 recovers from that case on the next render.

GitHub App permissions needed: `Pull requests: write` (create/edit PR comments), `Issues: write` (comment reactions), and `Checks: write`. The endpoints are listed under these permissions in https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps (verified 2026-09-30).

### Stale-sha handling

| Event | Behaviour |
| --- | --- |
| `pull_request.synchronize` (new push or force-push) | Update `pull_requests.head_sha` in D1 and in `PullRequestState`'s own tracked `head_sha` (same instance, no handoff). Flush immediately (`head_changed`, 1 s quiet) with the new sha's runs only (`queued` rows if runs exist, else `waiting for runs`). Keep a one-line `Previous head` footer summarizing the old sha's last known counts. |
| Late event for old sha | Updates its Check Runs (they are per-sha and still visible on that commit). Never marks the PR comment dirty: `PullRequestState` ignores a `notify_dirty` whose `head_sha` does not match its tracked value. |
| External ingest for sha that is not yet head (ingest beats the webhook) | The run is stored. On `synchronize`, the same `PullRequestState` instance reloads runs for the new head sha from D1 on its next flush, so the run is included. |
| Cancel-superseded | Done by `RepoState` (see [../architecture.md](../architecture.md)). Superseded runs appear only in the `Previous head` footer. |
| PR closed or merged | Keep updating until all runs on the head sha are terminal. Then render a final body with `(PR closed)` in the header and freeze (`pr_comments.frozen = 1`). |
| PR reopened | Unfreeze. The next run event re-renders. |

### Check Run lifecycle

- **Create:** `queued` when the job enters the DAG, with `external_id = <run_id>/<job_id>` and `details_url` set to the dashboard job page.
- **Start:** `in_progress` with `started_at`.
- **Progress:** summary PATCHes at most once per 30 s per check, and only when a shard finishes or the test count changes.
- **Complete:** `completed` with `conclusion` (`success`, `failure`, `cancelled`, `timed_out`, `skipped`; `neutral` for jobs marked `allow_failure`) and the final summary.
- **Annotations:** at most 50 per request, appended by further PATCHes (verified 2026-09-30, https://docs.github.com/en/rest/checks/runs). cloud-ci caps each Check Run at 200 annotations and puts a "more in dashboard" note in the summary.
- **Actions:** at most three (documented: label ≤ 20 chars, identifier ≤ 20, description ≤ 40, same source). On failed jobs: `Rerun failed` (`rerun_failed`), `Explain` (`explain`). On running jobs: `Cancel` (`cancel`). `check_run.requested_action` is authorized like the slash command of the same name.
- **Re-run from the GitHub UI:** `check_run.rerequested` maps to `rerun <job>`, and `check_suite.rerequested` maps to `rerun failed` for the sha. The rerun creates a new Check Run with the same name. GitHub keeps at most 1000 same-named check runs per suite and deletes older ones automatically (verified 2026-09-30, same source), which is harmless here.

Check Run writes come from the run's own `RunCoordinator`, not `PullRequestState`. Each check belongs to exactly one job, so ordering is already guaranteed by the per-run single-writer DO.

### Slash command handling

1. The webhook (`issue_comment`, action `created` only) is verified (HMAC) and deduplicated on `X-GitHub-Delivery`. Events whose `sender.type == "Bot"` and events not on a PR are ignored.
2. The comment is parsed into commands, and each command gets a row `(comment_id, line_no)` in `comment_commands` via `INSERT OR IGNORE`. A conflict means the command is already handled, so a redelivery is a no-op.
3. Role: `GET /repos/{owner}/{repo}/collaborators/{username}/permission` (cached 5 min per user/repo in D1) maps `read`/`triage` to viewer, `write` to operator, and `maintain`/`admin` to admin, per [./auth.md](./auth.md).
4. Acknowledgement: add reaction `eyes` on receipt, `rocket` on success, `confused` on denial or parse error (reaction contents verified 2026-09-30, https://docs.github.com/en/rest/reactions/reactions). Only `help` and errors produce a reply comment. Reply comments carry no marker and are never edited.
5. Execution: `rerun`/`cancel` are forwarded to the target run's `RunCoordinator`. `refresh`, `comment on|off`, and `explain` are routed directly to the PR's `PullRequestState` instance, addressed by `idFromName("{repo_id}/{pr_number}")`; no lookup is needed. `refresh` is limited to once per 60 s per PR.

## Data model

D1 (sketch; exact columns are finalized in `cloud-ci-proto` and its migrations):

```sql
CREATE TABLE pull_requests (
  repo_id INTEGER NOT NULL, number INTEGER NOT NULL,
  head_sha TEXT NOT NULL, head_ref TEXT NOT NULL, base_ref TEXT NOT NULL,
  is_fork INTEGER NOT NULL, state TEXT NOT NULL,          -- open | closed | merged
  merge_base_sha TEXT, updated_at INTEGER NOT NULL,
  PRIMARY KEY (repo_id, number)
);
CREATE INDEX pull_requests_head ON pull_requests (repo_id, head_sha) WHERE state = 'open';

CREATE TABLE pr_comments (
  repo_id INTEGER NOT NULL, pr_number INTEGER NOT NULL,
  comment_id INTEGER,                 -- GitHub comment id; NULL until created/adopted
  head_sha TEXT NOT NULL,
  render_seq INTEGER NOT NULL DEFAULT 0, rendered_hash BLOB, rendered_bytes INTEGER,
  truncated INTEGER NOT NULL DEFAULT 0, last_patched_at INTEGER,
  disabled INTEGER NOT NULL DEFAULT 0,  -- /cloud-ci comment off
  suppressed_sha TEXT,                  -- human deleted the comment for this sha
  frozen INTEGER NOT NULL DEFAULT 0, last_error TEXT,  -- mirrors PullRequestState's authoritative local state
  PRIMARY KEY (repo_id, pr_number)
);

CREATE TABLE check_runs (
  run_id TEXT NOT NULL, job_id TEXT NOT NULL,   -- job_id '' for per-run / aggregate checks
  gh_check_run_id INTEGER NOT NULL, name TEXT NOT NULL, head_sha TEXT NOT NULL,
  status TEXT NOT NULL, conclusion TEXT, annotations_posted INTEGER NOT NULL DEFAULT 0,
  last_patched_at INTEGER, PRIMARY KEY (run_id, job_id)
);

CREATE TABLE comment_commands (
  repo_id INTEGER NOT NULL, comment_id INTEGER NOT NULL, line_no INTEGER NOT NULL,
  pr_number INTEGER NOT NULL, sender_login TEXT NOT NULL, role TEXT NOT NULL,
  command TEXT NOT NULL, result TEXT NOT NULL,  -- ok | denied | invalid | noop
  created_at INTEGER NOT NULL, PRIMARY KEY (comment_id, line_no)
);
```

`PullRequestState` SQLite (one instance per PR, so a single row):

```sql
CREATE TABLE comment_state (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  head_sha TEXT NOT NULL,
  dirty_seq INTEGER NOT NULL, flushed_seq INTEGER NOT NULL,
  first_dirty_at INTEGER, last_dirty_at INTEGER, next_flush_at INTEGER
);
```

R2:

| Key | Content | Retention |
| --- | --- | --- |
| `pr-reports/{repo_id}/{pr}/{head_sha}/report.pb` | Latest full `PrReport` proto (overwritten per render) | PR artifact retention ([./assets.md](./assets.md)) |
| `pr-reports/{repo_id}/{pr}/{head_sha}/comment.md` | Latest untruncated markdown render | same |

## Security considerations

- **Marker spoofing:** comments are adopted only when `performed_via_github_app.id` matches our App, and rendered content cannot contain `<!--`.
- **Markdown/HTML injection:** see "Untrusted content escaping". Test output from fork PRs is attacker-controlled. Without escaping, it could forge status rows, ping users, or add misleading links.
- **Command authorization:** every command is checked against the live repo permission of `sender`, never `author_association`, which shows association rather than permission. Fork authors without write get viewer commands only. `autofix` also requires the repo-level opt-in from [./ai.md](./ai.md).
- **Fork PR config:** comment config and AI enablement are read from the base branch for forks (see Configuration).
- **Cost abuse:** `explain` and AI summaries are operator-gated or config-gated, and `refresh` is rate-limited per PR.
- **Link hygiene:** report links point to the assets hostname only ([./assets.md](./assets.md)). Dashboard links require auth ([./auth.md](./auth.md)), so a public PR comment exposes nothing that a viewer of a private dashboard could not see. In public repos, private-dashboard links simply 401 for outsiders.
- **AI output** is labelled `may be wrong`, escaped like test output, and capped at 800 bytes per failure.

## Failure modes

| Failure | Detection | Behaviour |
| --- | --- | --- |
| Secondary rate limit (403/429 with `retry-after` or `x-ratelimit-reset`) | GitHub response | `PullRequestState` sets `next_flush_at` to the reset time. `RepoState` bucket is drained for the installation. Coalescing guarantees the next write is current. |
| 422 body too long (budget miscalculated) | GitHub response | Retry once with the minimal form, record `last_error`, emit metric. |
| Comment deleted by user (404) | PATCH response | Suppress for this sha (see upsert step 2). |
| Conversation locked / App lacks `Pull requests: write` | 403 on POST/PATCH | Record `last_error`, disable comment for the PR, show warning on dashboard repo settings. Check Runs continue. |
| Installation token expired mid-flush | 401 | Refresh token (cached per installation) and retry once. |
| `PullRequestState` DO evicted or crashed | n/a | Alarm and `comment_state` persist in DO storage, and the alarm re-fires after restart [unverified: alarm retry semantics on exception]. |
| `notify_dirty` lost (fire-and-forget to an unreachable `PullRequestState`), or `pull_request.synchronize` missed | No ack on `notify_dirty`; cron reconcile every 10 min lists open PRs with recent run activity | Cron reconcile compares `head_sha` via `GET /pulls/{n}` (corrects a stale `head_sha`) and re-sends `notify_dirty` for any PR whose `pr_comments.last_patched_at` predates its newest job `completed_at`, catching a dropped final notification. |
| D1 unavailable | Query error | Flush is retried with backoff (alarm +30 s). Check Run updates are retried independently by each `RunCoordinator`. |
| AI summary slow or failing | Missing `ai_summaries` row | Comment renders without it; no placeholder. |
| Base coverage missing | No base report | Coverage section shows head totals with `no base report`. |

## Open questions

1. **Installation-wide rate budget.** Secondary limits apply per App installation, and a per-repo `RepoState` bucket cannot see other repos. Options: a single global limiter DO, or per-repo buckets sized as the installation budget divided by active repos. [unverified: whether GitHub counts App installation tokens per installation or per App for secondary limits]
2. **Do Check Run PATCHes count as content-generating requests?** [unverified]. If not, they can leave the shared bucket.
3. **Characters or bytes for the 65536 comment limit?** The design is safe either way. Confirming would recover up to about 5% of the budget for ASCII-heavy bodies.
4. **Comment for merge-ref builds.** If [./pipeline-config.md](./pipeline-config.md) builds `refs/pull/N/merge`, the head shown is still the PR head sha, with the merge sha in the dashboard only. Confirm with the pipeline-config author.
5. **Should an aggregate `cloud-ci` check also include `external` runs** for repos where GitHub Actions already gates merges? Default: yes; per-repo opt-out may be needed.
6. **External runs without a `RunCoordinator`** (depends on [./byo-ci.md](./byo-ci.md)). `PullRequestState` only ever receives `notify_dirty`; if external runs are not given a `RunCoordinator` or equivalent, the ingest path itself must call `notify_dirty` after each write.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| New comment per push or per run | Noisy timeline, notifications on every push, and reviewers must scroll to find the current state. |
| Check Runs only (no comment) | Summaries sit behind the Checks tab and can't show a cross-run, cross-CI view on one screen. Kept as the always-on baseline; the comment is opt-in. |
| Edit the PR description | Conflicts with author edits and needs broader permissions. |
| Splitting overflow across multiple comments | Upsert becomes multi-object with ordering problems; dashboard link is simpler. |
| Debounce via Queue delivery delay | No coalescing (every event still produces a message), and no single writer, so stale PATCHes can still race. |
| Debounce in `RepoState` (per repo) | One DO per repo would serialize comment rendering for every open PR in a busy monorepo; a dedicated `PullRequestState` DO per (repo, pr) isolates load per PR instead. |
| Make a run's `RunCoordinator` the comment writer (the brief's original assignment) | A `RunCoordinator` is scoped to one run, but a PR's comment spans every run, managed and external, across every push, for its head sha. Ownership belongs to the PR, not to any single run. |
| Elect a comment host among `RunCoordinator`s via `host_epoch` CAS, with a `RepoState` fallback | Leader-election and failover logic (claim, epoch fencing, re-claim on timeout) for a role a dedicated per-PR DO holds natively. `idFromName("{repo_id}/{pr_number}")` gives a stable, collision-free address with no claim protocol and no failover window. |
| Store state only in the comment body (stateless sticky-comment actions) | Requires a list/scan on every update and trusts content anyone can edit or delete. D1 is the source of truth, and the marker is only for recovery. |
| Minimize old comments via GraphQL `minimizeComment` instead of editing | Still creates one comment per push. Editing in place is quieter. |
