import { describe, expect, it } from "vitest";
import { CheckRegistry } from "../src/check.js";
import { DuplicateShardIdError, ShardPlannerNotConfiguredError } from "../src/errors.js";
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

describe("ci.shard dispatch", () => {
  it("resolves the plan and dispatches one ci.container per shard", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();

    const result = await runShard(
      { step, executor, planner, seenIds: new Set(), seenShardIds: new Set() },
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
      { step, executor, planner, seenIds: new Set(), seenShardIds: new Set() },
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
      { step, executor, planner, seenIds: new Set(), seenShardIds: new Set() },
      "e2e",
      opts,
    );
    // Simulated isolate-recycle replay: fresh seenIds/seenShardIds (fresh
    // CiContext), but the same persisted `step`.
    const second = await runShard(
      { step, executor, planner, seenIds: new Set(), seenShardIds: new Set() },
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
      { step, executor, planner, seenIds: new Set(), seenShardIds: new Set() },
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
      { step, executor, planner, seenIds: new Set(), seenShardIds: new Set() },
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

    await runShard({ step, executor, planner, seenIds: new Set(), seenShardIds }, "e2e", opts);
    await expect(
      runShard({ step, executor, planner, seenIds: new Set(), seenShardIds }, "e2e", opts),
    ).rejects.toThrow(DuplicateShardIdError);
  });

  it("per-shard container ids collide with the shared ci.container id namespace", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();
    const planner = new FakeShardPlanner();
    const seenIds = new Set<string>(["e2e#1"]);

    await expect(
      runShard({ step, executor, planner, seenIds, seenShardIds: new Set() }, "e2e", {
        split: "file",
        count: 1,
        files: ["a.spec.ts"],
        run: ({ files }) => `test ${files.join(" ")}`,
      }),
    ).rejects.toThrow('container id "e2e#1" was already used in this run');
  });

  it("throws ShardPlannerNotConfiguredError when no planner is injected", async () => {
    const step = new FakeWorkflowStep();
    const executor = new FakeContainerExecutor();

    await expect(
      runShard(
        { step, executor, planner: undefined, seenIds: new Set(), seenShardIds: new Set() },
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
});
