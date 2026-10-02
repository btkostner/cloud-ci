// Trivial, hardcoded 3-step sequential test script — NOT the real
// `@cloud-ci/pipeline-sdk` (doesn't exist yet) and NOT loaded from GitHub.
// Stands in for a real `.cloud-ci/pipelines/*.ts` file, per this package's
// scope boundary (see ../../README.md). Loaded as-is: this round's Worker
// Loader does not transpile TypeScript, so the fixture is plain JS, loaded
// verbatim by `POST /scripts`'s `script` field.
//
// The `step.sleep` between `step1-plan` and `step2-container-exec` exists
// only to give the isolate-recycle smoke test a real mid-execution window:
// trigger a `wrangler dev` hot-reload restart while the instance is
// asleep, between two completed/uncompleted steps, then confirm on resume
// that `step1-plan`'s already-recorded result is reused (not re-executed)
// rather than the instance restarting from scratch. `step2`'s real,
// observable side effect (a container process actually executes) is what
// makes "did this step re-run" distinguishable from the status API alone.
import { WorkflowEntrypoint } from "cloudflare:workers";

export class PipelineWorkflow extends WorkflowEntrypoint {
  async run(_event, step) {
    const planned = await step.do("step1-plan", async () => {
      return { planned: true, at: Date.now() };
    });

    await step.sleep("pause-for-recycle-window", "30 seconds");

    const containerResult = await step.do("step2-container-exec", async () => {
      const res = await this.env.CONTAINER_WORKER.fetch(
        "http://cloud-ci.internal/internal/container-probe/exec",
        {
          method: "POST",
          body: JSON.stringify({ cmd: ["echo", "hello from a real container"] }),
        },
      );
      if (!res.ok) {
        throw new Error(`container exec failed: ${res.status} ${await res.text()}`);
      }
      return await res.json();
    });

    return await step.do("step3-summarize", async () => {
      return {
        planned,
        containerExitCode: containerResult.exit_code,
        containerStdout: containerResult.stdout,
      };
    });
  }
}

// Required default export: the Worker Loader's `POST /scripts` forwards
// a request here, expecting this handler to call `env.WORKFLOWS.create()`
// (the wrapped binding `wrapWorkflowBinding` injected) and return the new
// instance's id — same shape as `@cloudflare/dynamic-workflows`'s own
// documented "Write the Dynamic Worker" example.
export default {
  async fetch(request, env) {
    const params = await request.json();
    const instance = await env.WORKFLOWS.create({ params });
    return Response.json({ id: await instance.id });
  },
};
