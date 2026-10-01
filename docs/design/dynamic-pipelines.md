# Dynamic pipelines: CI as durable TypeScript workflows

Status: Proposed. [ADR 0009](../adr/0009-typescript-pipeline-workflows.md), which this design
implements, is **Accepted**; this document's defaults and APIs are still proposals.

> Every threshold, default, and API name here is a proposed starting point. Implementation is out
> of scope until the Phase 0 spikes in [the roadmap](../roadmap.md) land.

Related: [ADR 0009](../adr/0009-typescript-pipeline-workflows.md),
[ADR 0010](../adr/0010-pluggable-executors.md), [settings](./settings.md),
[parallelization](./parallelization.md), [pr-comment](./pr-comment.md), [assets](./assets.md),
[analytics](./analytics.md), [auth](./auth.md)

## Summary

A pipeline is a TypeScript **script** in `.cloud-ci/pipelines/<name>.ts` — one file per pipeline
(`ci.ts`, `deploy.ts`, `maintenance.ts`). Each file declares its own `on:` triggers and runs as
its own Dynamic Workflow instance; several pipeline files can run for the same commit (e.g.
`ci.ts` on `pull_request`, `deploy.ts` on `push` to `main`), independently of each other. There is
no YAML pipeline format. The only static file is `.cloud-ci/settings.yml`
([settings](./settings.md)): non-executable repo config — PR comment behavior, check naming and
aggregation, concurrency policy, runner pool definitions, cache/retention preferences, and which
secrets a pipeline may request. settings.yml cannot start a container or run code; it only narrows
what pipeline scripts are allowed to do.

A pipeline script does not return a static plan. It runs for the whole CI run: it starts
containers, reads their results, decides what to do next, and loops. The script can run
`turbo run --dry=json` in a container, parse the edges, and run each task in its own container as
soon as its dependencies finish. That logic lives in plain code using helpers from
`@cloud-ci/pipeline` and its integration modules (`@cloud-ci/pipeline/turbo`,
`@cloud-ci/pipeline/mise`), not in a fixed engine feature.

Scripts run as **Dynamic Workflows**: a Cloudflare Workflow whose code is loaded at runtime into a
Dynamic Worker. Every side effect (starting a container, waiting for it, publishing a check) is a
durable step. If the isolate is recycled mid-run, the script is re-executed from the top, and
completed steps return their recorded results instead of running again
(developers.cloudflare.com/dynamic-workers/usage/dynamic-workflows, checked 2026-10-01).

`RunCoordinator` stays the single writer of run state. The script *requests* things through a
narrow API; the coordinator enforces policy (secrets, runner limits, concurrency) and records
facts.

## Goals

- One pipeline file per workflow, each with its own triggers; multiple pipelines can run against
  the same commit without coordinating with each other.
- Turborepo/mise graphs executed node by node across containers, written as ordinary code.
- Full control over steps, sidecars, retries, conditional work, and fan-out from inside the
  script.
- Checks created explicitly and named by the script, not implied by job names.
- Crash-safe: a recycled isolate or a redeploy of `cloud-ci-worker` never re-runs finished work,
  and never silently drops a completion that arrived mid-crash.

## Non-goals

- Running the script in `cloud-ci-worker`'s own isolate. It runs in a Dynamic Worker that the
  host Worker loads; whether that sandbox isolates well enough is a Phase 0 spike.
- Letting the script grant itself secrets or bypass admin policy.
- Arbitrary npm imports in scripts in v1 (see [Open questions](#open-questions)).

## User experience

### Turborepo, one container per task

```ts
// .cloud-ci/pipelines/ci.ts
import { workflow } from "@cloud-ci/pipeline";
import { turbo } from "@cloud-ci/pipeline/turbo";

export default workflow({
  on: { pull_request: {}, push: { branches: ["main"] } },
  async run(ci) {
    const toolchain = ci.snapshot("toolchain", {
      files: ["mise.toml", "mise.lock"],
      run: "mise install",
    });
    const deps = ci.snapshot("deps", {
      from: toolchain,
      files: ["**/package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"],
      run: "pnpm install --frozen-lockfile",
    });

    const graph = await turbo.plan(ci, {
      snapshot: deps,
      tasks: ["build", "lint", "test", "typecheck"],
      affected: ci.event.kind === "pull_request",
    });

    const build = ci.check("ci/build", { required: true });
    const test = ci.check("ci/test", { required: true });

    await turbo.execute(ci, graph, {
      snapshot: deps,
      concurrency: 24,
      runner: (node) => (node.task === "build" ? "standard-2" : "auto"),
      check: (node) =>
        node.task === "build" ? build : ["test", "typecheck"].includes(node.task) ? test : null,
    });
  },
});
```

`turbo.execute` is ordinary library code, roughly:

```ts
export async function execute(ci, graph, opts) {
  const done = new Map<string, Promise<NodeResult>>();
  const runNode = (id: string): Promise<NodeResult> => {
    if (!done.has(id)) {
      done.set(id, (async () => {
        const deps = await Promise.all(graph.deps(id).map(runNode));
        if (deps.some((d) => !d.ok)) return ci.skip(id, "dependency failed");
        if (await ci.turboCache.has(graph.node(id).hash)) return ci.cached(id);
        return ci.container(id, {
          snapshot: opts.snapshot,
          runner: opts.runner(graph.node(id)),
          run: `turbo run ${graph.node(id).task} --filter=${graph.node(id).package}`,
          check: opts.check(graph.node(id)),
        });
      })());
    }
    return done.get(id)!;
  };
  await ci.limit(opts.concurrency, graph.ids().map((id) => () => runNode(id)));
}
```

Users who need something different copy or wrap it.

### Steps and sidecars

```ts
await ci.container("e2e", {
  snapshot: deps,
  runner: "standard-3",
  sidecars: {
    postgres: { image: "postgres:17", env: { POSTGRES_PASSWORD: "ci" }, ready: "tcp:5432" },
  },
  steps: [
    { name: "migrate", run: "pnpm db:migrate" },
    { name: "playwright", run: "pnpm playwright test", reports: ["playwright-report/**"] },
  ],
});
```

### Conditional and dynamic work

```ts
if (ci.changedFiles.some((f) => f.startsWith("infra/"))) {
  const plan = await ci.container("tf-plan", { run: "terraform plan -out plan.bin" });
  await ci.container("tf-check", { run: "terraform show plan.bin", check: false });
}
```

### Splitting tests across shards

Sharding is a library helper, not a core primitive — it reuses the same deterministic split
algorithm as `cloud-ci split` ([parallelization](./parallelization.md)):

```ts
const shards = await ci.shard("test", {
  snapshot: deps,
  files: "tests/**/*.spec.ts",
  count: "auto",
  run: (index, total) => `npx playwright test --shard=${index}/${total} --reporter=blob`,
  reports: [{ type: "playwright-blob", merge: "html" }],
  check: test,
});
```

`ci.shard` resolves a shard count and per-shard file/test assignment (timing-aware or round-robin,
same algorithm and same `test_timings` data as `cloud-ci split`), starts one `ci.container` per
shard, and runs the generated merge step once every shard reaches a terminal state. See
[parallelization](./parallelization.md) for split strategies, the merge barrier, and OOM-retry
semantics — `ci.shard` is the dynamic-pipelines entry point into that same design, not a separate
one.

### Runner selection

Any node (`ci.container`, `turbo.execute`'s `runner`, `ci.shard`) accepts either a named size on
the default executor (`"standard-2"`, `"auto"`) or an explicit executor:

```ts
runner: { executor: "aws-ec2", type: "c7i.4xlarge" }
```

or a named runner pool defined in [settings.yml](./settings.md) by an admin. Executors besides
Cloudflare Containers, their capabilities, and the pull/callback agent model are
[ADR 0010](../adr/0010-pluggable-executors.md); the script-facing surface is just the `runner`
field above.

## Design

### Execution model

```mermaid
sequenceDiagram
    participant W as cloud-ci-worker (host)
    participant WF as Dynamic Workflow (ci.ts)
    participant RC as RunCoordinator
    participant C as Containers
    participant GH as GitHub

    W->>RC: create run (repo, sha, script ref)
    W->>WF: create Workflow instance
    WF->>RC: step.do("container:turbo-plan") -> startNode
    RC->>C: start (policy-checked size, image, secrets)
    C-->>RC: node finished
    RC-->>WF: sendEvent("node:turbo-plan")
    WF->>WF: step.waitForEvent -> graph JSON
    loop each ready node
      WF->>RC: step.do("container:web#build") -> startNode
      RC-->>WF: sendEvent on completion
    end
    RC->>GH: check runs, PR comment notify
```

Each `ci.container(id, spec)` call is two durable operations:

1. `step.do("start:" + id)` asks `RunCoordinator` to start the node. This returns quickly and is
   idempotent on `id`.
2. `step.waitForEvent("done:" + id)`. The coordinator sends the event when the node finishes.

Waiting in an event, rather than inside a long `step.do`, means a 40-minute test job does not hold
a step open, and the isolate can be recycled while containers run.

**Node ids are the replay key** and must be unique and stable within a run. The SDK rejects a
duplicate id at the call site. Library helpers derive ids from turbo `taskId`s.

**Steps are retried, so effects must be idempotent.** Workflows persists a step's result once it
succeeds, but a step that fails or times out is retried, and its side effect may already have
happened. The coordinator therefore treats `(run_id, node_id)` as an idempotency key:

| Situation | Coordinator behavior |
| --- | --- |
| `startNode` retried after the container already started | Returns the existing node; never starts a second container |
| `startNode` with the same id but a different spec hash | Rejected as nondeterministic; run fails |
| Completion event delivered twice | The coordinator tracks delivery per `(run_id, node_id)` and keeps re-sending until the script acknowledges it — `ack` is recorded on the node by the next step after `waitForEvent`; `waitForEvent` itself only ever surfaces the first delivery to the script, so a duplicate send is harmless |
| Isolate crashes after consuming the completion event but before the next step persists it | The event is un-acked until that next step runs, so the coordinator has no record of an ack and re-sends the completion on its next attempt instead of treating the node as done; the script's `ack` is what closes the gap |
| Event arrives before the script waits for it | Supported by the protocol `[unverified: Workflows buffering of events sent before waitForEvent]`; if not, the coordinator re-sends on a timer until acknowledged |
| Run cancelled | Coordinator stops containers, marks nodes `cancelled`, and terminates the Workflow instance; late completion events for cancelled nodes are dropped |
| `waitForEvent` timeout (default: node timeout + 10 min) | Script sees a failed node with `timed out`; coordinator stops the container |

Step params and results must be RPC-serializable (per the Workflows API), so `ci.container`
returns a plain result object (status, exit code, durations, report and artifact ids), never a
live handle. Each node costs two steps, so the 10,000-step default allows about 5,000 nodes
minus `ci.check` and helper steps. The 2,000-node cap below keeps well clear of that.

### Determinism

On replay the script re-executes from the top, and completed steps return recorded values. Code
*between* steps must therefore make the same calls in the same order. Safe inputs are `ci.event`,
`ci.changedFiles`, `ci.readFile()` (at the run's sha), and step results. `Date.now()`,
`Math.random()`, and `fetch` (egress is blocked) are the hazards. The SDK lints for them, and the
coordinator detects divergence: a replayed start for an id it has never seen, after a completed
id was skipped, fails the run with `nondeterministic script`.

### Why the coordinator stays

The script decides *what* to run. `RunCoordinator` still:

- owns run/node state and is its only writer (D1 projection, Check Runs, PR comment notify);
- enforces policy the script cannot override: secret grants (none for fork PRs), runner min/max
  per repo, the repo-wide concurrent-container cap from [settings.yml](./settings.md) (a script's
  own `ci.limit`/`concurrencyGroup` can only narrow further, never raise it), max nodes per run;
- starts and stops containers, mints per-node tokens, and receives agent uploads.

Without this split, a PR could edit its own script to request a production secret or 500
`standard-4` containers.

### Graph helpers

| Helper | Does |
| --- | --- |
| `turbo.plan(ci, opts)` | Runs `turbo run <tasks> --dry=json` in a container; returns a graph of `taskId`, `package`, `task`, `hash`, `outputs`, `dependencies` (fields per turborepo.dev/docs/reference/run, checked 2026-10-01) |
| `turbo.execute(ci, graph, opts)` | Dependency-ordered fan-out with cache-hit skipping and bounded concurrency (above) |
| `mise.plan(ci, opts)` | Same for mise tasks `[unverified: mise's machine-readable graph command and format]` |
| `graph.fromJson(json)` | Lifts any tool's own graph output (Nx, Bazel, Pants, a custom script that prints JSON) into cloud-ci's generic graph shape, so `ci.limit` and dependency-ordered fan-out work the same way as for turbo/mise. This is the escape hatch for tools without a built-in integration module |
| `ci.shard(id, opts)` | Deterministic test splitting, per-shard containers, and merge barrier, reusing the `cloud-ci split` algorithm ([parallelization](./parallelization.md)) |
| `ci.limit(n, thunks)` | Concurrency limiter that is replay-safe (ordering is by call, not completion); the per-call half of concurrency control — repo-wide caps come from settings.yml (see Limits below) |
| `ci.group(ids, spec)` | Run several nodes in one container to save startup cost |

`turbo` and `mise` are separate entry points (`@cloud-ci/pipeline/turbo`,
`@cloud-ci/pipeline/mise`), not exports of the core package. A script that only needs
`graph.fromJson` and the generic `ci` API imports just `@cloud-ci/pipeline` and does not bundle
turbo- or mise-specific code.

Upstream outputs move through cloud-ci's Turborepo remote cache. Each node runs
`turbo run <task> --filter=<pkg>` without `--only`, so turbo restores upstream outputs from the
cache and runs just this task. `--only` would skip that restore (turborepo.dev/docs/reference/run,
checked 2026-10-01). Serving turbo's Remote Cache API (turborepo.dev/docs/openapi) from R2 is its
own compatibility project: auth, per-repo isolation, and read-only access for fork PRs so a PR
cannot poison artifacts that `main` trusts. Until it exists, the helpers fall back to cloud-ci
cache artifacts from declared `outputs`.

### Sidecars

Cloudflare Containers run one container per Durable Object instance. Whether two containers can
reach each other over a private network is `[unverified]`. v1 therefore runs sidecars as extra
processes inside the job's container: the agent starts them from their images' entrypoints
(image layers pulled into the job image at build time, or a multi-image runner image), waits for
`ready`, and tears them down after the steps. A sidecar that needs its own container is an open
question.

### Layered snapshots

Snapshots are layered, like Docker layers, so a source-only change doesn't invalidate the
toolchain or dependency install:

```ts
const toolchain = ci.snapshot("toolchain", {
  files: ["mise.toml", "mise.lock"],
  run: "mise install",
});
const deps = ci.snapshot("deps", {
  from: toolchain,
  files: ["**/package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"],
  run: "pnpm install --frozen-lockfile",
});
```

Each layer is keyed by a hash of `(parent key, copied files, commands)`. A node sets
`snapshot: deps` (the property is named `snapshot`; its value is a snapshot task, not a boolean)
to start from that layer's filesystem. Layers are long-lived across runs — reused whenever their
key is unchanged, not rebuilt per run — so a source-only commit reuses both the `toolchain` and
`deps` layers and only pays for copying source into the per-task containers. Layers are limited
to 20 GB and kept 30 days (developers.cloudflare.com/containers/platform/limits, checked
2026-09-30). Whether one container can start from a snapshot another took, and cross-container
restore speed versus a cold install, are Phase 0 spikes; the fallback is a lockfile-keyed
package-store cache ([assets](./assets.md)).

### GitHub status checks

Checks are opt-in: nothing is created unless the script asks for it, and names are chosen by the
script, not derived from node or task names.

- `ci.check(name, { required })` creates the check run immediately (`queued`), so branch
  protection sees it before any work starts. Nodes attach to it via `check: build` on
  `ci.container`; its conclusion is the worst of its attached nodes, with cached counting as
  success. A check with no attached nodes when the script ends concludes `success` with "no
  matching tasks", or `failure` if the script itself failed.
- Nodes with `check: null` (or omitted) report no check run; they still appear in the PR comment
  and dashboard — this is how a noisy check stays hidden without losing visibility elsewhere.
- One aggregate check, default name `cloud-ci`, rolls every check a pipeline creates up into a
  single status. Its name is configurable, and it can be disabled entirely, per repo, in
  [settings.yml](./settings.md); when enabled it's the recommended branch-protection target
  instead of naming individual pipeline checks.
- Check name templates (e.g. `"{pipeline} / {check}"`) are configurable in settings.yml so names
  stay stable across pipeline-file renames.

Naming a required check after a discovered task is unsafe: if the task leaves the graph, GitHub
waits forever for it. `required` only drives documentation and dashboard warnings; GitHub branch
protection does the enforcing.

### Limits (proposed)

| Limit | Value | Reason |
| --- | --- | --- |
| Nodes per run | 2,000 | Coordinator state and UI |
| Workflow steps per run | Under the Workflows default of 10,000, configurable to 25,000 (developers.cloudflare.com/workflows/build/workers-api, checked 2026-10-01) | Each node costs about 2 steps |
| Script bundle | 1 MiB | Loaded on every replay |
| Concurrent containers | Repo policy in [settings.yml](./settings.md), default 32; a script's own `ci.limit`/`concurrencyGroup` can only lower this per call, never raise it | Account-level container limits |

## Data model

| Store | Key | Contents |
| --- | --- | --- |
| D1 `runs` | `run_id` | adds `script_path`, `script_blob_sha`, `workflow_instance_id` |
| D1 `nodes` | `(run_id, node_id)` | status (`pending`, `cached`, `running`, `succeeded`, `failed`, `skipped`), spec hash, check name, timings |
| D1 `checks` | `(run_id, name)` | required flag, check run id, conclusion |
| D1 `node_history` | `(repo_id, node_id)` | p50/p95 duration, cache hit rate; feeds `runner: "auto"` and grouping |
| R2 `cloud-ci-assets` | `runs/{run_id}/script.js` | Bundled script as executed, for replay and audit |
| R2 `cloud-ci-cache` | `turbo/{repo_id}/{hash}` | turbo cache artifacts |

A commit that triggers two pipeline files (e.g. `ci.ts` on `pull_request` and `deploy.ts` on
`push`) creates two independent `run_id`s with the same `sha` but different `script_path`; they
are not coordinated with each other beyond sharing the PR comment's run list
([pr-comment](./pr-comment.md)).

## Reruns

| Action | Behavior |
| --- | --- |
| Rerun run | New Workflow instance at the same sha; turbo cache makes completed nodes cheap or skipped |
| Rerun failed nodes | New instance with the previous run's succeeded node results injected; `ci.container` returns them without starting containers |
| Retry inside a run | Script-controlled (`retries` on `ci.container`); OOM retry one size up stays a coordinator policy |

## Security considerations

- Scripts come from the PR head, including forks. They run in a host-loaded Dynamic Worker with
  egress blocked and no bindings except the `ci` API. The Phase 0 spike must confirm this
  isolation; if it cannot, this design does not ship (see
  [ADR 0009](../adr/0009-typescript-pipeline-workflows.md)).
- Workflow instance metadata is readable by the Dynamic Worker, so it carries only ids, never
  tokens (per the caution in the Dynamic Workflows docs above).
- Secrets are requested by name in `ci.container({ secrets })` and granted by the coordinator
  per admin policy, narrowed per pipeline by [settings.yml](./settings.md); fork PRs get none
  unless an admin approves the run.
- Credentials for non-default executors (AWS keys, kubeconfig) live in Secrets Store and are
  never passed to scripts or jobs — see [ADR 0010](../adr/0010-pluggable-executors.md).
- Container commands come from repo code, as in any CI; the isolation boundary for them is the
  container, not the script sandbox.

## Failure modes

| Failure | Behavior |
| --- | --- |
| Script throws | Run `failed`; running nodes are cancelled; `cloud-ci / script` check shows the stack trace |
| Nondeterministic replay | Run `failed` with the first diverging call |
| Isolate recycled or Worker redeployed | Workflow resumes; finished steps are not repeated |
| Container lost | Coordinator marks the node failed and sends its event; the script decides whether to retry |
| Script never awaits a started node | At script end, the coordinator cancels orphaned nodes |

## Open questions

1. Can workers-rs host the Worker Loader and Workflow bindings, or is the host side a small
   TypeScript module? `@cloudflare/dynamic-workflows` is a JS library, so a TS host is likely.
2. Container-to-container networking for real sidecars.
3. Cross-container snapshot restore and its speed compared with a cold install.
4. npm imports in scripts: resolve from the repo lockfile at plan time, or keep v1 to
   `@cloud-ci/pipeline` only?
5. Step pricing and latency of Workflows for runs with about 1,000 nodes.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Static plan returned by `pipeline.ts`, expanded by adapters (previous draft) | Every new behavior (sidecars, conditional fan-out, custom retry) becomes an engine feature and a config key |
| Script runs inside a long-lived "orchestrator" container | Pays a container for the whole run; a crash loses orchestration state without hand-written checkpointing |
| Our own replay journal in `RunCoordinator` instead of Workflows | Re-implements durable execution that Workflows already provides |
| Starlark or Lua scripts | Neither runs in Dynamic Workflows; we would host and sandbox an interpreter ourselves |
| Built-in `parallel:`/sharding engine feature (previous draft) | Same reasoning as every other engine feature: `ci.shard` keeps split strategy, runner choice, and merge logic in script code and reuses the `cloud-ci split` algorithm, rather than adding a second, YAML-only sharding config |
