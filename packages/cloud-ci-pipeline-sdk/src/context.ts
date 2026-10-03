import { CheckRegistry } from "./check.js";
import { runContainer } from "./container.js";
import { runGroup } from "./group.js";
import { limit } from "./limit.js";
import { runShard } from "./shard.js";
import type {
  Check,
  CheckOptions,
  CiEvent,
  ContainerExecutor,
  ContainerOptions,
  ContainerResult,
  GroupOptions,
  GroupResult,
  ShardGroupRegistrar,
  ShardOptions,
  ShardPlanner,
  ShardResult,
  WorkflowStepLike,
} from "./types.js";

export interface CiContextOptions {
  readonly event: CiEvent;
  readonly changedFiles: readonly string[];
  readonly branch: string;
  readonly labels: readonly string[];
  readonly step: WorkflowStepLike;
  readonly executor: ContainerExecutor | undefined;
  readonly planner: ShardPlanner | undefined;
  readonly registrar: ShardGroupRegistrar | undefined;
}

/**
 * The `ci` object a script's `run(ci)` receives. Implements the subset of
 * `docs/design/dynamic-pipelines.md`'s full `ci` API this round builds:
 * `ci.event`, `ci.changedFiles`, `ci.check`, `ci.container`, `ci.shard`,
 * `ci.group`, `ci.limit`. Every other documented member (`ci.snapshot`,
 * `ci.skip`, `ci.cached`, `ci.turboCache`, `ci.readFile`) is intentionally
 * absent this round — see README's scope boundary list.
 */
export class CiContext {
  readonly event: CiEvent;
  readonly changedFiles: readonly string[];
  readonly branch: string;
  readonly labels: readonly string[];

  private readonly step: WorkflowStepLike;
  private readonly executor: ContainerExecutor | undefined;
  private readonly planner: ShardPlanner | undefined;
  private readonly registrar: ShardGroupRegistrar | undefined;
  private readonly containerIds = new Set<string>();
  private readonly shardIds = new Set<string>();
  readonly checks = new CheckRegistry();

  constructor(opts: CiContextOptions) {
    this.event = opts.event;
    this.changedFiles = opts.changedFiles;
    this.branch = opts.branch;
    this.labels = opts.labels;
    this.step = opts.step;
    this.executor = opts.executor;
    this.planner = opts.planner;
    this.registrar = opts.registrar;
  }

  check(name: string, opts: CheckOptions): Check {
    return this.checks.create(name, opts);
  }

  container(id: string, opts: ContainerOptions): Promise<ContainerResult> {
    return runContainer(
      { step: this.step, executor: this.executor, seenIds: this.containerIds },
      id,
      opts,
    );
  }

  shard(id: string, opts: ShardOptions): Promise<ShardResult> {
    return runShard(
      {
        step: this.step,
        executor: this.executor,
        planner: this.planner,
        registrar: this.registrar,
        seenIds: this.containerIds,
        seenShardIds: this.shardIds,
      },
      id,
      opts,
    );
  }

  group(ids: readonly string[], opts: GroupOptions): Promise<GroupResult> {
    return runGroup(
      { step: this.step, executor: this.executor, seenIds: this.containerIds },
      ids,
      opts,
    );
  }

  /** Backs `ci.limit(n, thunks)` — see `src/limit.ts`'s doc comment for
   * the full "ordering is by call, not completion" reasoning. */
  limit<T>(n: number, thunks: readonly (() => Promise<T>)[]): Promise<T[]> {
    return limit(n, thunks);
  }
}
