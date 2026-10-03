import { CiContext } from "./context.js";
import type {
  CiEvent,
  ContainerExecutor,
  OnTrigger,
  PipelineContext,
  ShardGroupRegistrar,
  ShardPlanner,
  WorkflowStepLike,
} from "./types.js";

/** Params a created Workflow instance carries — the event data `ci.event`/
 * `ci.changedFiles`/`ci.branch`/`ci.labels` are built from. Mirrors
 * `PipelineContext`'s fields exactly: the same data the `on` function form
 * sees as its discovery `ctx` argument is what travels as the instance's
 * `params` and becomes `event.payload` inside `run(event, step)`. */
export type PipelineRunParams = PipelineContext;

/** The `run(ci)` function a script passes to `workflow({ on, run })`. */
export type RunFn = (ci: CiContext) => Promise<void>;

/** Options accepted by `workflow({ on, run })`, matching every example in
 * `docs/design/dynamic-pipelines.md`'s "User experience" section. */
export interface WorkflowOptions {
  readonly on: OnTrigger;
  readonly run: RunFn;
}

/** Advanced, non-script-facing dependencies `workflow()` accepts as its
 * second argument. A real `.cloud-ci/pipelines/*.ts` script written per
 * the design doc never passes this — it only ever calls
 * `workflow({ on, run })`. It exists so whichever future round wires a
 * real `ContainerExecutor`/`ShardPlanner`/`ShardGroupRegistrar` (reaching
 * `RunCoordinator`/`ResolveShardPlan`/`RegisterShardGroup`, see README)
 * has a seam to inject it, and so this round's own tests can inject
 * fakes without the SDK silently no-opping `ci.container`/`ci.shard`
 * calls. */
export interface WorkflowDependencies {
  readonly executor?: ContainerExecutor;
  readonly planner?: ShardPlanner;
  readonly registrar?: ShardGroupRegistrar;
}

/** Structural shape of a real `Workflow` binding's `.create()` call, typed
 * narrowly enough to avoid pulling in a hard `@cloudflare/workers-types`
 * version dependency at this file's single call site — same structural-
 * typing convention `@cloudflare/dynamic-workflows` itself uses for
 * `WorkflowStepLike`/`WorkflowEventLike`. */
export interface WorkflowBindingLike {
  create(options: { params: PipelineRunParams }): Promise<{ id: string }>;
}

/** Env shape `workflow()`'s `fetch` needs: a `WORKFLOWS` binding, matching
 * `cloud-ci-dynamic-workflows-host/test/fixtures/pipeline-script.js`'s
 * default export exactly (`env.WORKFLOWS.create({ params })`). */
export interface WorkflowFetchEnv {
  readonly WORKFLOWS: WorkflowBindingLike;
}

/** Structural shape of a real `WorkflowEvent<T>`, matching
 * `@cloudflare/dynamic-workflows`'s own `WorkflowEventLike`. */
export interface WorkflowEventLike<T> {
  readonly payload: T;
}

/**
 * The value a `.cloud-ci/pipelines/*.ts` script's `export default
 * workflow({ on, run })` produces. Dual-purpose, matching the two roles
 * `cloud-ci-dynamic-workflows-host`'s current `POST /scripts` handler
 * already calls on a loaded script's exports (see README's "workflow()
 * adapter shape" section for the one piece of follow-up glue this leaves):
 *
 * - `fetch(request, env)`: same shape as both existing test fixtures'
 *   default export — parses the posted params and calls
 *   `env.WORKFLOWS.create({ params })`, returning `{ id }`.
 * - `run(event, step)`: satisfies `@cloudflare/dynamic-workflows`'s
 *   `WorkflowRunner` shape directly — builds a `CiContext` from
 *   `event.payload`, calls the script's `run(ci)`, then seals every check
 *   the script left unsealed (the "automatic" rule in "Check sealing").
 */
export interface PipelineWorkflowExport {
  readonly on: OnTrigger;
  fetch(request: Request, env: WorkflowFetchEnv): Promise<Response>;
  run(event: WorkflowEventLike<PipelineRunParams>, step: WorkflowStepLike): Promise<void>;
}

/**
 * `import { workflow } from "@cloud-ci/pipeline-sdk"`. Wraps a script's
 * `{ on, run }` into the shape described on `PipelineWorkflowExport`. See
 * `WorkflowDependencies` for why the second argument exists and who it is
 * for (not ordinary script authors).
 */
export function workflow(
  opts: WorkflowOptions,
  deps: WorkflowDependencies = {},
): PipelineWorkflowExport {
  return {
    on: opts.on,

    async fetch(request, env) {
      const params = (await request.json()) as PipelineRunParams;
      const instance = await env.WORKFLOWS.create({ params });
      // `instance.id` must be awaited: a real `Workflow` binding's
      // `.create()` resolves to an RPC stub (`InstanceStub` in
      // `@cloudflare/dynamic-workflows`'s `wrapWorkflowBinding`), whose
      // `id` getter is a remote property read — synchronous access
      // resolves to `undefined` over the wire. Confirmed against a real
      // `wrangler dev` session 2026-10-02: without this `await`,
      // `POST /scripts` silently dropped `instanceId` from its response
      // (`JSON.stringify` omits an `undefined` property) while the
      // in-memory `WorkflowBindingLike` fake in `test/workflow.test.ts`
      // (a plain, non-RPC object) masked the bug there. Both existing
      // raw-JS fixtures (`pipeline-script.js`/`pipeline-script-dag.js`)
      // already `await instance.id` for exactly this reason.
      return Response.json({ id: await instance.id });
    },

    async run(event, step) {
      const payload = event.payload;
      const ci = new CiContext({
        event: payload.event,
        changedFiles: payload.changedFiles,
        branch: payload.branch,
        labels: payload.labels,
        step,
        executor: deps.executor,
        planner: deps.planner,
        registrar: deps.registrar,
      });
      await opts.run(ci);
      // "Check sealing": automatic rule — "when the script's `run`
      // function finishes scheduling ... `run` returns and every
      // scheduling call it made has resolved".
      ci.checks.sealRemaining();
    },
  };
}

export type { CiEvent };
