import {
  ContainerExecutorNotConfiguredError,
  DuplicateContainerIdError,
  DuplicateGroupMemberError,
  EmptyGroupError,
} from "./errors.js";
import type {
  Check,
  ContainerExecutor,
  ContainerResult,
  GroupOptions,
  GroupResult,
  WorkflowStepLike,
} from "./types.js";

/**
 * Backs `ci.group(ids, opts)`. Per `types.ts`'s `GroupOptions` doc
 * comment, the design doc's only text on `ci.group`
 * (`docs/design/dynamic-pipelines.md:381`, a single "Graph helpers" table
 * row) is "Run several nodes in one container to save startup cost" —
 * this function dispatches exactly one container (one
 * `step.do("group:" + ..., ...)`, one `ContainerExecutor.start()` call)
 * for the whole `ids` list, rather than one container per id the way
 * `ci.container`/`ci.shard` each dispatch.
 *
 * Id bookkeeping deliberately does not reuse `runContainer`: that
 * function always starts its own container step, which would mean N
 * container dispatches for N ids, defeating the entire point of
 * `ci.group` ("save startup cost"). Reusing `runContainer` wholesale is
 * therefore wrong here, unlike `ci.shard` (which genuinely wants one
 * container per shard and reuses `runContainer` directly for exactly that
 * reason, per `shard.ts`'s doc comment). Instead this function reimplements
 * the same two checks `runContainer` makes — duplicate id against
 * `seenIds`, and check-attach before the step runs — against every id in
 * `ids`, then makes its own single `step.do` call.
 */
export async function runGroup(
  deps: {
    readonly step: WorkflowStepLike;
    readonly executor: ContainerExecutor | undefined;
    readonly seenIds: Set<string>;
  },
  ids: readonly string[],
  opts: GroupOptions,
): Promise<GroupResult> {
  if (ids.length === 0) {
    throw new EmptyGroupError();
  }

  const withinCall = new Set<string>();
  for (const id of ids) {
    if (withinCall.has(id)) {
      throw new DuplicateGroupMemberError(id);
    }
    withinCall.add(id);
  }

  for (const id of ids) {
    if (deps.seenIds.has(id)) {
      throw new DuplicateContainerIdError(id);
    }
  }
  for (const id of ids) {
    deps.seenIds.add(id);
  }

  const check: Check | null = opts.check ?? null;
  if (check !== null) {
    for (const id of ids) {
      check.attach(id);
    }
  }

  const stepName = `group:${ids.join(",")}`;
  const result: ContainerResult = await deps.step.do(stepName, async () => {
    if (!deps.executor) {
      throw new ContainerExecutorNotConfiguredError(ids.join(","));
    }
    return deps.executor.start({ id: ids.join(","), runner: opts.runner, run: opts.run });
  });

  const results: Record<string, ContainerResult> = {};
  for (const id of ids) {
    results[id] = result;
  }
  return { results };
}
