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
4. Investigates forced isolate-recycle survival for a multi-step Workflow
   instance — see "Isolate-recycle verification" below for the exact
   method and the real, partial result (completed-step persistence is
   proven; full run-to-completion survival is not, due to a local-dev
   limitation documented there).

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
- The "3 dependent containers" fan-out/DAG pattern. This round proves 2–3
  **sequential** steps surviving a forced isolate recycle — the actual
  content of the roadmap's recycle exit criterion clause. Fan-out is a
  separate, later round.
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
