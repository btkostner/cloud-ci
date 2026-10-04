# `runner: "auto"` real-time OOM recovery: wiring design

**Status: DRAFT - not approved for implementation; blocked on decisions H1, H3, Q1, Q9, Q10.**

Revision 2, 2026-10-03, after an independent review of revision 1 (34ef8c6). Revision 1 was
found not ready to drive code. This revision fixes the factual errors (C1-C4), folds in the
medium findings (M1-M6) as stated rules or open questions, and turns the three design holes
(H1-H3) into explicit "Decision required" blocks. Where a hole is a design gap and not a policy
choice (H2) it is specified concretely here; where it is a policy choice (H1, H3) no answer is
invented, only options, trade-offs and a recommendation.

It replaces the "remaining prerequisites" prose in
[parallelization.md](./parallelization.md#runner-auto-oom-recovery-pure-logic-only-not-wired-2026-10-03)
and elaborates the policy in [analytics.md](./analytics.md) ("OOM retry", "Instance selection").
Policy is unchanged: **one** automatic retry per shard lineage, on the next size up, bypassing
hysteresis; a second OOM fails for good naming the configured `max` and the measured peak.
`managed` runs only; a no-op for `external` runs.

Facts about Cloudflare are dated and sourced or marked `[unverified]` (AGENTS.md invariant).
Code symbols and line numbers were read in the tree on 2026-10-03 (revision 1 at main 4e53880;
main has since moved to 464decd, whose two newer commits touch only `setup_wizard.rs` and
`deployment.md`, none of the files cited here).

## Blocking decisions at a glance

| Id | Question | Where |
| --- | --- | --- |
| H1 / Q9 | What evidence triggers recovery, and when it must be evaluated relative to the node's failure being recorded. | §1.1 |
| H3 / Q10 | What feeds the shard barrier (`shard_state`) for a retried shard. | §4 |
| Q1 | Default instance size for managed nodes (they run at `lite` today). | Stage 0 |
| Q9, Q10 | (same as H1, H3) | |

H2 (the second decision's persistence model) is specified in §2.2 and needs review, not a policy
decision. Everything else is a stated rule or an owner question in §9.

## 0. Why the earlier wiring failed, mapped to this design

| Review finding | Root cause | Where this design closes it |
| --- | --- | --- |
| O1: second OOM undetectable | The retry's node id and attempt were never carried into the new container, so its report looked like a replay of the original (`decide_oom_recovery` doc: "indistinguishable from a duplicate delivery"). | §1: node id + shard attempt are injected at start; the report's `node_id` must equal the id derived from the report's own `(job, idx, attempt)`. |
| O2/O3: post-decision failures not healed | Effects were run inline once; a crash or D1/stop failure stranded the lineage. | §3: durable per-decision effect flags, drained by an alarm sweep. |
| O4: old container never stopped | The retry could be dispatched without `StopOld` completing. | §3: fixed effect order; `StartRetry` is gated on `old_stopped` and re-checks live state. |
| R1: shard-terminal lookup killed the retry | `complete_own_shard_node` resolved by lineage ignoring the reporting attempt. | §4: resolve by exact reporting attempt only. |
| R2: `CLOUD_CI_ATTEMPT` is the *run* attempt | Wrong carrier. | §1.3: `CLOUD_CI_SHARD_ATTEMPT` and `CLOUD_CI_NODE_ID` (already shipped: `cloud-ci-cli::identity::resolve_shard_attempt`, `cloud-ci-cli::agent::resolve_node_id`). |

## Prerequisite stage 0: managed nodes run at lite

This is a bug independent of OOM recovery, and the OOM feature is meaningless until it is
decided. It must ship **separately**, with owner sign-off, **before** any OOM work.

**Evidence (read 2026-10-03):**

* `node_container.rs` `NodeContainer::handle_start` builds a `ContainerStartupOptions` and calls
  only `options.set_image(&body.image)` (around line 151). No instance is ever set. Its own
  module doc says so: "No `durable_object`-policy `instance` sizing (lite only - every node gets
  the runtime default)".
* `wrangler.toml` (around lines 206-208) declares `NodeContainer` with
  `scheduling_policy = "durable_object"` and no size; Cloudflare documents that Wrangler's
  `instance_type` applies only to the `default` policy.
* Cloudflare, "Scheduling Policies" and "Durable Object Container API" pages (both "Last updated
  Sep 30, 2026", read 2026-10-03): under `durable_object`, "If you omit `instance`, the
  Container uses `lite`." The "Limits and Instance Types" page (same date) gives `lite` as
  1/16 vCPU, 256 MiB, 2 GB disk.
* `executor.rs` `ContainersExecutor::capabilities()` (around lines 251-262) advertises a single
  rung, `standard-4`, 4 vCPU / 12 GiB. Its only callers are tests (`executor.rs` around line
  444 exercises the `FakeExecutor`); nothing in production consumes the advertisement, so the
  mismatch is silent.

**Consequence:** every managed node runs at 256 MiB and 1/16 vCPU, and `capabilities()` is
false. Any real job larger than a trivial script is likely to be OOM-killed at `lite`, which is
also why trigger design (H1) matters.

**Cost and quota impact of fixing it (this is not an inert change):**

* Setting an explicit size changes the memory and vCPU of every managed node. Moving the default
  from `lite` to `standard-4` is a 48x memory increase per node (256 MiB to 12 GiB) and 64x vCPU
  (1/16 to 4). `standard-1` is 16x memory, 8x vCPU.
* Account limits (Limits page, 2026-09-30): 6 TiB concurrent memory, 1,500 concurrent vCPU,
  30 TB concurrent disk. At `standard-4`, 1,500 vCPU is 375 concurrent nodes; at `standard-1`
  it is 3,000 (memory then binds first at about 1,536 nodes by 6 TiB / 4 GiB). Interaction with
  `concurrency.repository` clamping (settings.md) must be stated by the owner.
* Billing impact: pricing is not stated in the sources read for this document and is
  `[unverified]` here. Owner must supply the expected cost change.

**Stage 0 scope:** pass an explicit `instance` through `JobSpec` to
`node_container::start_container` to `ContainerStartupOptions::set_instance`, with the default
chosen by the owner (Q1). No OOM logic. The fork method exists (§1.3). Ship as its own change.

## 1. Identity

### 1.1 What an OOM report names, and when it may be acted on

`SubmitResourceSamplesRequest` carries `job_id`, `shard_index`, `attempt` (the **shard**
attempt), `instance_type`, `oom_detected`, `memory_peak_bytes` and `optional string node_id = 8`.
`handle_submit_resource_samples` (`coordinator/mod.rs`) already validates `node_id`
(`logic::validate_node_id`, at most `MAX_NODE_ID_BYTES` = 256 bytes) and hashes it into the batch
content hash (`logic::resource_sample_batch_content_hash`). Idempotency identity stays
`(job_id, shard_index, attempt)`; the same identity with a different `node_id` is a 409
conflict.

**Binding rule (replaces revision 1's "row binds to (job, idx, attempt)"):** the coordinator
resolves `job_name` from `job_id` (`read_job_by_id`, DO-local `job` table), computes
`expected = logic::shard_node_id(job_name, shard_index, attempt)`, and acts only if
`req.node_id == Some(expected)` (exact string equality, no parsing of the id), the `node` row
exists, `node.runner_auto = 1`, and `node.shard_attempt == attempt`. A report with no
`node_id` (old agent, or a dispatcher that sets none) is samples-only and **never** triggers
recovery: absent identity means no action, not a guess.

**Size source (M5, Q15):** `decide_oom_recovery`'s `current_size` comes from the node row
(`node.size`), never from the report's `instance_type`. The agent takes `--instance-type` only
as a flag (no env var), so a retried container run with the unchanged command would report the
ORIGINAL size. `instance_type` in the report is informational; a mismatch with `node.size` is
logged and the node row wins.

**Plain `runner: "auto"` nodes are out of scope (M4).** A plain node has no job and no shard;
`SubmitResourceSamples` keys on `(job_id, shard_index, attempt)` and there is no binding from
that key to a plain node. Until a trigger binding for plain nodes exists, `startNode` rejects
`runner_auto` for any node id that is not a `shard:` id (422 `runner_auto_requires_shard`).
The `kind = 'node'` lineage and `oom_retry_node_id` use for plain nodes in revision 1 are
dropped from this design. (`oom_retry_node_id` stays in `logic.rs` unused by this design; it is
removed in stage 1 only if the owner confirms plain nodes stay out of scope.)

**Decision required: H1 / Q9 - the trigger.**

*The problem, as the review states it.* `cloud-ci agent` samples for a fixed `--duration-secs`
window (default 60 s; `cloud-ci-cli/src/agent.rs` module doc lines 3-4, `sample_for`; default
in `cloud-ci-cli/src/cli.rs`) and submits ONE batch at the end. An OOM after the window is never
reported. An OOM inside the window kills the job process; `run_and_report`
(`node_container.rs`) posts `/complete-node` `failed` (exit 137) immediately, and the batch
arrives later. The admissibility rule `logic::oom_event_admissible =
!run_terminal && !node_state.is_terminal()` then rejects the late report because the node is
already `failed`. So the dominant OOM outcome (process killed, node failed, late report) is
exactly the one the rule excludes. If the agent itself is OOM-killed there is no report at all.
Revision 1 never discussed the ordering between the report and `/complete-node`.

*Options (no answer is chosen here):*

* **A. Agent-only evidence, made reachable.** Keep `oom_detected` from the agent as the only
  trigger and (i) widen the agent's window to the whole job, and/or (ii) admit a `failed` node
  with OOM evidence and "upgrade" it to an OOM retry before the barrier or fail-fast sees the
  failure. Pros: no Worker-side container exec, no new internal contract, reuses the shipped
  `node_id` and attempt carriers. Cons: cannot work if the agent is itself killed; the report
  arrives after `/complete-node`, so the failure is already recorded and (under H3 option 2)
  already fed to the barrier; upgrading afterwards needs a verdict-holding window with no
  natural end (Q11); the batch is immutable per `(job, shard, attempt)`, so the agent cannot
  flush early without a contract change; a whole-job window changes `cloud-ci agent` semantics
  for BYO CI too.
* **B. Worker-side detection (the fallback analytics.md already specifies).** After the main
  `exec` exits non-zero, `NodeContainer` runs a second `exec` that reads the container's
  `memory.events` (`oom_kill` counter) and treats exit code 137 plus `oom_kill > 0` as OOM
  evidence, and reports it on the node's `/complete-node` call (an optional internal field on
  `CompleteNodeRequest`; it is a Durable-Object-internal request, not the public proto).
  The coordinator then records the node's failure and the OOM decision **in the same
  transaction**, so fail-fast and the barrier never see an un-upgraded failure. Pros: works when
  the agent is killed; matches analytics.md's two triggers; evidence and failure arrive
  together, which removes the ordering hazard. Cons: a second `exec` after the main one exited
  is `[unverified]` against a real runtime, as is whether the cgroup it sees is the job's; 137
  also arises from a manual kill or a timeout kill (mitigated by requiring `oom_kill > 0`);
  needs the container to remain up after the main process dies; adds a Worker-owned
  `memory.events` parser.
* **C. Both.** B as the trigger of record, the agent's `oom_detected` as corroborating evidence
  stored on the batch. Cons: two paths into one decision need one deduplication rule (the
  decision is keyed by lineage and seq, §2.2, so both converge, but tests double).

*Recommendation (owner decides):* B, optionally corroborated by A (C). Reason: it is the only
option that fires in the dominant case and it removes the report-after-failure ordering problem
by construction. Whichever is chosen, one rule is fixed: **OOM evidence must be evaluated no
later than the moment the node's failure is recorded**, because after that the barrier and
fail-fast may already have acted on it.

**Trigger classification: OOM / NOT-OOM / UNKNOWN (G1).** Whichever H1 option is chosen,
classifying one failed node's evidence must be total and three-valued, never a bare boolean:

* **OOM** - exit code 137 (SIGKILL) AND `oom_kill > 0`, both successfully read. Triggers
  `decide_oom_recovery`.
* **NOT-OOM** - evidence was successfully read and does not meet the OOM rule above. An
  ordinary failure: recorded normally (`resolve_complete_node`'s existing path), never retried.
* **UNKNOWN** - evidence could not be read or could not reach the coordinator at all.
  **Also an ordinary failure, never retried** - UNKNOWN is not a third kind of trigger, it is
  NOT-OOM's safe degradation: inability to prove OOM must never silently behave as if OOM
  evidence existed.

Cases, and which bucket they fall in:

| Case | Bucket | Why |
| --- | --- | --- |
| exit 137, `oom_kill` read successfully and `> 0` | OOM | matches the rule |
| exit 137, `oom_kill` read successfully and `= 0` | NOT-OOM | a definite negative read: something sent SIGKILL (manual kill, a timeout mechanism) with no kernel OOM event in this cgroup |
| `oom_kill > 0`, exit code is not 137 (e.g. a child process was OOM-killed but the job's own shell exited `1`) | NOT-OOM | the strict AND rule is not met; memory pressure existed but did not kill the measured process, so retrying would not necessarily help |
| the second `exec` (option B) fails, or `memory.events`/`memory.peak` cannot be read | UNKNOWN | evidence unreadable |
| the agent's sampling window ends before the job does (option A only) | UNKNOWN | no evidence was ever collected for the failure |
| a cancelled node | excluded structurally, not classified | `resolve_complete_node` already returns `DroppedCancelled` before any status write (`logic.rs` line 778); OOM classification never runs for a cancelled node |
| a container never started, or is lost (no `/complete-node` ever arrives) | UNKNOWN, and un-actionable | nothing reaches the coordinator to classify; the node stays non-terminal until the run's own timeout (§3.6's weakened backstop) |
| a timeout kill (a job-level wall-clock timeout mechanism sending SIGKILL/SIGTERM) | NOT-OOM if `oom_kill` reads `0`, UNKNOWN if unreadable | same rule as above; a timeout kill is not itself OOM evidence |
| a crash or infrastructure loss (the DO, the container, or the whole job is lost) | UNKNOWN | no evidence reaches the coordinator; same as "container never started or lost" |

**Evidence source, exact (read 2026-10-03).** The agent **already** reads both counters:
`cloud_ci_core::cgroup::CgroupReader::read_oom_kill` and `::read_memory_peak`
(`packages/cloud-ci-core/src/cgroup.rs` lines 209-213 and 197-205), backed by
`parse_memory_events_oom_kill` (line 111-112) and `parse_memory_peak` (line 98-106). Called
from `cloud-ci-cli/src/agent.rs::sample_for` once at the start (lines 174-176, the baseline
`oom_kill_at_start`) and once at the end (lines 199-204, `oom_kill_at_end`); `oom_detected =
oom_kill_at_end > oom_kill_at_start` (`cloud_ci_core::sampler::Sampler::finish`,
`packages/cloud-ci-core/src/sampler.rs` lines 119-123).

**Path, permissions, and failure mode (option A).** `CgroupReader::new(base)` takes
`--cgroup-path` (default `/sys/fs/cgroup`, `cloud-ci-cli/src/cli.rs` line 386).
`CgroupReader::read` (`cgroup.rs` lines 169-174) is a plain `std::fs::read_to_string` with no
privilege elevation - an ordinary file read inside the container's own cgroup mount. If a read
fails (missing file, permission denied), `sample_for` propagates the error with `?`
(`agent.rs` lines 174-176 and 199-204 both `map_err(...)?`), which **aborts the entire agent
run before any `SubmitResourceSamples` call is made** - there is no partial batch. So under
option A, "`memory.events` unreadable" does not produce a batch with missing fields; it
produces no batch at all, and the node's eventual `/complete-node` (posted independently by
`run_and_report`) carries only the job's own exit code, with no OOM evidence attached -
UNKNOWN by construction.

**When it flushes (option A).** Once, after `sample_for`'s bounded loop for `--duration-secs`
(default 60 s), as one `SubmitResourceSamples` call. `node_status_for_exit_code` (`logic.rs`
line 835) maps every nonzero exit code, including 137, to `Failed` - there is no OOM-awareness
in the exit-code mapping itself; all OOM awareness is in the agent's separately-delivered batch.

**Option B is new code with no precedent in the tree (`[unverified]` for the pinned runtime).**
A spike must prove, against a real deployment, before option B is chosen:

1. A second `exec()` into the container succeeds **after** the main `exec`'s process has
   already exited non-zero. Cloudflare's Durable Object Container API page (read 2026-10-03)
   says `exec()` "does not start a stopped container" - if the container itself (not just the
   main process) transitions to stopped when the job's process dies, a second `exec` is
   impossible and option B cannot work as described.
2. The cgroup the second `exec`'s process sees is the **same** cgroup the main process ran in,
   not a fresh one. `cgroup.rs`'s own module doc says each job's cgroup is "created fresh per
   job" - a second process started via a separate `exec` call could plausibly get its own
   distinct cgroup, which would read `oom_kill = 0` even if the main process was genuinely
   OOM-killed. This must be confirmed, not assumed.
3. `memory.events`/`memory.peak` are still readable after the main process has exited and
   before the container is torn down - the pseudofiles are per-cgroup and typically persist
   until the cgroup itself is removed, but the Containers runtime's exact teardown timing for
   a `durable_object`-policy container is not documented in the pages read for this design.

If the evidence cannot be read or cannot be transported to the coordinator under the chosen
option, the result is **UNKNOWN**, and recovery stays off: the ordinary failure is recorded
through the existing path and no retry is attempted. This is the safe failure mode.

### 1.2 Deriving the retry's identity

The retry's identity is a pure function of the lineage and the observed attempt, so a replayed
effect recomputes the same id and address.

* Retry node id = `logic::shard_node_id(job_name, idx, observed_attempt + 1)`, that is
  `shard:{job}:{idx}:{observed_attempt+1}`; the retry's shard attempt is `observed_attempt + 1`.
  This is deliberately **not** `oom_retry_node_id` (revision 1 explains why: the canonical id is
  already matched by `shard_node_matches` / `shard_nodes_to_cancel`, so fail-fast cancellation
  finds a retry with **no matcher change**, and `shard_state`'s `(job_name, idx, attempt)` key
  and `latest_attempt_per_shard` already model a higher attempt). The scheme is injective: `idx`
  and `attempt` are the last two numeric components, and `shard_node_matches` requires a
  canonical digit suffix, so `shard:a:1:2:3` cannot be confused with job `a` idx 1.
* Physical address = `coordinator::node_physical_address(run_do_name, retry_node_id)`
  (unchanged length-prefixed SHA-256), stored on the node row like any node.
* **Spec hash:** the retry's `spec_hash` MUST include the instance size and `shard_attempt`
  (plus image and command, as today). Otherwise a natural second dispatch of the same shard
  with the same id and hash would be silently adopted by `resolve_start_node` as the OOM retry.
  A pre-existing row with that id and a different hash is a conflict: the decision goes to
  `stuck` (§3.4), never overwritten.

### 1.3 Carrying node id and attempt into the new container

Today `node_container::start_container` posts `{run_do_name, node_id, image, command}` and
`exec_in_container` calls `container.exec(&cmd, None)` (`node_container.rs` line 362): no
environment at all.

Cloudflare's Durable Object Container API (docs last updated 2026-09-30, read 2026-10-03):
`start({ env })` variables are **not** passed to `exec()` processes (except `PATH`), and
`exec({ env })` replaces the environment ("Other variables from `start()` or the image are not
inherited, except for `PATH`"). So the carrier is the `env` option of `exec`.

**Confirmed against the pinned fork (2026-10-03).** The pinned `workers-rs` fork
(`btkostner/workers-rs@df96700ab45b9e5dc400c48946f5c57951dcfd11`, ADR 0011) was read from a
local checkout at that exact commit (`git rev-parse HEAD` printed the pinned hash). In
`worker/src/container.rs`: `ContainerStartupOptions::set_instance` (line 172),
`ContainerStartupOptions::set_custom_instance` (line 179), `ContainerExecOptions::add_env`
(line 284), and `Container::exec(cmd, Option<ContainerExecOptions>)` (line 53). Therefore no
fork extension and no command-wrapping fallback are needed: `NodeContainer` builds a
`ContainerExecOptions`, calls `add_env` for each variable, and passes `Some(options)` where it
passes `None` today.

* Variables set per node: `CLOUD_CI_NODE_ID={node_id}` and
  `CLOUD_CI_SHARD_ATTEMPT={shard_attempt}`, plus whatever the agent needs
  (`CLOUD_CI_TOKEN`, `CLOUD_CI_SERVER_URL`). **Because `exec` env replaces the environment,
  this changes the environment of every node, not only auto ones**, and no dispatcher mints the
  token or URL today (roadmap Phase 6: bootstrap-token path not built). See Q3.
* **Command-template contract (M5, Q14).** `resolve_shard_attempt` and `resolve_node_id` give an
  explicit flag priority over the env var (`cloud-ci-cli/src/identity.rs`,
  `cloud-ci-cli/src/agent.rs`). Therefore the command a `runner_auto` shard node is started
  with MUST carry **no** `--attempt`, `--node-id` or `--instance-type` flags. If it did, the
  retry (same command, new env) would report the old attempt and node id and fail the §1.1
  binding rule, so the second OOM would never be detected. `handle_start_node` rejects a
  `runner_auto` command containing any of those three flags (422
  `runner_auto_command_carries_identity_flag`). The current size is read from `node.size` (§1.1),
  not from `--instance-type`.
* `StartNodeRequest` gains `shard_attempt: Option<u32>`, `runner_auto` (§5.1) and the instance
  size; `JobSpec` (`executor.rs`) gains the same; all feed `spec_hash`.

A second OOM then maps unambiguously: the retry agent reports `node_id = shard:{job}:{idx}:{n+1}`
and `attempt = n+1`; the binding rule matches the retry's node row; `decide_oom_recovery` sees
the lineage's first decision with `attempt + 1 == incoming.attempt` and returns
`New(FailedAtMax)` (existing, tested branch). A replay of the original attempt now has a
different `node_id`/attempt pair and is rejected by the binding rule.

## 2. State

### 2.1 `node` table additions (guarded, nullable, no backfill)

Added with the existing literal `pragma_table_info` guard pattern (see `ensure_schema`'s
`physical_address` retrofit), one guard per column. **No shared `ensure_column` helper in the
early stages:** `ensure_schema` runs on every DO fetch and alarm for every existing deployment
and there is no DO-level test harness (Q4), so refactoring the two existing guards is deferred
until a harness exists. Revision 1's bound-parameter `pragma_table_info(?1)` form is
`[unverified]` in workerd (the existing guards use literals) and is not used.

| Column | Type | Meaning |
| --- | --- | --- |
| `runner_auto` | `INTEGER NOT NULL DEFAULT 0` | 1 iff the node declared `runner: "auto"`. |
| `size` | `TEXT` | Instance size this node was started at (ladder name). Authoritative for decisions (Q15). |
| `size_min`, `size_max` | `TEXT` | Bounds frozen at `startNode` (§5.2). |
| `shard_attempt` | `INTEGER` | Shard attempt this node runs as. |
| `closed_by` | `TEXT` | Set to `'oom'` when the coordinator terminated this node for an OOM decision (M1). |

**Legacy rows:** all `NULL`/`0`. They are never auto-recovered (`runner_auto = 0` fails the
binding rule; the report is samples-only). No backfill, no guess. The existing
`physical_address = NULL` rule (`stop_node_container` logs and returns `false`) is unchanged.

**M1 - coordinator-terminated nodes ignore later completions.** After `StopOld` destroys the old
container, `NodeContainer`'s background `exec` rejects or exits and `run_and_report` posts
`/complete-node` `failed` for the old node id. Today `resolve_complete_node` (`logic.rs`
774-785) returns `Recorded` for failed-after-failed, and `update_node_status` (`mod.rs`
5293-5308) overwrites BOTH `result` and `completed_at`, then re-projects to D1. That would
silently replace the `{"error":"oom"}` record, the `failed_at_max` message naming the
configured max and peak, and a `stuck` marker. **Rule:** a node with `closed_by = 'oom'`
ignores any later `/complete-node` (no write, no re-projection), the same way a `Cancelled` node
already returns `DroppedCancelled`. Implemented as a new `CompleteNodeDecision` variant in the
pure function, with tests.

### 2.2 `oom_decision` (new, DO-local, one row per decision) - the H2 model

Revision 1 modeled "one row per lineage, ever". That cannot hold the second decision:
`decide_oom_recovery` returns `New(FailedAtMax)` for the second OOM after a `RetryAt` record,
and that decision's effects (`MarkOldTerminal` and `StopOld`) target the **retry** node, not the
original; the first row's `old_*` flags are already true and `base_node_id` points at the
(already terminal) original, which is also what `resolve_oom_node_id` returns for `FailedAtMax`.
That is the wrong node, which would leave the retry's container unstopped and its node unmarked.

**Model: one row per decision, at most two per lineage, each with its own flags and target.**
The lineage is `(job_name, idx)` (shards only, §1.1).

```sql
CREATE TABLE IF NOT EXISTS oom_decision (
  job_name            TEXT    NOT NULL,
  idx                 INTEGER NOT NULL,
  seq                 INTEGER NOT NULL CHECK (seq IN (1, 2)),
  observed_attempt    INTEGER NOT NULL,  -- the shard attempt that OOM'd (OomDecisionRecord.attempt for seq 1)
  target_node_id      TEXT    NOT NULL,  -- the node that OOM'd = the node THIS decision marks and stops
  outcome             TEXT    NOT NULL CHECK (outcome IN ('retry', 'failed_at_max')),
  from_size           TEXT    NOT NULL,
  to_size             TEXT,              -- 'retry' only
  max_size            TEXT    NOT NULL,
  reason              TEXT    NOT NULL,  -- rightsizing::oom_retry string, unchanged
  measured_peak_bytes INTEGER,           -- NULL = unknown, never 0
  retry_node_id       TEXT,              -- 'retry' only: shard_node_id(job, idx, observed_attempt + 1)
  -- OomEffectFlags, one column each; per decision, never shared:
  old_marked INTEGER NOT NULL DEFAULT 0, old_stopped INTEGER NOT NULL DEFAULT 0,
  old_projected INTEGER NOT NULL DEFAULT 0, retry_inserted INTEGER NOT NULL DEFAULT 0,
  retry_started INTEGER NOT NULL DEFAULT 0, retry_projected INTEGER NOT NULL DEFAULT 0,
  -- bounded retry state (§3.4):
  effect_attempts     INTEGER NOT NULL DEFAULT 0,
  next_attempt_at     INTEGER,           -- ms epoch; NULL = due now
  last_error          TEXT,
  state               TEXT    NOT NULL DEFAULT 'pending'
                      CHECK (state IN ('pending', 'done', 'degraded', 'stuck', 'abandoned')),
  created_at          INTEGER NOT NULL,
  completed_at        INTEGER,
  PRIMARY KEY (job_name, idx, seq)
)
```

*Rules:*

* **Target is always the row's own `target_node_id`.** `target_node_id` is set at insert from
  the report, never from `base_node_id` and never from `resolve_oom_node_id` (which is replaced
  in stage 1 by `oom_effect_target(row)`). For seq 1, the target is the original node. For
  seq 2, it is the retry node, which equals seq 1's `retry_node_id`; the insert verifies that
  equality and refuses otherwise.
* **`existing` is the projection of the LATEST decision row for the lineage** (D1) - not
  always the seq 1 row: `attempt = that row's observed_attempt`, `outcome = that row's
  outcome`. `base_node_id` is not read by `decide_oom_recovery` and is not part of the
  mapping.
* **Seq assignment is race-free (D1).** The DO is single-threaded between `await` points. The
  insert path: read `SELECT MAX(seq) FROM oom_decision WHERE job_name = ? AND idx = ?` (or no
  row), compute `decide_oom_recovery(ladder, existing, incoming)` from that row's projection,
  and `INSERT` the new row with `seq = (max seq) + 1` (`1` if none) - with **no `await`
  between the read and the insert**. The primary key `(job_name, idx, seq)` is the backstop,
  not the primary race guard: with no intervening `await`, a second insert at the same
  `(job_name, idx, seq)` can only happen on re-delivery of the same evidence event, not from a
  genuine concurrent race.
* **A primary-key collision on insert is handled as "already decided", never surfaced as an
  SQL error (D1).** If the `INSERT` fails on the `(job_name, idx, seq)` constraint, the caller
  re-reads the row that now exists at that `seq`, re-projects it, and returns its `outcome` as
  `AlreadyDecided` - the same shape `decide_oom_recovery` already returns for a duplicate. This
  is the re-delivery case: the same evidence event (the same batch content hash under option A,
  or a duplicate `/complete-node` under option B) reaching the insert a second time.
* **Duplicate reports must pass the latest row, not always the seq-1 row (D1).** Revision 2
  said "any further report is `AlreadyDecided` (`decide_oom_recovery` already returns this for
  `FailedAtMax`)" - that is correct only once the **seq 2** row exists and is passed as
  `existing`. A duplicate report arriving when only the seq 1 `RetryAt` row exists, with
  `incoming.attempt == seq1.observed_attempt + 1`, is **not** yet `AlreadyDecided`: it is
  `New(FailedAtMax)` the first time (which creates seq 2) and only `AlreadyDecided` on a later
  call once seq 2's row is read and passed as `existing`. The caller must therefore always
  read and pass the **latest** row (seq 2 if present, else seq 1, else `None`), never a cached
  seq 1 projection.
* Seq 2 is written only when the latest row is seq 1, its outcome is `retry`, and
  `incoming.attempt == seq1.observed_attempt + 1` (the existing `decide_oom_recovery` branch).
  `New(FailedAtMax)` becomes `outcome = 'failed_at_max'`, `target_node_id =` the retry node,
  flags all fresh (`false`), `is_retry = false` for `next_oom_effect`. Its effects are exactly:
  `MarkOldTerminal` (the retry node, `closed_by = 'oom'`, result carrying the max and peak),
  `StopOld` (the retry node's own `physical_address`), `ProjectOld`.
* Seq 1's pending projections and seq 2's critical effects are drained independently (separate
  rows, separate counters); seq 2 cannot exist before the retry started, but seq 1 may still
  have projections pending.
* **D2 - the row-to-flags mapping sets `decision_projected = true` unconditionally on read**,
  not via a stored column: the `oom_decision` schema above has no `decision_projected` column
  (that field only matters while `sizing_decisions` is deferred, §2.3), so converting a SQL
  row to the pure `OomEffectFlags` always sets `decision_projected: true` regardless of row
  contents. When `sizing_decisions` is eventually built, this mapping - and the schema - both
  gain a real column and this hardcoded `true` is removed.
* The H2 specification is final enough to implement; what it needs from the reviewer is
  confirmation of the one-row-per-decision shape and the latest-row projection above.

**Atomicity (low notes).** The decision row is inserted inside the **same
`storage().transaction()` closure** that records the event carrying the evidence (the batch
acceptance in `accept_resource_sample_batch_and_queue_all` for trigger A, or the node-completion
write for trigger B). A duplicate batch returns early at `AlreadyAccepted`, so a decision
written in a separate step after a crash would never be re-driven. The reads feeding
`decide_oom_recovery` and the insert contain no `await` between them. The single alarm slot is
**armed before** the commit (as `handle_shard_terminal` does with
`schedule_overflow_flush_alarm`), so a crash after commit still has a wake pending. Effects are
drained inline once right after the commit (best effort) and by the alarm thereafter; the
inline drain is an optimization only, the alarm is the guarantee.

### 2.3 D1 `sizing_decisions`: deferred; the shape this design needs

**Deferred to the cron work (Q5).** `sizing_decisions`, its migration, its projection function
and the `GET /oom-lineages` route in revision 1 are all removed from this design's staged
delivery. Reasons: nothing reads the table, a writer-less table is what migration 0015's own
comment avoids, and `node.result` plus the stuck log line (§3.4) already give operator
visibility. The D1 migration stays forward-only and additive when it is eventually written.

**analytics.md must be updated when the table is built (C3).** `analytics.md` line 395 lists a
narrower shape:

```
CREATE TABLE sizing_decisions (repo_id, job_name PK, current_instance_type, p95_memory_bytes,
                               p95_cpu_frac, last_resized_at, reason, PRIMARY KEY (repo_id, job_name));
```

An OOM writer needs these **extra columns** beyond that shape:

| Extra column | Why |
| --- | --- |
| `current_memory_bytes INTEGER NOT NULL` | D1 SQL cannot see the executor ladder, so the monotone guard below compares memory (M6). |
| `source TEXT NOT NULL DEFAULT 'oom' CHECK (source IN ('oom','cron'))` | Two writers (this design, the nightly cron); the reader must know which wrote the row. |
| `run_id TEXT` | The run whose OOM produced an `oom` row (audit, dashboard). |
| `peak_bytes INTEGER` | Measured peak at the decision, `NULL` = unknown (never `0`). |
| a `CHECK` tying `source` to its allowed values (above) | Reject unknown writers. |

`pending_candidate`/`pending_nights` (the hysteresis counter, `rightsizing.rs`) belong to the
cron's migration.

**M6 - OOM writes only raise the size.** Revision 1 used a `last_resized_at`-monotone guard
across many concurrent `RunCoordinator`s writing one per-repo row; with clock skew and `>=`
ties, an OOM from run A could overwrite a larger size set earlier by run B. **Rule:** an
`oom`-source write is `INSERT ... ON CONFLICT (repo_id, job_name) DO UPDATE SET ... WHERE
excluded.current_memory_bytes > sizing_decisions.current_memory_bytes`. The cron writer owns
downsizing under hysteresis. This is not a violation of AGENTS.md "only a run's
`RunCoordinator` writes run state" (that rule is about run state; the OOM writer is still the
run's coordinator), but the cron second writer is an owner question (Q5).

## 3. Effects and healing

### 3.1 Ordered effect list

The order is already encoded in `logic::next_oom_effect_excluding` and is kept verbatim:

1. `MarkOldTerminal`: mark the decision's target node `failed`, `closed_by = 'oom'`, result
   `{"error":"oom", ...}` (naming the configured max and the measured peak, "unknown" if `None`,
   for `failed_at_max`).
2. `StopOld`: `stop_node_container(target_node_id, target.physical_address)`.
3. `InsertRetry` (retry only): `insert_node` with the §1.2 id, `runner_auto = 1`, `size = to_size`,
   `shard_attempt = observed_attempt + 1`.
4. `StartRetry` (retry only): `node_container::start_container` with the chosen instance; gated
   on `old_stopped` and on the §3.3 live re-check.
5. `ProjectOld`, `ProjectRetry`: `project_node_to_d1`. Projections never gate effects 1-4
   (`OomEffect::is_projection`).

`ProjectDecision` has no work while `sizing_decisions` is deferred (§2.3).

### 3.2 Idempotency per effect

| Effect | Idempotent because | Flag set when |
| --- | --- | --- |
| MarkOldTerminal | `update_node_status` to `failed` only if the node is non-terminal; a node already terminal for another reason (it naturally succeeded, or was cancelled) means the decision is `abandoned` (§3.6), not overwritten. | the row is terminal and `closed_by` is ours |
| StopOld | `NodeContainer::handle_stop` is a no-op when not running; `resolve_stop_outcome(addr, ok) == Pending` keeps the flag unset. | stop returned 2xx |
| InsertRetry | deterministic id plus `resolve_start_node` (same spec hash = no-op; different = conflict, `stuck`). | row exists with our spec hash |
| StartRetry | `NodeContainer::handle_start` returns `started:false` if already `running()`. A start that throws marks the retry row `failed` (as `handle_start_node` does) and still sets the flag. | start 2xx, or row marked failed |
| Project* | `INSERT .. ON CONFLICT DO UPDATE`. | D1 call returned ok |

`start()` returns before the container is ready, and later failures need `monitor()`
(Cloudflare, "Durable Object Container API", `start`: "To catch later errors, including a
container that fails to start, use `monitor()`"). A retry that fails after `start()` is
surfaced like any node, through `run_and_report` posting `/complete-node`. A container that
never starts posts nothing; see the weakened timeout statement in §3.6.

### 3.3 `StartRetry` re-reads live state immediately before starting (M3)

`StartRetry` is gated on the decision and the run, but revision 1 did not gate on the retry
node's own status or the group status at start time. If fail-fast or cancel marks an
inserted-but-not-yet-started retry `Cancelled` (`ensure_sibling_cancelled`; stop is a no-op
because it is not running), a later drain would still call `start_container` and leak a
container for a cancelled node. **Rule:** immediately before `start_container`, in the same
synchronous region as the check (no `await` between), re-read (a) the retry node row: it must
exist and be non-terminal (`NodeState::Running`, as `insert_node` writes `'running'`); (b) the
`job_group` row: `status = 'running'`; (c) the run: non-terminal. If any fails, do not start;
mark the decision `abandoned` and set `retry_started` so the sweep does not loop.

### 3.4 Retry cap, backoff and the operator-visible failure state

One cap, one counter per decision row (`effect_attempts`), with projections non-blocking
(revision 1's two counters, critical 8 and projection 12, are dropped):

* A drain pass that makes **no progress** because an effect returned `Err` increments
  `effect_attempts`, stores `last_error`, and sets
  `next_attempt_at = now + min(5 s * 2^(effect_attempts - 1), 5 min)` (5, 10, 20, 40, 80, 160,
  300, 300 s). The cap is `OOM_EFFECT_MAX_ATTEMPTS = 8` (5+10+20+40+80+160+300 = 615 s, about
  10 minutes cumulative to reach the cap).
* Critical effects (1-4) run before projections within a pass. A failing projection does not
  stop critical effects and does not block `done` for container state; it consumes the same
  counter.
* On reaching the cap: if a **critical** effect is incomplete, `state = 'stuck'`; if only
  projections are incomplete, `state = 'degraded'` (no container risk). `done` means every
  applicable effect completed.
* **Operator-visible**, three ways (the `GET /oom-lineages` route is deferred, §2.3):
  1. a `console_log!` line with the stable prefix `oom_decision stuck:` or `oom_decision
     degraded:` plus job, idx, seq, effect and `last_error`;
  2. the target node's `result` JSON gets `{"oom_recovery":"stuck","effect":..,"error":..}` and
     is projected to D1 `nodes.result` where the report reads it;
  3. the row itself (`SELECT` through the DO).
* `stuck` is terminal for the sweep. A stuck decision whose old container is **not** confirmed
  stopped is the dangerous case; its log line says so ("old container may still be running at
  {physical_address}"). Manual recovery tooling is out of scope; the state is the contract.
* An `InsertRetry` conflict goes to `stuck` immediately (it cannot succeed by retrying).

`shard_terminal_effect`'s own unbounded 5 s retry is **not** changed by this design (Q6).

### 3.5 Failures of critical effects, concretely

* `MarkOldTerminal` fails only on SQL error: retry/backoff.
* `StopOld` failing blocks `InsertRetry`/`StartRetry` (O4). While blocked the shard has no
  running retry. The run timeout bounds the **run record** but not the container (§3.6).
* `StartRetry` throwing marks the retry row `failed`, sets the flag, and the decision proceeds
  to projections. Whether the barrier then sees a failed attempt 2 depends on the H3 decision
  (§4): with today's code it does **not**, because nothing writes `shard_state` for it.
  Revision 1's statement that "the barrier counts a failed attempt 2" was wrong for the code as
  it stands.

### 3.6 Alarm composition, terminal runs, and the close path (C1, M2)

**Alarm order (M2).** Today `alarm()` (`coordinator/mod.rs` around 980-1045) drains
`shard_terminal_effect` rows and overflow **first**, then handles the terminal check and the
deadline. That order is deliberate: a terminal run must still drain its stranded effects and
backlog. Revision 1's sketch moved the terminal check and the deadline check above both drains,
which is a regression, not a behavior-preserving move: a terminal run with pending work would
re-arm forever (`has_pending_background_work` stays true) without draining. **Rule:** the
existing drain-first order is kept. The OOM sweep is inserted directly **after** the existing
shard-terminal-effect drain and **before** the overflow flush, so it also runs for terminal runs:

```
alarm():
  ensure_schema
  read run (if some):
    existing shard_terminal_effect drain            (unchanged)
    drain_oom_decisions(run_row, budget)            (new; runs for terminal runs too)
  overflow flush                                    (unchanged)
  run missing  -> return
  run terminal -> release_alarm_if_idle             (unchanged; now also sees pending decisions)
  now >= deadline -> handle_close_run(by_timeout)   (unchanged position)
  re-arm: min(5 s retry delay if any backlog/effect pending, earliest decision next_attempt_at)
          via logic::clamp_retry_delay_to_deadline  (existing)
```

*Why the close cannot be starved:* every OOM effect does bounded work per call, errors are
caught and recorded (`effect_attempts`, `next_attempt_at`) and never propagated with `?` (the
existing shard-effect drain has the same shape), a decision not yet due (`next_attempt_at > now`)
is skipped without I/O, at most `OOM_SWEEP_MAX_DECISIONS = 4` decisions are touched per fire,
and the cap turns every failing decision into `stuck`/`degraded` after about 10 minutes, after
which it never re-arms the alarm. `has_pending_background_work` (used by
`release_alarm_if_idle`) gains `oom_decision WHERE state = 'pending'`;
`stuck`/`degraded`/`abandoned`/`done` rows never hold the alarm slot.

**C1 - `handle_close_run` stops no nodes.** Revision 1 claimed `handle_cancel_run` and
`handle_close_run` "already stop every non-terminal node via `ensure_sibling_cancelled`". That
is wrong for close. `ensure_sibling_cancelled` is called only from `handle_cancel_run`
(`mod.rs` around 2927) and from fail-fast sibling cancellation (`cancel_shard_nodes`, around
3197). `handle_close_run` (around 2289-2380) never reads or stops any node. A timeout close or a
normal close therefore **leaves a started retry running and non-terminal**, exactly as it leaves
every other running node today. Consequences for this design:

* The statement "the run timeout is the backstop" (revision 1, §3.2 and §3.5) is **weakened**:
  the timeout closes the **run record**; it does not stop containers and does not terminate
  nodes. A leaked old or retry container runs until its command ends on its own.
* For a terminal run, the §3.2 rule below still applies: `MarkOldTerminal`/`StopOld` of a
  pending decision run (they free a container), the retry is never started.
* Whether `handle_close_run` should stop non-terminal nodes (for the retry and for all nodes)
  is a pre-existing gap, not an OOM-specific one. It is an owner question: Q16.

**Terminal run for an existing decision (was §3.6 in revision 1):** `oom_event_admissible`
already refuses a *new* decision for a terminal run. For an existing one when the run becomes
terminal (close, cancel, timeout): if `retry_inserted`/`retry_started` are still false the
decision becomes `abandoned` for those effects (no retry is created or started; a terminal run
must not get a new container), while `MarkOldTerminal`, `StopOld` and projections still run. A
retry already started is just another node: cancel stops it (`handle_cancel_run`), close does
not (C1, Q16).

**Healing does not depend on the flag.** `OOM_RECOVERY` gates only the **creation** of new work:
accepting `runner_auto` at `startNode` and writing new `oom_decision` rows. The sweep, the
effect drain, the §3.3 re-check and the terminal-run handling run for **existing** pending rows
whether the flag is on or off. Switching the flag off never strands an in-flight decision.

## 4. Shard-terminal interaction

Today `complete_own_shard_node(sql, job_name, idx, attempt, outcome)` computes
`logic::shard_node_id(job_name, idx, attempt)` and `drain_shard_terminal_effects` /
`resolve_shard_cancel_node_ids` (via `shard_nodes_to_cancel`) match by prefix. With §1.2 every
shard node, original or retry, is `shard:{job}:{idx}:{attempt}` with `attempt` its own shard
attempt, so **exact-attempt resolution already lands on the right node**. The R1 bug existed
only because the removed code replaced that lookup with a lineage lookup.

**Rules that hold under either H3 option:**

1. **Resolve by the reporting attempt, always.** A shared helper
   `resolve_live_shard_node(sql, job, idx, attempt) -> Option<NodeRow>` (exact `shard_node_id`,
   no lineage redirect) is the only lookup `complete_own_shard_node` and
   `drain_shard_terminal_effects` use. A late attempt-1 report completes attempt 1's node
   (already terminal via `MarkOldTerminal`, so `shard_self_completion_target` changes nothing)
   and can never touch attempt 2. A unit test proves it never returns attempt N+1 for N.
2. **Stop only the node you resolved**, via that row's own `physical_address`; attempt 1's stop
   cannot kill attempt 2 (addresses differ).
3. **Fail-fast** still freezes `cancel_node_ids` at decision time
   (`resolve_shard_cancel_node_ids`). A retry that exists at that moment matches
   `shard_node_matches` and is cancelled. A retry created after the freeze is not in the frozen
   list; §3.3 closes the window by refusing to start it when the group is no longer `running`.

**Decision required: H3 / Q10 - what feeds the shard barrier.**

*The problem, as the review states it.* The barrier input for the retry has no producer.
Nothing in the tree calls `RunCoordinatorStore::shard_terminal` (`mod.rs` around 6307) or
constructs a `ShardTerminalRequest`, and the public proto has no shard-terminal RPC
(`ingest.proto` rpc list; `CompleteShardRequest` has no `attempt` field and routes to
`handle_complete_shard`, a different job/shard-completion path). Revision 1's rules about "the
SDK/agent may still post `shard-terminal(failed, attempt=1)`" and "when attempt 2 reports"
presuppose a path that does not exist. Concrete consequence: when `StartRetry` fails (the
retry row marked `failed` by the coordinator), no `shard_state` row is ever written;
`latest_attempt_per_shard` reads `shard_state`, not `node`; with attempt 1 marked superseded,
the group waits for the run timeout.

*Options (no answer is chosen here):*

* **Option 1: a public `ShardTerminal` RPC carrying the shard attempt.** Additive proto change
  (`buf breaking` safe), authenticated like other run-bound ingest calls, routed to the existing
  internal `/shard-terminal` route. The SDK or agent must call it with the attempt. Pros:
  reuses `handle_shard_terminal` and its durable effect row unchanged, and carries `report_key`
  and `duration_ms`. Cons: no caller exists (SDK token plumbing is missing, roadmap item 2);
  a coordinator-failed retry (start failure) still has no reporter; needs the "superseded" marker
  (attempt N's failed report is recorded but must not trip the barrier while its retry is in
  flight) and a verdict-holding rule (Q11); two producers (agent and coordinator) for one fact.
* **Option 2: the coordinator derives `shard_state` from `/complete-node` of `shard:` nodes.**
  These nodes are coordinator-owned and carry the attempt in their id
  (`shard_node_id(job, idx, attempt)`), so the coordinator can write the terminal `shard_state`
  row, with the node-completion write, in one transaction. Pros: removes the superseded window
  (attempt N's failure is simply not written while a decision for `(job, idx)` is `retry`
  and pending; the retry's completion, including a coordinator-marked `failed` after a start
  error, is the shard's verdict); no client contract; no new RPC. Cons: a shard not dispatched
  as a `shard:` node (the `startNode` registration is optional per `shard_nodes_to_cancel`'s doc)
  has no producer, so it still needs option 1 or stays unsupported; `report_key`/`duration_ms`
  must come from somewhere else (open sub-question: how merge finds a shard's report key);
  changes who writes `shard_state`, a semantic change to `handle_shard_terminal`'s idempotent
  record.

*Recommendation (owner decides):* option 2 for `shard:` nodes, keeping the internal
`/shard-terminal` route as-is and adding no public RPC until a caller needs it. Reason: it is
the only option in which a coordinator-failed retry reaches the barrier without a client, and
it removes the superseded window instead of managing it. The `report_key` sub-question must be
answered first (Q10).

*If option 1 is chosen,* the additional rule applies: `handle_shard_terminal` consults
`oom_decision` for `(job, idx)`; if a `retry` decision exists whose `observed_attempt ==
req.attempt`, the call is recorded (`shard_state` row, idempotency kept) with `effect_kind =
"superseded_by_oom_retry"`, no barrier evaluation, and `read_all_shard_terminal_rows` excludes
superseded `(idx, attempt)` rows from the barrier input. For a `failed_at_max` decision, the
failure is the real verdict and is evaluated normally.

## 5. Size selection

### 5.1 Declaring `runner: "auto"`

* `StartNodeRequest` gains `runner_auto: Option<RunnerAutoRequest { min, max, initial }>`
  (per-node overrides; analytics.md: `runner: "auto"` is shorthand for `settings.runners.auto`),
  `#[serde(default)]`, so an old caller is unaffected. Only `shard:` node ids are accepted
  (§1.1).
* `handle_start_node` resolves, synchronously before `insert_node`, via the existing pure
  `logic::resolve_auto_start(ladder, min, max, initial)`. An `AutoStartError` fails the request
  422 (`bounds_not_in_ladder` / `initial_out_of_bounds`), so a configuration error surfaces at
  dispatch, not at OOM time.
* While the `OOM_RECOVERY` flag is off, `runner_auto` is rejected 422
  `oom_recovery_disabled` (so no node is created that would need recovery).

### 5.2 Where bounds come from: the frozen settings snapshot

`run.settings_sha` is frozen at `BeginRun` (module docs "Settings SHA";
`resolve_settings_sha_for_admission`). Bounds are resolved from **that sha**, never from live
HEAD: `repo_settings::settings_for_sha(.., repo_id, run.settings_sha)` (the immutable
`(repo_id, head_sha)` cache, migration 0019) gives `Settings.runners.auto.{min,max,initial}`,
then per-node overrides, then `resolve_auto_start` against the executor ladder. The result is
persisted on the node row (`size`, `size_min`, `size_max`) at `startNode`, so later decisions
read the **node's** frozen bounds and do not re-fetch settings. `run.settings_sha IS NULL`
(legacy run) rejects `runner_auto` 409 `no_settings_snapshot` (fail closed; never re-resolve
HEAD). Unset `min`/`max` default to the ladder's first/last rung; unset `initial` defaults to
`min` (`resolve_auto_start`'s contract).

The documented default `runners.auto.min = "basic"` (analytics.md, settings.md) is not a valid
`durable_object` size; `resolve_auto_start` returns `BoundsNotInLadder`, which would make every
default deployment fail `runner: "auto"`. Q2.

### 5.3 What the executor must provide

1. A **multi-size ladder** in `CapabilityDescriptor.sizes` (`ContainersExecutor::capabilities`,
   currently one rung `standard-4`): `[lite, standard-1, standard-2, standard-3, standard-4]`.
2. A **per-start size**: `JobSpec.instance: Option<String>` to
   `node_container::start_container(.., instance)` to `StartRequest.instance` to
   `ContainerStartupOptions::set_instance`. The setter **exists in the pinned fork**
   (`worker/src/container.rs` line 172, confirmed 2026-10-03, see §1.3), so nothing here is
   `[unverified]` at the binding level. (Stage 0 introduces this plumbing.)
3. Names validated against the ladder before reaching the runtime (an unknown name makes
   `start()` throw `TypeError`, Cloudflare docs below).

### 5.4 What Cloudflare Containers supports (dated source)

Per developers.cloudflare.com/containers/configuration/scheduling-policy/,
.../containers/platform/limits/ and .../containers/api/durable-object-container/ (all "Last
updated Sep 30, 2026", read 2026-10-03):

* Under the `durable_object` scheduling policy (public **beta**), `ctx.container.start({
  instance })` accepts `lite`, `standard-1`, `standard-2`, `standard-3`, `standard-4`, or a
  custom `{vcpu, memoryMib, diskMb}` (1-4 vCPU, up to 12 GiB, at least 3 GiB per vCPU). **`basic`
  and the legacy `dev`/`standard` aliases are not accepted.** An unknown name throws
  `TypeError`; an invalid custom object throws `RangeError`.
* **If `instance` is omitted the container is `lite`** (1/16 vCPU, 256 MiB).
* Memory: lite 256 MiB, basic 1 GiB (`default` policy only), standard-1 4 GiB, standard-2
  6 GiB, standard-3 8 GiB, standard-4 12 GiB.
* `wrangler.toml` already sets `scheduling_policy = "durable_object"` for `NodeContainer`.

So the runtime supports per-instance sizing and a real ladder exists. **Not exercised against a
real account** (a live start with a named size, and the `exec` env replacement at runtime) -
documentation and the pinned fork's source are the only sources; the live smoke (§6.3) is the
proof. The `rightsizing.rs` Cloudflare fixture (six rungs including `basic`) must gain a
`durable_object` ladder fixture.

**Degradation if sizing cannot be done per start** (the executor offers one rung, as today):
`resolve_auto_start` yields `min = max`; `oom_retry` returns `AlreadyAtMax` on the first OOM;
every OOM is an immediate `failed_at_max` recorded on the node with no container started. The
feature then classifies and reports OOM, not retries it. This is the current real-world
behavior.

## 6. Test plan

### 6.1 Pure logic (`coordinator/logic.rs`, `mise run //packages/cloud-ci-worker:test`)

* `oom_effect_target(row)`: seq 1 returns the original, seq 2 returns the retry node, never
  `base_node_id` (regression for H2's wrong-node result).
* Second-decision model: seq 1 `retry` then a report with `attempt == observed_attempt + 1`
  yields `New(FailedAtMax)`; a report with the original attempt and a different node id is
  rejected by the binding rule; `AlreadyDecided` after seq 2 or a seq 1 `failed_at_max`.
* `oom_report_matches_node`: exact equality with `shard_node_id(job, idx, attempt)`; rejects a
  missing `node_id`, a node id of another attempt, a legacy node (`runner_auto = 0`).
* `resolve_complete_node` with `closed_by = 'oom'` returns a no-write variant (M1); failed-after-
  failed on an ordinary node still `Recorded`.
* Retry id injectivity and `shard_nodes_to_cancel` matching `shard:J:I:2` (regression for the
  removed item 4); `resolve_live_shard_node` never returns attempt N+1 for N (R1).
* `StartRetry` precondition function (M3): node terminal, group not `running`, run terminal each
  refuse; all clear allows.
* Backoff: 5, 10, 20, 40, 80, 160, 300, 300 s; cap at 8 flips to `stuck` (critical incomplete)
  or `degraded` (projection only); `clamp_retry_delay_to_deadline` composes.
* Command-template guard rejects `--attempt`, `--node-id`, `--instance-type` in a `runner_auto`
  command (Q14); spec hash differs when size or `shard_attempt` differs.
* Effect state machine: existing `next_oom_effect_excluding` tests stay; add the `abandoned`
  path (no `InsertRetry`/`StartRetry`, `StopOld` still due).

### 6.2 Durable-Object-level behavior (needs a runtime harness)

`RunCoordinator` methods take a real `SqlStorage`, and there is **no in-repo DO harness** today
(Q4): options are (a) extract the effect executor behind a trait and test against an in-memory
SQLite dev-dependency with fault injection, or (b) `workerd` via `wrangler dev`/Miniflare.
Scenarios: crash between each pair of effects then re-drain converges; duplicate
`SubmitResourceSamples` delivery yields one decision; a report after the run is terminal is
samples-only; a second OOM yields seq 2 whose effects stop the **retry** node's container;
stuck/degraded after the cap with operator-visible state; **alarm order**: a terminal run with
pending decisions and overflow still drains both and then releases the slot; deadline passed
with a stuck decision closes the run; a late `/complete-node` for an OOM-closed node writes
nothing (M1); fail-fast between `InsertRetry` and `StartRetry` leaves no container (M3);
healing continues with `OOM_RECOVERY` switched off.

### 6.3 Live smoke (not runnable in this environment)

Requires `CLOUDFLARE_API_TOKEN` (and account id) for `wrangler dev` against real Containers,
Docker for the local image build of the `NodeContainer` image, and an image whose `cloud-ci
agent` can reach the ingest path (bootstrap token plumbing, Q3). Steps: start an auto shard at
`standard-1`, run an allocation bomb exceeding 4 GiB, observe the OOM evidence, the seq 1
`retry` at `standard-2`, the second container at `shard:J:I:2` with the right env, then a second
bomb yielding seq 2 `failed_at_max` and the retry container stopped; also a duplicate delivery
and a delivery after run close. The agent's window (default 60 s) must cover the job when
trigger A is used. Until run, the feature stays **unverified at runtime** and docs say so.

## 7. Non-goals

* The nightly rightsizing cron, p95 computation, hysteresis writer, `sizing_decisions` (table,
  migration, projection, reader) and the `GET /oom-lineages` route (§2.3).
* Plain (non-shard) `runner: "auto"` nodes until a trigger binding exists (§1.1, M4).
* Non-OOM retries of any kind (parallelization.md: OOM is the only automatic retry).
* `monitor()` for post-`start()` failures; sidecars; snapshots; custom `{vcpu, memoryMib,
  diskMb}` shapes.
* Wiring `ci.container`/`ci.shard` in the SDK to send `runner_auto` (direct `startNode` is used
  in the proof); the SDK's ingest-token plumbing (roadmap item 2).
* Other executors (EC2, Lambda, Kubernetes).
* Manual remediation tooling for `stuck` decisions beyond the visible state.
* Refactoring `ensure_schema`'s existing guards onto a shared helper (deferred, §2.1).
* `external` runs.

## 8. Staged delivery

Each stage is independently shippable and leaves the tree green (`mise run check`). The feature
is inert until the last stage: `OOM_RECOVERY` defaults off and `runner_auto` is rejected 422
while off (§5.1).

0. **Stage 0 (separate change, owner sign-off, Q1): managed nodes stop running at lite.** Pass an
   explicit `instance` through `JobSpec` to `set_instance` and choose the default. Not inert:
   cost and quota change. No OOM logic. See "Prerequisite stage 0".
1. **Pure logic only.** `oom_effect_target`, `oom_report_matches_node`, `resolve_live_shard_node`,
   backoff and `StartRetry` precondition functions, the `closed_by` variant of
   `resolve_complete_node`, the command-template guard, tests. Removes the
   `resolve_oom_node_id` / `oom_retry_node_id` inconsistency. **No schema change and no
   `ensure_column` refactor.** No behavior change.
2. **Executor ladder and `exec` env carrier.** The five-rung `durable_object` ladder, `exec`
   `ContainerExecOptions::add_env` for `CLOUD_CI_NODE_ID`, `CLOUD_CI_SHARD_ATTEMPT` and the
   agent's token/URL (blocked on Q3 for the latter). Changes the environment of all nodes, so it
   is not inert; ship with its own sign-off.
3. **Barrier feed (blocked on H3/Q10).** Whichever of the two options is chosen. Shippable
   without OOM recovery (it makes a coordinator-failed shard node reach the barrier under
   option 2).
4. **Evidence capture, recorded only (blocked on H1/Q9).** Option B's second `exec` and the
   optional `CompleteNodeRequest` field, or option A's agent contract change; evidence is stored
   and logged but triggers nothing.
5. **Decision and effects behind the flag.** Node columns (literal guards), `oom_decision`,
   `runner_auto` at `startNode`, decision inside the evidence transaction, effect drain, alarm
   sweep (§3.6), fault-injection tests (§6.2) against a fake executor. Healing does not depend
   on the flag.
6. **Enable and live proof.** Run §6.3 with credentials; update parallelization.md,
   analytics.md and roadmap.md from "not wired" to "wired, verified on <date>" **only** with
   that run's evidence; flip the default.

`sizing_decisions` ships with the cron work, not in this sequence.

## 9. Open questions needing an owner decision

* **Q1 (blocking).** `NodeContainer` starts every node on `lite` (256 MiB) today. Confirm it is
  a bug and pick the default size for non-auto nodes (`standard-4` to match the advertised
  capability, a smaller size, or `runners.default`), with the cost and quota impact stated
  (Stage 0).
* **Q2.** The documented default `runners.auto.min = "basic"` is rejected by the
  `durable_object` runtime. Change the documented default (`lite`/`standard-1`) or alias
  `basic` to `lite`? (settings.md, analytics.md.)
* **Q3.** `cloud-ci agent` needs `CLOUD_CI_TOKEN`/server URL inside the container and `exec`
  env replaces the environment; no dispatcher mints them today. Is bootstrap-token issuance a
  hard prerequisite for Stage 2 and the live proof, and who owns it?
* **Q4.** DO-level test harness: an in-memory SQLite dev-dependency with a trait seam, or
  `wrangler dev`/Miniflare?
* **Q5.** `sizing_decisions` is deferred. When built it has two writers (OOM now, nightly cron
  later). Is that an exception to the single-writer reading of AGENTS.md, does the cron own
  downsizing under the §2.3 "OOM only raises" rule, and should a later run's `startNode`
  honor `current_instance_type` before the cron exists?
* **Q6.** Apply the same attempt cap and visible failure state to `shard_terminal_effect` (its
  unbounded 5 s retry), in the same change or not?
* **Q7.** Shard retry id `shard:J:I:{attempt+1}` (no matcher change) versus parallelization.md's
  `:oom-retry:` text. The review found the scheme injective and recommends yes; confirm, and
  update parallelization.md prerequisite 4.
* **Q8.** Superseded by Q9 (trigger evidence).
* **Q9 (blocking).** Trigger: agent-only evidence versus the Worker-side fallback analytics.md
  already specifies (a second `exec` reading `memory.events` plus exit 137), and what window
  the agent runs for (H1, §1.1).
* **Q10 (blocking).** Barrier feed: a public `ShardTerminal` RPC with `attempt`, or deriving
  `shard_state` from `/complete-node` of `shard:` nodes; and where merge gets a shard's
  `report_key` under option 2 (H3, §4).
* **Q11.** How a first-attempt OOM failure is held from fail-fast and the barrier while a retry
  is possible: defer the verdict for `runner_auto` nodes until the OOM evidence is evaluated?
  (Depends on H1 and H3.)
* **Q12.** Second decision: confirm the one-row-per-decision model (§2.2); and whether a
  `failed_at_max` should write `sizing_decisions` at all once it exists (recommendation: only a
  seq 1 `retry` writes, raising the size; `failed_at_max` writes nothing and is reported on the
  node).
* **Q13.** Cost and quota: old and new containers can overlap briefly; a 12 GiB default (Q1)
  against 6 TiB / 1,500 vCPU account limits (Stage 0 numbers).
* **Q14.** Command-template contract: no `--attempt`, `--node-id`, `--instance-type` flags in
  `runner_auto` commands (rejected at `startNode`, §1.3), versus teaching the agent an env
  fallback for `--instance-type`.
* **Q15.** `node.size` or the agent-reported `instance_type` is authoritative for
  `decide_oom_recovery` (this design: `node.size`).
* **Q16.** Should `handle_close_run` stop non-terminal nodes, for the retry and for all nodes
  (C1)? Today a timeout or normal close leaves running containers.

## 10. Contradictions found in the existing docs and code

* `docs/roadmap.md` (Phase 4 "Not wired", Next steps item 3) and `parallelization.md` list the
  remaining work as "a multi-size executor ladder, lineage- and attempt-aware resolution, live
  smoke test". They omit that **no managed node is sized at all today** (everything is
  `lite`), that **`exec` gets no environment** so the shipped `CLOUD_CI_NODE_ID` /
  `CLOUD_CI_SHARD_ATTEMPT` carriers have nothing to carry them, that no dispatcher provides the
  agent's token/URL, that **nothing produces the barrier input for a retry** (H3), and that the
  dominant OOM outcome is excluded by the admissibility rule (H1). "Only the wiring and the
  live proof are missing" understates this.
* **analytics.md specifies two triggers; this design cannot yet use both (C4a).** `analytics.md`
  lines 232-235 and 704-707 give the agent's `memory.events` `oom_kill` counter **and** a
  Worker-side fallback ("container exited non-zero with no final Report"). Revision 1 used only
  the agent. H1 asks the owner to choose.
* **analytics.md "at job end" versus the shipped agent (C4b).** `analytics.md` says the agent
  reads/submits once "at job end" (lines 230-232 and the data-flow text). The shipped agent
  samples for a fixed `--duration-secs` window (default 60 s: `cloud-ci-cli/src/agent.rs`,
  `cloud-ci-cli/src/cli.rs`) and submits once after that window. A job shorter than the window
  is padded; a longer job is sampled only for its first minute. Any statement in this design
  that the agent reports "at job end" is wrong; it reports at the end of its window.
* **analytics.md `sizing_decisions` shape (C3).** Line 395 lists a narrower table than this
  design needs (§2.3 lists the extra columns); analytics.md must be updated when the table is
  built.
* `docs/design/dynamic-pipelines.md` (around line 309) still says per-call `image`/`instance`
  and `exec()` "are not reachable from Rust". ADR 0010, ADR 0011 and the roadmap spike row say
  they are (via the fork); the fork source confirms it (§1.3). The doc is stale.
* `parallelization.md` prerequisite 4 plans to teach `shard_node_matches` the
  `:oom-retry:<n>` shape; `oom_retry_node_id`'s own doc comment says shard nodes do not need
  it; `resolve_oom_node_id` uses it for shards anyway. This design uses
  `shard_node_id(job, idx, attempt+1)` (Q7).
* `executor.rs` `ContainersExecutor::capabilities()` advertises one `standard-4` rung while the
  real start path is `lite` (Stage 0).
* analytics.md: "becomes the new `current_instance_type` immediately (not just for the one
  retry)" implies a reader on later runs; no reader exists and none is planned here (Q5). Its
  sample CLI comment ("--node-id ... currently unset by any real dispatcher") stays true until
  Stage 2.
* analytics.md/settings.md default `runners.auto.min = "basic"` versus the runtime's rejection
  of `basic` under `durable_object` (the roadmap spike row already records the rejection, dated
  2026-10-02).
