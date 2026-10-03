/**
 * `@cloud-ci/pipeline-sdk/turbo` — a separate entry point, not an export of
 * the core package. `docs/design/dynamic-pipelines.md`'s "Graph helpers"
 * section is explicit: "`turbo` and `mise` are separate entry points
 * (`@cloud-ci/pipeline-sdk/turbo`, `@cloud-ci/pipeline-sdk/mise`), not
 * exports of the core package. A script that only needs
 * `graph.fromJson`/`graph.fromGraph` and the generic `ci` API imports just
 * `@cloud-ci/pipeline-sdk` and does not bundle turbo- or mise-specific
 * code" (`:383-386`). See this package's `package.json` `exports` map and
 * README for how that separation is wired.
 */
import type { CiContext } from "./context.js";
import { TurboPlanOutputMissingError } from "./errors.js";
import { type Graph, type GraphNode, graph } from "./graph.js";
import type { Check, ContainerResult } from "./types.js";

/** One task object inside `turbo run --dry=json`'s `tasks` array. Field
 * names and meanings per turborepo.dev/docs/reference/run's `--dry / --dry-run`
 * section (checked 2026-10-02) — that page's own field table lists
 * `taskId`, `task`, `package`, `hash`, `hashOfExternalDependencies`,
 * `command`, `inputs`, `outputs`, `dependencies`, `dependents`,
 * `environmentVariables` as (its own words) "non-exhaustive". This type
 * only declares the subset `turbo.plan` actually reads — the fields the
 * design doc's "Graph helpers" table (`:373`) names: `taskId`, `package`,
 * `task`, `hash`, `outputs`, `dependencies`. */
export interface TurboDryRunTask {
  readonly taskId: string;
  readonly package: string;
  readonly task: string;
  readonly hash: string;
  readonly dependencies: readonly string[];
  readonly outputs: readonly string[];
}

/** Top-level shape of `turbo run --dry=json`'s stdout: a `tasks` array of
 * `TurboDryRunTask`, alongside other top-level fields (`turbo.plan` does
 * not read them, so they are not declared here). */
export interface TurboDryRunOutput {
  readonly tasks: readonly TurboDryRunTask[];
}

/** Options accepted by `turbo.plan(ci, opts)`. The design doc's worked
 * example (`:87-91`) also passes `snapshot: deps` — `ContainerOptions` has
 * no `snapshot` field this round (see core package README's scope boundary
 * list), so `turbo.plan` does not accept one either: it would have nowhere
 * to forward it. `id` defaults to `"turbo-plan"`, matching the design
 * doc's "Execution model" sequence diagram's own step name
 * (`step.do("container:turbo-plan")`, `:294`). */
export interface TurboPlanOptions {
  readonly tasks: readonly string[];
  /** Forwarded as `turbo run <tasks> --affected` when true — per
   * turborepo.dev/docs/reference/run's `--affected` section (checked
   * 2026-10-02): "Filter to only packages that are affected by changes on
   * the current branch." */
  readonly affected?: boolean;
  readonly id?: string;
  readonly runner?: string;
  readonly check?: Check | null;
}

/**
 * Runs `turbo run <tasks> --dry=json` in one `ci.container` call and
 * parses its stdout into cloud-ci's generic `Graph` shape — per the design
 * doc's "Graph helpers" table (`:373`): "Runs `turbo run <tasks>
 * --dry=json` in a container; returns a graph of `taskId`, `package`,
 * `task`, `hash`, `outputs`, `dependencies`." Does not reimplement any
 * parsing logic beyond field renaming — `graph.fromGraph` (`src/graph.ts`)
 * is the actual node-shape builder and id/dependency validator this
 * delegates to, same as any other tool integration would.
 */
export async function plan(ci: CiContext, opts: TurboPlanOptions): Promise<Graph> {
  const id = opts.id ?? "turbo-plan";
  const flags = ["run", ...opts.tasks, "--dry=json"];
  if (opts.affected) {
    flags.push("--affected");
  }

  const result: ContainerResult = await ci.container(id, {
    runner: opts.runner,
    run: `turbo ${flags.join(" ")}`,
    check: opts.check,
  });

  if (result.stdout === undefined) {
    throw new TurboPlanOutputMissingError(id);
  }

  const dryRun = JSON.parse(result.stdout) as TurboDryRunOutput;

  return graph.fromGraph(
    dryRun.tasks,
    (task): GraphNode => ({
      id: task.taskId,
      package: task.package,
      task: task.task,
      hash: task.hash,
      dependencies: task.dependencies,
      outputs: task.outputs,
    }),
  );
}

/** A `turbo.execute` node's terminal status. `"skipped"` covers both "a
 * dependency failed" and "a dependency itself skipped" (skip propagates
 * transitively, same as the design doc's own `execute` sketch:
 * `if (deps.some((d) => !d.ok)) return ci.skip(id, "dependency failed")`,
 * `:145`). `"cached"` is reported only when `opts.cacheHit` (see
 * `TurboExecuteOptions`) resolves `true` for that node — this round has no
 * real `ci.turboCache` binding to check instead (see "`ci.limit`,
 * `ci.skip`, `ci.cached`, `ci.turboCache`, `ci.readFile`" in the core
 * package's README scope boundary list). */
export type TurboNodeStatus = "succeeded" | "failed" | "skipped" | "cached";

/** Result of one `turbo.execute`-dispatched node. `container` is present
 * only for `"succeeded"`/`"failed"` nodes — a `"skipped"`/`"cached"` node
 * never reached `ci.container`. */
export interface TurboNodeResult {
  readonly id: string;
  readonly status: TurboNodeStatus;
  readonly container?: ContainerResult;
}

/** Options accepted by `turbo.execute(ci, graph, opts)`. The design doc's
 * sketch (`:139-158`) reads `ci.skip`/`ci.turboCache.has`/`ci.cached` —
 * none of which exist on `CiContext` this round (see core package README).
 * `cacheHit` is this round's injectable stand-in for
 * `ci.turboCache.has(hash)`: an optional async predicate the caller
 * supplies; omitted means every node actually runs (same "fail toward
 * doing real work, not toward silently skipping" posture the rest of this
 * package takes for an unconfigured injectable — compare
 * `ContainerExecutorNotConfiguredError`, which is a hard error here only
 * because there is no sensible default for "never cache-skip" the way
 * there is for "never cache-skip" being the default itself). */
export interface TurboExecuteOptions {
  readonly concurrency: number;
  readonly runner?: (node: GraphNode) => string | undefined;
  readonly check: (node: GraphNode) => Check | null;
  readonly cacheHit?: (node: GraphNode) => Promise<boolean>;
}

/**
 * Dependency-ordered fan-out over `graph` with cache-hit skipping and
 * bounded concurrency — per the design doc's "Graph helpers" table
 * (`:374`). Structurally the same memoized-recursion shape as the design
 * doc's own `execute` sketch (`:139-158`): a `Map<string, Promise<...>>`
 * keyed by node id so each node's dependents all await the *same* promise
 * rather than re-running it, dependencies resolved via `Promise.all` before
 * a node's own work starts, and the whole dispatch bounded by `ci.limit`
 * (`src/limit.ts`) the same way the sketch's own last line does:
 * `await ci.limit(opts.concurrency, graph.ids().map((id) => () => runNode(id)))`.
 * Differs from the sketch only where this round's `CiContext` genuinely
 * lacks the member the sketch calls (`ci.skip`/`ci.cached`/
 * `ci.turboCache.has` — see `TurboExecuteOptions`'s doc comment): dependency
 * failure/skip propagation and cache-hit skipping are inlined here instead
 * of delegated to those missing methods, with the exact same status
 * outcomes (`"skipped"`, `"cached"`) the sketch's calls would have reported.
 */
export async function execute(
  ci: CiContext,
  graphValue: Graph,
  opts: TurboExecuteOptions,
): Promise<ReadonlyMap<string, TurboNodeResult>> {
  const done = new Map<string, Promise<TurboNodeResult>>();

  function runNode(id: string): Promise<TurboNodeResult> {
    const existing = done.get(id);
    if (existing) {
      return existing;
    }

    const promise = (async (): Promise<TurboNodeResult> => {
      const node = graphValue.node(id);
      const depResults = await Promise.all(graphValue.deps(id).map(runNode));
      if (depResults.some((dep) => dep.status === "failed" || dep.status === "skipped")) {
        return { id, status: "skipped" };
      }

      if (opts.cacheHit && (await opts.cacheHit(node))) {
        return { id, status: "cached" };
      }

      const container = await ci.container(id, {
        runner: opts.runner?.(node),
        run: `turbo run ${node.task} --filter=${node.package}`,
        check: opts.check(node),
      });
      return { id, status: container.ok ? "succeeded" : "failed", container };
    })();

    done.set(id, promise);
    return promise;
  }

  const results = await ci.limit(
    opts.concurrency,
    graphValue.ids().map((id) => () => runNode(id)),
  );

  return new Map(results.map((result) => [result.id, result]));
}

/** Options accepted by `turbo.checkPerTask(ci, graph, opts)`, matching the
 * design doc's "One check per package" worked example (`:169-173`)
 * exactly: `task`, `name`, `required`. `key` is this package's own
 * addition for the "default `package`" half of the Graph helpers table's
 * description (`:375`: "one `ci.check` per distinct value of a grouping
 * key (default `package`)") — the worked example only ever groups by
 * package, so `key` defaults to `(node) => node.package` and the worked
 * example never needs to pass it. */
export interface CheckPerTaskOptions {
  readonly task: string;
  readonly name: (node: GraphNode) => string;
  readonly required: boolean;
  readonly key?: (node: GraphNode) => string;
}

/**
 * Creates and memoizes one `ci.check` per distinct grouping-key value
 * among `graph`'s nodes matching `opts.task`, and returns a lookup
 * function for `turbo.execute`'s `check` callback — per the design doc's
 * "One check per package" section (`:182-189`): "`turbo.checkPerTask`
 * walks `graph` for nodes matching `task`, creates one `ci.check` per
 * distinct package the first time it sees that package ..., and returns a
 * lookup function that `turbo.execute`'s `check` callback uses to map each
 * node to its package's check."
 *
 * **The one claim this round does not implement, and why:** that same
 * section also says `turbo.checkPerTask` "can call `check.seal()` on each
 * check after it attaches that package's nodes, so a package check need
 * not wait for the whole script to finish." This function does not call
 * `seal()` itself. `checkPerTask` runs synchronously, before
 * `turbo.execute` has dispatched anything — at the point it would need to
 * seal a check, none of that check's nodes have been attached yet
 * (attachment happens inside `ci.container`'s own `check.attach(id)` call,
 * triggered later by `turbo.execute`'s dispatch, per `src/container.ts`).
 * Sealing here, before dispatch, would make every one of those later
 * `ci.container` calls throw `CheckSealedError` the instant it tried to
 * attach. Sealing *after* dispatch would require `checkPerTask` to
 * observe when the last node of a group has actually been attached — a
 * second, parallel completion-tracking mechanism this package does not
 * build, matching `ci.group`'s own precedent (see core package README's
 * "`ci.group`: one container, several node ids" section) of taking the
 * narrowest reading an ambiguous doc sentence supports rather than
 * inventing an unstated mechanism. The capability the doc describes is
 * already available without it: `CheckRegistry.sealRemaining()`
 * (`src/check.ts`, exercised by `workflow()`) already seals every
 * still-open check once the script's `run` function finishes scheduling —
 * the exact "automatic" sealing rule the design doc's own "Check sealing"
 * section (`:482-484`) names as the default for "a check sized once a full
 * plan is known, e.g. `turbo.execute`'s `check` mapping over an
 * already-resolved `graph`." A package check created by `checkPerTask`
 * seals through that existing, already-tested rule, not a new one.
 */
export function checkPerTask(
  ci: CiContext,
  graphValue: Graph,
  opts: CheckPerTaskOptions,
): (node: GraphNode) => Check | null {
  const keyOf = opts.key ?? ((node: GraphNode): string => node.package);
  const checksByKey = new Map<string, Check>();

  for (const id of graphValue.ids()) {
    const node = graphValue.node(id);
    if (node.task !== opts.task) {
      continue;
    }
    const key = keyOf(node);
    if (!checksByKey.has(key)) {
      checksByKey.set(key, ci.check(opts.name(node), { required: opts.required }));
    }
  }

  return (node: GraphNode): Check | null => {
    if (node.task !== opts.task) {
      return null;
    }
    return checksByKey.get(keyOf(node)) ?? null;
  };
}

export const turbo = { plan, execute, checkPerTask };
