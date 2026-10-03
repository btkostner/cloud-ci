import { describe, expect, it } from "vitest";
import type { CiContext } from "../src/context.js";
import type {
  ContainerExecutor,
  ContainerResult,
  ContainerStartRequest,
  WorkflowStepLike,
} from "../src/types.js";
import { workflow } from "../src/workflow.js";

/** Caches by step name, matching the real engine's `step.do` persistence
 * (see `test/container.test.ts`'s `FakeWorkflowStep` doc comment for the
 * full rationale) — needed here so the end-to-end replay test below can
 * prove the durability contract through the public `workflow()` surface,
 * not just through `runContainer` directly. */
class FakeWorkflowStep implements WorkflowStepLike {
  private readonly cache = new Map<string, unknown>();

  async do<T>(name: string, callback: () => Promise<T>): Promise<T> {
    if (this.cache.has(name)) {
      return this.cache.get(name) as T;
    }
    const result = await callback();
    this.cache.set(name, result);
    return result;
  }
}

class FakeContainerExecutor implements ContainerExecutor {
  callCount = 0;
  async start(_request: ContainerStartRequest): Promise<ContainerResult> {
    this.callCount += 1;
    return { ok: true, exitCode: 0, startedAt: 0, finishedAt: 1 };
  }
}

const samplePayload = {
  event: {
    kind: "pull_request",
    repo: "btkostner/cloud-ci",
    sha: "abc123",
    ref: "refs/pull/1/head",
  },
  changedFiles: ["src/index.ts"],
  branch: "feature/x",
  labels: [] as string[],
};

describe("workflow()", () => {
  it("exposes `on` unchanged from the options passed in", () => {
    const onTrigger = { pull_request: {} };
    const script = workflow({ on: onTrigger, async run() {} });
    expect(script.on).toBe(onTrigger);
  });

  it("fetch() creates a Workflow instance from the request body and returns its id", async () => {
    const script = workflow({ on: {}, async run() {} });
    const request = new Request("http://pipelines.internal/", {
      method: "POST",
      body: JSON.stringify(samplePayload),
    });
    let createdParams: unknown;
    const env = {
      WORKFLOWS: {
        async create(options: { params: unknown }) {
          createdParams = options.params;
          return { id: "instance-123" };
        },
      },
    };

    const response = await script.fetch(request, env);
    const body = (await response.json()) as { id: string };

    expect(body).toEqual({ id: "instance-123" });
    expect(createdParams).toEqual(samplePayload);
  });

  it("run() builds a CiContext from event.payload and passes it to the script's run function", async () => {
    let seenCi: CiContext | undefined;
    const script = workflow({
      on: {},
      async run(ci) {
        seenCi = ci;
      },
    });

    await script.run({ payload: samplePayload }, new FakeWorkflowStep());

    expect(seenCi?.event).toEqual(samplePayload.event);
    expect(seenCi?.changedFiles).toEqual(samplePayload.changedFiles);
    expect(seenCi?.branch).toBe(samplePayload.branch);
  });

  it("run() auto-seals every check the script left unsealed once run() returns", async () => {
    let checkName: string | undefined;
    const script = workflow({
      on: {},
      async run(ci) {
        const check = ci.check("ci/build", { required: true });
        checkName = check.name;
        expect(check.sealed).toBe(false);
      },
    });

    await script.run({ payload: samplePayload }, new FakeWorkflowStep());
    expect(checkName).toBe("ci/build");
  });

  it("run() wires an injected executor through to ci.container, proving the durability contract end-to-end through the public workflow() surface", async () => {
    const executor = new FakeContainerExecutor();
    const script = workflow(
      {
        on: {},
        async run(ci) {
          await ci.container("web#build", { run: "pnpm build" });
          await ci.container("web#build-again-replay", { run: "pnpm build" });
        },
      },
      { executor },
    );

    const step = new FakeWorkflowStep();
    await script.run({ payload: samplePayload }, step);
    expect(executor.callCount).toBe(2);

    // Re-running with the SAME step instance simulates an isolate-recycle
    // replay of the whole script: step.do's cache makes both container
    // calls resolve without touching the executor again.
    await script.run({ payload: samplePayload }, step);
    expect(executor.callCount).toBe(2);
  });
});
