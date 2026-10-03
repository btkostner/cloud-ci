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

**What is explicitly deferred, distinct from this package's own tested
logic:** integration-level step-durability against the *real* Workflows
engine (a local `wrangler dev` session, proving `step.do` itself persists
across a forced isolate reload) is not re-proven in this package.
`cloud-ci-dynamic-workflows-host`'s own README already carries that real
result for the underlying primitive (`step1-plan`'s recorded output
surviving a forced `wrangler dev` reload, see its "Isolate-recycle
verification" section) — this package's test suite builds on that proven
primitive via a faithful in-memory fake rather than re-running a `wrangler
dev` session, since the primitive itself is not this package's to
re-verify. If `step.do`'s real persistence semantics ever change, that
package's README is the place that would need re-verification, not this
one.

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

## `workflow()`'s adapter shape, and the one piece of follow-up glue

`workflow({ on, run })` returns a single object meant to be a script's
`export default`:

```ts
export default workflow({ on: {...}, async run(ci) { ... } });
```

That object is dual-purpose, matching the two roles
`cloud-ci-dynamic-workflows-host`'s current code already calls on a loaded
script's exports:

- **`fetch(request, env)`** — same shape as both of that package's existing
  test fixtures' default export: parses the posted params as the run's
  `PipelineRunParams` and calls `env.WORKFLOWS.create({ params })`,
  returning `{ id }`. `POST /scripts` already calls
  `stub.getEntrypoint().fetch(tenantRequest)` — the *default* entrypoint —
  so this half needs no host-side change.
- **`run(event, step)`** — satisfies `@cloudflare/dynamic-workflows`'s
  `WorkflowRunner` shape (`run(event, step): Promise<R>`) directly: builds
  a `CiContext` from `event.payload`, calls the script's `run(ci)`, then
  seals every check left unsealed.

**The follow-up glue needed, stated explicitly rather than guessed
silently:** `cloud-ci-dynamic-workflows-host/src/index.ts`'s
`DynamicWorkflow` entrypoint currently loads the Workflow-step runner via
`loadScript(env, metadata).getEntrypoint("PipelineWorkflow")` — it asks for
a **named** export of a class called `PipelineWorkflow`. Both of that
package's current test fixtures (plain JS, not using this SDK) satisfy
that by declaring `export class PipelineWorkflow extends
WorkflowEntrypoint { ... }` *alongside* a separate `export default { fetch
}`. An SDK-built script's single `export default workflow(...)` statement
cannot produce a second named export the way those fixtures do — ESM has
exactly one default export per module — and `workflow()`'s returned object
is a plain object, not a `WorkflowEntrypoint` subclass, since this package
has no reason to depend on `cloudflare:workers` runtime classes to run its
own unit tests outside a Workers runtime.

Because `workflow()`'s default export already implements `run(event,
step)` matching `WorkflowRunner` directly, loading an SDK-built script only
needs a **one-line change** on the host side:
`getEntrypoint("PipelineWorkflow")` → `getEntrypoint()` (the default
entrypoint, the same one `POST /scripts` already uses for `fetch`). That
change — and rewriting `cloud-ci-dynamic-workflows-host`'s test fixtures to
use this SDK instead of raw inline JS — is explicitly **not** made this
round: it touches a sibling package's loader and its existing fixtures,
both out of this round's stated scope ("do NOT rewrite the existing
fixtures or touch that package's existing tests this round"). This is the
one adapter-glue gap between what this package builds and a real script
actually loading end-to-end through that host today.

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

No integration test against a real `wrangler dev` Workflows engine was run
for this package specifically — see "The step-durability contract, and
what is and isn't proven this round" above for why that's a reasonable,
explicitly-stated boundary rather than a silent gap: the underlying
`step.do` primitive's real persistence is already proven in
`cloud-ci-dynamic-workflows-host`, and this package only adds ordinary,
directly-testable logic around a call to that primitive.
