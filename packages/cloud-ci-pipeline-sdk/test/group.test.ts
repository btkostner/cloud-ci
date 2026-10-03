import { describe, expect, it } from "vitest";
import { CheckRegistry } from "../src/check.js";
import { runContainer } from "../src/container.js";
import {
  ContainerExecutorNotConfiguredError,
  DuplicateContainerIdError,
  DuplicateGroupMemberError,
  EmptyGroupError,
} from "../src/errors.js";
import { runGroup } from "../src/group.js";
import { runShard } from "../src/shard.js";
import type {
  ContainerExecutor,
  ContainerResult,
  ContainerStartRequest,
  ShardPlan,
  ShardPlanner,
  ShardPlanRequest,
  WorkflowStepLike,
} from "../src/types.js";

/** Same `step.do` cache stand-in `container.test.ts`/`shard.test.ts` use —
 * reused here unmodified, since the contract it mirrors (`step.do`'s real
 * persistence) is identical regardless of which SDK function is under
 * test. */
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
 * `ContainerProbe` chain. Counts calls so tests can assert the executor is
 * invoked exactly once per `ci.group` call, no matter how many ids it
 * covers. */
class FakeContainerExecutor implements ContainerExecutor {
  callCount = 0;
  readonly requests: ContainerStartRequest[] = [];

  async start(request: ContainerStartRequest): Promise<ContainerResult> {
    this.callCount += 1;
    this.requests.push(request);
    return { ok: true, exitCode: 0, startedAt: 1000, finishedAt: 1001 };
  }
}

/** Minimal `ShardPlanner` fake — only needed for the
 * shared-id-namespace-with-`ci.shard` test below, which must prove a
 * grouped id collides with a shard id the same way it collides with a
 * plain container id. Round-robins a fixed `count`, same posture as
 * `shard.test.ts`'s own fake. */
class FakeShardPlanner implements ShardPlanner {
  async resolve(request: ShardPlanRequest): Promise<ShardPlan> {
    const count = typeof request.count === "number" ? request.count : 2;
    const files: string[][] = Array.from({ length: count }, () => []);
    request.filePaths.forEach((file, index) => {
      const bucket = files[index % count];
      if (bucket) {
        bucket.push(file);
      }
    });
    return { shardCount: count, files };
  }
}

describe("ci.group dispatch", () => {
  it("dispatches one executor call covering every id and returns that result for each id", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();

    const result = await runGroup({ step, executor, seenIds }, ["web#build", "api#build"], {
      run: "turbo run build --filter=web --filter=api",
    });

    expect(executor.callCount).toBe(1);
    expect(executor.requests[0]).toEqual({
      id: "web#build,api#build",
      runner: undefined,
      run: "turbo run build --filter=web --filter=api",
    });
    expect(result.results["web#build"]).toEqual({
      ok: true,
      exitCode: 0,
      startedAt: 1000,
      finishedAt: 1001,
    });
    expect(result.results["api#build"]).toBe(result.results["web#build"]);
  });

  it("passes runner through to the executor", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();

    await runGroup({ step, executor, seenIds }, ["a", "b"], {
      runner: "standard-2",
      run: "echo hi",
    });

    expect(executor.requests[0]?.runner).toBe("standard-2");
  });

  it("replaying the same ids (same WorkflowStep instance) returns the recorded result without re-invoking the executor", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();
    const opts = { run: "echo hi" };

    const first = await runGroup({ step, executor, seenIds }, ["a", "b"], opts);
    seenIds.clear();
    const second = await runGroup({ step, executor, seenIds }, ["a", "b"], opts);

    expect(executor.callCount).toBe(1);
    expect(second.results.a).toEqual(first.results.a);
  });

  it("rejects an empty ids array", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();

    await expect(runGroup({ step, executor, seenIds }, [], { run: "echo hi" })).rejects.toThrow(
      EmptyGroupError,
    );
    expect(executor.callCount).toBe(0);
  });

  it("rejects a duplicate id within one call's own ids", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();

    await expect(
      runGroup({ step, executor, seenIds }, ["a", "b", "a"], { run: "echo hi" }),
    ).rejects.toThrow(DuplicateGroupMemberError);
    expect(executor.callCount).toBe(0);
    expect(seenIds.size).toBe(0);
  });

  it("rejects a grouped id that collides with a previous ci.group call's id", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();

    await runGroup({ step, executor, seenIds }, ["a", "b"], { run: "echo hi" });

    await expect(
      runGroup({ step, executor, seenIds }, ["b", "c"], { run: "echo bye" }),
    ).rejects.toThrow(DuplicateContainerIdError);
    expect(executor.callCount).toBe(1);
  });

  it("rejects a grouped id that collides with a plain ci.container id, and vice versa", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();

    await runContainer({ step, executor, seenIds }, "shared-id", { run: "echo hi" });

    await expect(
      runGroup({ step, executor, seenIds }, ["shared-id", "other"], { run: "echo bye" }),
    ).rejects.toThrow(DuplicateContainerIdError);

    const seenIds2 = new Set<string>();
    await runGroup({ step, executor, seenIds: seenIds2 }, ["grouped-id", "other2"], {
      run: "echo hi",
    });
    await expect(
      runContainer({ step, executor, seenIds: seenIds2 }, "grouped-id", { run: "echo bye" }),
    ).rejects.toThrow(DuplicateContainerIdError);
  });

  it("rejects a grouped id that collides with a ci.shard-dispatched shard id, and vice versa", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const seenIds = new Set<string>();
    const seenShardIds = new Set<string>();

    await runShard(
      { step, executor, planner, registrar: undefined, seenIds, seenShardIds },
      "e2e",
      {
        split: "count",
        count: 2,
        files: ["a.spec.ts", "b.spec.ts"],
        run: () => "echo hi",
      },
    );

    // `runShard` dispatches containers with ids `` `${id}#${shardIndex}` ``,
    // i.e. "e2e#1" and "e2e#2" — collide a group with one of those exact ids.
    await expect(
      runGroup({ step, executor, seenIds }, ["e2e#1", "other"], { run: "echo bye" }),
    ).rejects.toThrow(DuplicateContainerIdError);
  });

  it("attaches every grouped id to its check before the step runs", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });

    await runGroup({ step, executor, seenIds }, ["web#build", "api#build"], {
      run: "turbo run build",
      check,
    });

    expect(check.memberCount).toBe(2);
  });

  it("does not attach to any check when check is omitted or null", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });

    await runGroup({ step, executor, seenIds }, ["a"], { run: "echo hi" });
    await runGroup({ step, executor, seenIds: new Set<string>() }, ["b"], {
      run: "echo hi",
      check: null,
    });

    expect(check.memberCount).toBe(0);
  });

  it("rejects attaching a grouped id to an already-sealed check", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const seenIds = new Set<string>();
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    check.seal();

    await expect(
      runGroup({ step, executor, seenIds }, ["a", "b"], { run: "echo hi", check }),
    ).rejects.toThrow();
    expect(executor.callCount).toBe(0);
  });

  it("throws ContainerExecutorNotConfiguredError when no executor is injected", async () => {
    const step = new FakeWorkflowStep();
    const seenIds = new Set<string>();

    await expect(
      runGroup({ step, executor: undefined, seenIds }, ["a", "b"], { run: "echo hi" }),
    ).rejects.toThrow(ContainerExecutorNotConfiguredError);
  });
});
