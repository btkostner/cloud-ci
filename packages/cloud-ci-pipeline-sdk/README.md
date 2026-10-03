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

- **`ci.shard`** — deterministic test sharding and the merge barrier
  (`docs/design/parallelization.md`). Not implemented; no sharding helper
  exists in this package.
- **`ci.snapshot`** — layered filesystem snapshots (`toolchain`/`deps`
  layers, sidecar-volume capture). Not implemented; `ContainerOptions` has
  no `snapshot` field.
- **`turbo`/`mise` integration modules** (`@cloud-ci/pipeline-sdk/turbo`,
  `@cloud-ci/pipeline-sdk/mise`) — `turbo.plan`/`turbo.execute`/
  `turbo.checkPerTask`/`mise.plan` and the generic `graph.fromJson`/
  `graph.fromGraph` helpers. Not implemented; this package has no `turbo`
  or `mise` export at all.
- **`ci.group`** — running several nodes in one container. Not
  implemented.
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
  above). No Rust or `wrangler.toml` changes were made anywhere in this
  round.
- **`ci.limit`, `ci.skip`, `ci.cached`, `ci.turboCache`, `ci.readFile`** —
  none of the design doc's other `ci` surface members exist on `CiContext`
  yet; only `ci.event`, `ci.changedFiles`, `ci.branch`, `ci.labels`,
  `ci.check`, `ci.container`.

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
