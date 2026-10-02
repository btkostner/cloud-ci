# PR comment and Check Runs

Status: Proposed

Related: [../architecture.md](../architecture.md#extension-boundary), [ADR 0009](../adr/0009-typescript-pipeline-workflows.md), [./dynamic-pipelines.md](./dynamic-pipelines.md#github-status-checks), [./settings.md](./settings.md), [./byo-ci.md](./byo-ci.md), [./parallelization.md](./parallelization.md), [./analytics.md](./analytics.md), [./ai.md](./ai.md), [./auth.md](./auth.md), [./assets.md](./assets.md)

## Summary

cloud-ci reports results to GitHub in two independent ways:

1. **Check Runs (optional, named).** A pipeline script creates checks explicitly, by name, via `ci.check(name, opts)`; nodes attach to whichever check they belong to. Nothing is created automatically per job, so a noisy or internal-only job can simply have no check. There is no aggregate/rollup check; branch protection targets the script-created named check(s) directly. See [dynamic-pipelines.md#github-status-checks](./dynamic-pipelines.md#github-status-checks) for exactly when a check is created and how its conclusion is computed.
2. **One sticky PR comment (optional, per repo).** The comment is created once, immediately, as a "running" placeholder, then edited in place for the life of the PR's current head sha. A hidden HTML marker identifies it. Rendering is forge-agnostic: a documented `PrReport` context (runs, pipelines, checks, scopes, tests, failures, coverage, perf, reports, deployments, links, actions) is turned into markdown by a template, and posting that markdown to a forge is a separate step behind the `Forge` trait ([../architecture.md#extension-boundary](../architecture.md#extension-boundary)). GitHub is the only `Forge` implementation that ships; the split exists so a later forge reuses the renderer unchanged. The default built-in template is short — it drops the per-job status table, perf/critical-path, and runner-sizing detail that used to live in the comment; those stay in the full report (dashboard run report page, linked from the comment). Repos can replace the template entirely. Writes are debounced and coalesced by a dedicated `PullRequestState` Durable Object (one instance per PR), and the rendered body is held under a byte budget below GitHub's comment limit.

`/cloud-ci` slash commands and checkbox actions in the comment let people rerun, cancel, refresh, and toggle the comment. Both are authorized against the actor's GitHub repo permission.

## Goals

- One comment per PR, never one per push or per run. Its content always describes the current head sha.
- Managed and external runs look the same in the comment (see [./byo-ci.md](./byo-ci.md)).
- The rendered body can never exceed GitHub's limit. Truncation is deterministic, and the full untruncated report is always one click away.
- A busy PR does not hit GitHub's secondary rate limit. With 40 shards finishing within 10 seconds, the comment gets at most 1–2 PATCHes.
- Named checks stay available for branch protection whether or not the PR comment is enabled for the repo.
- Slash commands and checkbox actions are idempotent under webhook redelivery and are authorized per [./auth.md](./auth.md).

## Non-goals

- Inline review comments on diff lines. Check Run annotations already cover that, and autofix suggested-changes reviews belong to [./ai.md](./ai.md).
- Commit statuses (`/statuses`). Check Runs replace them.
- Comments on pushes with no PR (branch builds). These get Check Runs only, if the pipeline creates any.
- Posting to forges other than GitHub. The `Forge` trait isolates rendering from posting so another forge can reuse the renderer later, but only the GitHub implementation ships here.
- Splitting the report across several comments. When there is too much content, it is truncated and linked, never paginated into extra comments.

## User experience

### Configuration

Per D1, settings.yml — not an admin dashboard switch — is the master on/off for the comment, since posting a comment spends neither money nor secrets. settings.yml is always read from the repo's default branch (never the PR's base ref, which can differ from the default branch, and never the PR head), so no PR, forked or not, can change the template or the coverage report compared by editing settings.yml on its own branch.

```yaml
# .cloud-ci/settings.yml
pr_comment:
  enabled: true                                  # repo default; per-PR override via /cloud-ci comment on|off
  template: .cloud-ci/templates/pr-comment.md     # optional; falls back to the built-in template
  coverage_report: unit                           # which coverage report name to diff vs base
commands:
  roles:
    autofix: admin                                # raises a command's min role above its default; never lowers it
```

Full schema: [./settings.md](./settings.md#pr-comment). Which checkbox actions are offered is decided by the template, not a settings.yml list (see Checkbox actions). External-run check policy (one check per job vs. per run vs. none) is also a settings.yml field; see [./settings.md](./settings.md#checks) and [./byo-ci.md](./byo-ci.md) for its exact key and defaults.

### Template rendering

The renderer turns a versioned `PrReport` context into markdown using [MiniJinja](https://docs.rs/minijinja) (crate `minijinja`, stable `2.24.0`, Apache-2.0, verified on crates.io 2026-10-01), a pure-Rust reimplementation of Jinja2 syntax with no required C dependencies. It compiles to `wasm32` the same way `cloud-ci-worker` already does (../architecture.md's `Rust → wasm32` row), and upstream ships a WASM build for its own browser playground and a `minijinja-js` binding, which suggests it builds for wasm; running it inside `cloud-ci-worker` is `[unverified]` until a Phase 0 spike (sources: the MiniJinja README and docs.rs crate root, 2026-10-01, https://docs.rs/minijinja/latest/minijinja/, https://github.com/mitsuhiko/minijinja). No sandboxing is needed against the template author: a custom template is repo config, trusted at the same level as settings.yml, and read from the base branch for forks. Untrusted *values* (test names, stack traces, external-run output, AI text) still need escaping; MiniJinja exposes a custom `escape_formatter` hook (`minijinja::escape_formatter`, verified docs.rs 2026-10-01) that cloud-ci wires to the same function described in "Untrusted content escaping" below, so every `{{ }}` interpolation is escaped automatically and template authors never have to remember to do it.

- **Built-in default template** ships with cloud-ci and renders the short layout in the mockup below.
- **Repo override** at the path in `pr_comment.template` (default `.cloud-ci/templates/pr-comment.md`). Missing file falls back to the built-in template; a template that fails to parse or render falls back to the built-in template and logs `pr_comment_template_error` (the comment still posts).
- **Context data model** is documented and versioned (`context_version`, independent of the comment marker's `v1`) so a template can check compatibility. Top-level fields: `runs`, `pipelines`, `checks`, `scopes`, `tests`, `failures`, `coverage`, `perf`, `reports`, `deployments`, `links`, `actions`, plus `all_passed` (a precomputed bool templates use to collapse a green PR to one line, replacing the old `collapse_when_green` setting — the template controls this directly, not a setting). `deployments` is the list of `deployment` reports (`name`, `preview_url`, `inspect_url?`, `scope?`) from [byo-ci.md](./byo-ci.md); the built-in template renders it as a Previews section. Exact field shapes are finalized alongside the `PrReport` proto (see Aggregation). Rendering uses `UndefinedBehavior::Lenient` at runtime (a template referencing a field that does not exist in this render, e.g. an empty `perf` on a run with no benchmarks, renders empty rather than erroring) and the same MiniJinja `{{ var }}` syntax as shard commands and upload paths ([dynamic-pipelines.md](./dynamic-pipelines.md)).
- Template output is still subject to the byte budget and truncation rules below; truncation operates on the rendered markdown, not on the template.

### Comment layout (mockup)

The comment is posted the instant the first run (managed or external) for the PR's head sha is created, before any job has reported anything, so it sits at the top of the conversation immediately instead of appearing only once work finishes:

````markdown
<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->
### cloud-ci: running on `def5678`

Base `main` @ `abc1234` · [Dashboard](https://ci.example.com/acme/web/pull/412)
````

It is then edited in place as runs report in. Here is the same PR mid-run, with two monorepo scopes (`web` failing, `api` passing) and one external run (GitHub Actions `build`):

````markdown
<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->
### cloud-ci: 2 failed, 14 passed, 1 running on `def5678`

Base `main` @ `abc1234` · updated 18:42:10 UTC · [Dashboard](https://ci.example.com/acme/web/pull/412) · [Full report](https://ci.example.com/acme/web/pull/412/report/def5678)

#### web — 2 failed

<details open><summary><code>ci / unit</code> · src/cart/total.test.ts › applies discount › rounds half-even</summary>

> **AI summary** (may be wrong): `roundHalfEven` now gets the pre-tax
> subtotal because of the reordering in `src/cart/total.ts:41`; the expected value assumes post-tax.

```text
AssertionError: expected 10.05 to equal 10.04
    at src/cart/total.test.ts:88:21
```
Shard 3/8 · attempt 1 · first failure on this PR · [log](https://ci.example.com/l/r_9f2/unit-3#L210) · [history](https://ci.example.com/acme/web/tests/t_77a)
</details>

<details open><summary><code>gha/build</code> (external) · api/handlers_test.go › TestCreateOrder/duplicate_id</summary>
...
</details>

**Tests:** 4,812 passed · **2 failed** · 37 skipped · 1 flaky · 11m 04s total across shards

**Coverage** (`unit`): Lines **84.12%** (+0.31) · Branches **71.02%** (-0.40) vs `abc1234` on `main`

<details><summary>api — all 6 passed</summary>

**Tests:** 312 passed · 0 failed
**Coverage** (`unit`): Lines **91.0%** (+0.0)
</details>

#### Reports
[Playwright report (merged)](https://assets.example.com/s/acme/web/pr-412/latest/playwright/) · [Vitest HTML](https://assets.example.com/s/acme/web/pr-412/latest/vitest/) · [Coverage HTML](https://assets.example.com/s/acme/web/pr-412/latest/coverage/) · [12 artifacts](https://ci.example.com/acme/web/runs/r_9f2/artifacts)

#### Previews
[storybook](https://chromatic.com/build?appId=...&number=412) · [docs](https://docs-pr-412.pages.dev)

#### Flaky
- `e2e` checkout.spec.ts › pays with card: failed attempt 1, passed attempt 2. Flake rate on `main` over 30 days: 4.1%.

#### Actions
- [ ] Rerun failed jobs <!-- cloud-ci:action rerun_failed -->
- [ ] Autofix <!-- cloud-ci:action autofix -->

<sub>Previous head `9a0c1e2`: 3 failed, 13 passed (superseded). Commands: `/cloud-ci help`.</sub>
````

When `all_passed` is true, the built-in template renders only the header line (`### cloud-ci: all 17 jobs passed on \`def5678\``) plus the per-scope coverage one-liners; everything else is omitted rather than collapsed into a `<details>`, since the full status table no longer exists in the comment at all (see Summary).

Report links go to the separate assets hostname, never the dashboard host (security invariant in [./assets.md](./assets.md)). PR-scoped `latest` URLs stay stable across pushes.

### Scopes (monorepo)

A report or job carries a `scope` (package/app name + path prefix), set by the script or by SDK helpers — the turbo integration in [dynamic-pipelines.md](./dynamic-pipelines.md) sets `scope` to the package name automatically. `cloud-ci upload`'s glob + manifest/`--scope-from turbo` scoping does the same for external runs ([byo-ci.md#globs-and-scopes](./byo-ci.md#globs-and-scopes)). The context's `scopes` field groups `tests`, `failures`, `coverage`, and `reports` by scope. The built-in template renders each scope as its own block; a scope with no failures collapses to a one-line `<details>` summary (as `api` does above), and only failing scopes render expanded by default. A repo with no scopes set (a single-package repo) gets one implicit scope and the grouping headers disappear.

### Check Runs

| Check Run | Created when | Name | Details URL |
| --- | --- | --- | --- |
| Named check (script-defined) | `ci.check(name, opts)` runs in the script | the exact `name` passed to `ci.check` | dashboard check page |
| Per external job | First ingest for job, if the repo's external-check policy is per-job | `<run_key> / <job>` | dashboard job page |
| Per external run | First ingest for run, if the repo's external-check policy is per-run (the default) | `<run_key>` | dashboard run page |

A script may create zero, one, or many named checks; a check with no attached nodes when the script ends concludes `success` with "no matching tasks" (see [dynamic-pipelines.md#github-status-checks](./dynamic-pipelines.md#github-status-checks)). External runs default to a per-run check because the external CI (for example GitHub Actions) usually posts its own per-job checks already. A shard group maps to a single Check Run, and the summary holds a shard table. There is no aggregate check; branch protection names whichever script-created check(s) it requires directly.

Check Run `output.summary` holds the slice of the rendered comment relevant to that check (tests, failures, AI summary), built from the same template context. Failures that carry file/line from reports become annotations (`annotation_level: failure`, flaky as `warning`).

### Slash commands

These are recognized in `issue_comment.created` events on PRs (`issue.pull_request` present). A command is any line that begins with `/cloud-ci`. At most 5 commands are processed per comment, and only from the first 20 lines.

| Command | Effect | Default min role |
| --- | --- | --- |
| `/cloud-ci help` | Replies with the command list | viewer |
| `/cloud-ci refresh` | Re-renders now. Recreates the comment if it was deleted | viewer |
| `/cloud-ci rerun failed` | Reruns failed jobs/shards of managed runs on head sha | operator |
| `/cloud-ci rerun all` | New attempt of all managed runs on head sha | operator |
| `/cloud-ci rerun <job>` | Reruns one job (plus its dependents) | operator |
| `/cloud-ci cancel` | Cancels in-flight managed runs on head sha | operator |
| `/cloud-ci comment off` / `on` | Per-PR override of the repo's `pr_comment.enabled` default (Check Runs unaffected) | operator |
| `/cloud-ci explain <job>[ <test>]` | Asks for a longer AI analysis, posted into the comment's failure entry | operator |
| `/cloud-ci autofix` | Delegates to the autofix flow in [./ai.md](./ai.md) | operator, plus repo opt-in |

Defaults can be raised, never lowered, per command via `commands.roles` in settings.yml. External runs cannot be rerun or cancelled from cloud-ci. Against them, `rerun`/`cancel` are no-ops, and the reply links the external run URL captured at ingest.

### Checkbox actions

GitHub comments have no real buttons, so whichever action checkboxes the template emits (`- [ ] Rerun failed jobs`) render as task-list checkboxes, each tagged with a stable id in a trailing HTML comment the renderer generates itself (`<!-- cloud-ci:action rerun_failed -->`). Which actions a repo offers is a template choice, not a settings.yml list: a repo wanting fewer or different actions edits its `pr_comment.template`. Checking one maps to the slash command of the same name and the same `commands.roles` gate. Check Run `actions` (max 3; GitHub-documented limits: label ≤ 20 chars, identifier ≤ 20, description ≤ 40, verified via the GitHub REST API OpenAPI description 2026-10-01, https://github.com/github/rest-api-description) are used instead wherever a check already exists for the job (failed job: `Rerun failed`/`Explain`; running job: `Cancel`); checkboxes are the fallback that works everywhere else, including the summary comment and multi-check views.

A GitHub webhook delivers `issue_comment` with `action: "edited"` and includes `changes.body.from` (the previous body) alongside the current `comment.body` and the `sender` who made the edit — confirmed against GitHub's webhook payload schema (`webhook-issue-comment-edited`, required fields `action, changes, issue, comment, repository, sender`; `changes.body.from` is a required string), verified 2026-10-01 against the GitHub REST API OpenAPI description (https://github.com/github/rest-api-description/blob/main/descriptions/api.github.com/api.github.com.json) and the mirrored JSON Schema (https://github.com/octokit/webhooks/blob/main/payload-schemas/api.github.com/issue_comment/edited.schema.json). GitHub's narrative webhook-events page lists `comment`/`issue` for `issue_comment` but does not separately enumerate `changes` in its payload-parameter table (verified 2026-10-01, https://docs.github.com/en/webhooks/webhook-events-and-payloads#issue_comment); the schema is the authoritative source here.

Handling:

1. Verify (HMAC) and dedupe on `X-GitHub-Delivery`, same as slash commands. Ignore if `sender.type == "Bot"` (this includes cloud-ci's own PATCH, which re-triggers `issue_comment.edited` on itself) or if `comment.id` is not the PR's tracked `pr_comments.comment_id`.
2. Diff `comment.body` against `changes.body.from` line by line. A checkbox counts as activated only if a line carrying a `cloud-ci:action` marker went from `[ ]` to `[x]`. Lines without the marker (a human's own checklist) are ignored. If the diff also shows any cloud-ci-marked line flipping `[x]` → `[ ]` in the same edit, the whole edit is treated as a bulk paste, not a click, and ignored — the next render restores the real state.
3. Role: looked up the same way as slash commands (`GET /repos/{owner}/{repo}/collaborators/{username}/permission`, cached 5 min), gated by `commands.roles`.
4. Idempotency: `(comment_id, render_seq, action_id)` in a `comment_actions` table via `INSERT OR IGNORE` — tied to the render the checkbox was clicked on, not to a line number, since scopes reorder content between renders.
5. Execution is forwarded exactly like the equivalent slash command. There is no reaction to add (edited events aren't something a bot reacts to meaningfully on its own comment); the forced re-render this triggers is itself the acknowledgment, and it naturally resets the checkbox to unchecked.

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
    RC->>GH: Check Run create/PATCH (per named check, throttled)
    RC->>D1: persist job/report state
    RC->>PRS: notify_dirty(head_sha, reason) [fire-and-forget, no ack]
    PRS->>PRS: ignore if head_sha mismatch; else coalesce, set/extend alarm
    Note over PRS: alarm fires
    PRS->>D1: load aggregate (pull_requests, runs, jobs, reports)
    PRS->>PRS: render context, apply template, apply byte budget; hash
    PRS->>R2: put full report (if hash changed)
    PRS->>GH: PATCH /issues/comments/{id} (or create)
    PRS->>D1: render_seq, rendered_hash, last_patched_at
```

### PullRequestState: one writer per PR

A PR comment aggregates many runs, and each run has its own `RunCoordinator`. Ownership belongs to the PR, not to any one run, so cloud-ci gives each PR a dedicated Durable Object, `PullRequestState`, one instance per `(repo_id, pr_number)`, addressed by `idFromName("{repo_id}/{pr_number}")`. The id is derived from stable identifiers, so any part of the Worker reaches the right instance directly; there is no lookup table, election, or claim step.

`PullRequestState` is the sole writer of the PR's sticky comment. It:
- Owns the debounce/coalescing alarm (see below) and every GitHub PATCH/create call for the comment.
- Tracks the PR's current `head_sha` in its own DO storage, seeded from the `pull_request` webhook and corrected by the ingest path for PRs that only ever see external runs.
- Receives `/cloud-ci refresh`, `/cloud-ci comment on|off`, `/cloud-ci explain`, and checkbox actions directly (see Slash command handling and Checkbox actions).

When a `RunCoordinator` records a job/shard/report event, it looks up every open PR whose head is the run's sha (`pull_requests_head` index; a sha can be the head of several stacked PRs) and sends `notify_dirty(head_sha, reason)` to each PR's `PullRequestState` instance, fire-and-forget: the `RunCoordinator` does not wait for an ack and does not retry. `PullRequestState` ignores a `notify_dirty` whose `head_sha` does not match what it has tracked; otherwise it folds the event into its pending flush. The same call fires when the first run (managed or external) for a head sha is created, so the placeholder comment goes up before any job has reported anything.

On `pull_request.synchronize` (new head sha), the same `PullRequestState` instance updates its tracked `head_sha` in place: no claim, no epoch bump, no handoff, because it was already the only writer for this PR across every sha it will ever have. There is nothing to fail over between on DO eviction either; Cloudflare restarts the instance with its storage intact, and it resumes from the alarm.

Check Runs are unaffected by any of this: they stay with the run's own `RunCoordinator`, keyed per (run, check), and never go through `PullRequestState`.

This design still assumes [./byo-ci.md](./byo-ci.md) gives every external run something that can call `notify_dirty` (a `RunCoordinator` or equivalent); see Open questions.

### Debounce and coalescing

`PullRequestState` keeps its state in local DO SQLite (`comment_state`) and drives it off the DO's own alarm directly: since each instance serves exactly one PR, there is nothing to multiplex.

| Parameter | Value | Purpose |
| --- | --- | --- |
| initial placeholder | 0 s, bypasses the quiet window and min interval, at most once per head sha (only when no `comment_id` exists yet for it) | Gets the "running" comment visible before any job output exists |
| quiet window | 4 s after the last dirty event | Waits out bursts (shards finishing together) |
| max delay | 20 s after the first unflushed dirty event | Bounds staleness under continuous activity |
| min interval | 10 s between PATCHes of one comment | Rate-limit guard |
| terminal flush | quiet window 1 s when the last run on sha turns terminal or `reason = head_changed` | Fast final state |
| hash skip | no PATCH if `sha256(body) == rendered_hash` | Avoids no-op writes (e.g. log-only events) |

Dirty reasons are typed (`run_created`, `job_state`, `report_merged`, `ai_summary_ready`, `coverage_ready`, `head_changed`, `command_refresh`, `action_executed`). Only the first `run_created` for a head sha forces the immediate placeholder path above; later ones (a second pipeline's run starting after the first) debounce normally. Log lines and resource samples never mark the comment dirty. A per-repo token bucket in `RepoState` gates all content-generating writes (comments and Check Run PATCHes): 40/min and 400/hour per repo. That stays under GitHub's general secondary limit of "no more than 80 content-generating requests per minute and no more than 500 content-generating requests per hour" (verified 2026-09-30, https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api). That limit applies per installation, not per repo, so many busy repos together can still hit it; see Open questions. When the bucket is empty, flushes are deferred, not dropped.

### Aggregation

On flush `PullRequestState` builds a `PrReport` (proto message in `cloud-ci-proto`, so the dashboard's full report page and the template context use the same model) from D1:

- **Pipelines:** every pipeline script (`.cloud-ci/pipelines/*.ts`) with a run on this head sha, used to group runs in the context (a PR can have several pipelines running independently, see D1).
- **Runs:** every run with `sha = head_sha` that is linked to the PR. Managed runs that test the merge ref record `pr_head_sha` and are matched on that. For each `(run_key, job)` only the latest attempt is shown; an earlier failed attempt that later passed counts as flaky.
- **Checks:** the named checks created for this sha (see Check Runs), for templates that want to mirror check state in the comment.
- **Scopes:** see Scopes (monorepo) above.
- **Tests:** merged totals per run, per scope. While shards are still running, partial totals are shown and labelled `partial`. After the merge barrier ([./parallelization.md](./parallelization.md)), the merged report replaces the partial sums.
- **Failures:** failed test cases ordered by (required check first, first-failure-on-PR first, job DAG order, test name). Each carries message, trimmed stack, shard, attempt, log deep link, and history link. AI summaries are attached when `ai.summaries` has a row for the failure cluster ([./ai.md](./ai.md)); the AI job reports `ai_summary_ready` asynchronously. The comment never waits on AI and never shows a placeholder.
- **Coverage:** the head report named by `coverage_report` (or the only one) is compared with the base report. The base is the coverage report of the same name from the latest completed run at the PR's merge-base sha, which comes from the compare API's `merge_base_commit` and is cached per (base, head). If no run exists at the merge-base, the latest base-branch run committed before it is used, labelled `(approximate base)`. Per-file deltas are shown only for files the PR touches or whose line coverage changed by at least 0.1 points.
- **Perf:** job wall-time deltas and benchmark report deltas, plus the run's critical path (longest job-dependency chain by wall time). Present in the context for any template that wants it, but the built-in template does not render it — the full report page always shows it (see [./analytics.md](./analytics.md)). Runner-sizing decisions are not part of this context at all; they live only on the full report page, read directly from D1 `insights`/`sizing_decisions` ([./analytics.md](./analytics.md)).
- **Reports:** site artifacts (merged Playwright, Vitest HTML, coverage HTML) at PR `latest` URLs, plus the artifact count ([./assets.md](./assets.md)), grouped by scope.
- **Flaky:** in-run retry flakes plus tests that analytics marks as known-flaky and that failed in this run.
- **Actions:** the checkbox actions enabled by `pr_comment.actions`, each with its stable id, used by the built-in template to render the Actions section and by any custom template that wants the same.

### Size budget and truncation

GitHub rejects comment bodies over the limit with `body is too long (maximum is 65536 characters)`. The REST docs do not state this limit. The source is the observed API error (verified 2026-09-30, https://github.com/orgs/community/discussions/41331, https://github.com/renovatebot/renovate/issues/15850). Check Run `output.summary` and `output.text` are documented at 65535 characters each (verified 2026-09-30, https://docs.github.com/en/enterprise-server@3.2/rest/checks/runs). However, the API has also reported the limit as a "bytesize" (https://github.com/github/docs/issues/35252). Whether the unit is characters or bytes is [unverified], so cloud-ci budgets in **UTF-8 bytes**: the byte length is at least the character length, so a byte cap is safe under either interpretation.

| Target | Hard cap (bytes) |
| --- | --- |
| PR comment body | 60,000 |
| Check Run `output.summary` | 60,000 |
| Check Run `output.text` | unused (summary only) |
| Annotation `title` | 255 chars (documented) |
| Annotation `message` | 4,096 (documented max 64 KB; we cap lower) |

The renderer is section-based: it renders the template, then walks the output looking for complete markdown blocks (a table row, a `<details>` element, a fenced code block, a per-scope group) in template-declared priority order, so truncation only ever removes whole blocks and can't leave an open fence or an unclosed `<details>`. The header (marker, title line, base/links line) and the footer are reserved first (≤ 2,000 bytes). The remaining budget goes to sections in priority order; a section's unused budget rolls to the next one. Scope grouping is a display concern within each section, not a separate budget dimension — a section's budget is spent across all of its scopes together, failing scopes first.

| Priority | Section | Soft budget | Degradation within section |
| --- | --- | --- | --- |
| 1 | failures | 34,000 | first 10 full (`<details>`); next 40 one-line (name, job, log link); rest `and N more` link |
| 2 | tests summary | 1,000 | never truncated (fixed size) |
| 3 | coverage | 5,000 | drop per-file table, keep totals |
| 4 | reports | 4,000 | keep merged sites; collapse artifacts to count link |
| 5 | flaky | 4,000 | keep top 10 by flake rate |

Per-failure caps: message 1,000 bytes, stack 20 lines / 2,000 bytes, AI summary 800 bytes. A trimmed field ends with `… (truncated, see log)`, and the cut always falls on a UTF-8 character boundary.

If the total is still over the cap after section budgets, whole sections are dropped in this order: flaky, reports (a single `Reports` link stays), coverage table, then failure stacks (one-line form for all). A final guard handles the case where the body is *still* over 60,000 bytes, which should not happen: the renderer emits the minimal form (header, status counts, `Full report` link) and logs `comment_render_overflow`. The full untruncated render is always written to R2 and shown on the dashboard's full report page, so truncation removes nothing from the record. A custom template's extra sections (perf, sizing, or anything else it chooses to render) share this same priority mechanism; a template declares each block's priority so the truncator knows where it falls.

### Untrusted content escaping

Test names, messages, stack traces, job names, and AI output come from untrusted code: fork PRs, external CI, and model output. Before any of them enters markdown:

- Inline text is escaped for markdown metacharacters, and `|` is escaped inside tables. This is wired into MiniJinja's `escape_formatter` hook (see Template rendering), so it runs automatically on every `{{ }}` interpolation.
- Text with newlines or backticks goes into a fenced block whose fence is one backtick longer than the longest backtick run in the content.
- `<!--` and `-->` are replaced with `<!-` / `->`, so content can never forge the cloud-ci comment marker or an action marker, or hide text.
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

- **Create / start / complete:** see [dynamic-pipelines.md#github-status-checks](./dynamic-pipelines.md#github-status-checks) for exactly when `ci.check(...)` creates a check and how its conclusion is derived from attached nodes. This doc covers only the GitHub API mechanics below.
- **Progress:** summary PATCHes at most once per 30 s per check, and only when a shard finishes or the test count changes.
- **Annotations:** at most 50 per request, appended by further PATCHes (verified 2026-09-30, https://docs.github.com/en/rest/checks/runs). cloud-ci caps each Check Run at 200 annotations and puts a "more in dashboard" note in the summary.
- **Actions:** at most three (documented: label ≤ 20 chars, identifier ≤ 20, description ≤ 40, verified via the GitHub REST API OpenAPI description 2026-10-01, https://github.com/github/rest-api-description/blob/main/descriptions/api.github.com/api.github.com.json). On failed jobs: `Rerun failed` (`rerun_failed`), `Explain` (`explain`). On running jobs: `Cancel` (`cancel`). `check_run.requested_action` is authorized like the slash command of the same name.
- **Re-run from the GitHub UI:** `check_run.rerequested` maps to `rerun <job>`, and `check_suite.rerequested` maps to `rerun failed` for the sha. The rerun creates a new Check Run with the same name. GitHub keeps at most 1000 same-named check runs per suite and deletes older ones automatically (verified 2026-09-30, same source), which is harmless here.

Check Run writes come from the run's own `RunCoordinator`, not `PullRequestState`. Each check belongs to exactly one job's worth of attached nodes at a time, so ordering is already guaranteed by the per-run single-writer DO.

### Slash command handling

1. The webhook (`issue_comment`, action `created` only) is verified (HMAC) and deduplicated on `X-GitHub-Delivery`. Events whose `sender.type == "Bot"` and events not on a PR are ignored.
2. The comment is parsed into commands, and each command gets a row `(comment_id, line_no)` in `comment_commands` via `INSERT OR IGNORE`. A conflict means the command is already handled, so a redelivery is a no-op.
3. Role: `GET /repos/{owner}/{repo}/collaborators/{username}/permission` (cached 5 min per user/repo in D1) maps `read`/`triage` to viewer, `write` to operator, and `maintain`/`admin` to admin, per [./auth.md](./auth.md), then applies any `commands.roles` override from settings.yml.
4. Acknowledgement: add reaction `eyes` on receipt, `rocket` on success, `confused` on denial or parse error (reaction contents verified 2026-09-30, https://docs.github.com/en/rest/reactions/reactions). Only `help` and errors produce a reply comment. Reply comments carry no marker and are never edited.
5. Execution: `rerun`/`cancel` are forwarded to the target run's `RunCoordinator`. `refresh`, `comment on|off`, and `explain` are routed directly to the PR's `PullRequestState` instance, addressed by `idFromName("{repo_id}/{pr_number}")`; no lookup is needed. `refresh` is limited to once per 60 s per PR.

Checkbox actions follow the parallel flow in Checkbox actions above, reusing the same role lookup and `RunCoordinator`/`PullRequestState` dispatch as step 5.

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
  disabled INTEGER NOT NULL DEFAULT 0,  -- /cloud-ci comment off (overrides pr_comment.enabled for this PR)
  suppressed_sha TEXT,                  -- human deleted the comment for this sha
  frozen INTEGER NOT NULL DEFAULT 0, last_error TEXT,  -- mirrors PullRequestState's authoritative local state
  PRIMARY KEY (repo_id, pr_number)
);

CREATE TABLE check_runs (
  run_id TEXT NOT NULL, job_id TEXT NOT NULL,   -- job_id '' for per-run checks
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

CREATE TABLE comment_actions (
  repo_id INTEGER NOT NULL, comment_id INTEGER NOT NULL, render_seq INTEGER NOT NULL,
  action_id TEXT NOT NULL, pr_number INTEGER NOT NULL, sender_login TEXT NOT NULL,
  role TEXT NOT NULL, result TEXT NOT NULL,  -- ok | denied | noop
  created_at INTEGER NOT NULL, PRIMARY KEY (comment_id, render_seq, action_id)
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
- **Template trust:** a custom template is repo config (read from the repo's default branch, never the PR base ref or head), trusted the same as settings.yml; it is not sandboxed against its author. Only the untrusted *values* passed into it are escaped.
- **Command and action authorization:** every slash command and checkbox click is checked against the live repo permission of `sender`, never `author_association`, which shows association rather than permission, gated additionally by `commands.roles`. Fork authors without write get viewer commands only. `autofix` also requires the repo-level opt-in from [./ai.md](./ai.md).
- **Checkbox loop avoidance:** cloud-ci's own PATCH to reset a checkbox triggers another `issue_comment.edited`; that event's `sender` is the App's own bot identity, so it's filtered before any diffing happens (see Checkbox actions).
- **Fork PR config:** `pr_comment.*`, `checks.*`, and `commands.roles` are all read from the repo's default branch, never the fork's branch or the PR base ref (see Configuration).
- **Cost abuse:** `explain` and AI summaries are operator-gated or config-gated by [./ai.md](./ai.md), and `refresh` is rate-limited per PR.
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
| AI summary slow or failing | Missing `ai.summaries` row | Comment renders without it; no placeholder. |
| Base coverage missing | No base report | Coverage section shows head totals with `no base report`. |
| Template fails to parse/render | Error from MiniJinja | Fall back to the built-in template for this render, log `pr_comment_template_error`. |

## Open questions

1. **Installation-wide rate budget.** Secondary limits apply per App installation, and a per-repo `RepoState` bucket cannot see other repos. Options: a single global limiter DO, or per-repo buckets sized as the installation budget divided by active repos. [unverified: whether GitHub counts App installation tokens per installation or per App for secondary limits]
2. **Do Check Run PATCHes count as content-generating requests?** [unverified]. If not, they can leave the shared bucket.
3. **Characters or bytes for the 65536 comment limit?** The design is safe either way. Confirming would recover up to about 5% of the budget for ASCII-heavy bodies.
4. **Comment for merge-ref builds.** If a pipeline script builds `refs/pull/N/merge`, the head shown is still the PR head sha, with the merge sha in the dashboard only. Confirm with [dynamic-pipelines.md](./dynamic-pipelines.md).
5. **External runs without a `RunCoordinator`** (depends on [./byo-ci.md](./byo-ci.md)). `PullRequestState` only ever receives `notify_dirty`; if external runs are not given a `RunCoordinator` or equivalent, the ingest path itself must call `notify_dirty` after each write.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| New comment per push or per run | Noisy timeline, notifications on every push, and reviewers must scroll to find the current state. |
| Check Runs only (no comment) | Summaries sit behind the Checks tab and can't show a cross-run, cross-CI view on one screen. The comment is opt-in but gives a single-screen view checks can't. |
| Edit the PR description | Conflicts with author edits and needs broader permissions. |
| Splitting overflow across multiple comments | Upsert becomes multi-object with ordering problems; dashboard link is simpler. |
| Debounce via Queue delivery delay | No coalescing (every event still produces a message), and no single writer, so stale PATCHes can still race. |
| Debounce in `RepoState` (per repo) | One DO per repo would serialize comment rendering for every open PR in a busy monorepo; a dedicated `PullRequestState` DO per (repo, pr) isolates load per PR instead. |
| Make a run's `RunCoordinator` the comment writer (the brief's original assignment) | A `RunCoordinator` is scoped to one run, but a PR's comment spans every run, managed and external, across every push, for its head sha. Ownership belongs to the PR, not to any single run. |
| Elect a comment host among `RunCoordinator`s via `host_epoch` CAS, with a `RepoState` fallback | Leader-election and failover logic (claim, epoch fencing, re-claim on timeout) for a role a dedicated per-PR DO holds natively. `idFromName("{repo_id}/{pr_number}")` gives a stable, collision-free address with no claim protocol and no failover window. |
| Store state only in the comment body (stateless sticky-comment actions) | Requires a list/scan on every update and trusts content anyone can edit or delete. D1 is the source of truth, and the marker is only for recovery. |
| Minimize old comments via GraphQL `minimizeComment` instead of editing | Still creates one comment per push. Editing in place is quieter. |
| Fixed, non-templated comment layout (previous design) | Couldn't drop the per-job table/perf/sizing without a code change, and repos with different needs (more or less detail) had no way to adjust it. A template makes the short default changeable per repo without a cloud-ci release. |
| Hard-coded section allowlist (`pr_comment.sections: [...]`) instead of a template | Only ever let repos hide/reorder a fixed set of built-in blocks; couldn't add repo-specific content or change wording. Superseded by the template path. |
