import { describe, expect, it } from "vitest";
import { CheckRegistry } from "../src/check.js";
import { runContainer } from "../src/container.js";
import { ContainerExecutorNotConfiguredError, DuplicateContainerIdError } from "../src/errors.js";
import type {
  ContainerExecutor,
  ContainerResult,
  ContainerStartRequest,
  WorkflowStepLike,
} from "../src/types.js";

/**
 * In-memory stand-in for the real Workflows engine's `step.do` persistence
 * (`@cloudflare/dynamic-workflows`'s `WorkflowStepLike`, proven for real
 * against `wrangler dev` in `cloud-ci-dynamic-workflows-host`'s
 * isolate-recycle verification — see that package's README). Mirrors the
 * one property that matters for this contract: once a step name's callback
 * has resolved, a later `do()` call with the same name returns the cached
 * result without invoking the callback again — the exact behavior
 * `step1-plan`'s already-recorded output demonstrated surviving a forced
 * `wrangler dev` reload in that package's real verification. Reused across
 * two `runContainer` calls below to simulate an isolate-recycle replay
 * within one test.
 */
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

/** Fake `ContainerExecutor` — explicitly NOT the real `RunCoordinator` /
 * `ContainerProbe` chain (see README's scope boundary list). Counts calls
 * so tests can assert the executor is invoked at most once per id across a
 * simulated replay. */
class FakeContainerExecutor implements ContainerExecutor {
  callCount = 0;
  readonly requests: ContainerStartRequest[] = [];

  async start(request: ContainerStartRequest): Promise<ContainerResult> {
    this.callCount += 1;
    this.requests.push(request);
    return { ok: true, exitCode: 0, startedAt: 1000, finishedAt: 1001 };
  }
}

describe("ci.container step-durability contract", () => {
  it("returns the executor's result on first call", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const result = await runContainer({ step, executor, seenIds: new Set() }, "web#build", {
      run: "pnpm build",
    });
    expect(result).toEqual({ ok: true, exitCode: 0, startedAt: 1000, finishedAt: 1001 });
    expect(executor.callCount).toBe(1);
  });

  it("replaying the same step (same WorkflowStep instance, fresh script execution) returns the recorded result without re-invoking the executor", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();

    // First pass: the script runs top-to-bottom for the first time.
    const first = await runContainer({ step, executor, seenIds: new Set() }, "web#build", {
      run: "pnpm build",
    });

    // Second pass: simulates an isolate recycle — the script re-executes
    // from the top (fresh `seenIds`, a fresh `CiContext` in the real
    // system), but the Workflows engine hands back the *same* persisted
    // `step` (here: the same `FakeWorkflowStep`, carrying its cache over,
    // exactly like a real Workflow instance's completed steps survive a
    // forced reload per `cloud-ci-dynamic-workflows-host`'s README).
    const second = await runContainer({ step, executor, seenIds: new Set() }, "web#build", {
      run: "pnpm build",
    });

    expect(second).toEqual(first);
    // The actual durability proof: the executor's call count stays at 1
    // across both calls with the same id.
    expect(executor.callCount).toBe(1);
  });

  it("different ids each invoke the executor once", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();
    await runContainer({ step, executor, seenIds }, "web#build", { run: "pnpm build" });
    await runContainer({ step, executor, seenIds }, "web#test", { run: "pnpm test" });
    expect(executor.callCount).toBe(2);
    expect(executor.requests.map((r) => r.id)).toEqual(["web#build", "web#test"]);
  });

  it("rejects a duplicate id within one execution (same seenIds set)", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();
    await runContainer({ step, executor, seenIds }, "web#build", { run: "pnpm build" });
    await expect(
      runContainer({ step, executor, seenIds }, "web#build", { run: "pnpm build again" }),
    ).rejects.toThrow(DuplicateContainerIdError);
  });

  it("attaches the node to its check before the step runs", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    await runContainer({ step, executor, seenIds: new Set() }, "web#build", {
      run: "pnpm build",
      check,
    });
    expect(check.memberCount).toBe(1);
  });

  it("does not attach to any check when check is omitted or null", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    await runContainer({ step, executor, seenIds: new Set() }, "tf-plan", {
      run: "terraform plan",
      check: null,
    });
    expect(check.memberCount).toBe(0);
  });

  it("rejects attaching a node to an already-sealed check", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    check.seal();
    await expect(
      runContainer({ step, executor, seenIds: new Set() }, "web#build", {
        run: "pnpm build",
        check,
      }),
    ).rejects.toThrow("check is already sealed");
  });

  it("throws ContainerExecutorNotConfiguredError when no executor is injected", async () => {
    const step = new FakeWorkflowStep();
    await expect(
      runContainer({ step, executor: undefined, seenIds: new Set() }, "web#build", {
        run: "pnpm build",
      }),
    ).rejects.toThrow(ContainerExecutorNotConfiguredError);
  });
});
