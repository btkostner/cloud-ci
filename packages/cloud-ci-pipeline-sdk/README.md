# @cloud-ci/pipeline-sdk

The TypeScript library a `.cloud-ci/pipelines/*.ts` script imports, per
[`docs/design/dynamic-pipelines.md`](../../docs/design/dynamic-pipelines.md).
This package builds the real, tested `workflow()`/`ci.check`/`ci.container`
primitives that design doc's "User experience" examples call — it is a pure
library (no Worker, no `wrangler.toml` of its own): it is loaded *into* a
Dynamic Worker hosted by the sibling
[`cloud-ci-dynamic-workflows-host`](../cloud-ci-dynamic-workflows-host)
package, the same way any other script dependency would be.

## What this package implements

- **`workflow({ on, run })`** — wraps a script's triggers and run function
  into the dual-purpose default export a Dynamic Worker script needs (see
  "`workflow()`'s adapter shape and the one piece of follow-up glue" below).
- **`ci.check(name, { required })`** — a real, unit-tested in-memory state
  machine (`src/check.ts`): unsealed → sealed, idempotent member-id
  attachment, member-count tracking, "attach after seal" and "seal after
  seal" both throw typed errors. Implements the automatic end-of-run sealing
  rule and the explicit `check.seal({ conclusion, summary })` form from
  `docs/design/dynamic-pipelines.md`'s "Check sealing" section
  (`:475-507`).
- **`ci.container(id, { runner, run, check })`** — wraps the injected
  `ContainerExecutor.start()` call in one real `step.do("container:" + id,
  ...)` (`src/container.ts`), using `@cloudflare/dynamic-workflows`'s actual
  `WorkflowStepLike.do` persistence — not an invented parallel mechanism.
  Rejects a duplicate id within one execution, and attaches the node to its
  `check` (throwing if that check is already sealed) before the step runs.
- **`ci.shard(id, { split, count, files, run, check })`** — resolves a
  shard count and per-shard file assignment via the injected
  `ShardPlanner.resolve()` call, wrapped in one real
  `step.do("split:" + id, ...)` (`src/shard.ts`), then dispatches one real
  `ci.container` call per shard (reusing `runContainer` directly — not a
  duplicated step-durability mechanism), id `` `${id}#${shardIndex}` ``,
  with `opts.run({ shard, shards, files })`'s returned command string.
  Rejects a duplicate shard id within one execution, and attaches every
  resolved shard to `check` via each per-shard `ci.container` call's own
  attach logic. See "`ci.shard`: shard-plan resolution and per-shard
  dispatch" below for the full split-algorithm-reuse rationale.
- **`ci.group(ids, { runner, run, check })`** — the design doc gives
  `ci.group` exactly one line (a "Graph helpers" table row: "Run several
  nodes in one container to save startup cost", no worked example or
  prose section anywhere else in that document). This package implements
  that line literally: one real `step.do("group:" + ids.join(","), ...)`
  wraps a single `ContainerExecutor.start()` call covering every id in
  `ids`, and that one `ContainerResult` is returned for each id (see
  "`ci.group`: one container, several node ids" below for the full
  rationale and the explicit shape choice this ambiguous spec required).
  Rejects an empty `ids` array, a duplicate id within one call's own
  `ids`, and a duplicate id already used by another `ci.group`/
  `ci.container`/`ci.shard` call in the same execution; attaches every id
  to `check` (throwing if that check is already sealed) before the step
  runs.

## The step-durability contract, and what is and isn't proven this round

The core claim — "calling `ci.container` for the same `id` a second time
returns the previously recorded result without re-dispatching" — comes
directly from `step.do`'s real, documented persistence semantics
(developers.cloudflare.com/dynamic-workers/usage/dynamic-workflows). This
package's own logic around that call (id-uniqueness, check attachment) is
real and unit-tested (`test/container.test.ts`) against a **fake**
`WorkflowStepLike` that reimplements exactly the one property that matters —
"same step name returns the cached result without invoking the callback
again" — and asserts a fake `ContainerExecutor`'s call count stays at 1
across two `ci.container` calls with the same id within one simulated
replay.

**Unit-test scope vs. this round's real-engine proof:** the unit suite
above deliberately stays against the fake — fast, deterministic,
no `wrangler dev`/Docker dependency for every `vitest run`. The real
engine proof instead lives where a loaded script actually runs:
`cloud-ci-dynamic-workflows-host`'s
`test/fixtures/pipeline-script-sdk.src.js` calls this exact
`ci.container` API against a real (not fake) `ContainerExecutor`, through
a real local `wrangler dev` + `@cloudflare/dynamic-workflows` session —
see that package's README's "SDK fixture: real end-to-end load and
step-durability verification" section for the full method and result
(completed-step persistence across a forced reload: proven; full
run-to-completion survival: not, due to a documented pre-existing local-
dev limitation). `step.do`'s own primitive-level persistence was already
proven independently for the underlying primitive in that package's
"Isolate-recycle verification" section (`step1-plan`'s recorded output
surviving a forced `wrangler dev` reload) — this round proved the same
contract again through this package's own `ci.container` call, not just
the raw primitive.

## `ci.container`'s two-step design vs. this round's one step

`docs/design/dynamic-pipelines.md`'s "Execution model" describes each
`ci.container(id, spec)` call as **two** durable operations:
`step.do("start:" + id)` to ask `RunCoordinator` to start the node, then
`step.waitForEvent("done:" + id)` to wait for its completion event — so a
long-running container doesn't hold a `step.do` open and block isolate
recycling. This round has no `RunCoordinator` wiring (see "Explicitly out
of scope" below), so there is no completion event to wait for yet: the
injected `ContainerExecutor.start()` call is wrapped in a single
`step.do("container:" + id, ...)` instead. Splitting this into the real
start/wait-for-event pair is explicit follow-up work for whoever wires a
`ContainerExecutor` that talks to the real `RunCoordinator` — see
`src/container.ts`'s doc comment for exactly where that split needs to
happen.

## `ContainerExecutor`: the injectable boundary to real container start

`ci.container` never talks to a container runtime directly. It calls a
`ContainerExecutor.start(request)` the caller injects via `workflow(opts, {
executor })`. This is the seam a future round wires to the real mechanism
`packages/cloud-ci-worker/src/container_probe.rs` /
`src/node_container.rs` prove (a Rust-hosted container-exec Durable Object,
reached from a TS host over the `CONTAINER_WORKER`-style service binding
`cloud-ci-dynamic-workflows-host` already demonstrates). This round's own
tests inject a clearly-labeled fake, in-memory `ContainerExecutor`
(`test/container.test.ts`'s `FakeContainerExecutor`) — not the real
`RunCoordinator` chain. Calling `ci.container` with no executor configured
throws `ContainerExecutorNotConfiguredError` rather than silently
succeeding or no-opping.

Ordinary `.cloud-ci/pipelines/*.ts` scripts never see or pass this — they
only ever call `workflow({ on, run })`. `WorkflowDependencies` (the second,
optional argument) exists purely as a test/integration seam.


## `ci.shard`: shard-plan resolution and per-shard dispatch

`ci.shard(id, { split, count, files, run, check })` implements
`docs/design/dynamic-pipelines.md`'s "### Splitting tests across shards"
and `docs/design/parallelization.md`'s "### Split strategies"/"### Shard
count resolution"/"### Deterministic assignment, end to end" — with one
architectural constraint those docs make explicit and this package takes
seriously: parallelization.md's "### Deterministic assignment, end to end"
goal is "the same binary and the same split algorithm ... used in both
places, so a BYO CI matrix and a cloud-ci-managed shard group produce
byte-identical assignments for the same inputs." The real split algorithm
(LPT bin-packing, round-robin, median-fallback imputation) lives in
`cloud_ci_core::split` (Rust) — the same pure crate `cloud-ci-cli`'s own
`cloud-ci split` command calls. **This package never reimplements that
algorithm in TypeScript.** `cloud-ci-pipeline-sdk` runs inside a Dynamic
Worker, a separate runtime from `cloud-ci-worker`'s Rust/wasm32 code,
reached only via a service binding — the same architectural shape
`ci.container`'s own `ContainerExecutor` injection point already uses for
the real container-start mechanism.

`ci.shard` resolves the plan through an injected `ShardPlanner` (`src/
types.ts`'s `ShardPlanner` interface — one `resolve(request)` method,
mirroring `ContainerExecutor.start()`'s shape exactly), wrapped in one real
`step.do("split:" + id, ...)` (`src/shard.ts`). That matches
parallelization.md's "### Deterministic assignment, end to end" step 2
precisely: "For managed runs, `RunCoordinator` computes the assignment
once ... when `ci.shard`'s `step.do("split:" + id)` runs, and stores it as
`shard_plan` rows before dispatching any shard container" — a real
`ShardPlanner` reaching `cloud-ci-worker`'s `ResolveShardPlan` RPC
(`packages/cloud-ci-worker/src/shard_plan.rs`, see that module's docs for
its own reuse of `cloud_ci_core::split`) is exactly the RPC step this
`step.do` wraps. Once the plan resolves, `ci.shard` dispatches one real
`ci.container` call per shard — `runShard` imports and calls
`runContainer` directly (`src/container.ts`'s already-proven function, not
a second, duplicated step-durability mechanism), id ``
`${id}#${shardIndex}` `` (1-based), each with `opts.run({ shard, shards,
files })`'s returned command string. Every per-shard container id lives in
the same `seenIds` set a script's own `ci.container` calls use, so a shard
id and a plain container id can never collide.

This round does NOT build the real network call to `ResolveShardPlan` —
same posture as `ContainerExecutor` (see above): `ci.shard`'s dispatch
contract is proven against the `ShardPlanner` interface and a fake,
in-memory implementation (`test/shard.test.ts`'s `FakeShardPlanner`), not
the real RPC. Calling `ci.shard` with no planner configured throws
`ShardPlannerNotConfiguredError` rather than silently succeeding or
no-opping — same "fail loudly, not silently" rule
`ContainerExecutorNotConfiguredError` already follows.

**Scope lines this round draws, precisely:**

- **`sidecars`** — `ShardOptions` has no `sidecars` field, matching
  `ContainerOptions`'s own "Sidecars" scope boundary below. The worked
  example in dynamic-pipelines.md's "### Splitting tests across shards"
  (a per-shard `postgres` sidecar restored from a migration snapshot) is
  not implemented.
- **`reports`/merge barrier** — `ShardOptions` has no `reports` field.
  dynamic-pipelines.md describes `ci.shard` as also running "the generated
  merge step once every shard reaches a terminal state"; `ShardResult`
  only returns the resolved shard count and each shard's raw
  `ContainerResult`, no merged-report id. The server-side merge logic this
  would eventually trigger already exists
  (`packages/cloud-ci-worker/src/shard_merge.rs`, from an earlier round) —
  wiring `ci.shard`'s `reports`/`merge` options to call it is separate,
  not-yet-scheduled follow-up work, not a missing dependency.
- **Real `--granularity test` per-test splitting** — `SplitStrategy`'s
  `"count"` value round-robins at whole-file granularity, identical to
  `"file"`. `cloud_ci_core::split` (the Rust crate `ResolveShardPlan`
  reuses) only supports file granularity today — per-test enumeration
  needs a framework-aware static parser
  (`packages/cloud-ci-cli/src/split.rs`'s own module docs) that does not
  exist in this codebase. This package does not promise more than the
  underlying algorithm actually provides.
- **`snapshot`** — `ShardOptions` has no `snapshot` field, matching
  `ci.snapshot`'s own absence from this round (see "Explicitly out of
  scope this round" below).

## `ci.group`: one container, several node ids

`docs/design/dynamic-pipelines.md`'s entire specification of `ci.group` is
one "Graph helpers" table row: `` `ci.group(ids, spec)` `` — "Run several
nodes in one container to save startup cost" (`:381`). Unlike
`ci.container`, `ci.shard`, and `ci.check`, there is no "User experience"
code sample, no "Design" section, and no other mention of `ci.group`
anywhere else in that document.

**The ambiguity, and the choice this package makes:** the row gives a
signature (`ids`, a param this package's `GroupOptions` type exposed as
`opts`) and a one-sentence purpose, but no field list for `spec`, no
return shape, and no statement of how a grouped node's id interacts with
`ci.container`/`ci.shard`'s own id-uniqueness and check-attachment rules.
This package resolves that by treating `ci.group` as the narrowest
coherent reading of "several nodes in one container": `spec` reuses
`ContainerOptions`'s exact field set (`runner`, `run`, `check`) rather
than inventing a second container-spec shape, with `run` being the single,
already-combined shell command the caller composes to cover every id in
`ids` (the same division of labor `ci.shard`'s `run({shard, shards,
files}) => command` callback already uses: the SDK never guesses how to
combine several nodes' work into one command). `GroupResult.results` maps
every id in `ids` to that one container's `ContainerResult`, since all of
them genuinely ran inside it. Every id in `ids` goes through the exact
same `seenIds` duplicate check and `check.attach()` call a plain
`ci.container`/`ci.shard` id would (`src/group.ts`), so a grouped id can
never silently collide with — or escape — the id-uniqueness and
check-sealing rules the rest of the SDK already enforces.

**What this reading explicitly is not:** a callback-based logical/UI
grouping of nested `ci.check`/`ci.container` calls (e.g. `ci.group(name,
fn)`). The design doc's actual signature takes an id array and a spec
object, never a function — this package does not implement a shape the
text does not state, however natural that alternate reading might seem
from the "logically group a set of steps" phrasing a feature named
`group` might suggest in other CI systems.

`runGroup` (`src/group.ts`) deliberately does not call `runContainer`
per id — doing so would dispatch one container per id, defeating the
"save startup cost" point of the primitive entirely (unlike `ci.shard`,
which genuinely wants, and gets, one container per shard). It instead
makes its own single `step.do("group:" + ids.join(","), ...)` call,
after running the same duplicate-id and check-attach checks
`runContainer` makes, against every id in `ids`.

## `workflow()`'s adapter shape, and the host-side load path (confirmed 2026-10-02)

`workflow({ on, run })` returns a single object meant to be a script's
`export default`:

```ts
export default workflow({ on: {...}, async run(ci) { ... } });
```

That object is dual-purpose, matching the two roles
`cloud-ci-dynamic-workflows-host`'s code calls on a loaded script's
exports:

- **`fetch(request, env)`** — same shape as both of that package's
  hand-rolled test fixtures' default export: parses the posted params as
  the run's `PipelineRunParams` and calls `env.WORKFLOWS.create({
  params })`, returning `{ id }`. `POST /scripts` calls
  `stub.getEntrypoint().fetch(tenantRequest)` — the *default* entrypoint,
  called with exactly one argument.
- **`run(event, step)`** — satisfies `@cloudflare/dynamic-workflows`'s
  `WorkflowRunner` shape (`run(event, step): Promise<R>`) directly: builds
  a `CiContext` from `event.payload`, calls the script's `run(ci)`, then
  seals every check left unsealed.

**This is genuinely wired up and proven end to end**, not just typed
compatibly: `cloud-ci-dynamic-workflows-host/src/index.ts`'s
`DynamicWorkflow` entrypoint loads the Workflow-step runner via
`loadScript(env, metadata).getEntrypoint()` — no entrypoint name, the
*default* export, the same one `POST /scripts` already uses for `fetch`.
`@cloudflare/dynamic-workflows`'s `WorkflowRunner` type is purely
structural (checked against its actual `dist/types.d.ts`,
2026-10-02) and `getEntrypoint()`'s `name` parameter is optional
(`@cloudflare/workers-types`) — nothing requires a `WorkflowEntrypoint`
subclass for this call.

**One real adapter a loaded script needs beyond calling `workflow()`,
discovered against a real `wrangler dev` session (not assumed):**
`@cloudflare/dynamic-workflows`'s `dispatchWorkflow` always calls
`runner.run(innerEvent, step)` with **two** arguments, and workerd's RPC
layer rejects that on a plain, non-class exported object — confirmed by
the exact runtime error (`"Attempted to call RPC function \"run\" with
the wrong number of arguments ... the server must use class-based syntax
(extending WorkerEntrypoint) instead"`), while `fetch`'s one-argument RPC
call works on the same plain object with no wrapper (confirmed: a real
`POST /scripts` call against an SDK-built script returned a real
`instanceId`). So a script loaded by `cloud-ci-dynamic-workflows-host`
wraps `workflow()`'s result in a trivial `WorkflowEntrypoint` subclass
that forwards both calls — `fetch(request) { return pipeline.fetch(request,
env); }`, `run(event, step) { return pipeline.run(event, step); }` — giving
`run` the class-based calling convention multi-argument RPC requires,
while `workflow()` itself stays exactly as it is: a plain,
`cloudflare:workers`-free object this package's own unit tests construct
outside a Workers runtime. See
`cloud-ci-dynamic-workflows-host/test/fixtures/pipeline-script-sdk.src.js`
for the real, working example of this wrapper and
`cloud-ci-dynamic-workflows-host/README.md`'s "SDK fixture: real
end-to-end load and step-durability verification" section for the full
evidence trail, including a second bug this round's real-engine proof
caught and fixed in this package itself: `workflow()`'s `fetch` used to
read `instance.id` without `await`, which worked against this package's
own in-memory test fake (a plain, synchronous object) but silently
dropped `instanceId` from `POST /scripts`'s real response, since a real
`Workflow` binding's `.create()` resolves to an RPC stub whose `id`
getter is a remote property read. Fixed to `await instance.id`, matching
what `cloud-ci-dynamic-workflows-host`'s raw-JS fixtures already did.

**The real, proven result, against the actual local `wrangler dev` +
`@cloudflare/dynamic-workflows` engine (not the in-memory
`FakeWorkflowStep` this package's own unit tests use):** an SDK-built
script's `ci.container` call is backed by `step.do`'s real persistence —
a completed container step's recorded result survives a forced
`wrangler dev` reload (`touch src/index.ts`, the same technique
`cloud-ci-dynamic-workflows-host`'s own isolate-recycle investigation
used) without the injected `ContainerExecutor` being invoked a second
time. Full run-to-completion survival across that same reload is **not**
proven — the same pre-existing `wrangler dev` 4.145.0 local-dev
wake-timer limitation that package's README already documents for
`step.sleep` blocks it here too, for an analogous reason (see that
README section for the precise, honest split between what is and isn't
proven).


## Explicitly out of scope this round

Each of these is a real, named gap — not a silent omission:

- **`ci.shard`'s `sidecars`, `reports`/merge barrier, and real `--granularity
  test` per-test splitting** — `ci.shard` itself IS implemented this round
  (`split`/`count`/`files`/`run`/`check`); see "`ci.shard`: shard-plan
  resolution and per-shard dispatch" above for exactly what is and isn't
  built, including the `ResolveShardPlan` RPC's own scope boundary.
- **`ci.snapshot`** — layered filesystem snapshots (`toolchain`/`deps`
  layers, sidecar-volume capture). Not implemented; `ContainerOptions` has
  no `snapshot` field.
- **`turbo`/`mise` integration modules** (`@cloud-ci/pipeline-sdk/turbo`,
  `@cloud-ci/pipeline-sdk/mise`) — `turbo.plan`/`turbo.execute`/
  `turbo.checkPerTask`/`mise.plan` and the generic `graph.fromJson`/
  `graph.fromGraph` helpers. Not implemented; this package has no `turbo`
  or `mise` export at all.
- **Sidecars** — `ContainerOptions` has no `sidecars` field; a container
  spec in this round is a single process, no sidecar lifecycle.
- **Multi-step containers** — `ContainerOptions` has no `steps` field; a
  `ci.container` call in this round runs exactly one `run` command, not a
  list of named steps with per-step reports.
- **`on`'s discovery isolate/CPU-budget semantics** — `workflow()` exposes
  `on` as a plain property for a future discovery caller to read/call; it
  does not implement the sandboxed, budget-limited, cached-by-blob-sha
  evaluation the design doc's "Discovery and triggers" section describes.
  That sandboxing is the discovery caller's job, not this library's.
- **Real `RunCoordinator`/`cloud-ci-worker` wiring** — `ci.container`
  never reaches the real `ContainerProbe` Durable Object or
  `RunCoordinator`; it calls whatever `ContainerExecutor` is injected (see
  above). `ci.shard` likewise never reaches the real `ResolveShardPlan` RPC
  this round — it calls whatever `ShardPlanner` is injected (see "`ci.shard`"
  above). The Rust `ResolveShardPlan` RPC and its `cloud-ci-worker` handler
  DO exist as of this round (`packages/cloud-ci-worker/src/shard_plan.rs`);
  what doesn't exist yet is the TypeScript-side network call reaching it —
  no `wrangler.toml` changes were made anywhere in this round.
- **`ci.limit`, `ci.skip`, `ci.cached`, `ci.turboCache`, `ci.readFile`** —
  none of the design doc's other `ci` surface members exist on `CiContext`
  yet; only `ci.event`, `ci.changedFiles`, `ci.branch`, `ci.labels`,
  `ci.check`, `ci.container`, `ci.shard`, `ci.group`.

## Testing

`vitest run` (`npm test` / `mise run //packages/cloud-ci-pipeline-sdk:test`):

- `test/check.test.ts` — the `Check`/`CheckRegistry` state machine: starts
  unsealed, member-count tracking, idempotent attach, attach-after-seal and
  seal-after-seal both throw, explicit vs. automatic sealing,
  `sealRemaining()`'s "don't touch already-sealed checks" behavior.
- `test/container.test.ts` — the step-durability contract via a fake
  `WorkflowStepLike` (real `step.do` cache semantics) and a fake
  `ContainerExecutor` (call-count assertion), plus duplicate-id rejection,
  check attachment, and the no-executor-configured error.
- `test/shard.test.ts` — `ci.shard`'s dispatch logic against a fake
  `ShardPlanner` and the real `runContainer` dispatched per shard (via a
  fake `ContainerExecutor`, same convention as `container.test.ts`):
  shard-plan resolution feeding `run({shard, shards, files})` per shard,
  request passthrough to the planner, replay reusing the recorded plan
  without re-dispatching containers, check attachment across every
  resolved shard, the shared container-id namespace with plain
  `ci.container` calls, duplicate shard-id rejection, and the
  no-planner-configured error.
- `test/group.test.ts` — `ci.group`'s single-container batch dispatch
  against a fake `WorkflowStepLike`/`ContainerExecutor` (same conventions
  as `container.test.ts`): one executor call covers every id in `ids`,
  the same `ContainerResult` is returned for each id, replay reuses the
  recorded step without a second executor call, check attachment across
  every grouped id, the shared id namespace with plain `ci.container`/
  `ci.shard` calls (collisions rejected both ways), within-call duplicate
  ids rejected, an empty `ids` array rejected, and the
  no-executor-configured error.
- `test/workflow.test.ts` — `workflow()`'s adapter shape: `on` passthrough,
  `fetch()`'s `env.WORKFLOWS.create()` call and id response, `run()`
  building a `CiContext` and auto-sealing checks, and an end-to-end replay
  proof through the public `workflow()` surface (not just `runContainer`
  directly).

No integration test against a real `wrangler dev` Workflows engine runs as
part of this package's own `vitest run` — that proof instead lives in
`cloud-ci-dynamic-workflows-host`'s
`test/fixtures/pipeline-script-sdk.src.js`, a real script built with this
SDK's `workflow()`/`ci.container`, run against a real local `wrangler dev`
session (see "The step-durability contract" above and that package's
README's "SDK fixture: real end-to-end load and step-durability
verification" section for the full method and result). This package's own
unit suite stays against a faithful in-memory fake for speed and
determinism; the real-engine proof lives where a loaded script actually
runs, not duplicated into every `vitest run` here.
