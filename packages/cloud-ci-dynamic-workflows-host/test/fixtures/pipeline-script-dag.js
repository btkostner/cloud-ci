// A genuine 3-node dependency graph (fan-out/fan-in), not the sequential
// chain `pipeline-script.js` tests — this is what the roadmap's Dynamic
// Workflows exit criterion's "a script runs 3 dependent containers" clause
// actually describes (see ../../README.md's "Explicit non-goals" entry
// this fixture closes).
//
// Shape: node A has no dependencies and runs first. Nodes B and C both
// depend on A and are *startable* concurrently once A completes (fan-out).
// A 4th "join" step waits on both B and C before finishing (fan-in).
// `step.do`'s own sequential-await semantics within `run()` are what
// gate B/C on A: B's and C's `step.do` calls are only issued after
// `await stepA` resolves below, so the Workflows engine cannot schedule
// them before A's result exists — no fixed-duration sleep stands in for
// the dependency the way it would in a hardcoded chain. Whether B and C
// then execute with genuinely overlapping wall-clock time (true
// concurrency) or get serialized anyway by the engine/container scheduler
// is an empirical question this fixture's output — not its code — answers:
// each node's container call records `Date.now()` before and after
// `exec()` for exactly that reason.
//
// Each of A/B/C calls a distinct `ContainerProbe` Durable Object instance
// (`?probe_id=node-a|node-b|node-c`, cloud-ci-worker's
// src/lib.rs `handle_container_probe_exec`) so the 3 containers are
// independent processes, not 3 execs serialized onto one container.
import { WorkflowEntrypoint } from "cloudflare:workers";

async function runNode(env, nodeId, echoText) {
  const startedAt = Date.now();
  const res = await env.CONTAINER_WORKER.fetch(
    `http://cloud-ci.internal/internal/container-probe/exec?probe_id=${nodeId}`,
    {
      method: "POST",
      body: JSON.stringify({ cmd: ["echo", echoText] }),
    },
  );
  if (!res.ok) {
    throw new Error(`${nodeId} container exec failed: ${res.status} ${await res.text()}`);
  }
  const result = await res.json();
  const finishedAt = Date.now();
  return { nodeId, startedAt, finishedAt, exitCode: result.exit_code, stdout: result.stdout };
}

export class PipelineWorkflow extends WorkflowEntrypoint {
  async run(_event, step) {
    // Node A: no dependencies.
    const nodeA = await step.do("node-a", async () => runNode(this.env, "node-a", "node-a"));

    // Fan-out: both `step.do` calls are issued (not merely declared)
    // only after `nodeA` above has resolved — there is no way for the
    // engine to start either step before this line executes, since the
    // `await` above blocks it. Unlike the sequential fixture, B and C are
    // *not* individually `await`ed here: both `step.do` promises are
    // created first, then joined with `Promise.all`, so the Workflows
    // engine is free to run them concurrently if it chooses to — an
    // `await stepB(); await stepC();` pair would force sequential
    // execution by construction regardless of what the engine supports.
    const nodeBPromise = step.do("node-b", async () => runNode(this.env, "node-b", "node-b"));
    const nodeCPromise = step.do("node-c", async () => runNode(this.env, "node-c", "node-c"));
    const [nodeB, nodeC] = await Promise.all([nodeBPromise, nodeCPromise]);

    // Fan-in: the join step waits on both B and C's already-resolved
    // results (both are in scope here because the `await`s above already
    // completed) and folds them into one final output.
    return await step.do("join", async () => {
      return {
        nodeA,
        nodeB,
        nodeC,
        overlapMs: Math.max(
          0,
          Math.min(nodeB.finishedAt, nodeC.finishedAt) - Math.max(nodeB.startedAt, nodeC.startedAt),
        ),
      };
    });
  }
}

// Required default export: same shape as pipeline-script.js's — the
// Worker Loader's `POST /scripts` forwards here, expecting
// `env.WORKFLOWS.create()` to be called and the new instance's id
// returned.
export default {
  async fetch(request, env) {
    const params = await request.json();
    const instance = await env.WORKFLOWS.create({ params });
    return Response.json({ id: await instance.id });
  },
};
