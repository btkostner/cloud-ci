# cloud-ci-dynamic-workflows-host

A real, minimal TypeScript Worker — graduating the Dynamic Workflows Phase 0
spike (`docs/roadmap.md`) from a scratch demo into a committed package — not
the full Dynamic Pipelines feature (`docs/design/dynamic-pipelines.md`).

## What this package does

1. Holds the `worker_loaders` binding (`LOADER`, `wrangler.toml`), per the
   Dynamic Workflows spike's proven pattern.
2. Exposes an HTTP API, reachable over an ordinary service binding (the
   same proven Rust-caller pattern `docs/adr/0009-typescript-pipeline-workflows.md`
   describes, used from the Rust `cloud-ci-worker` side):
   - `POST /scripts` — loads a given script **string** (standing in for a
     real `.cloud-ci/pipelines/*.ts` file) into a Dynamic Worker with
     `globalOutbound: null` egress blocking, and starts it as a Workflow
     instance.
   - `GET /instances/:id` — tracks that instance's lifecycle (status,
     completed-step output) via the real `Workflow` binding's `.status()`.
3. Proves one real container round trip from a Workflow step
   (`test/fixtures/pipeline-script.js`'s `step2-container-exec`): the step
   calls back into the Rust `cloud-ci-worker`'s `ContainerProbe` Durable
   Object (`packages/cloud-ci-worker/src/container_probe.rs`) over the
   `CONTAINER_WORKER` service binding, which calls the patched `worker`
   crate's `Container::start()`/`exec()` (`docs/adr/0011-patching-third-party-crates.md`).
4. Proves a genuine DAG — fan-out/fan-in, not a sequential chain — of 3
   dependent containers (`test/fixtures/pipeline-script-dag.js`): node A
   runs first, B and C both depend on A and are issued as concurrent
   `step.do` calls (joined via `Promise.all`, not individually `await`ed)
   once A resolves, and a 4th "join" step folds both results together.
   Each node addresses a distinct `ContainerProbe` DO instance via the
   `?probe_id=` query parameter `cloud-ci-worker/src/lib.rs`'s
   `handle_container_probe_exec` now reads (defaulting to the original
   singleton `"probe"` address when absent, so the sequential fixture
   above needs no change) — see "DAG verification" below for the real
   result, including whether B and C actually overlapped in wall-clock
   time.
5. Investigates forced isolate-recycle survival for a multi-step Workflow
   instance — see "Isolate-recycle verification" below for the exact
   method and the real, partial result (completed-step persistence is
   proven; full run-to-completion survival is not, due to a local-dev
   limitation documented there).
6. Loads a real `@cloud-ci/pipeline-sdk`-built script (not hand-rolled
   `WorkflowEntrypoint` JS) end to end: `DynamicWorkflow`'s
   `getEntrypoint()` call (`src/index.ts`) takes no entrypoint name, the
   default export `@cloud-ci/pipeline-sdk`'s `workflow()` already
   produces — see "SDK fixture: real end-to-end load and step-durability
   verification" below for the real result, including the one genuine
   adapter a loaded SDK script needs beyond `workflow()` itself.

## Container-from-Workflow-step access pattern (investigated this round)

A Workflow step running inside a Dynamic Worker has **no direct JS access
to `ctx.container`**: that API exists only `this.ctx.container` inside a
`DurableObject` subclass
(developers.cloudflare.com/containers/api/durable-object-container/,
checked 2026-10-02) — not on a `WorkflowEntrypoint`, and not anywhere
reachable from a plain Worker's `env`. So a step cannot start or exec into
a container by itself; it has to call something that *is* a
`DurableObject` with a container binding.

This package's `PipelineWorkflow.run`'s `step2-container-exec` therefore
calls back into a Rust-hosted Durable Object (`ContainerProbe`, in the
sibling `cloud-ci-worker` package) over the `CONTAINER_WORKER` service
binding — the only way to reach a container start/exec that goes through
the now-patched `worker` crate this round depends on
(`docs/adr/0011-patching-third-party-crates.md`). The chain proven this
round is:

```
TS Workflow step (this package)
  --fetch, service binding-->
Rust DO ContainerProbe::fetch (cloud-ci-worker, POST /internal/container-probe/exec)
  --container.start()/exec() via the patched `worker` crate-->
real Docker-backed container (local wrangler dev)
```

## Explicit non-goals (do not add to this package)

- The full `@cloud-ci/pipeline-sdk` TypeScript API (`ci.container`,
  `ci.check`, `ci.shard`, turbo/mise helpers) — Phase 2, doesn't exist yet.
- Real `.cloud-ci/pipelines/*.ts` discovery from a GitHub repo
  (`docs/design/settings.md`'s "Pipeline discovery" sequence) — a
  separate, unbuilt round. `POST /scripts` takes a script **string**
  directly; there is no GitHub fetch anywhere in this package.
- `RunCoordinator` integration for real job/shard/report state. The
  container call this round makes is a minimal proof of the access
  pattern, not real job tracking — nothing here calls `RunCoordinator`.
- Wiring this host Worker into `cloud-ci-worker`'s production request
  handling. There is no `/webhooks/github` → this host Worker call path —
  this stays a directly-testable, standalone service, same posture as
  other capability-ahead-of-its-caller rounds in this repo's history
  (see `docs/adr/0009-typescript-pipeline-workflows.md`'s Consequences).
- TypeScript transpilation of the loaded script. `POST /scripts`' `script`
  field is loaded verbatim as a JS module by the Worker Loader — this
  round's test fixture (`test/fixtures/pipeline-script.js`) is plain JS.
  Compiling a real `.ts` pipeline file at load time is separate, future
  work.

## Local development

Requires Docker running locally (same environment the Containers from
Rust and Dynamic Workflows spikes already proved works).

```sh
mise run //packages/cloud-ci-dynamic-workflows-host:dev
```

This runs `wrangler dev -c wrangler.toml -c ../cloud-ci-worker/wrangler.toml`,
a single local dev session hosting both Workers with real cross-Worker
service bindings
(developers.cloudflare.com/workers/local-development/multi-workers,
checked 2026-10-02) — not two separate `wrangler dev` processes, since
service bindings between separately-started local sessions were not
reliably supported until the multi-config `-c ... -c ...` flow this
command uses.

### Trigger a run

```sh
curl -s -X POST http://127.0.0.1:8787/scripts \
  -H "content-type: application/json" \
  -d "{\"script\": $(node -e 'console.log(JSON.stringify(require("fs").readFileSync("test/fixtures/pipeline-script.js", "utf8")))')}"
```

Returns `{"instanceId": "...", "scriptId": "..."}`. Poll status:

```sh
curl -s http://127.0.0.1:8787/instances/<instanceId>
```

### Trigger the DAG fixture

```sh
curl -s -X POST http://127.0.0.1:8787/scripts \
  -H "content-type: application/json" \
  -d "{\"script\": $(node -e 'console.log(JSON.stringify(require("fs").readFileSync("test/fixtures/pipeline-script-dag.js", "utf8")))')}"
```

Same polling as above. See "DAG verification" below for a real trace.

## DAG verification

See `/tmp/dynamic-workflows-dag-findings.txt` for the full transcript.
Real result against a `wrangler dev` session with Docker running: both
runs completed with distinguishable per-node output
(`"node-a\n"`/`"node-b\n"`/`"node-c\n"`, `exitCode: 0` for all 3), and the
fan-out gate held genuinely — node B's and node C's `startedAt` were both
strictly after node A's `finishedAt` in every run, matching `step.do`'s
sequential-await semantics (B/C's `step.do` calls are only issued after
`await stepA` resolves in the script), not a fixed-duration sleep.

**Concurrency finding (measured, not assumed):** node B and node C
overlapped almost entirely in wall-clock time — run 1:
B=[508547,509031] (484ms), C=[508549,509052] (503ms), 482ms overlap
(~99.6%); run 2: B=[556386,556846] (460ms), C=[556387,556877] (490ms),
459ms overlap (~99.8%). Both started within 1–3ms of each other both
times. The local Workflows engine genuinely ran B and C concurrently once
their `step.do` calls were issued together and joined via `Promise.all`
(rather than individually `await`ed) — this was checked honestly via each
node's own `Date.now()` timestamps recorded around its container `exec()`
call, not inferred from the code shape alone.

## Isolate-recycle verification

See `/tmp/dynamic-workflows-host-findings.txt` for the full transcript.
Summary: `instance.restart()` (exposed in local dev per
developers.cloudflare.com/changelog/post/2026-03-23-local-dev-instance-methods,
checked 2026-10-02) explicitly "restarts the instance from the
beginning" — the opposite of what this needs to prove, so it was not
used. No Cloudflare-documented local-dev hook forces just the Workflow
engine's isolate to recycle while leaving in-progress step state alone.
The proxy actually tried: `wrangler dev`'s hot-reload-on-file-change —
`touch src/index.ts` while an instance was asleep mid-run (after
`step1-plan` completed, before `step2-container-exec`), which produced a
real "⎔ Reloading local server... ⎔ Local server updated and ready" in
the dev log.

**Real result:** `step1-plan`'s already-recorded output
(`{"planned":true,"at":1790953715813}`) was unchanged and was not
re-executed — confirmed again via `wrangler workflows instances
describe` several minutes later, still showing the same output as
`Last Successful Step`. That is the actual claim this round needs:
already-completed step results survive a forced reload. **But** the
in-progress `pause-for-recycle-window` sleep step never resumed
afterward — `wrangler workflows instances describe` showed it stuck
"💤 Sleeping" for 3+ minutes against its 30-second target, while an
identical un-reloaded control run (same fixture, no touch) completed
normally in 31 seconds end to end, container exec included. This is a
genuine, precise local-dev limitation, not a code defect in this
package: `wrangler dev` 4.145.0's hot-reload does not reliably re-arm a
sleeping Workflow step's wake timer after the reload, and no documented
local hook exists to force or diagnose that directly. Full
run-to-completion recycle survival therefore stays **unproven locally**;
completed-step persistence across a forced reload is **proven**.

**Re-verified against the latest available `wrangler` (2026-10-02):** the
pinned version was 4.145.0; `npm view wrangler versions --json` showed
4.146.0 and 4.147.0 as newer releases. Neither release's changelog
(github.com/cloudflare/workers-sdk/releases/tag/wrangler%404.146.0 and
.../wrangler%404.147.0, checked 2026-10-02) mentions any Workflows
sleep/timer/hot-reload fix: 4.146.0 adds local-dev support for the
`createBatch()` API, and 4.147.0 only adds CLI-level retry for transient
API failures in `wrangler workflows instances list`/`describe` (unrelated
to the engine's wake-timer behavior). The exact repro above was re-run
against wrangler 4.147.0 anyway: `step1-plan` again completed and
survived the forced `touch src/index.ts` reload unchanged, but
`pause-for-recycle-window` was still stuck "💤 Sleeping" at 3 minutes
against its 30-second target, while an identical un-reloaded control run
under the same 4.147.0 session completed normally in 30 seconds,
container exec included — the same failure signature as 4.145.0. The
version pin was left at 4.145.0 (the bump was reverted after the retest)
since the newer version fixes nothing here. This limitation stays open;
re-check again only once a future `wrangler` changelog actually mentions
a Workflows sleep/timer/resume fix.

## SDK fixture: real end-to-end load and step-durability verification

`test/fixtures/pipeline-script-sdk.src.js` is a real pipeline script
authored through `@cloud-ci/pipeline-sdk`'s public `workflow()`/
`ci.check`/`ci.container` API — the first script this package loads that
was not hand-rolled raw `WorkflowEntrypoint` JS.
`test/fixtures/build-sdk-fixture.mjs` bundles it with esbuild into the
committed, self-contained `pipeline-script-sdk.js` `POST /scripts` loads
verbatim (see that file's module doc comment: the host still never
transpiles or resolves imports for a loaded script itself — bundling
happens at authoring time, outside the host). The two existing raw-JS
fixtures are unchanged and keep proving the host supports non-SDK scripts
too.

### The host-side fix, and the real adapter beyond it

`src/index.ts`'s `DynamicWorkflow` entrypoint changed
`getEntrypoint("PipelineWorkflow")` → `getEntrypoint()` (no name — the
default export, same one `POST /scripts` already calls `.fetch()` on).
Verified against `@cloudflare/dynamic-workflows`'s actual
`dist/types.d.ts`: `WorkflowRunner` is purely structural
(`run(event, step): Promise<R>`), and `WorkerStub.getEntrypoint`'s `name`
parameter is optional (`@cloudflare/workers-types`), returning the
default export's stub when omitted — nothing requires a
`WorkflowEntrypoint` subclass.

That one-line change was **not** sufficient by itself. A real
`wrangler dev` session (2026-10-02) surfaced a genuine runtime constraint
neither package's types caught: `@cloudflare/dynamic-workflows`'s
`dispatchWorkflow` always calls `runner.run(innerEvent, step)` with
**two** arguments, and workerd's RPC layer rejects that on a plain,
non-class exported object:

```
TypeError: Attempted to call RPC function "run" with the wrong number of
arguments. When calling a top-level handler function that is not
declared as part of a class, you must always send exactly one argument.
In order to support variable numbers of arguments, the server must use
class-based syntax (extending WorkerEntrypoint) instead.
```

`workflow()`'s own `fetch(request, env)` call from `POST /scripts` is
unaffected — it is called with exactly **one** argument
(`stub.getEntrypoint().fetch(tenantRequest)`), confirmed working against
the real engine (returned a real `instanceId`) with no wrapper. Only
`run`'s two-argument RPC call needed one: the fixture's actual default
export is a trivial `WorkflowEntrypoint` subclass (`cloudflare:workers`)
that forwards both calls straight to `workflow()`'s plain object —
`fetch(request) { return pipeline.fetch(request, env); }`,
`run(event, step) { return pipeline.run(event, step); }` — giving `run`
the class-based calling convention multi-argument RPC requires, while
`workflow()` itself stays the plain, `cloudflare:workers`-free object
`@cloud-ci/pipeline-sdk`'s own unit tests construct outside a Workers
runtime. This wrapper is the "real minimal adapter" a script built with
this SDK needs beyond calling `workflow()` — see
`pipeline-script-sdk.src.js`'s module doc comment for the full
evidence trail. A second, independent bug surfaced in the same session
and was fixed in `@cloud-ci/pipeline-sdk` itself: `workflow()`'s `fetch`
read `instance.id` without `await`— fine against the package's own
in-memory test fake (a plain object), but a real `Workflow` binding's
`.create()` resolves to an RPC stub whose `id` getter is a remote
property read, silently resolving to `undefined` without `await`
(`JSON.stringify` then drops the key, so `POST /scripts` returned
`{"scriptId": "..."}` with no `instanceId` at all). Fixed to
`await instance.id`, matching what both raw-JS fixtures already did.

### Real result: step-durability through the SDK's own `ci.container`

Method: `pipeline-script-sdk.src.js` runs two sequential `ci.container`
calls (`step-a` then `step-b`, `step-b`'s command structurally dependent
on `step-a`'s own recorded `startedAt`) with a real (not fake)
`ContainerExecutor` reaching the Rust `cloud-ci-worker`'s `ContainerProbe`
over `CONTAINER_WORKER` — the same proven chain `pipeline-script.js`
uses. A bare, non-durable `await new Promise(setTimeout, ...)` sits
between the two `ci.container` calls, in the script's plain `run`
function body outside any `step.do` — deliberately, to give a real,
easily-hittable window for a forced reload (same `touch src/index.ts`
technique as "Isolate-recycle verification" above).

Against a real `wrangler dev` session with Docker running: `step-a`
completed (`step.do("container:step-a", ...)` recorded
`{"startedAt":1790989774786, ...}`, confirmed via
`GET /instances/:id`), then `touch src/index.ts` was run during the bare
wait, producing the same real
"⎔ Reloading local server... ⎔ Local server updated and ready" log as
before. **Real result:** `step-a`'s recorded `startedAt` was unchanged
across repeated polling after the reload — the injected
`ContainerExecutor` was not invoked a second time for `step-a`, exactly
`step.do`'s real, documented persistence contract, now proven through the
SDK's own `ci.container` call instead of a raw `step.do`. An identical
un-reloaded control run completed normally end to end in ~20.5 seconds,
recording two distinct `startedAt` values (`1790991655937` and
`1790991676441`, ~20.5s apart, matching the deliberate wait), confirming
the fixture itself is correct independent of the reload.

**But**, matching this package's own documented, pre-existing limitation
for `step.sleep` above: the reloaded instance never resumed past the bare
wait — it stayed `"running"` with only `step-a`'s output recorded for
over 2 minutes after the reload (checked again after an additional 60
seconds), the same failure signature as `pause-for-recycle-window`'s
stuck sleep. This is consistent, not a new defect: a bare, non-durable
`await` inside `run()` has even less protection than `step.sleep` (which
at least is a Workflows-engine primitive) — when the isolate executing it
is torn down by a hot reload, there is nothing durable for the engine to
resume into. Full run-to-completion survival across a reload therefore
stays **unproven locally** for the SDK path too, for the same open
`wrangler dev` 4.145.0 local-dev limitation documented above;
already-completed step persistence is **proven**, same honest split as the
raw-JS fixture's result.

### Reproducing this

```sh
npm run build:fixture
mise run //packages/cloud-ci-dynamic-workflows-host:dev
curl -s -X POST http://127.0.0.1:8787/scripts \
  -H "content-type: application/json" \
  -d "{\"script\": $(node -e 'console.log(JSON.stringify(require("fs").readFileSync("test/fixtures/pipeline-script-sdk.js", "utf8")))'), \"params\": {\"event\": {\"kind\": \"push\"}, \"changedFiles\": [], \"branch\": \"main\", \"labels\": []}}"
```

Poll `GET /instances/:id` the same way as the other fixtures; touch
`src/index.ts` while `step-a`'s output is already recorded (within the
20-second window after it completes) to repeat the reload test above.

