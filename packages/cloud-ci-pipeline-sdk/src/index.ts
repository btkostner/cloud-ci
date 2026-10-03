/**
 * `@cloud-ci/pipeline-sdk` — public entry point.
 *
 * Implements the subset of `docs/design/dynamic-pipelines.md`'s `ci` API
 * this round builds: `workflow()`, `ci.check`, `ci.container`, `ci.shard`,
 * `ci.group`, `ci.limit`, `graph.fromJson`/`graph.fromGraph`. See this
 * package's README for the full, explicit scope boundary (what is and is
 * not implemented this round) before relying on anything not re-exported
 * here. `@cloud-ci/pipeline-sdk/turbo` and `@cloud-ci/pipeline-sdk/mise`
 * are separate entry points (`./turbo.js`, `./mise.js`), not re-exported
 * from here — see those files' own doc comments.
 */
export { CheckRegistry } from "./check.js";
export { CiContext } from "./context.js";
export {
  CheckAlreadySealedError,
  CheckSealedError,
  ContainerExecutorNotConfiguredError,
  DuplicateCheckNameError,
  DuplicateContainerIdError,
  DuplicateGraphNodeIdError,
  DuplicateGroupMemberError,
  DuplicateShardIdError,
  EmptyGroupError,
  InvalidConcurrencyError,
  ShardPlannerNotConfiguredError,
  ShardPlanRpcError,
  ShardReportsNotSupportedError,
  UnknownGraphDependencyError,
  UnknownGraphNodeError,
} from "./errors.js";
export type { Graph, GraphNode } from "./graph.js";
export { graph } from "./graph.js";
export { limit } from "./limit.js";
export { MAX_SHARD_COUNT, RpcShardPlanner } from "./rpc-shard-planner.js";
export type {
  Check,
  CheckConclusion,
  CheckOptions,
  CheckSealOptions,
  CiEvent,
  ContainerExecutor,
  ContainerOptions,
  ContainerResult,
  ContainerStartRequest,
  GroupOptions,
  GroupResult,
  OnTrigger,
  PipelineContext,
  ShardCountOption,
  ShardCountRange,
  ShardOptions,
  ShardPlan,
  ShardPlanFetcher,
  ShardPlanFetchInit,
  ShardPlanFetchResponse,
  ShardPlanner,
  ShardPlanRequest,
  ShardReportSpec,
  ShardResult,
  ShardRunArgs,
  SplitStrategy,
  WorkflowStepLike,
} from "./types.js";
export type {
  PipelineRunParams,
  PipelineWorkflowExport,
  RunFn,
  WorkflowBindingLike,
  WorkflowDependencies,
  WorkflowEventLike,
  WorkflowFetchEnv,
  WorkflowOptions,
} from "./workflow.js";
export { workflow } from "./workflow.js";
