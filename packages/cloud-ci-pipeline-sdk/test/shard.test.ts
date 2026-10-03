import { describe, expect, it } from "vitest";
import { CheckRegistry } from "../src/check.js";
import {
  DuplicateShardIdError,
  ShardGroupRegistrarNotConfiguredError,
  ShardPlannerNotConfiguredError,
} from "../src/errors.js";
import { runShard } from "../src/shard.js";
import type {
  ContainerExecutor,
  ContainerResult,
  ContainerStartRequest,
  ShardGroupRegisterRequest,
  ShardGroupRegisterResult,
  ShardGroupRegistrar,
  ShardPlan,
  ShardPlanner,
  ShardPlanRequest,
  WorkflowStepLike,
} from "../src/types.js";

/** Same `step.do` cache stand-in `container.test.ts`'s `FakeWorkflowStep`
 * uses — reused here unmodified rather than redefined, since the contract
 * it mirrors (`step.do`'s real persistence) is identical regardless of
 * which SDK function is under test. */
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

/** Fake `ContainerExecutor` — same role `container.test.ts`'s own fake
 * plays: explicitly NOT the real `RunCoordinator`/`ContainerProbe` chain. */
class FakeContainerExecutor implements ContainerExecutor {
  callCount = 0;
  readonly requests: ContainerStartRequest[] = [];

  async start(request: ContainerStartRequest): Promise<ContainerResult> {
    this.callCount += 1;
    this.requests.push(request);
    return { ok: true, exitCode: 0, startedAt: 1000, finishedAt: 1001 };
  }
}

/** Fake `ShardPlanner` — explicitly NOT the real `ResolveShardPlan` RPC
 * (see `ShardPlanner`'s doc comment in `types.ts`). Round-robins the given
 * file list into `count` shards (a fixed `count` only — this fake never
 * needs to model `{min,max,target}` auto-sizing or real `test_stats`
 * timing data, since `ci.shard`'s own dispatch logic is what this test
 * file proves, not the split algorithm itself — that is
 * `cloud_ci_core::split`'s job, proven by `cargo test` on the Rust side). */
class FakeShardPlanner implements ShardPlanner {
  callCount = 0;
  readonly requests: ShardPlanRequest[] = [];

  async resolve(request: ShardPlanRequest): Promise<ShardPlan> {
    this.callCount += 1;
    this.requests.push(request);
    const shardCount = typeof request.count === "number" ? request.count : request.count.max;
    const files: string[][] = Array.from({ length: shardCount }, () => []);
    request.filePaths.forEach((file, index) => {
      files[index % shardCount]?.push(file);
    });
    return { shardCount, files };
  }
}

/** Fake `ShardGroupRegistrar` — explicitly NOT the real `RegisterShardGroup`
 * RPC (see `ShardGroupRegistrar`'s doc comment in `types.ts`). Records
 * every call so tests can assert what `runShard` sent, same role
 * `FakeShardPlanner` plays for `ShardPlanner`. */
class FakeShardGroupRegistrar implements ShardGroupRegistrar {
  callCount = 0;
  readonly requests: ShardGroupRegisterRequest[] = [];

  async register(request: ShardGroupRegisterRequest): Promise<ShardGroupRegisterResult> {
    this.callCount += 1;
    this.requests.push(request);
    return { jobName: request.jobName };
  }
}

describe("ci.shard dispatch", () => {
  it("resolves the plan and dispatches one ci.container per shard", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();

    const result = await runShard(
      {
        step,
        executor,
        planner,
        registrar: undefined,
        seenIds: new Set(),
        seenShardIds: new Set(),
      },
      "e2e",
      {
        split: "file",
        count: 2,
        files: ["a.spec.ts", "b.spec.ts", "c.spec.ts", "d.spec.ts"],
        run: ({ shard, shards, files }) =>
          `playwright test ${files.join(" ")} --shard ${shard}/${shards}`,
      },
    );

    expect(result.shardCount).toBe(2);
    expect(result.results).toHaveLength(2);
    expect(executor.callCount).toBe(2);
    expect(executor.requests.map((r) => r.id)).toEqual(["e2e#1", "e2e#2"]);
    expect(executor.requests[0]?.run).toBe("playwright test a.spec.ts c.spec.ts --shard 1/2");
    expect(executor.requests[1]?.run).toBe("playwright test b.spec.ts d.spec.ts --shard 2/2");
  });

  it("passes split/count/files through to the planner unchanged", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();

    await runShard(
      {
        step,
        executor,
        planner,
        registrar: undefined,
        seenIds: new Set(),
        seenShardIds: new Set(),
      },
      "e2e",
      {
        split: "timing",
        count: { min: 2, max: 8, target: "5m" },
        files: ["a.spec.ts", "b.spec.ts"],
        run: ({ files }) => `playwright test ${files.join(" ")}`,
      },
    );

    expect(planner.callCount).toBe(1);
    expect(planner.requests[0]).toEqual({
      filePaths: ["a.spec.ts", "b.spec.ts"],
      strategy: "timing",
      count: { min: 2, max: 8, target: "5m" },
    });
  });

  it("replaying the same shard id (same step, fresh execution) reuses the recorded plan and does not re-dispatch containers", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const opts = {
      split: "file" as const,
      count: 2,
      files: ["a.spec.ts", "b.spec.ts"],
      run: ({ files }: { files: readonly string[] }) => `test ${files.join(" ")}`,
    };

    const first = await runShard(
      {
        step,
        executor,
        planner,
        registrar: undefined,
        seenIds: new Set(),
        seenShardIds: new Set(),
      },
      "e2e",
      opts,
    );
    // Simulated isolate-recycle replay: fresh seenIds/seenShardIds (fresh
    // CiContext), but the same persisted `step`.
    const second = await runShard(
      {
        step,
        executor,
        planner,
        registrar: undefined,
        seenIds: new Set(),
        seenShardIds: new Set(),
      },
      "e2e",
      opts,
    );

    expect(second).toEqual(first);
    expect(planner.callCount).toBe(1);
    expect(executor.callCount).toBe(2);
  });

  it("attaches every resolved shard to the given check", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const registry = new CheckRegistry();
    const check = registry.create("ci/e2e", { required: true });

    await runShard(
      {
        step,
        executor,
        planner,
        registrar: undefined,
        seenIds: new Set(),
        seenShardIds: new Set(),
      },
      "e2e",
      {
        split: "file",
        count: 3,
        files: ["a.spec.ts", "b.spec.ts", "c.spec.ts"],
        run: ({ files }) => `test ${files.join(" ")}`,
        check,
      },
    );

    expect(check.memberCount).toBe(3);
  });

  it("does not attach to any check when check is omitted or null", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const registry = new CheckRegistry();
    const check = registry.create("ci/e2e", { required: true });

    await runShard(
      {
        step,
        executor,
        planner,
        registrar: undefined,
        seenIds: new Set(),
        seenShardIds: new Set(),
      },
      "e2e",
      {
        split: "file",
        count: 2,
        files: ["a.spec.ts", "b.spec.ts"],
        run: ({ files }) => `test ${files.join(" ")}`,
        check: null,
      },
    );

    expect(check.memberCount).toBe(0);
  });

  it("rejects a duplicate shard id within one execution", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const seenShardIds = new Set<string>();
    const opts = {
      split: "file" as const,
      count: 1,
      files: ["a.spec.ts"],
      run: ({ files }: { files: readonly string[] }) => `test ${files.join(" ")}`,
    };

    await runShard(
      { step, executor, planner, registrar: undefined, seenIds: new Set(), seenShardIds },
      "e2e",
      opts,
    );
    await expect(
      runShard(
        { step, executor, planner, registrar: undefined, seenIds: new Set(), seenShardIds },
        "e2e",
        opts,
      ),
    ).rejects.toThrow(DuplicateShardIdError);
  });

  it("per-shard container ids collide with the shared ci.container id namespace", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const seenIds = new Set<string>(["e2e#1"]);

    await expect(
      runShard(
        { step, executor, planner, registrar: undefined, seenIds, seenShardIds: new Set() },
        "e2e",
        {
          split: "file",
          count: 1,
          files: ["a.spec.ts"],
          run: ({ files }) => `test ${files.join(" ")}`,
        },
      ),
    ).rejects.toThrow('container id "e2e#1" was already used in this run');
  });

  it("throws ShardPlannerNotConfiguredError when no planner is injected", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();

    await expect(
      runShard(
        {
          step,
          executor,
          planner: undefined,
          registrar: undefined,
          seenIds: new Set(),
          seenShardIds: new Set(),
        },
        "e2e",
        {
          split: "file",
          count: 1,
          files: ["a.spec.ts"],
          run: ({ files }) => `test ${files.join(" ")}`,
        },
      ),
    ).rejects.toThrow(ShardPlannerNotConfiguredError);
  });

  it("throws ShardGroupRegistrarNotConfiguredError when a non-empty reports option is given with no registrar", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();

    await expect(
      runShard(
        {
          step,
          executor,
          planner,
          registrar: undefined,
          seenIds: new Set(),
          seenShardIds: new Set(),
        },
        "e2e",
        {
          split: "file",
          count: 1,
          files: ["a.spec.ts"],
          run: ({ files }) => `test ${files.join(" ")}`,
          reports: [{ type: "junit" }],
        },
      ),
    ).rejects.toThrow(ShardGroupRegistrarNotConfiguredError);
    // Never reached the planner — rejected before the split step runs.
    expect(planner.callCount).toBe(0);
  });

  it("registers the shard group via the injected registrar when reports is non-empty", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const registrar = new FakeShardGroupRegistrar();

    await runShard(
      { step, executor, planner, registrar, seenIds: new Set(), seenShardIds: new Set() },
      "e2e",
      {
        split: "file",
        count: 2,
        files: ["a.spec.ts", "b.spec.ts"],
        run: ({ files }) => `test ${files.join(" ")}`,
        failFast: true,
        reports: [{ type: "junit" }],
      },
    );

    expect(registrar.callCount).toBe(1);
    expect(registrar.requests[0]).toEqual({
      jobName: "e2e",
      expectedTotal: 2,
      failFast: true,
      mergeOnFailure: "if_any_passed",
    });
  });

  it("defaults failFast to false when registering a shard group", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const registrar = new FakeShardGroupRegistrar();

    await runShard(
      { step, executor, planner, registrar, seenIds: new Set(), seenShardIds: new Set() },
      "e2e",
      {
        split: "file",
        count: 1,
        files: ["a.spec.ts"],
        run: ({ files }) => `test ${files.join(" ")}`,
        reports: [{ type: "junit" }],
      },
    );

    expect(registrar.requests[0]?.failFast).toBe(false);
  });

  it("accepts an empty or omitted reports option without registering anything (no-op, not rejected)", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const registrar = new FakeShardGroupRegistrar();

    await expect(
      runShard(
        { step, executor, planner, registrar, seenIds: new Set(), seenShardIds: new Set() },
        "e2e",
        {
          split: "file",
          count: 1,
          files: ["a.spec.ts"],
          run: ({ files }) => `test ${files.join(" ")}`,
          reports: [],
        },
      ),
    ).resolves.toBeDefined();
    expect(registrar.callCount).toBe(0);
  });

  it("does not register the shard id on a reports rejection, so a retry without reports succeeds", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const seenShardIds = new Set<string>();

    await expect(
      runShard(
        { step, executor, planner, registrar: undefined, seenIds: new Set(), seenShardIds },
        "e2e",
        {
          split: "file",
          count: 1,
          files: ["a.spec.ts"],
          run: ({ files }) => `test ${files.join(" ")}`,
          reports: [{ type: "junit" }],
        },
      ),
    ).rejects.toThrow(ShardGroupRegistrarNotConfiguredError);
    expect(seenShardIds.has("e2e")).toBe(false);

    // A caller that catches the rejection and retries the same id without
    // `reports` must succeed, not hit `DuplicateShardIdError`.
    await expect(
      runShard(
        { step, executor, planner, registrar: undefined, seenIds: new Set(), seenShardIds },
        "e2e",
        {
          split: "file",
          count: 1,
          files: ["a.spec.ts"],
          run: ({ files }) => `test ${files.join(" ")}`,
        },
      ),
    ).resolves.toBeDefined();
  });
});
