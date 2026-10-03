import { describe, expect, it } from "vitest";
import { CiContext } from "../src/context.js";
import { TurboPlanOutputMissingError } from "../src/errors.js";
import { graph } from "../src/graph.js";
import { checkPerTask, execute, plan } from "../src/turbo.js";
import type {
  ContainerExecutor,
  ContainerResult,
  ContainerStartRequest,
  WorkflowStepLike,
} from "../src/types.js";

/** Same `step.do` cache stand-in every other test file in this package
 * uses — see `test/container.test.ts`'s `FakeWorkflowStep` doc comment. */
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

/** Fake `ContainerExecutor` whose result (including `ok`/`stdout`) is
 * chosen per request by a caller-supplied function — needed here because,
 * unlike every other test file's fake, `turbo.plan`/`turbo.execute`'s
 * tests need different containers to report different outcomes (a
 * dry-run's stdout for the plan container, success/failure for different
 * task nodes). */
class FakeTurboExecutor implements ContainerExecutor {
  readonly requests: ContainerStartRequest[] = [];
  readonly startOrder: string[] = [];

  constructor(private readonly resultFor: (request: ContainerStartRequest) => ContainerResult) {}

  async start(request: ContainerStartRequest): Promise<ContainerResult> {
    this.requests.push(request);
    this.startOrder.push(request.id);
    return this.resultFor(request);
  }
}

function makeCi(executor: ContainerExecutor): CiContext {
  return new CiContext({
    event: { kind: "pull_request", repo: "owner/repo", sha: "deadbeef", ref: "refs/heads/feat" },
    changedFiles: [],
    branch: "main",
    labels: [],
    step: new FakeWorkflowStep(),
    executor,
    planner: undefined,
  });
}

const dryRunJson = JSON.stringify({
  tasks: [
    {
      taskId: "pkg-a#build",
      package: "pkg-a",
      task: "build",
      hash: "hash-a",
      dependencies: [],
      outputs: ["dist/**"],
    },
    {
      taskId: "pkg-b#build",
      package: "pkg-b",
      task: "build",
      hash: "hash-b",
      dependencies: ["pkg-a#build"],
      outputs: ["dist/**"],
    },
  ],
});

describe("turbo.plan", () => {
  it("runs turbo <tasks> --dry=json via ci.container and parses the result into a Graph", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
      stdout: dryRunJson,
    }));
    const ci = makeCi(executor);

    const result = await plan(ci, { tasks: ["build", "lint"] });

    expect(executor.requests).toHaveLength(1);
    expect(executor.requests[0]?.id).toBe("turbo-plan");
    expect(executor.requests[0]?.run).toBe("turbo run build lint --dry=json");
    expect(result.ids()).toEqual(["pkg-a#build", "pkg-b#build"]);
    expect(result.node("pkg-b#build")).toEqual({
      id: "pkg-b#build",
      package: "pkg-b",
      task: "build",
      hash: "hash-b",
      dependencies: ["pkg-a#build"],
      outputs: ["dist/**"],
    });
    expect(result.deps("pkg-b#build")).toEqual(["pkg-a#build"]);
  });

  it("appends --affected when opts.affected is true", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
      stdout: JSON.stringify({ tasks: [] }),
    }));
    const ci = makeCi(executor);

    await plan(ci, { tasks: ["build"], affected: true });

    expect(executor.requests[0]?.run).toBe("turbo run build --dry=json --affected");
  });

  it("uses a custom container id when opts.id is given", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
      stdout: JSON.stringify({ tasks: [] }),
    }));
    const ci = makeCi(executor);

    await plan(ci, { tasks: ["build"], id: "my-plan" });

    expect(executor.requests[0]?.id).toBe("my-plan");
  });

  it("throws TurboPlanOutputMissingError when the container result has no stdout", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
    }));
    const ci = makeCi(executor);

    await expect(plan(ci, { tasks: ["build"] })).rejects.toThrow(TurboPlanOutputMissingError);
  });
});

describe("turbo.execute", () => {
  function buildGraph() {
    return graph.fromJson([
      { id: "a", package: "pkg-a", task: "build", hash: "ha", dependencies: [], outputs: [] },
      { id: "b", package: "pkg-b", task: "build", hash: "hb", dependencies: ["a"], outputs: [] },
      { id: "c", package: "pkg-c", task: "build", hash: "hc", dependencies: ["a"], outputs: [] },
    ]);
  }

  it("dispatches dependency-ordered: dependents never start before their dependency's container resolves", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
    }));
    const ci = makeCi(executor);
    const g = buildGraph();

    const results = await execute(ci, g, {
      concurrency: 4,
      check: () => null,
    });

    expect(executor.requests.map((r) => r.id)).toEqual(expect.arrayContaining(["a", "b", "c"]));
    expect(executor.startOrder.indexOf("a")).toBeLessThan(executor.startOrder.indexOf("b"));
    expect(executor.startOrder.indexOf("a")).toBeLessThan(executor.startOrder.indexOf("c"));
    expect(results.get("a")?.status).toBe("succeeded");
    expect(results.get("b")?.status).toBe("succeeded");
    expect(results.get("c")?.status).toBe("succeeded");
  });

  it("skips dependents (transitively) when a dependency's container fails, without dispatching a container for them", async () => {
    const executor = new FakeTurboExecutor((request) => ({
      ok: request.id !== "a",
      exitCode: request.id === "a" ? 1 : 0,
      startedAt: 0,
      finishedAt: 1,
    }));
    const ci = makeCi(executor);
    const g = buildGraph();

    const results = await execute(ci, g, {
      concurrency: 4,
      check: () => null,
    });

    expect(results.get("a")?.status).toBe("failed");
    expect(results.get("b")?.status).toBe("skipped");
    expect(results.get("c")?.status).toBe("skipped");
    // Only "a" ever reached the container executor.
    expect(executor.requests.map((r) => r.id)).toEqual(["a"]);
  });

  it("skips a node whose cacheHit predicate resolves true, without dispatching a container for it", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
    }));
    const ci = makeCi(executor);
    const g = buildGraph();

    const results = await execute(ci, g, {
      concurrency: 4,
      check: () => null,
      cacheHit: async (node) => node.id === "a",
    });

    expect(results.get("a")?.status).toBe("cached");
    expect(results.get("b")?.status).toBe("succeeded");
    expect(executor.requests.map((r) => r.id)).toEqual(expect.arrayContaining(["b", "c"]));
    expect(executor.requests.map((r) => r.id)).not.toContain("a");
  });

  it("bounds concurrency via ci.limit: at most `concurrency` containers run at once", async () => {
    let inFlight = 0;
    let maxInFlight = 0;
    const gateByNode = new Map<string, PromiseWithResolvers<void>>();
    const g = graph.fromJson([
      { id: "x", package: "x", task: "build", hash: "hx", dependencies: [], outputs: [] },
      { id: "y", package: "y", task: "build", hash: "hy", dependencies: [], outputs: [] },
      { id: "z", package: "z", task: "build", hash: "hz", dependencies: [], outputs: [] },
    ]);

    class GatedExecutor implements ContainerExecutor {
      async start(request: ContainerStartRequest): Promise<ContainerResult> {
        inFlight += 1;
        maxInFlight = Math.max(maxInFlight, inFlight);
        const gate = Promise.withResolvers<void>();
        gateByNode.set(request.id, gate);
        await gate.promise;
        inFlight -= 1;
        return { ok: true, exitCode: 0, startedAt: 0, finishedAt: 1 };
      }
    }

    const ci = makeCi(new GatedExecutor());
    const resultPromise = execute(ci, g, { concurrency: 2, check: () => null });

    // Poll via microtask ticks (no wall-clock wait) until the two
    // concurrency slots have each claimed and started a node.
    for (let tick = 0; tick < 50 && gateByNode.size < 2; tick++) {
      await Promise.resolve();
    }
    expect(gateByNode.size).toBe(2);

    for (const gate of gateByNode.values()) {
      gate.resolve();
    }

    // Resolving those frees a slot for the third node; poll again until it
    // has started and resolve its gate too, or `resultPromise` would hang.
    for (let tick = 0; tick < 50 && gateByNode.size < 3; tick++) {
      await Promise.resolve();
    }
    for (const gate of gateByNode.values()) {
      gate.resolve();
    }

    await resultPromise;
    expect(maxInFlight).toBeLessThanOrEqual(2);
  });
});

describe("turbo.checkPerTask", () => {
  function buildGraph() {
    return graph.fromJson([
      {
        id: "pkg-a#test",
        package: "pkg-a",
        task: "test",
        hash: "ha",
        dependencies: [],
        outputs: [],
      },
      {
        id: "pkg-a#build",
        package: "pkg-a",
        task: "build",
        hash: "hab",
        dependencies: [],
        outputs: [],
      },
      {
        id: "pkg-b#test",
        package: "pkg-b",
        task: "test",
        hash: "hb",
        dependencies: [],
        outputs: [],
      },
      {
        id: "pkg-b#test2",
        package: "pkg-b",
        task: "test",
        hash: "hb2",
        dependencies: [],
        outputs: [],
      },
    ]);
  }

  it("creates one ci.check per distinct package among nodes matching task, memoized", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
    }));
    const ci = makeCi(executor);
    const g = buildGraph();

    const lookup = checkPerTask(ci, g, {
      task: "test",
      name: ({ package: pkg }) => `${pkg}#test`,
      required: true,
    });

    // Two nodes share package "pkg-b" and task "test" — only one check for
    // "pkg-b" should have been created.
    expect(
      ci.checks
        .all()
        .map((c) => c.name)
        .sort(),
    ).toEqual(["pkg-a#test", "pkg-b#test"]);
    expect(ci.checks.all()).toHaveLength(2);

    const checkA = lookup(g.node("pkg-a#test"));
    const checkB1 = lookup(g.node("pkg-b#test"));
    const checkB2 = lookup(g.node("pkg-b#test2"));

    expect(checkA?.name).toBe("pkg-a#test");
    expect(checkB1?.name).toBe("pkg-b#test");
    expect(checkB1).toBe(checkB2);
  });

  it("returns null for a node whose task does not match", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
    }));
    const ci = makeCi(executor);
    const g = buildGraph();

    const lookup = checkPerTask(ci, g, {
      task: "test",
      name: ({ package: pkg }) => `${pkg}#test`,
      required: true,
    });

    expect(lookup(g.node("pkg-a#build"))).toBeNull();
  });

  it("supports a custom grouping key instead of the package default", async () => {
    const executor = new FakeTurboExecutor(() => ({
      ok: true,
      exitCode: 0,
      startedAt: 0,
      finishedAt: 1,
    }));
    const ci = makeCi(executor);
    const g = buildGraph();

    const lookup = checkPerTask(ci, g, {
      task: "test",
      name: () => "every-test",
      required: true,
      key: () => "single-group",
    });

    expect(ci.checks.all()).toHaveLength(1);
    expect(lookup(g.node("pkg-a#test"))?.name).toBe("every-test");
    expect(lookup(g.node("pkg-b#test"))?.name).toBe("every-test");
  });
});
