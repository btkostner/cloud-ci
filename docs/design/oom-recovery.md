# `runner: "auto"` real-time OOM recovery: wiring design

Status: **DRAFT** - not approved for implementation; blocked on decisions H1, H3, Q1, Q9, Q10.

Revision 3, 2026-10-03, after a second independent review of revision 2 (e4364ac). Revision 2
was judged acceptable as a DRAFT but not ready to drive code: one defect in the second-decision
persistence model (D1, plus D2) and two places where the H1/H3 "Decision required" blocks still
leaked an assumed answer into their own supporting rules (L1-L3), plus five gaps (G1-G6) the
first revision never addressed. This revision fixes D1/D2, rewrites every leaking rule so it is
stated per H1/H3 option rather than assuming one (L1-L3), adds the missing OOM/NOT-OOM/UNKNOWN
trigger classification with its exact evidence sourcing (G1), adds the release rule for a
withheld shard failure (G2), bounds the "close cannot be starved" claim and closes its deadline
gap (G3), gives Stage 0 a rollout gate and rollback (G4), notes `max_instances` is unsupported
(G5), adds the missing test scenarios (G6), adds Q7's downside and Q9-Q16, dates two previously
undated tooling facts, and records when each stale doc claim in §10 is corrected - one of them,
`dynamic-pipelines.md`'s already-wrong per-call-sizing claim, is fixed now, in a separate small
commit on this branch (`f9fa884`), independent of any pending decision here. **No owner
decision (H1, H3, Q1, or any other) is made in this revision**: every fix states the rule
correctly under each option, or marks the point as still open.

**Owner decisions are not the only thing blocking code (F5/N7/revision 4).** This document
tracks two different kinds of open item, and they do not resolve the same way: an **owner
decision** (H1, H3, Q1-Q18) is a policy choice this design deliberately leaves open and will
not guess; a **technical prerequisite** is a fact about the runtime or an existing code path
that is currently unknown, unverified, or absent, and that blocks implementation regardless of
how every owner decision is answered. See "Technical prerequisites before implementation"
(after §8) for the full list - it is not a restatement of the owner-decision list, and
resolving every Q above does not by itself make this design implementable.

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
| Q17 | If Q11 is "hold": which UNKNOWN-holding mechanism, of several non-equivalent shapes. | §1.1 (G1) |
| Q18 | Window length, and what resolves a still-deferred node when the run closes first. | §1.1, §3.6 (F6) |

H2 (the second decision's persistence model) is specified in §2.2 and needs review, not a policy
decision. Q17 and Q18 only apply if Q11 is answered "hold" (§9). Everything else is a stated
rule or an owner question in §9.

## 0. Why the earlier wiring failed, mapped to this design

| Review finding | Root cause | Where this design closes it |
| --- | --- | --- |
| O1: second OOM undetectable | The retry's node id and attempt were never carried into the new container, so its report looked like a replay of the original (`decide_oom_recovery` doc: "indistinguishable from a duplicate delivery"). | §1: under **option A (and C)**, node id + shard attempt are injected into the container's env at start, and the report's `node_id` must equal the id derived from the report's own `(job, idx, attempt)`; under **option B**, the coordinator takes the node id and attempt directly from the node row at `/complete-node` time, so there is nothing to carry into a report at all. |
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
* **No application-level concurrency cap exists to backstop that math (G5).** The
  `wrangler.toml` comment on `NodeContainer`'s `[[containers]]` entry already notes
  `max_instances` is `default`-policy only; Cloudflare's "Scheduling Policies" page (read
  2026-10-03) confirms it: "The `durable_object` policy does not support `max_instances`."
  The account limits above are therefore the *only* guard against over-provisioning, not a
  belt-and-suspenders check behind a lower application cap. This matters directly for Q13.
* Billing impact: pricing is not stated in the sources read for this document and is
  `[unverified]` here. Owner must supply the expected cost change.

**Rollout gate and rollback (G4).** Stage 0 is not inert and needs owner sign-off (above), so
it ships behind its own gate, separate from `OOM_RECOVERY`: a deployment setting (for example
`Settings.runners.default_instance`, resolved the same way other deployment-wide bounds are -
settings.md's "Deployment-wide limits" table) naming the instance size `handle_start_node`
passes for a **non**-`runner_auto` node. It **defaults to the current behavior** - omitted,
which the runtime resolves to `lite` (§5.4) - so a deployment that never sets it sees no
change at all. An owner opts in per deployment by setting it explicitly (`Q1` is which value
to recommend as the opt-in default, not what the code defaults to). **Rollback** is resetting
the setting (or redeploying the prior Worker version) and redeploying; no data migration is
needed, because the setting is read only at `startNode` time and affects only containers
started after the change - already-running containers are unaffected either way, and a node
row does not persist which default produced its size beyond the existing `node.size`-style
columns this design already adds for `runner_auto` nodes (§2.1; a non-auto node's actual
running size is not currently recorded on its row at all, a pre-existing gap Stage 0 does not
need to close).

## 1. Identity

### 1.1 What an OOM report names, and when it may be acted on

`SubmitResourceSamplesRequest` carries `job_id`, `shard_index`, `attempt` (the **shard**
attempt), `instance_type`, `oom_detected`, `memory_peak_bytes` and `optional string node_id = 8`.
`handle_submit_resource_samples` (`coordinator/mod.rs`) already validates `node_id`
(`logic::validate_node_id`, at most `MAX_NODE_ID_BYTES` = 256 bytes) and hashes it into the batch
content hash (`logic::resource_sample_batch_content_hash`). Idempotency identity stays
`(job_id, shard_index, attempt)`; the same identity with a different `node_id` is a 409
conflict.

**Binding rule - depends on the H1 option (L2).** How the coordinator decides "this evidence
is for this node" is shaped by which H1 option is chosen; the general principle is the same
under both: absent or unmatched identity means no action, not a guess.

* **Under option A** (the agent is the evidence source, `SubmitResourceSamples`): the
  coordinator resolves `job_name` from `job_id` (`read_job_by_id`, DO-local `job` table),
  computes `expected = logic::shard_node_id(job_name, shard_index, attempt)`, and acts only if
  `req.node_id == Some(expected)` (exact string equality, no parsing of the id), the `node` row
  exists, `node.runner_auto = 1`, and `node.shard_attempt == attempt`. A report with no
  `node_id` (old agent, or a dispatcher that sets none) is samples-only and **never** triggers
  recovery. This check is what the pure helper `oom_report_matches_node` implements (§6.1); it
  is specific to the `SubmitResourceSamples` request shape and has no equivalent under B.
* **Under option B** (`NodeContainer` is the evidence source, on `/complete-node`): there is no
  separate report to bind identity against. The evidence (exit code, `oom_kill`) arrives as
  optional fields on the existing `CompleteNodeRequest` for `req.node_id` itself, which
  `handle_complete_node` already resolves to one real `node` row (line 2813) before any OOM
  logic runs - the node id is the call's own primary key, so there is nothing to cross-check
  it against. The gate is simply `node.runner_auto = 1` (plus the structural exclusion for an
  already-`Cancelled` node, §1.1's classification table). No pure matcher function is needed;
  the classification (above) runs directly on the request's exit code and `oom_kill` fields.

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
  **The transaction that accepts this `/complete-node` call records the `oom_decision` row
  (the evidence and the outcome) but deliberately does NOT write the node's terminal status**
  for an OOM-classified completion - that write is `MarkOldTerminal` (§3.1), the first effect
  of the decision just recorded, which sets `failed` with `closed_by = 'oom'`. `MarkOldTerminal`
  runs inline immediately after the commit (§2.2's "Atomicity" note), so in the common case
  fail-fast and the barrier never observe a gap where the node looks non-terminal with no
  decision yet - but a crash between the commit and that inline drain leaves the node
  `Running` with a `pending` decision until the alarm sweep (§3.6) completes `MarkOldTerminal`;
  this is the same "drained by the alarm, not only inline" healing property every other effect
  already has (§3's "O2/O3" row), not a new gap. **By default**, a completion the
  classification (below) marks NOT-OOM or UNKNOWN writes the node's status immediately and
  normally, with no decision row and no deferral - this is the behavior as specified so far in
  this design, independent of H1's pending choice. **Holding a failure write instead of
  recording it immediately is a Q11 option only, not part of this default**; if the owner
  picks that option, see the "Explicit UNKNOWN scenarios" discussion in §6.2 for what it would
  cost under each H1 option. Pros: works when the agent is killed; matches analytics.md's two
  triggers; evidence and the OOM-classified outcome are recorded durably in the same write,
  so there is no window between "evidence arrived" and "the decision exists" for the
  coordinator to lose track of. Cons: a second `exec` after the main one exited is
  `[unverified]` against a real runtime, as is whether the cgroup it sees is the job's; 137
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

**Unresolved technical prerequisite: how the agent is launched and shares the job's cgroup
(N7).** The "Evidence source" paragraph above assumes `cloud-ci agent` runs inside the same
cgroup as the job command it is measuring, but this design never states the mechanism that
makes that true. `NodeContainer::handle_start` execs exactly **one** `command: &[String]`
per node (`run_and_report`, `node_container.rs` lines 325-350, via `exec_in_container`,
itself one `container.exec(&cmd, ...)` call); there is no second, agent-specific exec and no
documented convention for a command that runs both the agent and the job (a wrapper script, a
sidecar process, an agent that forks and execs the real job command and waits on it, or
something else). Whichever shape is used determines whether the agent's `--cgroup-path` read
(below) actually observes the job's own memory pressure, or a different cgroup entirely. This
is listed as an unresolved technical prerequisite, not an owner decision (see "Technical
prerequisites before implementation" after §8): no amount of deciding H1 settles it.

**Path, permissions, and failure mode (option A).** `CgroupReader::new(base)` takes
`--cgroup-path` (default `/sys/fs/cgroup`, `cloud-ci-cli/src/cli.rs` line 386).
`CgroupReader::read` (`cgroup.rs` lines 169-174) is a plain `std::fs::read_to_string` with no
privilege elevation - an ordinary file read inside the container's own cgroup mount. If a read
fails (missing file, permission denied), `sample_for` propagates the error with `?`
(`agent.rs` lines 174-176 and 199-204 both `map_err(...)?`), which **aborts the entire agent
run before any `SubmitResourceSamples` call is made** - there is no partial batch. So under
option A, "`memory.events` unreadable" does not produce a batch with missing fields; it
produces no batch at all, and the node's eventual `/complete-node` (posted independently by
`run_and_report`) carries only the job's own exit code, with no OOM evidence attached - which
**should** classify as UNKNOWN per §1.1's classification table, but today nothing turns that
silence into an actual UNKNOWN determination the coordinator acts on. See the next paragraph.

**The classification is correct; the detection mechanism does not exist yet (G1).** Today,
"no batch arrives" is indistinguishable, from the coordinator's point of view, from "the agent
hasn't finished yet", "the agent was never dispatched", or "the agent is still inside its
sampling window" - there is no positive signal that evidence was attempted and failed. The
classification table above treats "unreadable evidence" as UNKNOWN, but that is a design
requirement on what must be *built*, not a description of what the current agent already
does: today a read failure simply aborts the process with no observable trace at the
coordinator beyond the ordinary `/complete-node` the job's own exit produces (§1.1's "Option
A" problem statement above). Whichever H1 option is chosen, this design needs one of:
* **An explicit "evidence unavailable" upload.** The agent catches its own `CgroupReadError`
  instead of propagating it with `?`, and still calls `SubmitResourceSamples` with an explicit
  marker so UNKNOWN is a positive signal, not an absence. **The only workable marker is a new,
  additive proto field (F4)** - for example `optional bool evidence_unavailable = 9`. The two
  fields already on the request cannot carry this: `oom_detected` (field 7) is a plain,
  required `bool`, not `optional`, so "absent" is not a representable state for it at all; and
  a batch with `oom_detected = false`, no `memory_peak_bytes` and zero samples is already a
  *legitimate* ordinary batch (`memory.peak` reading the literal `max`, or a job shorter than
  the first 2 s tick - `handle_submit_resource_samples` does not reject an empty `samples`
  list), so "both absent" cannot be told apart from a real empty batch. The new field must be
  folded into `resource_sample_batch_content_hash` only when set, exactly as `node_id` already
  is (`logic.rs` ~1280: `if let Some(node_id) = node_id.filter(|n| !n.is_empty())`), so an old
  agent's content hash - and therefore its idempotency - is unchanged.
* **A coordinator-side window.** Two distinct shapes exist under this name, and they are not
  interchangeable (T1/T3):
  * **Bound how long a late batch can still upgrade an already-recorded failure** (no change
    to `handle_complete_node`): under option A, `/complete-node` already writes `failed`
    immediately and unconditionally, as it does today; the window only bounds how long the
    §3.2 upgrade rule stays willing to accept a late-arriving batch and flip `closed_by` to
    `'oom'`. After the bound, a late batch is just a late, inert delivery. This is additive
    and does not touch the existing completion handler.
  * **Defer the write itself until evidence arrives or the window expires** (§6.2's "Holding a
    failure write" discussion): under option A this is not additive - `handle_complete_node`
    would have to learn to withhold its write for `runner_auto` nodes, which it does not do
    today. Under option B it is unnecessary, since B's evidence is already synchronous with
    the completion it accompanies (§6.2).
  **The window's deadline is bounded by the run, not independent of it (F6).**
  `resolve_timeout_seconds` (`logic.rs` line 517) accepts any positive requested timeout up to
  `MAX_TIMEOUT_SECONDS` - a run can legitimately request a timeout shorter than any fixed
  window (the existing test `resolve_timeout_seconds_passes_through_a_requested_value_under_
  the_max` accepts `3` seconds unchanged) - so a fixed window cannot be assumed "strictly
  shorter than the run timeout". The window must instead be computed as
  `min(configured_window, remaining_run_time)` at the moment it would be armed, exactly like
  `clamp_retry_delay_to_deadline` already does for the OOM-effect retry delay (§3.4). This
  still leaves a gap: `handle_close_run` (`mod.rs` line 2289) never reads or stops any node
  (§3.6's own "C1" finding), so if the run closes before a windowed node's deadline is
  reached, that node is left `Running` (or un-classified, under the defer shape) with no
  further event to resolve it - closing the run does not itself resolve the window. **Whether
  to add a rule resolving every still-deferred or still-unclassified `runner_auto` node at run
  close (and what it resolves to - most conservatively, record the ordinary failure with no
  retry, the fail-closed default this design uses elsewhere) is itself an owner question where
  it is a genuine policy choice, not something this revision decides** (§9, "run-close
  resolution for a deferred node").

None of these exist today. The upload and the late-upgrade-bound window shape are additive (an
agent code change for the first, a coordinator timer for the second, neither touching an
existing handler); the defer-the-write window shape additionally requires modifying
`handle_complete_node` under option A (above). Whichever combination is chosen is a genuine
stage deliverable (§8 Stage 4), not something Stage 4 can treat as already covered by the
agent's current abort-on-error behavior.

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

* **Variables set per node.** `CLOUD_CI_NODE_ID={node_id}` and
  `CLOUD_CI_SHARD_ATTEMPT={shard_attempt}`, plus whatever the agent needs
  (`CLOUD_CI_TOKEN`, `CLOUD_CI_SERVER_URL`). They serve two different purposes, and only one of
  them depends on the H1 option:
  * **OOM detection: needed under option A (and C) only.** The agent's report is the evidence
    event, so its `node_id` and `attempt` must name the right node (§1.1 binding rule).
  * **Sample correctness: needed under every option in which the agent submits samples at all
    (A, B and C).** The agent reports its own `attempt` and `node_id` on every
    `SubmitResourceSamples` batch, whether or not that batch is OOM evidence. Without the
    carriers (and without the flag ban below) a retry's batch is reported as the original
    attempt.

  **Because `exec` env replaces the environment, this changes the environment of every node,
  not only auto ones**, and no dispatcher mints the token or URL today (roadmap Phase 6:
  bootstrap-token path not built). See Q3.
* **Command-template contract (M5, Q14) - all options in which the agent submits samples.**
  `resolve_shard_attempt` and `resolve_node_id` give an explicit flag priority over the env
  var (`cloud-ci-cli/src/identity.rs` `resolve_shard_attempt`, `cloud-ci-cli/src/agent.rs`
  `resolve_node_id`; read 2026-10-03), and `--instance-type` has no env fallback at all.
  Therefore the command a `runner_auto` shard node is started with MUST carry **no**
  `--attempt`, `--node-id` or `--instance-type` flags. If it did, the retry (same command, new
  env) would still report the ORIGINAL attempt and node id on its batch:
  * under **option A (and C)** the report would then fail the §1.1 binding rule, so the second
    OOM would never be detected;
  * under **option B**, OOM detection does not read the batch (the coordinator already has
    `req.node_id` as the `/complete-node` call's primary key, §1.1), but the retry's batch
    would still be attributed to attempt N. Because the batch identity is `(job_id,
    shard_index, attempt)`, it either collides with the original attempt's batch (409 on a
    different content hash) or, if the original batch never arrived, silently takes its place.
    Command flags are therefore **not** a valid attribution source under B either.

  `handle_start_node` rejects a `runner_auto` command containing any of those three flags (422
  `runner_auto_command_carries_identity_flag`). Under option B alone the ban protects sample
  attribution only, not OOM detection; whether to enforce it on B is the same Q14 question. The
  current size is read from `node.size` (§1.1), not from `--instance-type`, under every option.

**Unresolved technical prerequisite: the `--instance-type` ban is not implementable as stated
today (F5).** `handle_start_node` is specified above to reject a `runner_auto` command
containing `--instance-type` at all - but the shipped agent hard-fails immediately without
it: `cloud-ci-cli/src/agent.rs` lines 110-113 return `AgentError::new("resolve instance type",
"--instance-type is required")` when the flag is absent, and `cli.rs` lines 424-425 declare it
as a flag-only argument (`#[arg(long = "instance-type")] pub instance_type: Option<String>`,
no corresponding env var anywhere in `identity.rs` or `agent.rs`, unlike `--attempt`/
`--node-id`). As specified, banning the flag from a `runner_auto` command's command line makes
that command unable to run `cloud-ci agent` at all, not merely unable to report an accurate
size. **This ban is unimplementable until the agent gains an env fallback for
`--instance-type`** (the same pattern `CLOUD_CI_SHARD_ATTEMPT`/`CLOUD_CI_NODE_ID` already use)
- that fallback is a technical prerequisite (see "Technical prerequisites before
implementation" after §8), not a Q14 policy choice; Q14 itself only decides whether the ban
additionally applies under option B once the prerequisite exists.
* `StartNodeRequest` gains `shard_attempt: Option<u32>`, `runner_auto` (§5.1) and the instance
  size; `JobSpec` (`executor.rs`) gains the same; all feed `spec_hash`.

**Under option A (and C), a second OOM then maps unambiguously** via the report: the retry
agent reports `node_id = shard:{job}:{idx}:{n+1}` and `attempt = n+1`; the binding rule
matches the retry's node row; `decide_oom_recovery` sees the lineage's first decision with
`attempt + 1 == incoming.attempt` and returns `New(FailedAtMax)` (existing, tested branch). A
replay of the original attempt now has a different `node_id`/attempt pair and is rejected by
the binding rule. **Under option B**, there is no such report to map: the coordinator already
knows which node completed (`req.node_id`, §1.1) and which `oom_decision` row (if any) that
node is the live target of, so the same unambiguous mapping holds trivially, and OOM detection
needs neither the env carriers nor the command-template ban. Those remain required only so
that the agent's separately-delivered samples are attributed to the right attempt (see the
sample-correctness bullet above); they are not an attribution source for the OOM decision.

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
  the evidence event, not from the agent's corroborating evidence when both exist. Under
  **option A**, the evidence event is the agent's report, so the target is the report's own
  `node_id`. Under **option B, and under option C** (C's trigger of record is B, §0 "C. Both"),
  the evidence event is the `/complete-node` call, so the target is `req.node_id` from that
  `CompleteNodeRequest` (§1.1) - never from `base_node_id` and never from `resolve_oom_node_id`
  (which is replaced in stage 1 by `oom_effect_target(row)`). For seq 1, the target is the
  original node. For seq 2, it is the retry node, which equals seq 1's `retry_node_id`; the
  insert verifies that equality and refuses otherwise.
* **`existing` is the projection of the LATEST decision row for the lineage** (D1) - not
  always the seq 1 row: `attempt = that row's observed_attempt`, `outcome = that row's
  outcome`. `base_node_id` is not read by `decide_oom_recovery` and is not part of the
  mapping.
* **Seq assignment is race-free (D1).** The DO is single-threaded between `await` points. The
  insert path: read `SELECT MAX(seq) FROM oom_decision WHERE job_name = ? AND idx = ?` (or no
  row), compute `decide_oom_recovery(ladder, existing, incoming)` from that row's projection,
  and `INSERT` the new row with `seq = (max seq) + 1` (`1` if none) - with **no `await`
  between the read and the insert**. The primary key `(job_name, idx, seq)` is a defensive
  backstop, not the primary race guard, and in practice **effectively unreachable**: with no
  intervening `await`, a re-delivery of the same evidence event re-reads the now-latest row
  (which the first delivery already inserted) and returns `AlreadyDecided` *before* ever
  reaching a second `INSERT` - it never gets far enough to collide. The constraint exists only
  to catch that invariant being violated by a future change (for example two evidence-bearing
  transactions for the same lineage somehow interleaving with an `await` between the read and
  the insert, which the current design does not do) - it is a backstop against silent state
  corruption, not a signal this design expects a caller to ever see.
* **If the backstop is ever reached anyway, it resolves to "already decided" without ever
  surfacing an SQL error to the caller (D1, N12).** The insert is split into two steps, not
  one: a `read_latest_decision(sql, job, idx)` step (the `MAX(seq)` read above) and a
  `insert_decision_at_seq(sql, job, idx, seq, ...)` step that takes the computed `seq` as an
  explicit parameter rather than recomputing it - the same split the test seam in §6.2 calls
  directly. `insert_decision_at_seq` is where the constraint error is caught: its `INSERT`
  call's result is matched on the error text, the same *technique* `finalize_test_stats`
  (`mod.rs` ~2623-2634) already uses for its own idempotency check against a `D1Error`'s
  `.to_string()` - `if msg.contains("UNIQUE constraint failed") || msg.contains("PRIMARY KEY
  constraint failed")` - rather than a typed constraint variant (D1 errors surface as strings
  through this binding, not a typed enum, as that existing call site shows). `oom_decision` is
  DO-local `SqlStorage`, not D1, so its `sql.exec()` call returns a `worker::Error`, a
  different type than `D1Error`; whether its own `.to_string()` contains the identical
  substrings for the same SQLite constraint violation is `[unverified]` - it rides on the same
  underlying SQLite engine, but the two bindings (`D1Database` vs `SqlStorage`) are not
  guaranteed to format the error identically, and this design has not confirmed it against a
  real `worker::Error` instance. The match strings used for `insert_decision_at_seq` must be
  confirmed against a real `worker::Error` for a `SqlStorage` constraint violation before
  implementation, not assumed from the D1 precedent.
  On that match, `insert_decision_at_seq` itself re-reads the row at the `seq` it tried to
  insert, re-projects it, and returns its `outcome` as
  `AlreadyDecided` - the same shape `decide_oom_recovery` already returns for a duplicate; the
  constraint violation itself is caught and absorbed inside this one function, never
  propagated past it. Any other `INSERT` failure (not matching either string) is a genuine,
  unexpected error and is propagated normally, not swallowed.
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
`storage().transaction()` closure** that records the event carrying the evidence: under option
A (and C's corroborating path) the batch acceptance in
`accept_resource_sample_batch_and_queue_all`; under option B (and C's trigger of record) the
`/complete-node` acceptance. **That transaction writes the evidence and the `oom_decision` row
only - never the node's terminal status** (§1.1 option B, §3.2): `MarkOldTerminal`, the
decision's first effect, writes the status afterwards. Under option A the node was already
written `failed` by the existing `handle_complete_node` path before the batch arrived (§3.2's
upgrade rule). A duplicate batch returns early at `AlreadyAccepted`, so a decision written in
a separate step after a crash would never be re-driven. The reads feeding
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
   for `failed_at_max`). **This overwrites, and does not preserve, whatever completion payload
   (`exit_code`, `stdout`, `stderr`) `run_and_report` already wrote to `result` (F2) -**
   `node_container.rs` lines 325-350 posts that payload on every `/complete-node` call
   regardless of exit reason, and under option A's upgrade rule (§3.2) the node's `result` at
   the time `MarkOldTerminal` runs already holds exactly that output. The `oom_decision`
   schema (§2.2) has no column to carry it either. **This is a stated design choice, not an
   oversight: the job's own stdout/stderr - often exactly what an operator wants when
   diagnosing an OOM - is lost at the moment a node is marked for recovery**, under both H1
   options. A future revision that wants to preserve it would need either a column on
   `oom_decision` or a merge (`result = {"error": "oom", ..., "original": <prior result>}`)
   instead of an overwrite; this design does not do either.
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
| MarkOldTerminal | `update_node_status` to `failed` only if the node is non-terminal. Option-dependent (L1): see the paragraph below the table for what "already terminal" means under option A versus option B. | the row is terminal, `closed_by = 'oom'`, and (option A only) `result` carries the OOM message |
| StopOld | `NodeContainer::handle_stop` is a no-op when not running; `resolve_stop_outcome(addr, ok) == Pending` keeps the flag unset. | stop returned 2xx |
| InsertRetry | deterministic id plus `resolve_start_node` (same spec hash = no-op; different = conflict, `stuck`). | row exists with our spec hash |
| StartRetry | `NodeContainer::handle_start` returns `started:false` if already `running()`. A start that throws marks the retry row `failed` (as `handle_start_node` does) and still sets the flag. | start 2xx, or row marked failed |
| Project* | `INSERT .. ON CONFLICT DO UPDATE`. | D1 call returned ok |

**MarkOldTerminal's "already terminal" case is option-dependent (L1).** Under **option B**,
the evidence-accepting transaction writes only the `oom_decision` row, never the node's
status (§1.1's option B description), so the target node is always still non-terminal when
`MarkOldTerminal` - the decision's own first effect - runs and performs that write; a node
that is terminal for another reason (it naturally `succeeded`, was `cancelled`, `skipped`,
`timed_out`, **or already `failed` for a reason unrelated to this decision's own evidence**
(F3) - concretely, seq 2's target is the retry node, and `StartRetry` failing already marks
that same node `failed` via its own, separate path (§3.2's `StartRetry` row) before any OOM
evidence about it exists) genuinely means someone else won the race, and the decision is
`abandoned` (§3.6), not overwritten. Under **option A**, the target node is very often
*already* `failed` (plain exit 137, `closed_by` unset) by the time the batch arrives - H1's
own problem statement is that this is the dominant case, not an edge case. `update_node_status`
still writes only when non-terminal, but a node that is `failed` with `closed_by` unset
**and** the arriving batch classifies as OOM (§1.1's classification) is **upgraded**, not
abandoned: `closed_by` is set to `'oom'` and `result` is overwritten with the OOM message,
even though the row was already terminal before this call. A node `failed` for a reason this
decision's own evidence does not corroborate - the same `StartRetry`-already-failed case as
option B, above, which is option-independent - is abandoned like any other "someone else won"
case, never upgraded; only a `failed` node whose closing evidence IS this decision's own is
upgraded. Only a node terminal as `succeeded`/`cancelled`/`skipped`/`timed_out`, or `failed`
for an unrelated reason, is abandoned under option A; a node terminal as plain `failed` with
matching OOM evidence is upgraded.

`start()` returns before the container is ready, and later failures need `monitor()`
(Cloudflare, "Durable Object Container API" page, "Last updated Sep 30, 2026", read
2026-10-03, `start`: "To catch later errors, including a container that fails to start, use
`monitor()`"). A retry that fails after `start()` is surfaced like any node, through
`run_and_report` posting `/complete-node`. A container that never starts posts nothing; see
the weakened timeout statement in §3.6.

### 3.3 `StartRetry` re-reads live state immediately before starting (M3)

`StartRetry` is gated on the decision and the run, but revision 1 did not gate on the retry
node's own status or the group status at start time. If fail-fast or cancel marks an
inserted-but-not-yet-started retry `Cancelled` (`ensure_sibling_cancelled`; stop is a no-op
because it is not running), a later drain would still call `start_container` and leak a
container for a cancelled node. **Rule:** immediately before `start_container`, in the same
synchronous region as the check (no `await` between), re-read (a) the retry node row: it must
exist and be non-terminal (`NodeState::Running`, as `insert_node` writes `'running'`); (b) the
`job_group` row: `status = 'running'`; (c) the run: non-terminal **and `now < deadline`** (G3 -
the OOM sweep now runs before the deadline check in `alarm()`, §3.6, so a run whose deadline
has already passed but which has not yet been closed could otherwise still start a retry
container moments before `handle_close_run` runs, which never stops it, C1). If any check
fails, do not start; mark the decision `abandoned` and set `retry_started` so the sweep does
not loop.

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

*Why the close is bounded, not starved outright (G3):* every OOM effect does bounded work per
call, errors are caught and recorded (`effect_attempts`, `next_attempt_at`) and never
propagated with `?` (the existing shard-effect drain has the same shape), a decision not yet
due (`next_attempt_at > now`) is skipped without I/O, at most `OOM_SWEEP_MAX_DECISIONS = 4`
decisions are touched per fire, and the cap turns every failing decision into
`stuck`/`degraded` after about 10 minutes, after which it never re-arms the alarm.
`has_pending_background_work` (used by `release_alarm_if_idle`) gains `oom_decision WHERE
state = 'pending'`; `stuck`/`degraded`/`abandoned`/`done` rows never hold the alarm slot. This
is a bound, not a guarantee: a single hung DO-to-DO `await` inside one `StopOld`/`StartRetry`
call before the deadline check still delays that fire's close by however long the call takes
to time out, exactly the same exposure the existing `shard_terminal_effect` drain already has
today - this design does not introduce a new unbounded wait, it inherits the existing one.

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

**Rules under H3 option 1 (a public `ShardTerminal` RPC, or any future caller of the existing
internal `/shard-terminal` route) - not applicable under option 2 (L3):**

1. **Resolve by the reporting attempt, always.** `complete_own_shard_node` (`mod.rs` line
   3685, the only call site) runs only inside the internal `/shard-terminal` handling path,
   which has no traffic until an option-1 caller exists. A shared helper
   `resolve_live_shard_node(sql, job, idx, attempt) -> Option<NodeRow>` (exact `shard_node_id`,
   no lineage redirect) is the only lookup it and `drain_shard_terminal_effects` use. A
   **late attempt-1 shard-terminal report** - only possible once option 1 has a caller -
   completes attempt 1's node (already terminal via `MarkOldTerminal`, so
   `shard_self_completion_target` changes nothing) and can never touch attempt 2. A unit test
   proves it never returns attempt N+1 for N.
2. **Stop only the node you resolved**, via that row's own `physical_address`; attempt 1's stop
   cannot kill attempt 2 (addresses differ).

**Under H3 option 2, rules 1-2 above do not apply: there is no separate shard-terminal report
to resolve.** `/complete-node` already addresses one specific `node_id` - the attempt-specific
id itself - so there is no lineage redirection to get wrong and no "late attempt-1 report"
scenario of this kind. A late `/complete-node` for attempt 1 under option 2 is handled
entirely by the pre-existing, H3-independent rules already stated in §2.1 and §3.2
(`resolve_complete_node`'s terminal-status idempotency, and M1's `closed_by` rule), not by
anything specific to shard-terminal resolution.

**Rule that holds under either H3 option:**

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

**Release rule for a withheld failure (G2, Q11) - fail closed: release, never strand.** Both
options withhold attempt N's failure from the barrier while a `retry` decision for
`(job, idx)` is still in flight. If that decision never resolves to a running retry - it
becomes `stuck` (the retry cannot be confirmed started or stopped, §3.4) or `abandoned` (the
run went terminal before the retry started, §3.6) - the withheld failure MUST be released to
the barrier instead of leaving the shard with no verdict forever (the alternative is the group
silently waiting for the run timeout, exactly the gap the review found). The release trigger
is the same alarm sweep that marks the decision `stuck`/`abandoned` (§3.4, §3.6): on that
transition, in the same pass, release attempt N's failure.

* **Under option 1,** the superseded `shard_state` row already exists (recorded at the time of
  the original report with `effect_kind = "superseded_by_oom_retry"`); release means
  `read_all_shard_terminal_rows`/`latest_attempt_per_shard` stop excluding it - no new write,
  only re-running barrier evaluation (`evaluate_barrier`) for `(job, idx)` with that row
  un-excluded, dispatching whatever `BarrierOutcome` it now produces (`Waiting`,
  `FailFastTriggered`, or satisfied).
* **Under option 2,** nothing was ever written for attempt N (it was never the coordinator's
  own node completion that mattered - the original report, under option 1's producer, does
  not exist under option 2 at all; the shard's only producer is `/complete-node` for the
  `shard:` node itself). Release means writing attempt N's own original failure (the
  `oom_decision` row already carries `observed_attempt` and the original `target_node_id`,
  which is enough to reconstruct the terminal `shard_state` row) as the shard's verdict, then
  running barrier evaluation exactly as option 2's normal path would have.
* If a retry **did** start and then independently fails or is cancelled, that is not this
  rule: the retry's own `/complete-node` (option 2) or a genuine new shard-terminal report for
  attempt N+1 (option 1) is the verdict, handled by the ordinary paths above. This rule applies
  only when the retry never reached a reportable state at all.

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
  yields `New(FailedAtMax)`; under **option A (and C)**, a report with the original attempt
  and a different node id is rejected by the binding rule (§1.1); under **option B**, the
  equivalent case is a *late* `/complete-node` for the original attempt's own node arriving
  after seq 2 already exists - that node already has `closed_by = 'oom'` (M1, §2.1), so the
  completion is dropped by the `resolve_complete_node` `closed_by` rule before it ever reaches
  `decide_oom_recovery`, not by a `runner_auto`/`shard_attempt` admission check (there is none
  on the completion path); either way, `AlreadyDecided` after seq 2 or a seq 1 `failed_at_max`.
* `oom_report_matches_node` (option A only): exact equality with `shard_node_id(job, idx,
  attempt)`; rejects a missing `node_id`, a node id of another attempt, a legacy node
  (`runner_auto = 0`). Option B has no equivalent matcher (§1.1): a test instead asserts the
  simpler gate (`node.runner_auto = 1`, node not `Cancelled`) admits or rejects correctly.
* Trigger classification (G1): exit 137 + `oom_kill > 0` -> OOM; exit 137 + `oom_kill = 0` ->
  NOT-OOM; `oom_kill > 0` + non-137 exit -> NOT-OOM; unreadable evidence -> UNKNOWN; a
  `Cancelled` node is excluded before classification runs (ties to `resolve_complete_node`'s
  existing `DroppedCancelled` test).
* The explicit UNKNOWN mechanism itself (G1, Stage 4 deliverable, if Q11/Q17 pick it): an
  "evidence unavailable" upload parses to UNKNOWN the same as a successful batch with no OOM
  evidence (not NOT-OOM, not a parse error) - **UNKNOWN has no decision row** by this design's
  own three-way classification (§1.1), exactly like NOT-OOM, so the test asserts an ordinary
  failure write with no `oom_decision` row, never a row with some UNKNOWN status; if a
  coordinator-side window is the chosen shape instead, its deadline is
  `min(configured_window, remaining_run_time)` (F6), and the node's outcome is recorded as the
  ordinary failure (classified UNKNOWN, still no decision row) only once that bounded deadline
  passes with no report, never before.
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
* `decide_oom_recovery` given the **latest** row (D1): a seq-1 `RetryAt` existing record plus
  an `attempt + 1` report returns `New(FailedAtMax)` (not yet `AlreadyDecided`); the same call
  repeated with the seq-2 `FailedAtMax` record now passed as `existing` returns
  `AlreadyDecided` - the two calls must use different `existing` values and return differently.

### 6.2 Durable-Object-level behavior (needs a runtime harness)

`RunCoordinator` methods take a real `SqlStorage`, and there is **no in-repo DO harness** today
(Q4): options are (a) extract the effect executor behind a trait and test against an in-memory
SQLite dev-dependency with fault injection, or (b) `workerd` via `wrangler dev`/Miniflare.
Scenarios, option-independent: crash between each pair of effects then re-drain converges; a
second OOM yields seq 2 whose effects stop the **retry** node's container; stuck/degraded
after the cap with operator-visible state; **alarm order**: a terminal run with pending
decisions and overflow still drains both and then releases the slot; deadline passed with a
stuck decision closes the run; a late `/complete-node` for an OOM-closed node writes nothing
(M1); fail-fast between `InsertRetry` and `StartRetry` leaves no container (M3); healing
continues with `OOM_RECOVERY` switched off; **the PK-collision backstop (D1, N6)** - since the
single-threaded read-then-insert path makes a real collision effectively unreachable in
production (§2.2), the production code path (read `MAX(seq)`, compute `seq + 1`, insert)
cannot be used to force it: passing a stale `existing` into that path does not help, because
`seq` is always recomputed fresh from the read, not taken from `existing`. **The seam this
test needs is the lower-level, `seq`-parameterized insert step itself**
(`insert_decision_at_seq(sql, job, idx, seq, ...)`, the function the normal `MAX(seq)`-then-
insert flow calls internally with its freshly computed `seq + 1`): the test pre-populates a
row at some `seq` via the harness's own SQL access, then calls
`insert_decision_at_seq` directly with that same `seq`, bypassing the normal `MAX(seq)` read
entirely, and asserts the resulting constraint violation resolves to `AlreadyDecided`, never a
surfaced SQL error; **a late or duplicate report after seq 2 exists is a clean no-op** (G6) -
neither a seq 3 row nor an SQL error; **the seq-2 target node is already terminal when
`MarkOldTerminal` for seq 2 runs** (G6, e.g. the retry was independently cancelled) - the
decision proceeds to `StopOld`/projections without a conflicting write.

Option A scenarios: duplicate `SubmitResourceSamples` delivery yields one decision; a report
after the run is terminal is samples-only; a report arriving for a node already `failed`
(plain exit 137) with matching OOM evidence **upgrades** it rather than abandoning the
decision (L1); a report for a node terminal as `succeeded`/`cancelled`/`skipped`/`timed_out`
abandons the decision.

Option B scenarios (L1/L2): a duplicate `/complete-node` delivery for the same node carries
evidence only once (no double decision); exit 137 with `oom_kill = 0` classifies NOT-OOM, not
OOM, and the node's failure is recorded normally with no decision row; `memory.events`
unreadable by the second `exec` classifies UNKNOWN, same outcome as NOT-OOM.

**Explicit UNKNOWN scenarios (G1, option-independent).** A node whose `/complete-node` carries
an explicit "evidence unavailable" marker is recorded as an ordinary failure with no decision
row, exactly like a successful-batch NOT-OOM - this is the default behavior (§1.1 option B)
and needs no new test beyond the ordinary NOT-OOM path.

**Holding a failure write pending evidence is a Q11 option only; this design does not build
it by default (T1/T3).** If the owner picks holding as Q11's answer, what is actually
buildable differs sharply by H1 option, and the test plan must reflect that rather than
presuppose one shape:
* **Under option B**, holding is unnecessary to achieve the goal: `NodeContainer` is this
  project's own code, so instead of a coordinator-side wait it can simply include the explicit
  "evidence unavailable" marker (above) on the very first `/complete-node` call whenever the
  second `exec` fails or the cgroup read fails - there is no later evidence to wait for, since
  B's evidence collection already happens synchronously before the completion is posted at
  all. A coordinator-side window adds nothing under B.
* **Under option A**, holding is not something this design can add without changing existing
  code: the node is already written `failed` by `handle_complete_node` (the current,
  unconditional path) before the agent's batch - which may carry OOM evidence - has any chance
  to arrive. A coordinator-side window that holds the write would require `handle_complete_node`
  itself to learn to defer for `runner_auto` nodes, a change to an existing, currently
  unconditional handler, not an additive one. Whether that change is worth making, and for how
  long to hold, is Q11 plus the two new questions this revision tracks (§9, "mechanism and
  window length").

If and only if the owner answers Q11 "hold, under option A, via a coordinator-side window",
the DO-level test plan needs: a node whose completion arrives with no evidence report stays
un-classified (no decision, no ordinary-failure write forced early) until the window's own
deadline passes, at which point it is recorded as UNKNOWN; a report that arrives after the
node was already recorded this way is a late report, handled like any other late evidence
(§1.1). The window's deadline bound is specified in §1.1 (see F6).

### 6.3 Live smoke (not runnable in this environment)

Requires `CLOUDFLARE_API_TOKEN` (and account id) for `wrangler dev` against real Containers
(a real local Docker-backed `wrangler dev` is this project's own established pattern for
Containers testing - `docs/roadmap.md`'s "Containers from Rust" spike row, confirmed
2026-10-02 - Docker is required to run the local Containers runtime `wrangler dev` shells out
to, not a Cloudflare-hosted dependency), Docker for the local image build of the
`NodeContainer` image, and an image whose `cloud-ci
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
   agent's token/URL (blocked on Q3 for the latter). The env carrier is a hard prerequisite
   for OOM detection only under **option A (and C)** (§1.3); under **option B alone** it is
   still shipped (the agent still needs its token/URL to upload ordinary reports), but is not
   itself gating OOM recovery. Changes the environment of all nodes either way, so it is not
   inert; ship with its own sign-off.
3. **Barrier feed (blocked on H3/Q10).** Whichever of the two options is chosen. Shippable
   without OOM recovery (it makes a coordinator-failed shard node reach the barrier under
   option 2).
4. **Evidence capture, recorded only (blocked on H1/Q9).** Option B's second `exec` and the
   optional `CompleteNodeRequest` field, or option A's agent contract change; evidence is stored
   and logged but triggers nothing. **Includes the explicit UNKNOWN mechanism (§1.1's "The
   classification is correct; the detection mechanism does not exist yet"):** either the
   agent's "evidence unavailable" upload or the coordinator-side window, whichever the owner
   picks alongside H1 - the current abort-on-read-error agent behavior does not already cover
   this; a batch never arriving is not yet an observable signal, so this stage must build one.
   **This stage is inert only because `runner_auto` is rejected 422 `oom_recovery_disabled`
   until stage 5 enables the flag (F8)** - if Q11/Q17 pick the defer-the-write window shape,
   this stage's deliverable genuinely changes how node failures get recorded once stage 5
   turns it on (T1/T3); "recorded only ... triggers nothing" describes this stage's own code
   path, not the eventual behavior once enabled.
5. **Decision and effects behind the flag.** Node columns (literal guards), `oom_decision`,
   `runner_auto` at `startNode`, decision inside the evidence transaction, effect drain, alarm
   sweep (§3.6), fault-injection tests (§6.2) against a fake executor. Healing does not depend
   on the flag.
6. **Enable and live proof.** Run §6.3 with credentials; update parallelization.md,
   analytics.md and roadmap.md from "not wired" to "wired, verified on <date>" **only** with
   that run's evidence; flip the default.

`sizing_decisions` ships with the cron work, not in this sequence.

## Technical prerequisites before implementation

Separate from the owner-decision list in §9: these are facts about the runtime or existing
code that are currently unknown, unverified, or absent. Every one of them blocks real
implementation of at least part of this design regardless of how H1, H3 and every Q are
answered - resolving every owner decision does not by itself make this design implementable.

1. **The cgroup v2 layout visible inside a Cloudflare `durable_object`-policy container.**
   Whether `/sys/fs/cgroup` (the agent's own default path, `cli.rs` line 386) inside such a
   container actually exposes `memory.events`/`memory.peak`/`cpu.stat` the way a standard
   Linux cgroup v2 hierarchy does is not documented in any Cloudflare page read for this
   design; §1.1's own `CgroupReader` evidence section assumes it does. `[unverified]`.
2. **The second `exec()` option B needs.** §1.1's "Option B is new code with no precedent in
   the tree" discussion already lists this and the spike it requires (whether a second `exec`
   succeeds after the main process exited, whether it sees the same cgroup, and whether the
   pseudofiles are still readable); restated here because it blocks option B specifically, the
   same way the other four entries block parts of every option. `[unverified]`.
3. **The agent launch model and cgroup sharing (N7, §1.1).** How `cloud-ci agent` is launched
   relative to the job command it measures, and whether they share one cgroup, is not
   specified anywhere in this design or in the existing code (`NodeContainer::handle_start`
   execs exactly one `command: &[String]`). Blocks option A's evidence collection and every
   trigger classification that assumes the agent measured the right process.
4. **An env fallback for `--instance-type` (F5, §1.3).** The command-template contract bans
   the flag from a `runner_auto` command, but the shipped agent hard-fails without it
   (`agent.rs` lines 110-113) and has no env fallback today, unlike `--attempt`/`--node-id`.
   Blocks the command-template contract (§1.3) for every H1 option.
5. **A new additive proto field for the explicit UNKNOWN marker (F4, §1.1).** `oom_detected`
   (a required `bool`) and `memory_peak_bytes` cannot represent "evidence unavailable"; a new
   field is required, and does not exist on `SubmitResourceSamplesRequest` or
   `CompleteNodeRequest` today. Blocks Q11's "hold via upload" mechanism (Q17) specifically,
   not the window mechanisms.
6. **The `SqlStorage` constraint-violation error string (N12, §2.2).** `insert_decision_at_seq`
   needs to recognize a `(job_name, idx, seq)` primary-key collision from `sql.exec()`'s
   returned `worker::Error`, by string matching the same way `finalize_test_stats` (`mod.rs`
   ~2623-2634) already does for a `D1Error`. `oom_decision` is DO-local `SqlStorage`, not D1,
   and whether `worker::Error`'s `.to_string()` contains the identical `"UNIQUE constraint
   failed"`/`"PRIMARY KEY constraint failed"` substrings for the same underlying SQLite
   violation has not been confirmed. Blocks the PK-collision backstop (§2.2) for every option -
   low risk, since the backstop is already "effectively unreachable" in production (§2.2), but
   its failure mode (an unmatched string falls through to a genuine propagated error, not a
   silent miscategorization) should be confirmed before relying on it.

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
  update parallelization.md prerequisite 4. **Downside to weigh:** reusing the plain `attempt`
  slot means this is the *one* scheme for both an OOM retry and any future non-OOM retry
  mechanism, so `attempt+1` can collide with a genuine re-dispatch under a different retry
  reason (there is none today - OOM is the only automatic retry, §7's non-goals - but a future
  feature that also bumps `attempt` would collide). This design handles that collision only
  through the spec-hash conflict check (§1.2): a colliding insert goes to `stuck`, it is not
  silently merged. If a future non-OOM retry mechanism is ever added, it must share this same
  attempt-numbering space deliberately, not invent a second one.
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
* **Q17 (new, F7).** If Q11 is answered "hold": which mechanism (§1.1's G1 discussion) -
  the agent's explicit "evidence unavailable" upload, the late-upgrade-bound coordinator
  window, the defer-the-write coordinator window (option A only, requires changing
  `handle_complete_node`), or some combination? These are not mutually exclusive, but the
  defer-the-write shape is the only one with a non-additive cost (T1/T3).
* **Q18 (new, F7).** Whichever window shape Q17 picks: how long (the configured window before
  `min(window, remaining_run_time)` applies, F6), and what resolves a `runner_auto` node still
  deferred or still un-classified when the run closes first (F6) - record the ordinary failure
  with no retry (this design's fail-closed default elsewhere), or something else?

## 10. Contradictions found in the existing docs and code

* `docs/roadmap.md` (Phase 4 "Not wired", Next steps item 3) and `parallelization.md` list the
  remaining work as "a multi-size executor ladder, lineage- and attempt-aware resolution, live
  smoke test". They omit that **no managed node is sized at all today** (everything is
  `lite`), that **`exec` gets no environment** so the shipped `CLOUD_CI_NODE_ID` /
  `CLOUD_CI_SHARD_ATTEMPT` carriers have nothing to carry them, that no dispatcher provides the
  agent's token/URL, that **nothing produces the barrier input for a retry** (H3), and that the
  dominant OOM outcome is excluded by the admissibility rule (H1). "Only the wiring and the
  live proof are missing" understates this. **Corrected:** at Stage 6's live proof (§8), per
  that stage's own rule of only updating these docs with that run's evidence - not before,
  since the understatement is tied to decisions (H1, H3, Q1) that are not yet made.
* **analytics.md specifies two triggers; this design cannot yet use both (C4a).** `analytics.md`
  lines 232-235 and 704-707 give the agent's `memory.events` `oom_kill` counter **and** a
  Worker-side fallback ("container exited non-zero with no final Report"). Revision 1 used only
  the agent. H1 asks the owner to choose. **Corrected:** when H1/Q9 is decided and Stage 4
  (evidence capture) implements the chosen option - analytics.md's own wording does not need
  to change regardless of which option is picked, since it already documents both.
* **analytics.md "at job end" versus the shipped agent (C4b).** `analytics.md` says the agent
  reads/submits once "at job end" (lines 230-232 and the data-flow text). The shipped agent
  samples for a fixed `--duration-secs` window (default 60 s: `cloud-ci-cli/src/agent.rs`,
  `cloud-ci-cli/src/cli.rs`) and submits once after that window. A job shorter than the window
  is padded; a longer job is sampled only for its first minute. Any statement in this design
  that the agent reports "at job end" is wrong; it reports at the end of its window.
  **Corrected:** this is wrong today, independent of any decision in this design - same shape
  as the `dynamic-pipelines.md` fix below - but is not in this revision's scope; flag for a
  separate small analytics.md docs commit, same pattern as `dynamic-pipelines.md` (below), at
  the latest by Stage 6.
* **analytics.md `sizing_decisions` shape (C3).** Line 395 lists a narrower table than this
  design needs (§2.3 lists the extra columns); analytics.md must be updated when the table is
  built. **Corrected:** when `sizing_decisions` is built (the cron work, Q5) - not part of this
  design's staged delivery, which defers that table entirely (§2.3).
* `docs/design/dynamic-pipelines.md` (around line 309) said per-call `image`/`instance` and
  `exec()` "are not reachable from Rust". **Already corrected**, in a small docs-only commit on
  this branch separate from this design (`f9fa884`, 2026-10-03): ADR 0010, ADR 0011 and the
  roadmap spike row already say they are reachable (via the fork); the fork source confirms it
  (§1.3). This fix did not wait for Stage 6, because - unlike the roadmap/parallelization entry
  above - it was wrong independent of any pending decision in this design.
* `parallelization.md` prerequisite 4 plans to teach `shard_node_matches` the
  `:oom-retry:<n>` shape; `oom_retry_node_id`'s own doc comment says shard nodes do not need
  it; `resolve_oom_node_id` uses it for shards anyway. This design uses
  `shard_node_id(job, idx, attempt+1)` (Q7). **Corrected:** when Q7 is confirmed, since Q7's
  answer is what Stage 1 implements; updating the doc before that would describe code that
  does not yet exist either way.
* `executor.rs` `ContainersExecutor::capabilities()` advertises one `standard-4` rung while the
  real start path is `lite` (Stage 0). **Corrected:** by Stage 0 itself, once it ships (the
  rung list and the real start path converge; see Stage 0's own rollout gate, G4, for why
  shipping it does not itself fix every deployment's running size).
* analytics.md: "becomes the new `current_instance_type` immediately (not just for the one
  retry)" implies a reader on later runs; no reader exists and none is planned here (Q5). Its
  sample CLI comment ("--node-id ... currently unset by any real dispatcher") stays true until
  Stage 2. **Corrected:** the `current_instance_type` reader claim, when `sizing_decisions` and
  its reader are built (Q5, cron work, outside this design); the CLI comment, at Stage 2.
* analytics.md/settings.md default `runners.auto.min = "basic"` versus the runtime's rejection
  of `basic` under `durable_object` (the roadmap spike row already records the rejection, dated
  2026-10-02). **Corrected:** when Q2 is answered - an independent documentation edit, not
  gated on any stage of this design, since the wrong default exists regardless of whether OOM
  recovery is ever wired.
