import { CheckAlreadySealedError, CheckSealedError, DuplicateCheckNameError } from "./errors.js";
import type { Check, CheckOptions, CheckSealOptions } from "./types.js";

/**
 * Real state machine backing `ci.check(name, opts)`. See
 * `docs/design/dynamic-pipelines.md`'s "Check sealing" section
 * (`:475-492`) for the semantics this implements:
 *
 * - a check starts unsealed, with zero attached members;
 * - `attach(nodeId)` adds a member; attaching the same id twice is a no-op
 *   (idempotent), attaching to a sealed check throws `CheckSealedError`;
 * - `seal()` is a one-time transition (unsealed -> sealed); sealing an
 *   already-sealed check throws `CheckAlreadySealedError`;
 * - once sealed, the member set is fixed — `memberCount` no longer
 *   changes.
 *
 * This is purely in-memory, scoped to one script execution. Durable
 * cross-replay persistence of check state is `RunCoordinator`'s job in the
 * full design — out of this round's scope (see README).
 */
export class CheckImpl implements Check {
  readonly name: string;
  readonly required: boolean;
  private _sealed = false;
  private readonly members = new Set<string>();

  constructor(name: string, opts: CheckOptions) {
    this.name = name;
    this.required = opts.required;
  }

  get sealed(): boolean {
    return this._sealed;
  }

  get memberCount(): number {
    return this.members.size;
  }

  attach(nodeId: string): void {
    if (this._sealed) {
      throw new CheckSealedError(this.name, nodeId);
    }
    this.members.add(nodeId);
  }

  seal(opts?: CheckSealOptions): void {
    if (this._sealed) {
      throw new CheckAlreadySealedError(this.name);
    }
    this._sealed = true;
    this.sealOpts = opts;
  }

  /** Explicit seal options passed to `seal()`, if any. `undefined` means
   * this check sealed via the automatic end-of-run rule rather than an
   * explicit `check.seal({ conclusion, ... })` call. Exposed for tests and
   * for a future `RunCoordinator` integration to read what the script
   * asked for. */
  sealOpts: CheckSealOptions | undefined;
}

/**
 * Tracks every check a script creates during one execution, so `workflow()`
 * can apply the "automatic" sealing rule from "Check sealing":
 * "automatically, when the script's `run` function finishes scheduling —
 * i.e. `run` returns and every scheduling call it made has resolved".
 */
export class CheckRegistry {
  private readonly checks = new Map<string, CheckImpl>();

  create(name: string, opts: CheckOptions): Check {
    if (this.checks.has(name)) {
      throw new DuplicateCheckNameError(name);
    }
    const check = new CheckImpl(name, opts);
    this.checks.set(name, check);
    return check;
  }

  /** Seals every check the script did not seal itself. Called once by
   * `workflow()` after the script's `run` resolves. */
  sealRemaining(): void {
    for (const check of this.checks.values()) {
      if (!check.sealed) {
        check.seal();
      }
    }
  }

  all(): readonly Check[] {
    return [...this.checks.values()];
  }
}
