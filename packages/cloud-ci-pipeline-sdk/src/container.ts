import { ContainerExecutorNotConfiguredError, DuplicateContainerIdError } from "./errors.js";
import type {
  Check,
  ContainerExecutor,
  ContainerOptions,
  ContainerResult,
  WorkflowStepLike,
} from "./types.js";

/**
 * Backs `ci.container(id, opts)`. The durability contract this implements
 * — "calling `ci.container` for the same `id` a second time (simulating an
 * isolate-recycle replay) returns the previously recorded result without
 * re-dispatching" — comes directly from `step.do`, the real primitive
 * `@cloudflare/dynamic-workflows`'s `WorkflowStepLike` exposes (same shape
 * as Cloudflare's own `WorkflowStep.do`, proven in
 * `cloud-ci-dynamic-workflows-host/test/fixtures/pipeline-script.js`'s
 * `step1-plan`/`step2-container-exec` steps): the Workflows engine persists
 * a step's result once its callback resolves, and a later call with the
 * same step name returns that recorded result without invoking the
 * callback again — including across isolate recycle. This function does
 * not invent a second, parallel persistence mechanism; it is a thin
 * `step.do` wrapper plus this SDK's own id-uniqueness and check-attachment
 * bookkeeping.
 *
 * `docs/design/dynamic-pipelines.md`'s "Execution model" describes the
 * full design as *two* durable operations per node
 * (`step.do("start:" + id)` then `step.waitForEvent("done:" + id)`, with
 * `RunCoordinator` sending the completion event once the container
 * finishes) so a long-running container does not hold a `step.do` open and
 * block isolate recycling. This round has no `RunCoordinator` wiring (see
 * README's scope boundary list), so there is no completion event to wait
 * for; the injected `ContainerExecutor.start()` call itself is wrapped in
 * one `step.do("container:" + id, ...)`. Splitting this into the real
 * start/wait-for-event pair is explicit follow-up work for whoever wires a
 * real `ContainerExecutor` that talks to `RunCoordinator` — documented
 * here so the single-step shape is not mistaken for the final design.
 */
export async function runContainer(
  deps: {
    readonly step: WorkflowStepLike;
    readonly executor: ContainerExecutor | undefined;
    readonly seenIds: Set<string>;
  },
  id: string,
  opts: ContainerOptions,
): Promise<ContainerResult> {
  if (deps.seenIds.has(id)) {
    throw new DuplicateContainerIdError(id);
  }
  deps.seenIds.add(id);

  const check: Check | null = opts.check ?? null;
  if (check !== null) {
    check.attach(id);
  }

  return deps.step.do(`container:${id}`, async () => {
    if (!deps.executor) {
      throw new ContainerExecutorNotConfiguredError(id);
    }
    return deps.executor.start({ id, runner: opts.runner, run: opts.run });
  });
}
