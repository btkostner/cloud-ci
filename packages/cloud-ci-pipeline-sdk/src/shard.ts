import { runContainer } from "./container.js";
import {
  DuplicateShardIdError,
  ShardPlannerNotConfiguredError,
  ShardReportsNotSupportedError,
} from "./errors.js";
import type {
  Check,
  ContainerExecutor,
  ContainerResult,
  ShardOptions,
  ShardPlan,
  ShardPlanner,
  ShardResult,
  WorkflowStepLike,
} from "./types.js";

/**
 * Backs `ci.shard(id, opts)`. Per `docs/design/dynamic-pipelines.md`'s
 * "### Splitting tests across shards": "`ci.shard` resolves the shard
 * count and per-shard file assignment ..., starts one `ci.container` per
 * shard ..." — this function does exactly that, in two durable phases:
 *
 * 1. `step.do("split:" + id, ...)` resolves the plan via the injected
 *    `ShardPlanner` (`cloud_ci_core::split`'s real algorithm; the caller
 *    injects either the test-only `FakeShardPlanner` or the real
 *    `RpcShardPlanner`, which reaches `ResolveShardPlan` over an injected
 *    `ShardPlanFetcher` — see `ShardPlanner`'s doc comment).
 *    Wrapping this in `step.do` matches parallelization.md's "###
 *    Deterministic assignment, end to end" step 2 exactly: "For managed
 *    runs, `RunCoordinator` computes the assignment once ... when
 *    `ci.shard`'s `step.do("split:" + id)` runs, and stores it as
 *    `shard_plan` rows before dispatching any shard container" — so a
 *    replay after the plan is resolved reuses the recorded plan instead of
 *    re-resolving it (and, per step 4 of that same section, never changes
 *    the file assignment of a shard that has already started).
 * 2. One `runContainer` call per resolved shard, id `` `${id}#${shardIndex}`
 *    `` (1-based), each wrapping its own `step.do("container:" + ...)` —
 *    `runContainer`'s own proven step-durability contract (`container.ts`'s
 *    doc comment), not a second, parallel mechanism. Every shard's
 *    container id lives in the same `seenIds` set `ci.container` itself
 *    uses, so a shard id and a plain container id can never collide.
 *
 * Per "### Check status and sealing (`ci.shard`)": "Resolving the shard
 * count attaches all shards to the check" — each per-shard `runContainer`
 * call attaches its own id to `opts.check` (if given) before that shard's
 * container step runs, same as a plain `ci.container` call; `ci.shard`
 * itself never seals the check (sealing is `workflow()`'s job, via
 * `CheckRegistry.sealRemaining()`).
 *
 * Dispatches shards concurrently (`Promise.all`) rather than sequentially —
 * nothing in the design doc requires shard containers to start one at a
 * time, and dispatching concurrently is what lets several shards actually
 * run in parallel.
 *
 * `opts.reports` is rejected with `ShardReportsNotSupportedError` rather
 * than silently ignored: there is no public RPC this function could call
 * to wire it to the real server-side merge barrier yet — see
 * `ShardOptions.reports`'s doc comment (`types.ts`) for exactly what proto
 * addition is missing.
 */
export async function runShard(
  deps: {
    readonly step: WorkflowStepLike;
    readonly executor: ContainerExecutor | undefined;
    readonly planner: ShardPlanner | undefined;
    readonly seenIds: Set<string>;
    readonly seenShardIds: Set<string>;
  },
  id: string,
  opts: ShardOptions,
): Promise<ShardResult> {
  if (deps.seenShardIds.has(id)) {
    throw new DuplicateShardIdError(id);
  }

  if (opts.reports !== undefined && opts.reports.length > 0) {
    throw new ShardReportsNotSupportedError(id);
  }

  deps.seenShardIds.add(id);

  const check: Check | null = opts.check ?? null;

  const plan: ShardPlan = await deps.step.do(`split:${id}`, async () => {
    if (!deps.planner) {
      throw new ShardPlannerNotConfiguredError(id);
    }
    return deps.planner.resolve({
      filePaths: opts.files,
      strategy: opts.split,
      count: opts.count,
    });
  });

  const results: ContainerResult[] = await Promise.all(
    plan.files.map((files, index) => {
      const shard = index + 1;
      const run = opts.run({ shard, shards: plan.shardCount, files });
      return runContainer(
        { step: deps.step, executor: deps.executor, seenIds: deps.seenIds },
        `${id}#${shard}`,
        { run, check },
      );
    }),
  );

  return { shardCount: plan.shardCount, results };
}
