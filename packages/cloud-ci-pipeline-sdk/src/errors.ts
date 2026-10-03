/**
 * Typed errors the SDK throws for script-authoring mistakes. These are
 * genuine "the script is wrong" conditions — matching
 * `docs/design/dynamic-pipelines.md`'s "Determinism" and "Check sealing"
 * sections, which describe these exact situations as script/run errors the
 * coordinator fails the run on, not conditions the SDK should swallow or
 * paper over.
 */

/** Thrown when a script attaches a node to a check that is already sealed.
 * Per "Check sealing": "Attaching a node to an already-sealed check is a
 * script error; the coordinator fails the run." */
export class CheckSealedError extends Error {
  constructor(checkName: string, nodeId: string) {
    super(`cannot attach node "${nodeId}" to check "${checkName}": check is already sealed`);
    this.name = "CheckSealedError";
  }
}

/** Thrown when a script calls `check.seal()` on a check that is already
 * sealed. Sealing is a one-time transition in this SDK's state machine. */
export class CheckAlreadySealedError extends Error {
  constructor(checkName: string) {
    super(`check "${checkName}" is already sealed`);
    this.name = "CheckAlreadySealedError";
  }
}

/** Thrown when a script creates two checks with the same name in one run.
 * Check names back real GitHub check runs (one per name); a duplicate name
 * within a run is a script bug, same category as the duplicate-container-id
 * rejection the design doc specifies for node ids. */
export class DuplicateCheckNameError extends Error {
  constructor(name: string) {
    super(`check "${name}" was already created in this run`);
    this.name = "DuplicateCheckNameError";
  }
}

/** Thrown when a script calls `ci.container` twice with the same id in one
 * execution. Per "Determinism": "Node ids are the replay key and must be
 * unique and stable within a run. The SDK rejects a duplicate id at the
 * call site." */
export class DuplicateContainerIdError extends Error {
  constructor(id: string) {
    super(`container id "${id}" was already used in this run`);
    this.name = "DuplicateContainerIdError";
  }
}

/** Thrown when `ci.container` is called without a `ContainerExecutor`
 * injected into `workflow()`. This round's scope boundary (see README):
 * there is no real `RunCoordinator` wiring yet, so a script run with no
 * executor configured fails loudly instead of silently no-opping. */
export class ContainerExecutorNotConfiguredError extends Error {
  constructor(id: string) {
    super(
      `ci.container("${id}", ...) called but no ContainerExecutor was configured; ` +
        `pass one via workflow(opts, { executor }) — see README's "ContainerExecutor" section`,
    );
    this.name = "ContainerExecutorNotConfiguredError";
  }
}

/** Thrown when a script calls `ci.shard` twice with the same id in one
 * execution — same "node ids are the replay key" rule
 * `DuplicateContainerIdError` enforces for `ci.container`, applied to
 * shard-group ids instead of single-container ids. */
export class DuplicateShardIdError extends Error {
  constructor(id: string) {
    super(`shard id "${id}" was already used in this run`);
    this.name = "DuplicateShardIdError";
  }
}

/** Thrown when `ci.shard` is called without a `ShardPlanner` injected into
 * `workflow()`. Same posture as `ContainerExecutorNotConfiguredError`:
 * there is no real `ResolveShardPlan` RPC wiring yet (see README's scope
 * boundary list), so a script run with no planner configured fails loudly
 * instead of silently no-opping. */
export class ShardPlannerNotConfiguredError extends Error {
  constructor(id: string) {
    super(
      `ci.shard("${id}", ...) called but no ShardPlanner was configured; ` +
        `pass one via workflow(opts, { planner }) — see README's "ShardPlanner" section`,
    );
    this.name = "ShardPlannerNotConfiguredError";
  }
}

/** Thrown when `ResolveShardPlan` cannot produce a usable plan: transport
 * failure, a Connect error body (`code`/`message`, per
 * `cloud-ci-worker/src/connect.rs`'s `ErrorBody`), or a malformed success
 * body. `code` is the Connect code string (`"unauthenticated"`,
 * `"invalid_argument"`, ...) or `"transport"`/`"malformed_response"` for
 * failures that never produced a Connect error body. There is deliberately
 * no local fallback plan: re-implementing `cloud_ci_core::split` in
 * TypeScript would break the byte-identical-assignment guarantee.
 *
 * `cause`, when given, is the original error this one was rewrapped from
 * (e.g. a caller's `token` callback throwing) — set via the standard
 * `Error.cause` mechanism (`super(message, { cause })`) so a caller's
 * `console.error`/error-reporting tooling that already walks `.cause`
 * still sees the original stack, rather than that error being silently
 * dropped by the rewrap. */
export class ShardPlanRpcError extends Error {
  readonly code: string;
  readonly httpStatus: number | undefined;
  constructor(code: string, message: string, httpStatus?: number, cause?: unknown) {
    super(
      `ResolveShardPlan failed (${code}): ${message}`,
      cause === undefined ? undefined : { cause },
    );
    this.name = "ShardPlanRpcError";
    this.code = code;
    this.httpStatus = httpStatus;
  }
}

/** Thrown when `ci.shard` is given a non-empty `reports` option. The
 * public ingest proto has no RPC for registering a shard group's
 * merge/report configuration (see README), so the option cannot take
 * effect; failing loudly beats silently dropping the merge barrier. */
export class ShardReportsNotSupportedError extends Error {
  constructor(id: string) {
    super(
      `ci.shard("${id}", ...) was given \`reports\`, but no public RPC can register ` +
        `a shard group's merge configuration yet — see README's "ci.shard reports/merge" section`,
    );
    this.name = "ShardReportsNotSupportedError";
  }
}

/** Thrown when a script calls `ci.group` with an empty `ids` array.
 * Grouping zero nodes into one container dispatches nothing and names no
 * node — same "fail loudly on a meaningless call" posture as
 * `ContainerExecutorNotConfiguredError`. */
export class EmptyGroupError extends Error {
  constructor() {
    super("ci.group(ids, ...) called with an empty ids array");
    this.name = "EmptyGroupError";
  }
}

/** Thrown when a script calls `ci.group` with the same id twice in one
 * `ids` array. Same "node ids are the replay key and must be unique"
 * rule `DuplicateContainerIdError` enforces across calls, applied within
 * a single `ci.group` call's own member list. */
export class DuplicateGroupMemberError extends Error {
  constructor(id: string) {
    super(`ci.group(ids, ...) was given duplicate id "${id}" within one call`);
    this.name = "DuplicateGroupMemberError";
  }
}

/** Thrown when a script calls `ci.limit(n, thunks)` with `n < 1`. A
 * concurrency limit below 1 would never start any thunk — same "fail
 * loudly on a meaningless call" posture as `EmptyGroupError`. */
export class InvalidConcurrencyError extends Error {
  constructor(n: number) {
    super(`ci.limit(n, ...) called with n=${n}; n must be an integer >= 1`);
    this.name = "InvalidConcurrencyError";
  }
}

/** Thrown by `graph.fromJson(nodes)` when two nodes in the input array
 * share the same `id`. A graph's `id` is its lookup key (`graph.node(id)`/
 * `graph.deps(id)`); a duplicate makes that lookup ambiguous. */
export class DuplicateGraphNodeIdError extends Error {
  constructor(id: string) {
    super(`graph.fromJson(nodes) was given duplicate node id "${id}"`);
    this.name = "DuplicateGraphNodeIdError";
  }
}

/** Thrown by `graph.node(id)`/`graph.deps(id)` when `id` is not in the
 * graph. */
export class UnknownGraphNodeError extends Error {
  constructor(id: string) {
    super(`graph has no node with id "${id}"`);
    this.name = "UnknownGraphNodeError";
  }
}

/** Thrown by `graph.fromJson(nodes)` when a node's `dependencies` entry
 * names an id that is not itself a node in the same array. A graph whose
 * edges point outside its own node set cannot be walked
 * (`graph.deps(id)` would hand back a dangling id). */
export class UnknownGraphDependencyError extends Error {
  constructor(nodeId: string, dependencyId: string) {
    super(`graph node "${nodeId}" depends on unknown node id "${dependencyId}"`);
    this.name = "UnknownGraphDependencyError";
  }
}

/** Thrown by `turbo.plan(ci, opts)` when the injected `ContainerExecutor`
 * returns a `ContainerResult` with no `stdout` — `turbo.plan` cannot parse
 * `turbo run --dry=json`'s output out of a result that never captured any
 * output. Fails loudly rather than silently returning an empty graph. */
export class TurboPlanOutputMissingError extends Error {
  constructor(id: string) {
    super(
      `turbo.plan's container "${id}" finished with no stdout to parse; ` +
        `the injected ContainerExecutor must populate ContainerResult.stdout`,
    );
    this.name = "TurboPlanOutputMissingError";
  }
}

/** Thrown by `mise.plan(ci, opts)`. mise has no documented per-task
 * dependency-graph JSON command equivalent to turbo's `--dry=json` — see
 * `src/mise.ts`'s doc comment and this package's README for the exact,
 * sourced investigation. Fails loudly rather than guessing a command or
 * JSON shape. */
export class MisePlanNotImplementedError extends Error {
  constructor() {
    super(
      "mise.plan is not implemented: mise has no documented, machine-readable " +
        "per-task dependency-graph command with hash/outputs fields equivalent to " +
        "`turbo run --dry=json` — see src/mise.ts and README for the sourced " +
        "investigation (`mise tasks graph --json`'s undocumented project-graph " +
        "shape vs. `mise tasks deps`'s lack of a --json flag)",
    );
    this.name = "MisePlanNotImplementedError";
  }
}
