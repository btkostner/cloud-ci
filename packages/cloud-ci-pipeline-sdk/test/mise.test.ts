import { describe, expect, it } from "vitest";
import { CiContext } from "../src/context.js";
import { MisePlanNotImplementedError } from "../src/errors.js";
import type { GraphNode } from "../src/graph.js";
import { mise, plan } from "../src/mise.js";
import type { ContainerExecutor, ContainerResult, ContainerStartRequest } from "../src/types.js";

class FakeContainerExecutor implements ContainerExecutor {
  callCount = 0;

  async start(_request: ContainerStartRequest): Promise<ContainerResult> {
    this.callCount += 1;
    return { ok: true, exitCode: 0, startedAt: 0, finishedAt: 1 };
  }
}

function makeCi(executor: ContainerExecutor): CiContext {
  return new CiContext({
    event: { kind: "pull_request", repo: "owner/repo", sha: "deadbeef", ref: "refs/heads/feat" },
    changedFiles: [],
    branch: "main",
    labels: [],
    step: { do: (_name, callback) => callback() },
    executor,
    planner: undefined,
    registrar: undefined,
  });
}

/**
 * This round's `mise.plan` has no real mise CLI contract to run against
 * (see `src/mise.ts`'s doc comment for the full, dated investigation: no
 * documented `--json` task-dependency-graph command exists, and the one
 * real `--json` command this package confirmed — `mise tasks graph
 * --json` — returns an undocumented `projects` shape, not turbo-equivalent
 * per-task nodes). These tests therefore only cover what is actually
 * implemented: the `MisePlanNotImplementedError` fail-loud path when no
 * `source` is supplied, and the `graph.fromGraph`-delegating path when a
 * caller supplies their own `source`. They do NOT pin any specific mise
 * CLI invocation or JSON shape as if it were verified.
 */
describe("mise.plan", () => {
  it("throws MisePlanNotImplementedError when no source is supplied, and never touches ci.container", async () => {
    const executor = new FakeContainerExecutor();
    const ci = makeCi(executor);

    await expect(plan(ci, { tasks: ["build"] })).rejects.toThrow(MisePlanNotImplementedError);
    expect(executor.callCount).toBe(0);
  });

  it("builds a graph from a caller-supplied source via graph.fromGraph, without invoking ci.container", async () => {
    interface RawMiseTask {
      readonly name: string;
      readonly deps: readonly string[];
    }

    const executor = new FakeContainerExecutor();
    const ci = makeCi(executor);

    const result = await plan<RawMiseTask>(ci, {
      tasks: ["build"],
      source: {
        rawNodes: async () => [
          { name: "lib#build", deps: [] },
          { name: "app#build", deps: ["lib#build"] },
        ],
        mapFn: (raw): GraphNode => ({
          id: raw.name,
          package: raw.name.split("#")[0] ?? raw.name,
          task: "build",
          hash: "unverified",
          dependencies: raw.deps,
          outputs: [],
        }),
      },
    });

    expect(result.ids()).toEqual(["lib#build", "app#build"]);
    expect(result.deps("app#build")).toEqual(["lib#build"]);
    expect(executor.callCount).toBe(0);
  });

  it("is exposed via the mise namespace export", () => {
    expect(mise.plan).toBe(plan);
  });
});
