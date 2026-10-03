/**
 * Shared public types for `@cloud-ci/pipeline-sdk`.
 *
 * Field names and shapes follow `docs/design/dynamic-pipelines.md`'s "User
 * experience" and "Design" sections as closely as this round's scope
 * allows; fields that section documents but this round does not implement
 * (`sidecars`, `snapshot`, multi-`steps`, `ci.shard`, `ci.group`, …) are
 * intentionally absent — see README's scope boundary list.
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
 * ids yet, since those require the real `RunCoordinator` upload path. */
export interface ContainerResult {
  readonly ok: boolean;
  readonly exitCode: number;
  readonly startedAt: number;
  readonly finishedAt: number;
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
