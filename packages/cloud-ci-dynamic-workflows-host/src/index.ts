/**
 * The Worker Loader this round's scope boundary describes: holds the
 * `worker_loaders` binding, loads a given script string into a Dynamic
 * Worker with `globalOutbound: null` egress blocking, starts it as a
 * Workflow instance, and tracks that instance's lifecycle. See this
 * package's README for the full scope boundary (what this round proves
 * vs. what stays explicitly unbuilt).
 *
 * Follows `@cloudflare/dynamic-workflows`'s documented shape exactly
 * (developers.cloudflare.com/dynamic-workers/usage/dynamic-workflows,
 * checked 2026-10-02): the loaded script's own `fetch` handler calls
 * `env.WORKFLOWS.create()` (the wrapped binding `wrapWorkflowBinding`
 * returns), so `POST /scripts` here forwards to that handler rather than
 * calling `.create()` itself — `wrapWorkflowBinding`'s stub is only valid
 * inside the Dynamic Worker it was handed to.
 * `wrapWorkflowBinding`/`createDynamicWorkflowEntrypoint` wire the Worker
 * Loader to the Workflows engine, and `DynamicWorkflowBinding` must be
 * re-exported so the runtime can build per-script RPC stubs.
 *
 * # Script persistence across isolate recycle
 *
 * `createDynamicWorkflowEntrypoint`'s callback is invoked fresh every time
 * the Workflows engine needs to run a step — including after an isolate
 * recycle — and only receives back whatever `metadata` was passed to
 * `wrapWorkflowBinding` when the instance was created (the engine persists
 * that metadata with the instance itself: "the tenant ID is saved with the
 * instance automatically"). There is nowhere else a reloaded isolate could
 * recover the script text from in this round's scope (no KV/D1 registry —
 * see README), so the script's full source text travels *inside* that
 * metadata, not in any module-scope cache. A module-scope `Map` would
 * silently break exactly the isolate-recycle case this round has to prove
 * survives.
 */
import {
  createDynamicWorkflowEntrypoint,
  DynamicWorkflowBinding,
  type WorkflowRunner,
  wrapWorkflowBinding,
} from "@cloudflare/dynamic-workflows";
import { parseInstanceIdPath, validateScriptRequestBody } from "./routing.js";

// Required re-export: the Cloudflare runtime needs this class on
// `cloudflare:workers` exports to build the wrapped per-script binding a
// Dynamic Worker uses (see the module doc comment above).
export { DynamicWorkflowBinding };

interface Env {
  LOADER: WorkerLoader;
  WORKFLOWS: Workflow;
  // Ordinary service binding to the Rust `cloud-ci-worker`
  // (`[[services]]` in wrangler.toml) — forwarded into the loaded
  // Dynamic Worker's own `env` below so a step can reach the real
  // `ContainerProbe` Durable Object.
  CONTAINER_WORKER: Fetcher;
}

/** Metadata persisted with every Workflow instance this host creates.
 * Must satisfy `DispatcherMetadata` (`Record<string, unknown>`) — the
 * library stores it opaquely. */
interface ScriptMetadata extends Record<string, unknown> {
  scriptId: string;
  source: string;
}

function loadScript(env: Env, metadata: ScriptMetadata) {
  return env.LOADER.get(metadata.scriptId, async () => ({
    compatibilityDate: "2026-10-02",
    mainModule: "index.js",
    modules: { "index.js": metadata.source },
    // docs/design/dynamic-pipelines.md's "Non-goals": "egress-blocked
    // `fetch`/`connect` both throw" — the prior spike's proven
    // isolation, carried into this real build. `null` blocks every
    // outbound `fetch`/`connect` the loaded script makes directly;
    // the service-binding `CONTAINER_WORKER` passed into its `env`
    // below is unaffected (bindings are not egress, they are
    // explicitly granted capabilities).
    globalOutbound: null,
    env: {
      WORKFLOWS: wrapWorkflowBinding(metadata),
      CONTAINER_WORKER: env.CONTAINER_WORKER,
    },
  }));
}

// Entrypoint name must match `class_name` in wrangler.toml's `[[workflows]]`.
// Reloads the right Dynamic Worker whenever the Workflows engine needs to
// run a step, including after an isolate recycle (see module doc comment).
//
// `getEntrypoint()` with no argument returns the script's *default* export
// — same entrypoint `POST /scripts` already calls `.fetch()` on below.
// `@cloudflare/dynamic-workflows`'s `WorkflowRunner` contract only requires
// a `run(event, step): Promise<R>` method (developers.cloudflare.com/
// dynamic-workers/usage/dynamic-workflows, checked 2026-10-02): nothing in
// that library's types or runtime requires the returned value to be a
// `WorkflowEntrypoint` subclass — `getEntrypoint()`'s return type is
// `Rpc.Stub<unknown>` reflecting the default export's own shape, and this
// cast to `WorkflowRunner` is the same structural assertion the previous
// `getEntrypoint("PipelineWorkflow")` line already made for the named
// export. A plain object satisfying `{ fetch, run }` (what
// `@cloud-ci/pipeline-sdk`'s `workflow()` returns, see
// `pipeline-script-sdk.js`) is therefore an acceptable default export,
// alongside the two raw-JS fixtures' `WorkflowEntrypoint` subclasses
// (`getEntrypoint("PipelineWorkflow")` would still work for those too,
// since they also declare a plain `export default { fetch }` — but this
// single `getEntrypoint()` call now serves every fixture uniformly,
// matching what `POST /scripts` already does for `fetch`).
export const DynamicWorkflow = createDynamicWorkflowEntrypoint<Env>(async ({ env, metadata }) => {
  const stub = loadScript(env, metadata as ScriptMetadata);
  return stub.getEntrypoint() as unknown as WorkflowRunner;
});

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // POST /scripts {"script": "<js source>", "params"?: object}
    // Loads `script` as a Dynamic Worker, then forwards to its own
    // default `fetch` handler — which is expected to call
    // `env.WORKFLOWS.create({ params })` and return `{ id }` — same
    // shape as the library's own documented example. Returns the
    // instance id for status polling below.
    if (request.method === "POST" && url.pathname === "/scripts") {
      const validated = validateScriptRequestBody(await request.json());
      if (!validated.ok) {
        return Response.json({ error: validated.error }, { status: 400 });
      }
      const metadata: ScriptMetadata = {
        scriptId: crypto.randomUUID(),
        source: validated.value.script,
      };
      const stub = loadScript(env, metadata);
      const tenantRequest = new Request("http://dynamic-worker.internal/", {
        method: "POST",
        body: JSON.stringify(validated.value.params ?? {}),
      });
      const tenantResponse = await stub.getEntrypoint().fetch(tenantRequest);
      if (!tenantResponse.ok) {
        return Response.json(
          { error: `script's fetch handler failed: ${await tenantResponse.text()}` },
          { status: 502 },
        );
      }
      const { id } = (await tenantResponse.json()) as { id: string };
      return Response.json({ instanceId: id, scriptId: metadata.scriptId });
    }

    // GET /instances/:id — instance lifecycle (status, completed step
    // output), via the real `WORKFLOWS` binding (unwrapped: "Workflow
    // IDs, .status(), .pause(), retries, hibernation, and durable
    // steps are unaffected by this architecture").
    const instanceId = request.method === "GET" ? parseInstanceIdPath(url.pathname) : null;
    if (instanceId !== null) {
      const instance = await env.WORKFLOWS.get(instanceId);
      const status = await instance.status();
      return Response.json(status);
    }

    return new Response("not found", { status: 404 });
  },
};
