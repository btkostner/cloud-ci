# AI analysis enqueue: outbox, sweep, or accepted risk

Status: **DRAFT** - design only, no decision. Nothing here is implemented. Options are compared and one is recommended, but the owner decides (see [Owner questions](#owner-questions)).

> Code citations are `file:line` against commit f600e4d on branch `outbox-design`. Cloudflare facts are dated and sourced inline; anything not checked is marked `[unverified]`.

## Problem

`RunCoordinator::handle_close_run` enqueues one `AnalysisRequested` message when a run first becomes terminal (`packages/cloud-ci-worker/src/coordinator/mod.rs:2384`). The helper `enqueue_analysis_requested` (`coordinator/mod.rs:2401-2422`) logs and swallows both a missing `ANALYSIS_QUEUE` binding (`:2406-2414`) and a failed `queue.send` (`:2416-2421`). Nothing is persisted. If the send does not happen, that run never gets an AI summary, and nothing downstream notices.

The other side effects of a close are recoverable. `finalize_test_stats` is gated on the D1 `test_stats_applications` marker and is retried by redelivery (`coordinator/mod.rs:2300-2308`, `:2476-2484`). Shard-terminal effects are retried by the alarm (`alarm()`, `:990-1002`). Neither pattern covers `AnalysisRequested`.

### Failure modes (what is lost, and when)

All of these end the same way: no `AnalysisRequested` message exists, no `ai_insight` row exists, and the PR comment has no AI section.

| # | Cause | In the roadmap gap? |
| --- | --- | --- |
| F1 | `queue.send` returns an error (transient Queues API error, or the Queue is paused or deleted). Logged at `coordinator/mod.rs:2417`. | yes |
| F2 | `env.queue("ANALYSIS_QUEUE")` fails: the binding is missing or misnamed after a deploy. Logged at `:2409`. Systematic, not transient: every run loses its summary. | yes |
| F3 | An earlier step in `handle_close_run` returns an error with `?` after `update_run_status` has committed: `project_run_to_d1` (`:2366`), `finalize_check_runs` (`:2367`) or `finalize_test_stats` (`:2368`). The enqueue at `:2384` is never reached. The retry of the close takes the already-terminal early return (`:2300-2308`), which calls only `finalize_test_stats` and never the enqueue. The summary is lost with no Queue failure at all. | **no** (see [Contradictions](#contradictions-with-the-roadmap-and-existing-docs)) |
| F4 | The DO is evicted or crashes after `update_run_status` (`:2356`) and before the enqueue. Same retry path, same loss. | no |
| F5 | The message is delivered, but the consumer exhausts `max_retries = 3` and the message goes to `cloud-ci-analysis-dlq` (`wrangler.toml:112-117`). No code reads that DLQ (a grep for `dlq` in `packages/cloud-ci-worker/src` finds nothing), so nothing stores the `error` insight that `ai.md:379` promises. Out of scope here, because the outbox cannot fix it, but it ends in the same state. | no |

F1-F4 are the producer side and are what this document addresses. F5 is noted so the consumer-side contract is not oversold: an outbox guarantees that the message is *delivered to the Queue*, not that a summary results.

### What the user sees when a summary is missing

The consumer's own skip paths write no row either: AI disabled (`lib.rs:340`), over the daily cap (`lib.rs:351`), no failing-test data (`lib.rs:392`). The PR comment section is omitted on `error` and `invalid_output` (`ai.md:379-380`) and carries a one-line note only for `skipped_budget` (`ai.md:286`, `:382`). So a lost enqueue is **indistinguishable from "no failures" or "AI off"**. There is no operator-visible signal at all.

## Constraints from AGENTS.md

| Invariant (AGENTS.md "Invariants") | What it means here |
| --- | --- |
| Only a run's `RunCoordinator` writes run state | The outbox is coordinator-owned state in the run's own DO storage. A sweep or consumer must never write `run` or `job` status. |
| Inputs enqueue, coordinators decide | The `AnalysisRequested` payload stays `{run_id, repo_id}` (`ai_queue.rs:111-115`). The consumer already re-reads D1 and ignores payload state (`lib.rs:299-330`). The outbox keeps that: it is a delivery guarantee for a *signal*, not a carrier of decisions. |
| Uploads are idempotent | Not directly touched. The same discipline applies to the consumer: delivery is at-least-once, so the consumer must be idempotent per run (below). |
| D1 migrations are forward-only and safe on a live deployment | Any D1 change below is additive and must tolerate existing rows. |

## Retry and idempotency semantics (shared by Options A and C)

Cloudflare Queues deliver **at least once**; a message may rarely be delivered more than once, and Cloudflare recommends a unique id on the message used as a primary key or idempotency key (https://developers.cloudflare.com/queues/reference/delivery-guarantees/, last updated 2026-04-21, read 2026-10-03).

### The consumer is not idempotent today

`handle_analysis_requested` inserts one `ai_insight` row per assembled entry in a loop with a fresh ulid each time (`lib.rs:396-438`, `INSERT` at `:410`). The table has only a non-unique index on `(repo_id, kind, fingerprint, diff_hash)` and one on `run_id` (`migrations/0016_ai_usage_and_insight.sql:46-47`), and `id` is the only key (`:34`). Consequences:

- **Duplicate delivery** of the same message inserts a second full set of `pending_model_call` rows, and the `*/5` model-call pass (`lib.rs:166`) calls the model twice for the same failures and spends the budget twice.
- **Retry after partial success**: if the INSERT for entry *k* fails, `handle_analysis_requested` returns `Err`, the invocation fails, and the Queue redelivers. The retry re-inserts entries 0..k-1. This is reachable *today* with no outbox.

An outbox makes duplicates more likely (a send can succeed on the Queue side and still report failure to the DO, so the retry sends again). **The consumer fix is therefore a prerequisite of Option A or C, not an extra.**

### Idempotency key

- **Key:** `analysis:v1:<run_id>`. One analysis request per run. The `RunCoordinator` DO is per run, and a run goes terminal once, so `run_id` is sufficient. The `v1` is the message-contract version, so a future second analysis kind (flaky hints enqueue from the cron rollup, `ai.md:111`) cannot collide.
- **Where it lives:** the outbox row's primary key (producer side), carried in the message as a new field with a serde default so messages already in flight, without the field, still decode (`ai_queue.rs:111` derives `Deserialize`; the field must be `#[serde(default)]`-compatible). The consumer derives the key from `run_id` if the field is absent.
- **Consumer dedupe:** a D1 marker table `ai_analysis_applied (run_id PRIMARY KEY, applied_at)`, written in the **same `batch()`** as the `ai_insight` inserts, so the marker and rows commit together or not at all. This copies the `test_stats_applications` pattern (`coordinator/mod.rs:2552-2564`, and the prose at `:2476-2484`). A redelivery finds the marker and acks without inserting. A partial-success retry finds no marker and no rows (the batch rolled back) and starts clean. That D1 `batch()` is atomic is `[unverified]` here; the repo already relies on it for `test_stats_applications`, so it must be confirmed against the D1 docs before this is built.
- **Why not a unique index on `ai_insight`:** existing rows may already contain duplicates (the bug above), and a forward-only migration that adds a `UNIQUE` index would fail on a live deployment that has them. A new marker table needs no data cleanup.

### Reordered delivery

There is exactly one message per run, so reordering across messages cannot change a run's outcome. What can reorder is the message against the D1 projection of the run: the consumer reads D1 `runs`, `jobs`, `reports` (`lib.rs:312-368`), and the close path projects those *before* it enqueues (`coordinator/mod.rs:2348`, `:2366`). With an outbox that may drain from an alarm, the send must stay ordered after a successful projection, or the consumer can read incomplete D1 state, take the "no failing-test data" skip path (`lib.rs:392`), return `Ok`, and lose the summary silently. The outbox row is therefore marked **ready** only after the projections succeed (Option A, step 2), and the consumer should treat "run not yet terminal in D1" as a retryable error rather than a skip. See [prerequisites](#unresolved-implementation-prerequisites).

## Option A: coordinator outbox plus alarm retry

### Table (RunCoordinator DO SQLite)

One row per run, added in `ensure_schema` (`coordinator/mod.rs:4350`), which runs on every `fetch` and every `alarm` (`:886`, `:983`):

| Column | Meaning |
| --- | --- |
| `key TEXT PRIMARY KEY` | `analysis:v1:<run_id>` |
| `repo_id INTEGER NOT NULL` | payload field |
| `state TEXT NOT NULL` | `pending` / `sent` / `stuck` |
| `attempts INTEGER NOT NULL` | failed sends so far |
| `next_attempt_at INTEGER NOT NULL` | epoch ms; due time |
| `last_error TEXT` | last send error string, truncated |
| `created_at INTEGER NOT NULL`, `sent_at INTEGER` | audit |

DO-local only, like `shard_terminal_effect` (`:4527-4541`): no D1 projection for the row itself, apart from the stuck signal below.

### Flow

1. **Record intent with the state transition.** In `handle_close_run`, `INSERT OR IGNORE` the row with `state = 'pending'` and `next_attempt_at = now` *before* `update_run_status` (`:2356`), in the same synchronous section with no `await` between the two statements. Because the insert is `OR IGNORE` and keyed on `run_id`, a retried close can never create a second row. If the DO dies between the two statements, the row exists for a non-terminal run; the drain must treat that as "not ready" (it only sends for a terminal run) and the retried close takes the normal path and re-inserts harmlessly. That DO SQL writes within one synchronous section are atomic is `[unverified]` here; the ordering above is chosen so correctness does not depend on it.
2. **Try an inline send, then fall back.** After `project_run_to_d1` succeeds (`:2366`) the coordinator sends once inline, for latency, as today. On success it marks `sent`. On failure it increments `attempts`, sets `next_attempt_at` from the backoff table, arms the alarm, and does not fail the close response (the F1 posture is kept). Failures of `finalize_check_runs` or `finalize_test_stats` no longer prevent the enqueue, because the row already exists (this closes F3 and F4): the outbox drain, not the close path, owns delivery.
3. **Drain from `alarm()`** (below), and also from the terminal-guard early return so a retried close can kick the drain.
4. **Give up visibly.** After the retry cap the row becomes `stuck`.

### Retry cap, backoff, stuck state

- **Backoff:** 30 s, 2 min, 10 min, 30 min, 2 h, 6 h after failed attempts 1-6. The first three match the consumer-side `delaySeconds` ladder in `ai.md:130` (30 s, 120 s, 600 s); the rest extend it so an hours-long Queues outage is survived. These numbers are proposals, not measured. Jitter is unnecessary: the DO is per run and sends to a different queue shard from other runs only by chance `[unverified]`.
- **Cap:** 7 total sends (1 inline plus 6 retries), about 9 hours of coverage. Past that, `state = 'stuck'`.
- **Stuck state, operator-visible:** (a) a structured `console_log!` line with a stable prefix for log search; (b) a coordinator-written row so that it shows in D1 and the dashboard. Candidates: an `ai_insight` row with a new `status = 'enqueue_failed'` (`run_id`, no `context_json` content), or a dedicated `ai_outbox_stuck` table. The row is coordinator-written but is not run state, so the first invariant holds; the choice and the status name are an owner question. (c) A stuck row is never deleted automatically, and an admin action (re-arm: reset `attempts`, set `state = 'pending'`) is a prerequisite for operating it, not part of this design.
- **A `sent` row is retained** for the lifetime of the DO, which is the run's lifetime. This is cheap and answers "did we ever enqueue this run?".

### Alarm composition and timeout non-starvation

The DO has one alarm slot (`coordinator/mod.rs:1732-1736`). Today it is shared between the run timeout, shard-terminal retries and overflow flushing. Adding the outbox must not weaken the rule documented at `:962-970`: the run's own timeout is never starved by a permanently-stuck retry.

- **Outbox rows exist only after a run is terminal.** Step 1 inserts at close. For a non-terminal run there is no outbox row, so the outbox can never delay a pending timeout close. The timeout close itself (`alarm()` `:1029-1030`) goes through `handle_close_run(by_timeout = true)`, which inserts the row, so a timed-out run gets a summary too.
- **Terminal runs hit an early return** in `alarm()` (`:1023-1026`), which calls `release_alarm_if_idle` and returns. The drain must run **before** that return, next to the shard-effects loop (`:990-1002`), or an outbox row on a terminal run would never be drained after the first alarm fire. This is the one real change to `alarm()`.
- **`has_pending_background_work`** (`:1725-1730`) gains a third term: any outbox row with `state = 'pending'`. `stuck` and `sent` rows are not pending work, so a stuck row cannot hold the alarm forever. This matches how `read_pending_shard_terminal_effect_keys` excludes `legacy_unknowable` rows (`:1719-1721`).
- **Alarm time is `min(next_attempt_at over pending rows, existing candidates)`**, not a flat delay. `release_alarm_if_idle` currently re-arms through `schedule_overflow_flush_alarm`, which arms a fixed `now + 5 s` (`:1706-1716`, `OVERFLOW_FLUSH_DELAY_MS = 5_000` at `:871`). That is wrong for a 6-hour backoff: it would wake the DO every 5 seconds. A variant that arms an absolute time, still guarded by `logic::should_advance_alarm` (`logic.rs:1322`), is needed.
- **Alarm handler must not throw.** Cloudflare retries an `alarm()` that throws with exponential backoff from 2 s, up to 6 retries, and recommends catching exceptions and scheduling a new alarm (https://developers.cloudflare.com/durable-objects/api/alarms/, last updated 2026-04-21, read 2026-10-03). The drain catches send errors, records them in the row, and arms the next attempt itself. `alarm()` already takes this posture for shard effects (`:992-1000`).
- **One alarm, many due items:** Cloudflare documents the pattern of storing the schedule in storage and having `alarm()` process due items then reschedule (same page). The outbox follows it.

### Live-DO schema change, guarded migration

A DO's storage is not migrated centrally. `ensure_schema` runs on every request and every alarm (`:886`, `:983`), so each run's DO migrates itself on first touch after deploy.

- A **new table** needs only `CREATE TABLE IF NOT EXISTS`, which is idempotent and never alters existing tables, matching every other table in `ensure_schema` (`:4350-4600`).
- If a later change *adds a column* to the new table, it must use the `pragma_table_info` guard, not an unconditional `ALTER` with swallowed errors, exactly as the `run.settings_sha` retrofit does (`:4366-4393`) and as `node.physical_address` does (`:4483-4500`): that guard avoids hiding a real ALTER failure.
- **Runs already terminal at deploy have no outbox row.** Their DO storage cannot tell us whether the original enqueue succeeded. They are neither backfilled nor retried by Option A. This is a one-time gap bounded by the deploy date, and only Option C (below) can cover it.
- **Rollback safety:** the extra table is ignored by older code, so a rollback is safe. Rows written by newer code that an older deployment never drains simply stay `pending` until the newer code is redeployed `[INFERENCE]`.
- **Message contract:** adding an optional `idempotency_key` to `AnalysisRequested` is backward compatible for in-flight messages only if the field is optional on decode (see above). `AnalysisRequested` is a Rust `serde` struct (`ai_queue.rs:111-115`), not a protobuf message; no `.proto` file defines it (a repo-wide search for `AnalysisRequested` in `*.proto` finds nothing), so the `buf breaking` rule does not apply to it.

### How Option A complies with the invariants

- *Only RunCoordinator writes run state:* the outbox is DO-owned, and the only cross-boundary write is the optional stuck row (not run state).
- *Inputs enqueue, coordinators decide:* the coordinator decides *when* to send and *whether* the run is ready; the consumer still re-reads D1 and trusts nothing in the payload.
- *Idempotency:* producer side by `INSERT OR IGNORE` on `key`; consumer side by the `ai_analysis_applied` marker in one D1 batch.

## Option B: accept the risk, document it

Keep `enqueue_analysis_requested` as is, and record the decision in `ai.md`'s Failure modes table (`ai.md:375-387`) and in the roadmap.

- **Cost to the user:** the AI summary for that run is permanently absent. CI results, Check Runs and test stats are unaffected, and `ai.md` states that CI status is never affected (`ai.md:379`). The user cannot tell a lost summary from "no failures" or "AI off" (see above).
- **What the PR comment shows:** nothing. The section is omitted, with no "AI summary unavailable" note, because the coordinator has nowhere to record that anything was lost. Unlike `skipped_budget` (`ai.md:286`), there is no row to render. A cheap partial mitigation, which still needs a row, would be a coordinator-written marker, which is most of Option A's stuck state.
- **Likelihood:** `[INFERENCE]` low for F1 and F4 (a Queues API error or a DO crash in a window of a few awaits); high and total for F2 (a misnamed binding) until someone notices; F3 depends on how often `finalize_check_runs` and `project_run_to_d1` fail. This design has not measured any of these.
- **Not recommended alone** because F2 and F3 are loud-failure candidates that this option leaves silent. If chosen, F3 should still be fixed (see prerequisites) since it is a plain ordering bug, not a delivery-guarantee question.

## Option C: D1 sweep over terminal runs with no analysis marker

A cron pass finds D1 `runs` rows that are terminal, recent, and have no `ai_analysis_applied` marker (or `ai_insight` row), and re-enqueues `AnalysisRequested` for them.

- **Why it fits the invariants:** the sweep only *enqueues*; it writes no run state; the consumer re-reads D1 and is idempotent per `run_id`. This is "inputs enqueue, coordinators decide" in its plain form.
- **It cannot distinguish "lost" from "intentionally skipped".** The consumer returns `Ok` with no `ai_insight` row on AI disabled, over cap, and no failing tests (`lib.rs:340`, `:351`, `:392`). A sweep keyed on "no `ai_insight` row" would re-enqueue every green run and every AI-off run on every pass. Option C needs the consumer to write an outcome marker for **every** terminal outcome (`applied`, `skipped_settings`, `skipped_budget`, `no_failures`). That is a consumer change roughly the size of the idempotency fix, and it also gives the PR comment a row to render.
- **No free cron slot.** `lib.rs:152-175` already handles three schedules (`*/15`, `*/20`, `*/5`). Cloudflare's alarms page states "A Worker can have up to three Cron Triggers configured at once" (https://developers.cloudflare.com/durable-objects/api/alarms/, 2026-04-21); the limit may differ by plan `[unverified]`. The sweep must therefore piggy-back on an existing handler, most naturally the `*/5` `ai_model_call_pass`.
- **Coverage differs from Option A:** it recovers F1, F2 (once the binding is fixed) and the pre-deploy gap in Option A. It does **not** recover a run that never reached D1 as terminal (an F3 failure in `project_run_to_d1`, `coordinator/mod.rs:2366`), because the sweep reads D1, and that D1 row is stale. Latency is the cron period, not seconds.
- **Cost:** a D1 scan every pass, bounded by a time window and an index (the existing `idx_ai_insight_run`, `0016:47`, helps the anti-join but a `runs.status, updated_at` index may be needed `[unverified: runs indexes not read]`).

## Comparison

| | A: outbox + alarm | B: accept risk | C: D1 sweep |
| --- | --- | --- | --- |
| Covers F1 (send error) | yes | no | yes |
| Covers F2 (bad binding) | yes, retries until fixed or stuck | no | yes, after fix |
| Covers F3/F4 (close aborted after commit) | yes | no | only if D1 projected |
| Pre-deploy runs | no | no | yes |
| Operator-visible lost state | `stuck` row | none | indirect (absent marker) |
| New DO storage | one table | none | none |
| New D1 | marker table (+ optional stuck status) | none | marker/outcome rows for every outcome |
| Touches `alarm()` | yes, carefully | no | no |
| Needs consumer idempotency | yes | **already needed** (duplicate delivery) | yes |
| Latency to recovery | seconds to hours (backoff) | never | cron period |
| Invariant risk | low if the drain stays inside the DO | none | low |
| Relative size | medium | docs only | medium-large |

### Recommendation (not a decision)

**Option A**, preceded by the consumer idempotency fix, which is needed under every option because duplicate delivery and partial retries are reachable today. A is the only option whose recovery is driven by the component that already owns the run's lifecycle, and it follows the established `shard_terminal_effect` and `has_pending_background_work` pattern. It also closes F3/F4, which the roadmap item does not mention but which lose the same summary without any Queue failure. Option C is a reasonable later backstop for the pre-deploy gap and for projection failures, once the consumer writes outcome markers for the PR comment anyway. Option B is defensible only if the owner explicitly values the least moving parts over a lost summary; if so, the F3 ordering bug should still be fixed.

## Owner questions

1. Is a permanently lost AI summary acceptable at all? If yes for rare Queue errors, is it acceptable for a misconfigured binding (F2)?
2. If A: is a retry window of about 9 hours (cap of 7 sends) right, or should a stuck row be retried by a manual admin action only?
3. How should a stuck or missing summary appear: only logs, a D1 row for the dashboard, or a one-line PR comment note like `skipped_budget` (`ai.md:286`)? This decides the stuck-state storage.
4. Should the consumer write an outcome row for every terminal outcome (the PR comment can then explain "AI off" or "no failures" versus "lost")? That is a user-visible behavior change to `ai.md`.
5. Is the pre-deploy gap (runs terminal before the outbox ships) acceptable, or is Option C's sweep wanted as a one-time or standing backstop?
6. Is the DLQ (`cloud-ci-analysis-dlq`) to be consumed so F5 also yields an `error` insight (`ai.md:379`)? That is separate work.
7. Is spending the third `*/5` cron handler for a sweep acceptable, given all three Cron Trigger slots are in use?

## Test plan

### Pure logic (plain `cargo test`, no `worker` dependency; follows `coordinator/logic.rs` and `ai_queue.rs` conventions)

- Backoff function: attempt number to delay, bounded at the table's last entry; cap reached yields `stuck`; never negative or overflowing.
- Next-alarm selection: `min(next_attempt_at, timeout deadline, overflow retry)` for combinations including none pending; the timeout always wins when it is earlier (extends the `should_advance_alarm` and `clamp_retry_delay_to_deadline` tests, `logic.rs:2886-2930`).
- Outbox state transitions as a pure `(state, attempts, send_result) -> (state, attempts, next_attempt_at)` function: success from `pending`; failure below the cap; failure at the cap; `sent` and `stuck` are absorbing; replay of an already-`sent` row is a no-op.
- Idempotency key derivation from `run_id`, and decode of `AnalysisRequested` both with and without the key.
- `has_pending_background_work` gate as a pure predicate over row states: `pending` counts, `stuck` and `sent` do not.

### DO-level behavior (needs a harness that does not exist today)

The repo's own convention is that coordinator wiring is "exercised only by the live smoke test" (`ai_queue.rs:94-98`). A real harness (for example Miniflare/workerd with the DO and a fake Queue binding) does not exist in this repo `[unverified: not searched beyond the worker crate]`, and building it is a prerequisite. With one, the cases are:

- `handle_close_run` inserts exactly one outbox row, even when called twice (the second call takes the terminal guard).
- A failing inline send leaves a `pending` row with `attempts = 1` and an alarm armed at its `next_attempt_at`, and the close response is still successful.
- F3: a forced failure in `finalize_test_stats` leaves the outbox row `pending`, and the alarm still delivers.
- Alarm on a terminal run drains the outbox before returning (the early-return reorder).
- Timeout non-starvation: a non-terminal run with a due timeout closes by timeout regardless of outbox state, and the resulting run still gets an outbox row.
- A `stuck` row neither re-arms the alarm nor blocks `release_alarm_if_idle` from deleting it.
- Guarded migration: start a DO with the pre-change schema (no outbox table), call a handler, and assert the table exists and old rows are untouched.
- Consumer: duplicate delivery inserts one marker and one set of rows; a simulated D1 failure on entry *k* leaves zero rows and no marker, and the retry produces a full set.

### Needs a live run

- A real `queue.send` failure (pause or detach the Queue in a staging deployment) and a real recovery via alarm backoff.
- At-least-once duplicate delivery from real Queues cannot be forced; only the dedupe logic can be proven offline.
- DO eviction between the status update and the send (F4) cannot be reproduced deterministically; rely on the ordering argument plus the harness test above.
- Deploy upgrade over a live DO that holds pre-change storage.

## Unresolved implementation prerequisites

Technical items that must be settled before or during implementation; separate from the owner questions above.

1. **Consumer idempotency (blocking, independent of A/B/C).** Add the `ai_analysis_applied` marker table (additive D1 migration, number after `0020`) and write the marker plus all `ai_insight` rows in one D1 `batch()`. Confirm the D1 docs state that `batch()` is atomic `[unverified]`.
2. **Consumer retryability on stale D1.** The consumer must raise an error, not skip, when D1 does not yet show the run as terminal or reports are not yet projected (`lib.rs:392` currently skips with `Ok`). Decide what "projected" means (a D1 `runs.status` check is the minimum).
3. **Order the close path.** Decide whether the outbox row is "ready" only after `project_run_to_d1` succeeds, or whether the drain itself re-verifies. Define behavior if `project_run_to_d1` fails permanently.
4. **`alarm()` restructuring.** Move the outbox drain before the terminal early return (`coordinator/mod.rs:1023`), add the third term to `has_pending_background_work` (`:1725`), and add an absolute-time arm helper beside `schedule_overflow_flush_alarm` (`:1706`) so a multi-hour backoff does not wake the DO every 5 s.
5. **Atomicity of the outbox insert with the status update.** Confirm whether DO SQL writes between `await`s are atomic, or keep the documented "insert before status, `OR IGNORE`" ordering. No `await` may be introduced between the two statements.
6. **Stuck-state storage and surface.** Pick a coordinator-written D1 row or table, add the status value to the `ai_insight.status` set (which `ai.md:312` and `migrations/0016:42` document differently), and decide how the dashboard or PR comment renders it.
7. **Admin re-arm path.** A way to reset a `stuck` row, through a coordinator RPC and not a direct DO storage write.
8. **Message contract change.** An optional `idempotency_key` on `AnalysisRequested` (`ai_queue.rs:111-115`) with a decode-compatible default; confirm no other producer (the cron rollup enqueue in `ai.md:111`, not found in the code I read) constructs this struct.
9. **Test harness.** No DO-level harness exists; scope it, or accept that the DO-level cases ride the live smoke test only.
10. **Fix the `ai.md` / migration schema drift** before extending `ai_insight.status`. `ai.md:306-317` describes `input_hash`, `model`, `body_json` and statuses `ok|invalid_output|skipped_budget|skipped_settings|error`, while migration 0016 (`:33-44`) has `diff_hash`, `prompt_version TEXT`, `context_json` and `pending_model_call`. A new status must be added to whichever is authoritative.
11. **DLQ handling** (F5) is unowned: nothing consumes `cloud-ci-analysis-dlq`. Decide whether it is in scope for this work.
12. **Cron slot and index.** If Option C is chosen, confirm the Cron Trigger limit for the deployment's plan and whether the D1 sweep query needs a new index.

## Contradictions with the roadmap and existing docs

Noted while reading; none are changed by this document.

- **`docs/roadmap.md` has no entry for this gap.** A search of `docs/roadmap.md` for `AnalysisRequested`, `outbox`, `swallowed` and `enqueue` finds nothing. The task statement quotes the gap; the roadmap in this checkout does not contain it.
- **The code comment at `coordinator/mod.rs:2369-2383` overstates the guarantee.** It argues that "only the state transition itself enqueues" is a complete idempotency discipline. That holds against duplicate enqueue but not against a *missed* enqueue: any error after `update_run_status` (F3, F4) skips the enqueue and the retry's early return (`:2300-2308`) never reaches it.
- **The same comment says there is "no D1-level dedupe anywhere downstream yet".** That is accurate and is the duplicate-delivery hole described above, but `ai_queue.rs:105-110` and `ai.md` describe the consumer as if one message produced one set of insights.
- **`ai.md:130` describes `delaySeconds` backoff (30 s, 120 s, 600 s) on a 429; the consumer never calls `message.retry` with a delay.** On a handler error it returns `Err` (`lib.rs:222`), which relies on the Queue's default retry timing. The backoff in `ai.md` is not implemented.
- **`ai.md:379` says a DLQ'd analysis is "stored as `error`".** Nothing reads the DLQ, so nothing is stored.
- **`ai.md:296-318` (data model) does not match migration 0016** (see prerequisite 10).
- **`wrangler.toml:112-117` matches `ai.md:130` on `max_batch_size = 1`, `max_retries = 3` and the DLQ name.** No contradiction there; recorded so it is not re-checked.

## Sources

- Cloudflare Queues, Delivery guarantees (at-least-once; use a unique id as an idempotency key): https://developers.cloudflare.com/queues/reference/delivery-guarantees/ , page last updated 2026-04-21, read 2026-10-03.
- Cloudflare Durable Objects, Alarms (single alarm per DO; at-least-once; retry on uncaught exception with exponential backoff from 2 s, up to 6 retries; catch and re-arm; schedule-in-storage pattern; three Cron Triggers per Worker): https://developers.cloudflare.com/durable-objects/api/alarms/ , page last updated 2026-04-21, read 2026-10-03.
- Not verified, marked inline: D1 `batch()` atomicity; DO SQL write atomicity across a synchronous section; Cron Trigger limit by plan; queue-shard distribution; existence of a DO test harness.
