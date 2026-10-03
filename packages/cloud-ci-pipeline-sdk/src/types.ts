/**
 * Shared public types for `@cloud-ci/pipeline-sdk`.
 *
 * Field names and shapes follow `docs/design/dynamic-pipelines.md`'s "User
 * experience" and "Design" sections as closely as this round's scope
 * allows; fields that section documents but this round does not implement
 * (`sidecars`, `snapshot`, multi-`steps`, …) are intentionally absent —
 * see README's scope boundary list.
 */

/** The event kind a pipeline run was triggered by. The design doc's
 * discovery section lists `pull_request`/`push` as the worked examples;
 * the SDK does not restrict this to a closed set since `on` triggers are
 * script-defined. */
export interface CiEvent {
  readonly kind: string;
  readonly repo: string;
  readonly sha: string;
  readonly ref: string;
}

/** Read-only event data a script sees as `ci.event`/`ci.changedFiles`
 * during a run, and that `on`'s function form sees as its `ctx` argument
 * (design doc's "Discovery and triggers" section) — same shape, different
 * callers. */
export interface PipelineContext {
  readonly event: CiEvent;
  readonly changedFiles: readonly string[];
  readonly branch: string;
  readonly labels: readonly string[];
}

/** `on:` as a static trigger object (design doc's first `workflow()`
 * example) or a discovery function (design doc's "Discovery and
 * triggers"). This round implements the static form's pass-through and the
 * function form's direct call; it does NOT implement the function form's
 * isolate/CPU-budget sandboxing the design doc describes ("loads the
 * script once per event into a Dynamic Worker isolate with egress blocked
 * and a CPU budget") — that sandboxing lives in the future discovery
 * caller, not in this library. See README's scope boundary list. */
export type OnTrigger = Record<string, unknown> | ((ctx: PipelineContext) => boolean);

/** Options accepted by `ci.check(name, opts)`. The design doc's examples
 * only ever pass `required`; this round implements exactly that field. */
export interface CheckOptions {
  readonly required: boolean;
}

/** A check's terminal conclusion when explicitly sealed via
 * `check.seal({ conclusion, summary })` — see design doc's "An always-on
 * required check" example (`docs/design/dynamic-pipelines.md:494-513`).
 * `success`/`failure` are included for completeness even though this
 * round's examples only exercise `skipped`; computing a real conclusion
 * from attached node results is `RunCoordinator`'s job (design doc: "its
 * conclusion is the worst of its attached nodes"), out of this round's
 * scope. */
export type CheckConclusion = "success" | "failure" | "skipped" | "neutral";

/** Explicit-seal options, matching `check.seal({ conclusion, summary })`
 * in the design doc's always-on-check example. */
export interface CheckSealOptions {
  readonly conclusion: CheckConclusion;
  readonly summary?: string;
}

/**
 * Handle returned by `ci.check(name, opts)`. Real state machine (see
 * `src/check.ts`): tracks its own sealed/unsealed state and attached
 * member ids in memory for the lifetime of one script execution. Durable
 * persistence of check state across isolate recycle is `RunCoordinator`'s
 * job in the full design (design doc: "`RunCoordinator` stays the single
 * writer of run state") — out of this round's scope (see README).
 */
export interface Check {
  readonly name: string;
  readonly required: boolean;
  /** `true` once sealed (explicitly or automatically at end of `run`). */
  readonly sealed: boolean;
  /** Count of distinct node ids currently attached. */
  readonly memberCount: number;
  /** Attach a node id to this check. Throws `CheckSealedError` if the
   * check is already sealed — design doc: "Attaching a node to an
   * already-sealed check is a script error." Idempotent per node id:
   * attaching the same id twice does not double-count it. */
  attach(nodeId: string): void;
  /** Seal the check, fixing its member set. Called automatically once
   * at end of `run` for any check the script did not seal itself (design
   * doc's "Check sealing" automatic rule). Throws `CheckAlreadySealedError`
   * if already sealed. */
  seal(opts?: CheckSealOptions): void;
}

/** Minimal shape `ci.container` needs from a Workflow's real `step`
 * handle (`WorkflowStep` in `@cloudflare/workers-types`/
 * `@cloudflare/dynamic-workflows`). Typed structurally here — matching
 * `@cloudflare/dynamic-workflows`'s own `WorkflowStepLike` convention — so
 * this package does not have to depend on a specific `workers-types`
 * version at the call site, only in its own devDependency for typechecking
 * this file. */
export interface WorkflowStepLike {
  do<T>(name: string, callback: () => Promise<T>): Promise<T>;
}

/** Options accepted by `ci.container(id, opts)`. This round implements
 * `runner`/`run`/`check` — the design doc's full shape also documents
 * `snapshot`, `sidecars`, and multi-`steps` (`docs/design/dynamic-pipelines.md`'s
 * "Steps and sidecars" section), none of which this round implements; see
 * README's scope boundary list for why. */
export interface ContainerOptions {
  /** Runner size hint (e.g. `"standard-2"`, `"auto"`). Opaque to this SDK
   * — forwarded to the `ContainerExecutor` as-is. */
  readonly runner?: string;
  /** Shell command the container runs. */
  readonly run: string;
  /** Check to attach this node to, or `null`/omitted to report no check
   * run (design doc: "Nodes with `check: null` (or omitted) report no
   * check run"). */
  readonly check?: Check | null;
}

/** Result of a finished container node. Per "Execution model": "Step
 * params and results must be RPC-serializable ... so `ci.container`
 * returns a plain result object (status, exit code, durations, report and
 * artifact ids), never a live handle." This round implements the subset a
 * `ContainerExecutor` can actually report (see README): no report/artifact
 * ids yet, since those require the real `RunCoordinator` upload path.
 *
 * `stdout` is `undefined` unless the injected `ContainerExecutor` chooses
 * to populate it — this round's plain `ci.container`/`ci.shard`/`ci.group`
 * callers never read it. `@cloud-ci/pipeline-sdk/turbo`'s `turbo.plan`
 * (`src/turbo.ts`) is the one caller that needs it: it runs
 * `turbo run <tasks> --dry=json` via `ci.container` and parses the dry-run
 * JSON back out of this field, same as any other `ci.container` caller
 * reads whatever a real `ContainerExecutor` captured — not a second,
 * parallel output-capture mechanism. */
export interface ContainerResult {
  readonly ok: boolean;
  readonly exitCode: number;
  readonly startedAt: number;
  readonly finishedAt: number;
  readonly stdout?: string;
}

/** A single container-start request `ci.container` hands to the injected
 * `ContainerExecutor`. */
export interface ContainerStartRequest {
  readonly id: string;
  readonly runner?: string;
  readonly run: string;
}

/**
 * Injectable boundary to the real container-start mechanism (the Rust
 * `RunCoordinator` → `ContainerProbe` DO chain `cloud-ci-worker` already
 * proves — see `packages/cloud-ci-worker/src/container_probe.rs` and
 * `src/node_container.rs`). This round does NOT build that wiring (see
 * README's scope boundary list): `ci.container`'s step-durability contract
 * is proven against this interface and a fake in-memory implementation
 * instead. A future round supplies a real `ContainerExecutor` that reaches
 * `RunCoordinator` over the `CONTAINER_WORKER`-style service binding
 * `cloud-ci-dynamic-workflows-host` already demonstrates.
 */
export interface ContainerExecutor {
  start(request: ContainerStartRequest): Promise<ContainerResult>;
}

/** `ci.shard`'s split strategy, per `docs/design/parallelization.md`'s
 * "### Split strategies" table: `"timing"` (LPT bin-packing against
 * `test_stats` history, median-fallback-imputed for unmeasured items,
 * degrading all the way to `"file"` if no item in the matched set has any
 * history), `"file"` (round-robin by path, whole-file granularity), or
 * `"count"` (round-robin, same as `"file"` — this round's underlying
 * `cloud_ci_core::split` crate only supports file granularity, so
 * `"count"`'s documented "whole test, if discoverable" distinction from
 * `"file"` is not implemented; see README's scope boundary list). */
export type SplitStrategy = "timing" | "file" | "count";

/** `{ min, max, target }` shard-count auto-sizing, matching
 * parallelization.md's "### Shard count resolution" table's object form
 * exactly: `shard_count = clamp(ceil(historical_total_duration / target),
 * min, max)`. `target` is a plain duration string (`"5m"`, `"30s"`,
 * `"1h"`) — this SDK never computes the clamp itself; it is forwarded
 * as-is to the injected `ShardPlanner`, which resolves it against real
 * `cloud_ci_core::split` logic (see `ShardPlanner`'s doc comment). */
export interface ShardCountRange {
  readonly min: number;
  readonly max: number;
  readonly target: string;
}

/** `ci.shard`'s `count` option: a fixed integer shard count, or a
 * `{ min, max, target }` auto-sizing spec (only meaningful with
 * `split: "timing"` — the injected `ShardPlanner`'s real resolver rejects
 * the combination of an auto-sizing spec with `"file"`/`"count"`, per
 * `cloud_ci_core::split::SplitError::AutoSizingNeedsTiming`). */
export type ShardCountOption = number | ShardCountRange;

/** Arguments `ci.shard`'s `run` function receives for one resolved shard —
 * dynamic-pipelines.md's "### Splitting tests across shards": "calls the
 * `run` function once per shard with `{ shard, shards, files }` — no shell
 * glue and no template syntax in `run`, just a TypeScript function that
 * returns the command." `shard` is 1-based (shard 1 of `shards`). */
export interface ShardRunArgs {
  readonly shard: number;
  readonly shards: number;
  readonly files: readonly string[];
}

/** One entry of `ci.shard`'s `reports` option — parallelization.md's
 * worked example: `reports: [{ type: "playwright-blob", path: "blob-report/",
 * merge: "html" }]`. `type` selects the report kind
 * `cloud-ci-worker/src/shard_merge.rs`'s `mergeable_kind` dispatches on
 * (`"junit"`/`"lcov"` merge natively; anything else, including
 * `"playwright-blob"`/`"vitest-blob"`, is logged and skipped server-side —
 * see that module's docs). `path` and `merge` are carried through
 * unparsed; this SDK does not interpret them. */
export interface ShardReportSpec {
  readonly type: string;
  readonly path?: string;
  readonly merge?: string;
}

/** Options accepted by `ci.shard(id, opts)`. This round implements
 * `split`/`count`/`files`/`run`/`check` — the design doc's full shape also
 * documents `snapshot`, `sidecars`, and `reports`/merge-barrier options
 * (dynamic-pipelines.md's "### Splitting tests across shards" worked
 * example); `reports` is a typed field here (`ShardReportSpec[]`), but
 * giving it a non-empty value throws `ShardReportsNotSupportedError` — the
 * server-side merge barrier (`cloud-ci-worker/src/shard_merge.rs`,
 * `coordinator::mod`'s `job_group`/`register-shard-group`) has no public
 * `IngestService` RPC `ci.shard` can call to register a shard group's
 * merge configuration (`expected_total`/`fail_fast`/`merge_on_failure`):
 * `/register-shard-group` is an internal Durable Object HTTP route, not a
 * `cloud_ci.ingest.v1.IngestService` procedure, and `StartJob`/
 * `CompleteShard`/`SubmitReport` carry no such fields either — see
 * parallelization.md's "Implementation status" merge paragraph and
 * README's scope boundary list for exactly what proto addition is
 * missing. `snapshot`/`sidecars` remain entirely absent, same as
 * `ContainerOptions`'s own scope boundary. */
export interface ShardOptions {
  readonly split: SplitStrategy;
  readonly count: ShardCountOption;
  /** The shard's already-expanded, already-sorted file list —
   * parallelization.md's "### Deterministic assignment, end to end" step
   * 1: "The shard's `files` glob is expanded against the checked-out
   * worktree at dispatch time, sorted by path, and hashed into a run
   * manifest." Glob expansion itself is the caller's job (or a future
   * round's), not this SDK's — `ci.shard` takes the already-resolved list. */
  readonly files: readonly string[];
  readonly run: (args: ShardRunArgs) => string;
  /** Check to attach every resolved shard to, or `null`/omitted to report
   * no check run — same `ContainerOptions.check` convention `ci.container`
   * already uses, since each shard is dispatched as one `ci.container`
   * call under the hood. */
  readonly check?: Check | null;
  /** Native (`junit`/`lcov`) and framework-blob report merge spec —
   * parallelization.md's "### Shard groups / merge barrier". See
   * `ShardReportSpec`'s doc comment for why a non-empty value throws
   * `ShardReportsNotSupportedError` rather than silently taking effect. */
  readonly reports?: readonly ShardReportSpec[];
}

/** Result of one finished `ci.shard` call: the resolved shard count and
 * each shard's `ci.container` result, in shard-index order (index 0 =
 * shard 1). No merged-report id — merging per-shard reports
 * (`cloud-ci-worker/src/shard_merge.rs`) is not wired to `ci.shard` this
 * round; see README's scope boundary list. */
export interface ShardResult {
  readonly shardCount: number;
  readonly results: readonly ContainerResult[];
}

/** Request `ci.shard` hands to the injected `ShardPlanner` to resolve a
 * shard count and per-shard file assignment. */
export interface ShardPlanRequest {
  readonly filePaths: readonly string[];
  readonly strategy: SplitStrategy;
  readonly count: ShardCountOption;
}

/** A resolved shard plan: the final shard count and each shard's assigned
 * file list, in shard-index order (index 0 = shard 1) — matching
 * `cloud_ci_core::split::assign`'s own "index 0 = shard 1" convention and
 * the `ResolveShardPlanResponse` proto message's `shards` field. */
export interface ShardPlan {
  readonly shardCount: number;
  readonly files: readonly (readonly string[])[];
}

/**
 * Injectable boundary to the real shard-plan resolution mechanism — the
 * Rust `ResolveShardPlan` RPC `cloud-ci-worker` exposes
 * (`packages/cloud-ci-worker/src/shard_plan.rs`), which calls
 * `cloud_ci_core::split`'s real LPT-bin-packing/round-robin/median-
 * imputation functions directly, the same ones `cloud-ci-cli`'s `cloud-ci
 * split` (BYO CI) calls. This SDK does NOT reimplement that algorithm in
 * TypeScript — doing so would risk silently drifting from the Rust
 * original, which directly violates parallelization.md's "###
 * Deterministic assignment, end to end" goal: "the same binary and the
 * same split algorithm ... used in both places, so a BYO CI matrix and a
 * cloud-ci-managed shard group produce byte-identical assignments for the
 * same inputs." `ci.shard`'s dispatch logic is proven against this
 * interface and a fake in-memory implementation
 * (`test/shard.test.ts`'s `FakeShardPlanner`); `RpcShardPlanner`
 * (`src/rpc-shard-planner.ts`) is a real implementation that reaches
 * `ResolveShardPlan` over an injected `ShardPlanFetcher` — the same
 * `CONTAINER_WORKER`-style service binding `ContainerExecutor`'s own doc
 * comment describes — proven only against an in-process fake `Fetcher`
 * (`test/rpc-shard-planner.test.ts`), never a real deployed
 * `cloud-ci-worker`.
 */
export interface ShardPlanner {
  resolve(request: ShardPlanRequest): Promise<ShardPlan>;
}

/** Minimal shape `RpcShardPlanner` needs from a real Workers service
 * binding (`Fetcher` in `@cloudflare/workers-types`). Typed structurally
 * here — matching `WorkflowStepLike`'s own convention above — so this
 * package does not have to depend on a specific `workers-types` version at
 * the call site, only in its own devDependency for typechecking this
 * file. A real `Fetcher.fetch` and a real `Response` both satisfy this
 * shape as-is. */
export interface ShardPlanFetcher {
  fetch(url: string, init: ShardPlanFetchInit): Promise<ShardPlanFetchResponse>;
}

/** `RequestInit` subset `RpcShardPlanner` sends — always a `POST` with a
 * JSON body, per `cloud-ci-worker/src/connect.rs`'s `negotiate()` (unary
 * Connect RPC, no streaming). */
export interface ShardPlanFetchInit {
  readonly method: "POST";
  readonly headers: Readonly<Record<string, string>>;
  readonly body: string;
}

/** `Response` subset `RpcShardPlanner` reads. */
export interface ShardPlanFetchResponse {
  readonly ok: boolean;
  readonly status: number;
  json(): Promise<unknown>;
}

/** Options accepted by `ci.group(ids, opts)`. The design doc's only text
 * on `ci.group` is a single "Graph helpers" table row: `` `ci.group(ids,
 * spec)` `` — "Run several nodes in one container to save startup cost"
 * (`docs/design/dynamic-pipelines.md:381`). There is no worked example, no
 * prose section, and no further mention anywhere else in that document —
 * unlike every other primitive (`ci.container`, `ci.shard`, `ci.check`),
 * which each get a full "User experience" code sample plus a "Design"
 * section spelling out their contract. This SDK therefore implements the
 * narrowest shape that single row actually states, matching
 * `ContainerOptions`'s own fields exactly (this round never invented a
 * second container-spec shape): `ids` names the several graph-node ids
 * being batched into one container dispatch; `opts.run` is the single,
 * already-combined shell command that single container runs to produce
 * every named node's result (composing that command — e.g. one `turbo
 * run` invocation covering several packages — is the caller's job, same
 * division of labor `ci.shard`'s `run` callback uses for per-shard
 * commands). This is a batch-dispatch primitive, not a logical/UI
 * label or a callback scope for nested `ci.check`/`ci.container` calls —
 * the doc's signature takes an id array and a spec object, never a
 * callback. */
export interface GroupOptions {
  /** Runner size hint, forwarded to the `ContainerExecutor` as-is — same
   * field and meaning as `ContainerOptions.runner`. */
  readonly runner?: string;
  /** Shell command the one shared container runs, covering every id in
   * `ids`. */
  readonly run: string;
  /** Check every id in `ids` attaches to, or `null`/omitted to report no
   * check run — same semantics as `ContainerOptions.check`, applied to
   * each grouped id individually (mirrors `ci.shard`'s per-shard attach,
   * `src/shard.ts`). */
  readonly check?: Check | null;
}

/** Result of one finished `ci.group` call: every id in the call's `ids`
 * maps to the same single container's `ContainerResult`, since all of them
 * ran inside that one container. Keyed by id (not a `Map`, to stay
 * RPC-serializable — see `ContainerResult`'s own doc comment on that
 * constraint). */
export interface GroupResult {
  readonly results: Readonly<Record<string, ContainerResult>>;
}
