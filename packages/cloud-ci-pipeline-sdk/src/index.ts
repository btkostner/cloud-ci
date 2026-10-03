/**
 * `@cloud-ci/pipeline-sdk` — public entry point.
 *
 * Implements the subset of `docs/design/dynamic-pipelines.md`'s `ci` API
 * this round builds: `workflow()`, `ci.check`, `ci.container`, `ci.shard`,
 * `ci.group`. See this package's README for the full, explicit scope
 * boundary (what is and is not implemented this round) before relying on
 * anything not re-exported here.
 */
export { CheckRegistry } from "./check.js";
export { CiContext } from "./context.js";
export {
  CheckAlreadySealedError,
  CheckSealedError,
  ContainerExecutorNotConfiguredError,
  DuplicateCheckNameError,
  DuplicateContainerIdError,
  DuplicateGroupMemberError,
  DuplicateShardIdError,
  EmptyGroupError,
  ShardPlannerNotConfiguredError,
} from "./errors.js";
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
  ShardPlanner,
  ShardPlanRequest,
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
