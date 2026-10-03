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
