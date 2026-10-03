import { CheckRegistry } from "./check.js";
import { runContainer } from "./container.js";
import { runShard } from "./shard.js";
import type {
  Check,
  CheckOptions,
  CiEvent,
  ContainerExecutor,
  ContainerOptions,
  ContainerResult,
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
}

/**
 * The `ci` object a script's `run(ci)` receives. Implements the subset of
 * `docs/design/dynamic-pipelines.md`'s full `ci` API this round builds:
 * `ci.event`, `ci.changedFiles`, `ci.check`, `ci.container`, `ci.shard`.
 * Every other documented member (`ci.snapshot`, `ci.group`, `ci.limit`,
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
        seenIds: this.containerIds,
        seenShardIds: this.shardIds,
      },
      id,
      opts,
    );
  }
}
