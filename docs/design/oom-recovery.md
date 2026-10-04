# `runner: "auto"` real-time OOM recovery: wiring design

Status: **design only, 2026-10-03.** No code accompanies this document. It replaces the
"remaining prerequisites" prose in [parallelization.md](./parallelization.md#runner-auto-oom-recovery-pure-logic-only-not-wired-2026-10-03)
and the analytics policy in [analytics.md](./analytics.md) ("OOM retry", "Instance selection")
with a concrete, reviewable plan. Policy is unchanged: **one** automatic retry per shard/node
lineage, on the next size up, bypassing hysteresis; a second OOM fails for good naming the
configured `max` and the measured peak. `managed` runs only; a no-op for `external` runs.

Facts about Cloudflare are dated and sourced or marked `[unverified]` (AGENTS.md invariant).
Symbols named below were read in the tree at HEAD 4e53880.

## 0. Why the earlier wiring failed, mapped to this design

| Review finding | Root cause | Where this design closes it |
| --- | --- | --- |
| O1: second OOM undetectable | The retry's node id and attempt were never carried into the new container, so its report looked like a replay of the original (`decide_oom_recovery` doc: "indistinguishable from a duplicate delivery"). | §1: node id + shard attempt are injected at start; the report's `node_id` is authoritative. |
| O2/O3: post-decision failures not healed | Effects were run inline once; a crash or D1/stop failure stranded the lineage. | §3: durable per-lineage effect flags, drained by an alarm sweep. |
| O4: old container never stopped | The retry could be dispatched without `StopOld` completing. | §3: fixed effect order; `StartRetry` is gated on `old_stopped`. |
| R1: shard-terminal lookup killed the retry | `complete_own_shard_node` resolved by lineage ignoring the reporting attempt. | §4: resolve by exact `(attempt)` first, lineage only to explain a miss. |
| R2: `CLOUD_CI_ATTEMPT` is the *run* attempt | Wrong carrier. | §1: `CLOUD_CI_SHARD_ATTEMPT` / `--attempt` and `CLOUD_CI_NODE_ID` / `--node-id` (already shipped in `cloud-ci-cli::identity::resolve_shard_attempt`, `cloud-ci-cli::agent::resolve_node_id`). |

## 1. Identity

### 1.1 What an OOM report names

`SubmitResourceSamplesRequest` carries `job_id`, `shard_index`, `attempt` (the **shard**
attempt), `instance_type`, `oom_detected`, `memory_peak_bytes` and `optional node_id = 8`.
`handle_submit_resource_samples` (`coordinator/mod.rs`) already validates `node_id`
(`logic::validate_node_id`, max `MAX_NODE_ID_BYTES` = 256) and hashes it into the batch
identity (`logic::resource_sample_batch_content_hash`). Idempotency identity stays
`(job_id, shard_index, attempt)`; `node_id` is part of the content hash, so a replay naming a
different node is a 409 conflict, never a silent overwrite.

**Rule:** an OOM is acted on only if the report's `node_id` is present, names an existing
`node` row in this run, that row's `shard_attempt`/`job` binding matches the report's
`(job_id, shard_index, attempt)`, and the row is non-terminal
(`logic::oom_event_admissible(run_terminal, node_state)`). A report with no `node_id` (an old
agent, or a dispatcher that does not set it) is recorded as samples only and **never**
triggers recovery. That is the safe degradation: absent identity means no action, not a guess
from `(job, idx, attempt)`.

`oom_detected` is existing, agent-side evidence (`memory.events` `oom_kill`, analytics.md).
The alternative trigger "container exited non-zero with no final Report" is **out of scope**
(§7): `run_and_report` (`node_container.rs`) maps any non-zero exec exit to `failed` with no
memory signal, so it cannot distinguish OOM from an ordinary test failure.

### 1.2 Deriving the retry's identity

Two node kinds, one rule: the retry's identity is a pure function of the lineage and the
decision, so a replayed effect recomputes the same id and address.

* **Shard node.** Retry node id = `logic::shard_node_id(job_name, idx, attempt + 1)`, i.e.
  `shard:{job}:{idx}:{attempt+1}`, and the retry's shard attempt is `attempt + 1`.
  This is deliberately **not** `oom_retry_node_id`. The current pure code is inconsistent:
  `oom_retry_node_id`'s own doc says shard nodes do not need it, yet `resolve_oom_node_id`
  uses it for shard lineages. Using the canonical id has three benefits: `shard_node_matches`
  / `shard_nodes_to_cancel` already match it (so fail-fast sibling cancellation finds and
  cancels a retry with **no change**, closing item 4 of the old "what the removal took away"
  list without teaching the matcher a second shape); `shard_state`'s `(job_name, idx, attempt)`
  key already models "attempt 2" (parallelization.md: "`shard_state.attempt` becomes 2");
  and `latest_attempt_per_shard` already counts only the highest attempt.
  Collision handling: `handle_start_node`/`resolve_start_node` is keyed by `spec_hash`. If a
  row with that id already exists with a *different* spec hash, the lineage goes to the
  `stuck` state (§3.5) with `last_error = "retry node id occupied by a different spec"`; it is
  never overwritten.
* **Plain `runner: "auto"` node** (no shard). Retry node id =
  `logic::oom_retry_node_id(base_node_id, n)` = `{base}:oom-retry:{n}` where `n = 2` (the
  lineage's single retry). The id must pass `validate_node_id`; a base id longer than
  256 − len(":oom-retry:2") bytes is rejected **at `startNode` time** for `runner_auto` nodes
  (fail closed, not at OOM time).
* **Physical address.** Always `coordinator::node_physical_address(run_do_name, retry_node_id)`
  — unchanged hashing (`Sha256` over length-prefixed fields), so the retry has a distinct,
  run-scoped container and `NodeContainer` DO (`node_container::start_container`). It is stored
  on the node row exactly like today (`insert_node(..., physical_address, ...)`).
* **Pure-logic change (Stage 1):** `resolve_oom_node_id` is replaced by
  `oom_retry_target(kind, job_name, idx, base_node_id, decision_attempt) -> RetryTarget
  { node_id, shard_attempt }`. `oom_retry_node_id` stays for plain nodes only.

### 1.3 Carrying node id and attempt into the new container

Today `node_container::start_container` posts `{run_do_name, node_id, image, command}` and
`exec_in_container` calls `container.exec(&cmd, None)` — **no env at all**.

Cloudflare's Durable Object Container API (docs last updated 2026-09-30,
developers.cloudflare.com/containers/api/durable-object-container/, read 2026-10-03):
`start({ env })` variables are **not** passed to `exec()` processes (except `PATH`), and
`exec({ env })` replaces the environment entirely ("Other variables from `start()` or the
image are not inherited, except for `PATH`"). Therefore:

* The carrier is the **`exec` options `env`** (not `start` env), set by `NodeContainer` to
  exactly: `CLOUD_CI_NODE_ID={node_id}`, `CLOUD_CI_SHARD_ATTEMPT={shard_attempt}`,
  plus whatever the command already needs. This is a behavior change for *every* node (today
  they get an empty env), so Stage 2 must also pass the existing variables the agent needs
  (`CLOUD_CI_TOKEN`, `CLOUD_CI_SERVER_URL`, `CLOUD_CI_INSTANCE_TYPE`...) — **which no real
  dispatcher sets today** (roadmap Phase 6: "bootstrap-token-issuance path ... not built"). That
  is a hard prerequisite owned by the executor/bootstrap work, see Open Question Q3.
* Whether the pinned `worker` fork (`btkostner/workers-rs@df96700`, branch
  `container-exec-and-durable-object-sizing`, ADR 0011) exposes `ContainerExecOptions.env` and
  `ContainerStartupOptions.instance` setters is **`[unverified]`**: the fork source is not in the
  tree and I could not read it. Stage 2's first task is to read the pinned commit and, if
  `exec` env is absent, extend the fork (ADR 0011 process) **or** fall back to wrapping the
  command as `["env", "CLOUD_CI_NODE_ID=...", "CLOUD_CI_SHARD_ATTEMPT=...", "--", ...command]`
  (requires `env` in the image; document that).
* `StartNodeRequest` gains `shard_attempt: Option<u32>` (serde default) and the `JobSpec`
  (`executor.rs`) the same field; both are folded into `spec_hash` by the caller, preserving
  `resolve_start_node`'s determinism check.

A second OOM then maps unambiguously: the retry agent reports `node_id = retry id` and
`attempt = attempt+1`; `decide_oom_recovery` sees a lineage with an existing decision whose
`attempt+1 == incoming.attempt` and returns `New(FailedAtMax)` (exactly the existing,
tested branch). The `node_id` ↔ `(job, idx, attempt)` cross-check in §1.1 is what removes the
"indistinguishable from a duplicate" hole, because a replay of the original attempt now has
`attempt == existing.attempt` and a different `node_id`, and is rejected as a mismatch.

## 2. State

All DO-local tables live in `coordinator/mod.rs::ensure_schema`. Every new column on an
existing table is added with a `pragma_table_info` guard, **per column**, through one shared
helper (replaces the two copy-pasted blocks for `run.settings_sha` and `node.physical_address`):

```rust
fn ensure_column(sql: &SqlStorage, table: &str, column: &str, ddl: &str) -> worker::Result<()>
// SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2  -> if empty: ALTER TABLE .. ADD COLUMN ..
```

`table`/`column` are compile-time constants (no user input reaches `ALTER`).

### 2.1 `node` table additions (guarded, nullable, no backfill)

| Column | Type | Meaning |
| --- | --- | --- |
| `runner_auto` | `INTEGER NOT NULL DEFAULT 0` | 1 iff the node declared `runner: "auto"`. |
| `size` | `TEXT` | Instance size this node was started at (ladder name). |
| `size_min`, `size_max` | `TEXT` | Bounds frozen at `startNode` (§5.2). |
| `shard_attempt` | `INTEGER` | Shard attempt the node runs as; `NULL` for plain nodes. |
| `lineage_id` | `TEXT` | `oom_lineage.lineage_id` this node belongs to; `NULL` = not auto. |

**Legacy rows:** nodes written before this version have all of these `NULL`/`0`. They are
never auto-recovered: `runner_auto = 0` fails the §1.1 admissibility check and an OOM report
for them is samples-only. No backfill, no guess. The existing `physical_address = NULL` rule
(`stop_node_container` logs and returns `false`, "drain manually") is unchanged and applies
to a legacy node that is somehow a lineage's old node.

### 2.2 `oom_lineage` (new, DO-local, `CREATE TABLE IF NOT EXISTS`)

One row per lineage, ever (the single-retry rule means at most one decision per lineage).

```sql
CREATE TABLE IF NOT EXISTS oom_lineage (
  lineage_id        TEXT PRIMARY KEY,   -- shard: 'shard:{job}:{idx}'; plain: base node id
  kind              TEXT NOT NULL CHECK (kind IN ('shard','node')),
  job_name          TEXT,               -- shard lineages
  idx               INTEGER,
  base_node_id      TEXT NOT NULL,      -- OomDecisionRecord.base_node_id
  decision_attempt  INTEGER NOT NULL,   -- OomDecisionRecord.attempt (the attempt that OOM'd)
  outcome           TEXT NOT NULL CHECK (outcome IN ('retry','failed_at_max')),
  from_size         TEXT NOT NULL,
  to_size           TEXT,               -- 'retry' only
  max_size          TEXT NOT NULL,
  reason            TEXT NOT NULL,      -- rightsizing::oom_retry string, unchanged
  measured_peak_bytes INTEGER,          -- NULL = unknown, never 0
  retry_node_id     TEXT,               -- 'retry' only; recomputed value stored for audit
  -- OomEffectFlags, one column each:
  old_marked INTEGER NOT NULL DEFAULT 0, old_stopped INTEGER NOT NULL DEFAULT 0,
  old_projected INTEGER NOT NULL DEFAULT 0, decision_projected INTEGER NOT NULL DEFAULT 0,
  retry_inserted INTEGER NOT NULL DEFAULT 0, retry_started INTEGER NOT NULL DEFAULT 0,
  retry_projected INTEGER NOT NULL DEFAULT 0,
  -- bounded retry state (§3.4):
  effect_attempts   INTEGER NOT NULL DEFAULT 0,
  next_attempt_at   INTEGER,            -- ms epoch; NULL = due now
  last_error        TEXT,
  state             TEXT NOT NULL DEFAULT 'pending'
                    CHECK (state IN ('pending','done','stuck','abandoned')),
  created_at        INTEGER NOT NULL,
  completed_at      INTEGER
)
```

The decision row and the first effect flags are written in **one**
`storage().transaction()` (the pattern `accept_resource_sample_batch_and_queue_all` and the
`shard_terminal_effect` insert already use), so a crash cannot leave a decision without its
effect record. `decide_oom_recovery` is called inside the same synchronous region that reads
the existing row, so concurrent reports for one lineage serialize on the DO's single thread
between awaits (no `await` between read-existing and insert, same discipline as
`handle_shard_terminal`'s "re-validate after the one await").

### 2.3 D1: `sizing_decisions` (forward-only migration `0021_sizing_decisions.sql`)

analytics.md already specifies the shape. Migration only **adds** a table (safe on a live
deployment, AGENTS.md); it is created in Stage 3 together with its single writer (the
repo's migrations avoid writer-less tables, see 0015's comment):

```sql
CREATE TABLE sizing_decisions (
  repo_id               INTEGER NOT NULL,
  job_name              TEXT    NOT NULL,
  current_instance_type TEXT    NOT NULL,
  p95_memory_bytes      INTEGER,          -- NULL until the nightly cron fills it
  p95_cpu_frac          REAL,
  last_resized_at       INTEGER NOT NULL,
  reason                TEXT    NOT NULL, -- "standard-2 -> standard-3: oom-retry" etc.
  source                TEXT    NOT NULL DEFAULT 'oom' CHECK (source IN ('oom','cron')),
  run_id                TEXT,             -- run that produced an 'oom' row
  peak_bytes            INTEGER,          -- measured peak at the decision, NULL = unknown
  PRIMARY KEY (repo_id, job_name)
);
```

`pending_candidate`/`pending_nights` (the hysteresis counter `rightsizing.rs` says belongs on
this table) are **not** added now; the nightly cron's migration adds them with its writer.
Projection is `INSERT ... ON CONFLICT (repo_id, job_name) DO UPDATE SET ... WHERE
excluded.last_resized_at >= sizing_decisions.last_resized_at` so a replayed or late
projection never regresses a newer row (idempotent and order-safe). Which `job_name` keys a
shard lineage: the **group's** `job_name` (shards of a job are one sized node, analytics.md
Inputs), so the retry's `to_size` becomes the group's `current_instance_type`.

**Legacy rows:** the table is new; there are none. A deployment that has not applied 0021
yet makes `ProjectDecision` fail with "no such table"; projections are non-critical effects
(§3.1) so this delays only the projection, never the retry, and surfaces as `stuck` after the
cap (§3.4). The setup wizard already applies D1 migrations in order.

**Reader of `current_instance_type`:** a *future* `startNode` for the same job in a later run
reads it to pick `initial` (analytics.md: "becomes the new `current_instance_type`
immediately"). That read is **not** part of this work (§7, Q5); until then the persisted row
is audit/dashboard data and later runs start at `initial`.

## 3. Effects and healing

### 3.1 Ordered effect list

The order is already encoded in `logic::next_oom_effect_excluding` and is kept verbatim:

1. `MarkOldTerminal` — mark the OOM'd node `failed` with `result = {"error":"oom", ...}`.
2. `StopOld` — `stop_node_container(old_node_id, old.physical_address)`.
3. `InsertRetry` — `insert_node` for the retry (retry-only).
4. `StartRetry` — `node_container::start_container` with the chosen size (retry-only), gated
   on `old_stopped`.
5. `ProjectOld`, `ProjectDecision`, `ProjectRetry` — D1 projections (`project_node_to_d1`,
   new `project_sizing_decision_to_d1`). `OomEffect::is_projection` effects never gate 1–4.

For `failed_at_max` there is no retry: effects are 1, 2, 5(old), 5(decision); the node's
failure message names the configured `max` and the measured peak
(`OomRecoveryOutcome::FailedAtMax`), peak rendered as "unknown" when `None`.

### 3.2 Idempotency per effect

| Effect | Idempotent because | Completion flag set when |
| --- | --- | --- |
| MarkOldTerminal | `update_node_status` to `failed` only if `!NodeState::is_terminal()`; a node already terminal (naturally succeeded/cancelled first) leaves the lineage `abandoned` instead (see 3.6). | the row is terminal (by us or by someone else) |
| StopOld | `NodeContainer::handle_stop` is a no-op when not running; `resolve_stop_outcome(addr, ok) == Pending` keeps the flag unset. | stop returned 2xx |
| InsertRetry | deterministic id + `resolve_start_node` (same spec hash = no-op, different = conflict → `stuck`). | row exists with our spec hash |
| StartRetry | `NodeContainer::handle_start` returns `started:false` if already `running()`. A start that throws sets the retry row `failed` (as `handle_start_node` does) and **still sets the flag**, because the retry was attempted; the shard is then genuinely failed, loudly (§3.5). | start 2xx, or row marked failed |
| Project* | `INSERT .. ON CONFLICT DO UPDATE` (nodes, runs) and the monotonic guard in §2.3. | D1 call returned ok |

`StartRetry` has one residual hazard: `start()` returns before the container is ready, and
failures after that need `monitor()` (Cloudflare docs, `start()`: "To catch later errors,
including a container that fails to start, use `monitor()`"). The retry's eventual failure is
surfaced exactly as any other node's: `run_and_report` → `/complete-node` `failed`. If the
container never starts, no completion ever arrives; the **run timeout** is the backstop
(§3.3). Adding `monitor()` is out of scope (§7).

### 3.3 Composition with the alarm — the run-timeout close can never be starved

Current `alarm()` order: drain `shard_terminal_effect` rows, flush overflow, terminal check,
**then** deadline check. A hung `await` inside an early drain delays the close. The OOM sweep
therefore goes **after** the deadline check and the close wins:

```
alarm():
  ensure_schema
  read run; if none -> return
  if run terminal            -> release_alarm_if_idle (existing)      // OOM sweep: see 3.6
  if now >= deadline         -> handle_close_run(by_timeout)          // BEFORE any OOM work
  existing shard_terminal_effect drain + overflow flush (unchanged)
  drain_oom_lineages(budget)                                          // new, last
  re-arm: min(OVERFLOW_FLUSH_DELAY_MS-style retry, earliest oom next_attempt_at, deadline)
```

Concretely: the deadline check is hoisted above the existing shard-effect drain (a
behavior-preserving move for the close case, and strictly more robust than today). The OOM
sweep is bounded per fire: at most `OOM_SWEEP_MAX_LINEAGES = 4` lineages and one effect step
chain each; a lineage not yet due (`next_attempt_at > now`) is skipped without I/O. The
re-arm delay is `logic::clamp_retry_delay_to_deadline(delay, deadline_ms, now_ms)`
(existing), where `delay` = time to the earliest `next_attempt_at`, floor `OVERFLOW_FLUSH_DELAY_MS`.
`has_pending_background_work` (used by `release_alarm_if_idle`) gains
`oom_lineage WHERE state = 'pending'`, so a terminal run still drains container stops
(§3.6) and then releases the alarm slot; `stuck`/`abandoned`/`done` rows never hold it.

### 3.4 Retry cap and backoff (the previous limitation: unbounded 5 s retries)

Per lineage, tracked in `effect_attempts`, `next_attempt_at`, `last_error`, `state`:

* A drain pass that makes **no progress** (an effect returned `Err`) increments
  `effect_attempts`, stores `last_error`, and sets
  `next_attempt_at = now + min(5s * 2^(effect_attempts-1), 5min)` (5 s, 10 s, 20 s ... 5 min).
* Critical effects (1–4) cap at `OOM_CRITICAL_MAX_ATTEMPTS = 8` (~10 min cumulative);
  projections cap at `OOM_PROJECTION_MAX_ATTEMPTS = 12` (D1 outages are longer-lived and
  lower-stakes). The counters are separate: `effect_attempts` resets to 0 when the last
  critical effect completes.
* On cap: `state = 'stuck'`, `completed_at` unset. **Operator-visible**, three ways:
  1. a `console_log!` line with a stable prefix `oom_lineage stuck:` plus lineage id, effect,
     `last_error`;
  2. the lineage's old/retry node `result` JSON gets `{"oom_recovery":"stuck","effect":..,
     "error":..}` and is projected to D1 `nodes.result` where the report/dashboard read it;
  3. a read-only DO route `GET /oom-lineages` returning the rows (for `wrangler tail`-free
     inspection by the existing admin tooling).
* `stuck` is terminal for the sweep (no more automatic attempts, no more alarm). A stuck
  lineage whose old container is **not** confirmed stopped is the dangerous case (a leaked
  container): its log line says so explicitly ("old container may still be running at
  {physical_address}"). Manual recovery is out of scope; the state is the contract.
* `ensure_schema`'s `CHECK` on `state` and the counters are DO-local only.

This also answers the standing limitation for `shard_terminal_effect` ("same unbounded-retry
shape"): that table is **not** changed by this design (Q6 asks whether to give it the same
cap in the same stage).

### 3.5 Failures of the critical effects, concretely

* `MarkOldTerminal` cannot fail except on SQL error → treated like any `Err` (retry/backoff).
* `StopOld` failing blocks `InsertRetry`/`StartRetry` (O4). While blocked the shard has *no*
  running retry; the run timeout still bounds it.
* `InsertRetry` conflict → `stuck` immediately (no backoff; it cannot succeed by retrying).
* `StartRetry` start error → retry row `failed`, flag set, lineage proceeds to projections;
  the shard-terminal path (§4) then sees a failed node and the barrier counts a failed
  attempt 2 (`latest_attempt_per_shard`).

### 3.6 Terminal run and `abandoned`

`oom_event_admissible` already refuses a *new* decision for a terminal run. For an existing
lineage when the run goes terminal (close, cancel, timeout):

* `retry_inserted`/`retry_started` still `false` → the lineage becomes `abandoned`; the retry
  is **never** started (a terminal run must not get a new container). Remaining effects
  `MarkOldTerminal` and `StopOld` still run (they free a container), then projections.
* The retry already started → `handle_cancel_run`/`handle_close_run` already stop every
  non-terminal node via `ensure_sibling_cancelled`; the retry is just another node. No special
  case.

## 4. Shard-terminal interaction

Today `complete_own_shard_node(sql, job_name, idx, attempt, outcome)` computes
`logic::shard_node_id(job_name, idx, attempt)` and `drain_shard_terminal_effects` /
`resolve_shard_cancel_node_ids` (via `shard_nodes_to_cancel`) match by prefix. With §1.2 every
shard node — original or retry — is `shard:{job}:{idx}:{attempt}` with `attempt` its own shard
attempt, so **exact `(attempt)` resolution already lands on the right node**. The R1 bug
existed only because the removed code *replaced* that lookup with a lineage lookup. Rules:

1. **Resolve by the reporting attempt, always.** `complete_own_shard_node` keeps the
   `shard_node_id(job, idx, req.attempt)` lookup. The lineage table is consulted only to
   *explain a miss* (e.g. the OOM'd attempt 1 node was already marked by the lineage), never to
   redirect a hit to a different attempt. A late attempt-1 `shard-terminal` therefore completes
   attempt 1's node (already terminal by `MarkOldTerminal` → `shard_self_completion_target`
   returns no change) and can never touch attempt 2.
2. **Stop only the node you resolved.** The `stop_node_container` in
   `complete_own_shard_node` addresses the resolved row's own `physical_address`. Attempt 1's
   stop cannot kill attempt 2's container: addresses differ (§1.2).
3. **An OOM'd attempt's failed report must not trip the barrier.** When attempt 1's
   container is OOM-killed the SDK/agent may still post `shard-terminal(failed, attempt=1)`.
   `evaluate_barrier`/fail-fast would then cancel siblings before the retry runs. So
   `handle_shard_terminal` consults `oom_lineage` for `(job, idx)`: if a `retry` decision
   exists whose `decision_attempt == req.attempt`, the call is recorded (`shard_state` row,
   idempotency preserved) with `effect_kind = "superseded_by_oom_retry"`: no barrier
   evaluation, no fail-fast, no group transition. And `read_all_shard_terminal_rows`
   excludes superseded `(idx, attempt)` rows from the barrier input, so
   `latest_attempt_per_shard` cannot see a stale failed attempt 1 as the shard's verdict
   while the retry is in flight. When attempt 2 reports, it is simply the highest attempt.
   If the lineage is `failed_at_max` (no retry), attempt N's failure is the real verdict and
   is evaluated normally.
4. **Fail-fast cancellation** still freezes `cancel_node_ids` at decision time
   (`resolve_shard_cancel_node_ids`, documented rationale). Because the retry id matches
   `shard_node_matches`, a retry that exists at that moment is included and cancelled; a
   retry created *after* the freeze is not in the frozen list. To close that window,
   `InsertRetry` checks the group state: group `status != 'running'` → lineage `abandoned`
   (§3.6), no retry created.
5. **`drain_shard_terminal_effects`** is unchanged, except it must resolve its own node with
   the same exact-attempt rule (it shares `complete_own_shard_node`).

A shared helper `resolve_live_shard_node(sql, job, idx, attempt) -> Option<NodeRow>` (exact
`shard_node_id`, no lineage redirect) is the only lookup both call sites use; a unit test
proves it never returns attempt N+1 for a request naming attempt N.

## 5. Size selection

### 5.1 Declaring `runner: "auto"`

* `StartNodeRequest` gains `runner_auto: Option<RunnerAutoRequest>` where
  `RunnerAutoRequest { min: Option<String>, max: Option<String>, initial: Option<String> }`
  are **per-node overrides** (analytics.md: `runner: "auto"` is shorthand for
  `settings.runners.auto`). `#[serde(default)]`, so an old caller is unaffected.
  For shards the SDK dispatcher sets it on each shard's `startNode` (same values for the
  group); this is the (unwired) `ci.container`/`ci.shard` change and is **not** in this
  design's Stage list — Stage 4 proves the path with direct `startNode` calls.
* `handle_start_node` resolves, **synchronously before `insert_node`**, via the existing pure
  `logic::resolve_auto_start(ladder, min, max, initial)`; an `AutoStartError` fails the
  request 422 (`bounds_not_in_ladder` / `initial_out_of_bounds`), so a configuration error
  surfaces at dispatch, not at OOM time.

### 5.2 Where bounds come from: the frozen settings snapshot

`run.settings_sha` is frozen at `BeginRun` (module docs "Settings SHA";
`resolve_settings_sha_for_admission`). Bounds are resolved from **that sha**, never from live
HEAD: `repo_settings::settings_for_sha(.., repo_id, run.settings_sha)` (the immutable
`(repo_id, head_sha)` cache, migration 0019) → `Settings.runners.auto.{min,max,initial}`,
then per-node overrides on top, then `resolve_auto_start` against the executor ladder. The
result is persisted on the node row (`size`, `size_min`, `size_max`) at `startNode` time, so
OOM decisions later read the **node's** frozen bounds and do not re-fetch settings.
`run.settings_sha IS NULL` (legacy run) → `runner_auto` is rejected 409
`no_settings_snapshot` (fail closed; never re-resolve HEAD, per the module's own rule).
Unset `min`/`max` default to the ladder's first/last rung; unset `initial` defaults to `min`
(`resolve_auto_start`'s existing contract). **Contradiction handled:** the documented default
`runners.auto.min = "basic"` (analytics.md, settings.md) is not a valid
`durable_object`-policy size; `resolve_auto_start` already returns `BoundsNotInLadder`, which
under the real ladder (below) would make *every default deployment* fail `runner: "auto"`.
Q2 asks the owner to change the documented default to `lite` (or `standard-1`) or to define
`basic` as an alias.

### 5.3 What the executor must provide

1. A **multi-size ladder** in `CapabilityDescriptor.sizes`
   (`ContainersExecutor::capabilities`, currently one rung `standard-4`, 4 vCPU / 12 GiB).
2. A **per-start size**: `JobSpec.instance: Option<String>` →
   `node_container::start_container(.., instance)` → `StartRequest.instance` →
   `ContainerStartupOptions.instance`. `None` keeps today's behavior.
3. Sizes validated against the ladder before they reach the runtime (an unknown name makes
   `start()` throw `TypeError`, Cloudflare docs below).

### 5.4 What Cloudflare Containers supports (dated source)

Per developers.cloudflare.com/containers/configuration/scheduling-policy/ and
.../containers/platform/limits/ (both "Last updated Sep 30, 2026", read 2026-10-03):

* Under the `durable_object` scheduling policy (public **beta**), `ctx.container.start({
  instance })` accepts `lite`, `standard-1`, `standard-2`, `standard-3`, `standard-4`, or a
  custom `{vcpu, memoryMib, diskMb}` (1–4 vCPU, ≤12 GiB, ≥3 GiB memory per vCPU).
  **`basic` and the legacy `dev`/`standard` aliases are not accepted** by the runtime. An unknown name throws `TypeError` from
  `start()`.
* **If `instance` is omitted the container is `lite` (1/16 vCPU, 256 MiB).**
* Memory per size: lite 256 MiB, basic 1 GiB (`default` policy only), standard-1 4 GiB,
  standard-2 6 GiB, standard-3 8 GiB, standard-4 12 GiB.
* `wrangler.toml` already sets `scheduling_policy = "durable_object"` for `NodeContainer`.

So the **runtime does support per-instance sizing**, and a real ladder
`[lite, standard-1, standard-2, standard-3, standard-4]` exists. `rightsizing::ladder_range`
and `oom_retry` take it as a parameter; the cloudflare fixture in `rightsizing.rs` tests has
six rungs including `basic` and must gain a `durable_object` ladder fixture. Whether the
patched Rust `worker` fork exposes `instance` is `[unverified]` (see §1.3).

**Finding that affects correctness today (not just this feature):** `NodeContainer::handle_start`
never sets `instance`, so **every managed node currently runs on `lite` (256 MiB)**, while
`ContainersExecutor::capabilities()` advertises `standard-4` (12 GiB) and the agent would
report whatever `--instance-type` it is given. `node_container.rs`'s own doc says "lite only —
every node gets the runtime default". The advertised capability is false. Stage 2 fixes this
independently (pass an explicit size, default `standard-4`... or the deployment default), and
it should be treated as a bug fix, not feature work (Q1).

**Degradation if sizing cannot be done per start** (fork lacks the setter and Stage 2 cannot
extend it, or the owner chooses a one-size executor): the ladder has one rung;
`resolve_auto_start` yields min = max; `oom_retry` returns `AlreadyAtMax` on the first OOM;
every OOM is an immediate `FailedAtMax` with the max/peak message, recorded in
`sizing_decisions`/lineage with no container started. The feature is then an OOM *classifier
and reporter*, not a retrier. This is the current real-world behavior and is acceptable.

## 6. Test plan

### 6.1 Pure logic (`coordinator/logic.rs`, `cargo test` via `mise run //packages/cloud-ci-worker:test`)

New/changed tests (behavior, not wiring):

* `oom_retry_target` shard vs plain; id passes `validate_node_id`; rejects over-long base
  (boundary 256).
* `shard_nodes_to_cancel` matches the retry id `shard:J:I:2` and not `...:oom-retry:...` for
  shards (regression for the removed item 4).
* `resolve_live_shard_node`: request attempt 1 never resolves attempt 2 (R1 regression);
  request attempt 2 never resolves attempt 1.
* Barrier input filter: a `retry` lineage supersedes the OOM'd attempt; superseded failed row
  does not trigger `FailFastTriggered`; `failed_at_max` does.
* Backoff: delay sequence 5 s, 10 s, ... capped 5 min; cap flips to `stuck`; critical vs
  projection caps independent; `clamp_retry_delay_to_deadline` composes.
* Effect state machine: existing `next_oom_effect_excluding` tests stay; add the
  `abandoned`-on-terminal-run path (no `InsertRetry`/`StartRetry`, `StopOld` still due).
* `decide_oom_recovery` second-OOM and cross-attempt-mismatch (`node_id` of attempt 1 reported
  with `attempt 2` → rejected by the §1.1 check, a pure `oom_report_matches_node`).
* `ensure_column` SQL construction (pure string builder) with a fixed allow-list.

### 6.2 Durable-Object-level behavior (needs a runtime harness)

`RunCoordinator` methods take a real `SqlStorage`; there is **no in-repo DO harness** today
(coordinator behavior is covered only by pure-logic tests; roadmap items 4 and the module
docs say DO paths are never runtime-verified). Two options for Stage 4 (Q4):

* (a) A fake `Executor`-backed seam: extract the effect executor behind a trait
  (`OomEffectPort { stop, start, project, ... }`) so the lineage drain is unit-testable with
  an in-memory SQL (e.g. `rusqlite` dev-dependency mirroring `ensure_schema`) and fault
  injection. Needs a new dev-dependency decision.
* (b) `workerd` via `wrangler dev`/Miniflare driven by a script.

Scenarios for either: crash between each pair of effects (inject failure after effect k,
re-drain, assert converged state); duplicate `SubmitResourceSamples` delivery (same hash) →
one decision; late delivery after run terminal → samples-only; second OOM → `failed_at_max`;
stuck path after cap with operator-visible state; alarm: deadline passed + stuck lineage →
run still closes; shard-terminal attempt 1 after retry started → attempt 2 untouched.

### 6.3 Live smoke (not runnable in this environment)

Requires: `CLOUDFLARE_API_TOKEN` (+ account id) for `wrangler dev` with real Containers,
Docker for the local image build (`NodeContainer` image), and a deployment where the runner
image's `cloud-ci agent` can reach `/ingest` (bootstrap token plumbing, Q3). Steps: start an
auto shard at `standard-1`, run an allocation-bomb command that exceeds 4 GiB, observe the
OOM report (`node_id`, `attempt 1`), the lineage `retry` at `standard-2`, a second container
at `shard:J:I:2`, then a second bomb → `failed_at_max`; also duplicate delivery and a delivery
after run close. Until run, the feature stays **unverified at runtime** and the docs say so.

## 7. Non-goals

* The nightly rightsizing cron, p95 computation, hysteresis writer, or any reader of
  `sizing_decisions` (roadmap item 6).
* Non-OOM retries of any kind (parallelization.md: OOM is the only automatic retry).
* Detecting OOM from exit code / "no final Report" (no memory signal, §1.1).
* `monitor()` for post-`start()` failures; sidecars; snapshots.
* Custom `{vcpu, memoryMib, diskMb}` instance shapes.
* Wiring `ci.container`/`ci.shard` in the SDK to send `runner_auto` (Stage 4 uses direct
  `startNode`); the SDK's missing ingest-token plumbing (roadmap item 2).
* Other executors (EC2, Lambda, Kubernetes): the lineage design is executor-agnostic but only
  `ContainersExecutor` and `FakeExecutor` are in scope.
* Manual remediation tooling for `stuck` lineages beyond the visible state.
* `external` runs.

## 8. Staged delivery

Each stage is independently shippable, `mise run check` green, and the feature is **inert
until Stage 5 enables it** (the trigger path is gated by a deployment var `OOM_RECOVERY=1`
read in `handle_submit_resource_samples`, default off; `runner_auto` nodes are rejected 422
`oom_recovery_disabled` while off, so no node can be created that would need recovery).

1. **Pure logic + schema helpers.** `oom_retry_target`, `oom_report_matches_node`,
   `resolve_live_shard_node`, backoff/cap functions, barrier-supersession filter, `ensure_column`
   builder + the shared migration of the two existing guarded `ALTER`s onto it. Fix the stale
   `resolve_oom_node_id`/`oom_retry_node_id` inconsistency. No behavior change; tests only.
2. **Executor ladder and per-start size (fixes the `lite` bug).** Real
   `[lite, standard-1..standard-4]` ladder, `JobSpec.instance`, `start_container(.., instance)`,
   `exec` env carrier (`CLOUD_CI_NODE_ID`, `CLOUD_CI_SHARD_ATTEMPT`), fork read/extension if
   needed. Existing nodes get an explicit size (default decided by Q1). Inert for auto.
3. **DO state + D1.** `node` columns, `oom_lineage`, `0021_sizing_decisions.sql`,
   `project_sizing_decision_to_d1`, `GET /oom-lineages`. Nothing writes lineages yet.
4. **Decision + effect drain behind the flag, with a fake executor.** `startNode` with
   `runner_auto`, `handle_submit_resource_samples` → `decide_oom_recovery` → lineage write,
   effect drain, alarm sweep (§3.3), shard-terminal changes (§4); fault-injection tests (§6.2).
   Against the real executor this can only ever produce `FailedAtMax` unless Stage 2's ladder
   has >1 rung in the deployment.
5. **Enable + live proof.** Run §6.3 with credentials; update parallelization.md, analytics.md
   and roadmap.md from "not wired" to "wired, verified on <date>" **only** with that run's
   evidence; flip the default.

## 9. Open questions needing an owner decision

* **Q1.** `NodeContainer` starts every node on `lite` (256 MiB) today. Confirm that is a bug,
  and pick the default size for non-auto nodes (suggest `standard-4` to match the advertised
  capability, or the deployment's `runners.default`).
* **Q2.** Documented default `runners.auto.min = "basic"` is rejected by the
  `durable_object` runtime. Change the documented default (`lite`/`standard-1`), or alias
  `basic`→`lite`? (Affects settings.md and analytics.md.)
* **Q3.** `cloud-ci agent` needs `CLOUD_CI_TOKEN`/server URL inside the container; no
  dispatcher mints them today. Is bootstrap-token issuance a hard prerequisite for Stage 4's
  live proof, and who owns it?
* **Q4.** DO-level test harness: add a `rusqlite`-style in-memory SQLite dev-dependency, or
  require `wrangler dev`/Miniflare?
* **Q5.** `sizing_decisions` has two writers by design (`RunCoordinator` for OOM,
  nightly cron). AGENTS.md says only a run's coordinator writes *run state*; is a per-repo
  sizing row an exception, and who wins on conflict (this design: monotonic
  `last_resized_at`)? Should a later run's `startNode` honor `current_instance_type` before the
  cron exists?
* **Q6.** Apply the same attempt cap + stuck state to `shard_terminal_effect` in Stage 3/4, or
  leave it?
* **Q7.** Shard retry id: this design uses `shard:J:I:{attempt+1}` (no matcher change). The
  existing parallelization.md text and `resolve_oom_node_id` assumed `:oom-retry:`. Confirm.
* **Q8.** Trigger evidence: accept `oom_detected` from the agent only (this design), or also
  infer OOM from a 137 exit with `memory.events` readable by the Worker?

## 10. Contradictions found in the existing docs and code

* `docs/roadmap.md` (Phase 4 "Not wired", Next steps item 3) and `parallelization.md` list
  the remaining work as "a multi-size executor ladder, lineage- and attempt-aware
  resolution, live smoke test". They omit that **no managed node is sized at all today**
  (everything is `lite`), that **`exec` gets no environment** so the shipped
  `CLOUD_CI_NODE_ID`/`CLOUD_CI_SHARD_ATTEMPT` carriers have nothing to carry them, and that
  no dispatcher provides the agent's token/URL. "Only the wiring and the live proof are
  missing" understates this.
* `docs/design/dynamic-pipelines.md` (§ around the `RC->>C: start` paragraph, ~line 309)
  still says per-call `image`/`instance` and `exec()` "are not reachable from Rust". ADR 0010
  (Cloudflare Containers row), ADR 0011 and the roadmap spike row say they are (via the fork).
  The doc is stale.
* `parallelization.md` prerequisite 4 plans to teach `shard_node_matches` the
  `:oom-retry:<n>` shape; `oom_retry_node_id`'s own doc comment says shard nodes do not need
  it; `resolve_oom_node_id` uses it for shards anyway. This design resolves it by using
  `shard_node_id(job, idx, attempt+1)` (Q7).
* `executor.rs` `ContainersExecutor::capabilities()` advertises one `standard-4` rung
  (12 GiB) while the real start path is `lite` (256 MiB) — §5.4.
* analytics.md: "becomes the new `current_instance_type` immediately (not just for the one
  retry)" implies a reader on later runs; no reader exists and none is planned here (Q5). Its
  sample CLI comment ("--node-id ... currently unset by any real dispatcher") remains true
  until Stage 2.
* analytics.md/settings.md default `runners.auto.min = "basic"` vs the runtime's rejection of
  `basic` under `durable_object` (roadmap spike row already records the rejection, dated
  2026-10-02).
