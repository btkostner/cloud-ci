/**
 * `@cloud-ci/pipeline-sdk/mise` — a separate entry point, not an export of
 * the core package, same posture as `@cloud-ci/pipeline-sdk/turbo` (see
 * `src/turbo.ts`'s doc comment for the design doc citation).
 *
 * `docs/design/dynamic-pipelines.md`'s "Graph helpers" table (`:376`)
 * marks `mise.plan` `[unverified: mise's machine-readable graph command
 * and format]`. This file does not guess one. The investigation below is
 * real and dated — not a placeholder — and its conclusion is that the tag
 * stays unverified: mise has no documented command that plays turbo's
 * `--dry=json` role (a per-task dependency graph with `hash`/`outputs`
 * fields a cache-aware executor needs).
 *
 * **What was checked (2026-10-02, mise `2026.9.14 macos-arm64`):**
 *
 * - `mise tasks deps [TASKS]...` (mise.jdx.dev/cli/tasks/deps.html,
 *   checked 2026-10-02) — "Display a tree visualization of a dependency
 *   graph." Its own `--help` output lists exactly four flags: `--compact`,
 *   `--dot`, `--hidden`, `--help`. **No `--json`/`-J` flag exists.** Its
 *   two machine-parseable-ish forms are a text tree and `--dot` (Graphviz
 *   DOT) — DOT encodes edges, not per-task `hash`/`outputs` fields, and
 *   parsing DOT as this module's graph source would mean inventing a
 *   second, undocumented micro-format on top of an already-unofficial
 *   output mode.
 * - `mise tasks graph [FLAGS]` (mise.jdx.dev/cli/tasks/graph.html, checked
 *   2026-10-02) — "`[experimental]` Inspect the workspace project graph,"
 *   with a real `-J`/`--json` flag ("Output the project graph as JSON").
 *   Run for real against this repo (`mise tasks graph --json`,
 *   2026-10-02): `{"projects": []}` — confirming two things directly, not
 *   by inference: (1) the command and flag are real and produce valid
 *   JSON, and (2) its JSON is a `projects` array — mise's monorepo
 *   "projects" concept (mise.jdx.dev/tasks/monorepo.html) — not a `tasks`
 *   array of individual task nodes. The command's own `--help` text
 *   documents no field list for what a non-empty `projects` entry
 *   contains, unlike turborepo.dev/docs/reference/run's explicit `--dry`
 *   field table (`taskId`/`task`/`package`/`hash`/`outputs`/
 *   `dependencies`/...). Whether a populated project entry even carries
 *   per-task `hash`/`outputs` fields at all is the open question this
 *   investigation could not resolve against this repo, which does not use
 *   mise's "projects" feature (hence the empty array) — this is `mise
 *   tasks graph`'s real, current, but genuinely under-documented behavior,
 *   not a guess.
 *
 * **Conclusion:** no command and JSON shape pairing here is both
 * documented with a field list and confirmed non-empty against a real
 * project, the bar turbo's `--dry=json` clears. Fabricating a parser
 * against `mise tasks graph --json`'s unconfirmed per-task field shape
 * would pin a shape nobody has verified. `mise.plan`'s signature below is
 * real and usable by a caller that supplies its own `source` function
 * (the same escape hatch `graph.fromGraph` already generalizes); this
 * module does not invoke any mise CLI itself.
 *
 * **Exact open question for whoever revisits this:** does `mise tasks
 * graph --json`'s `projects[].tasks` (if that is even the real nested
 * shape — unconfirmed) carry a `hash`/cache-key field suitable for
 * `turbo.execute`-style cache-hit skipping, or does mise simply not expose
 * a per-task content hash via any CLI output today? That needs testing
 * against a real repo that uses mise's "projects" feature with mise
 * actually populating a non-empty graph — not available in this
 * environment.
 */
import type { CiContext } from "./context.js";
import { MisePlanNotImplementedError } from "./errors.js";
import { type Graph, type GraphNode, graph } from "./graph.js";
import type { Check } from "./types.js";

/** Options accepted by `mise.plan(ci, opts)`, shaped to mirror
 * `TurboPlanOptions` (`src/turbo.ts`) field-for-field so a script that
 * swaps `turbo.plan` for `mise.plan` does not also have to restructure its
 * call site — `tasks` names the mise tasks to plan, `id`/`runner`/`check`
 * forward to the `ci.container` call a real implementation would make.
 *
 * `source` is this round's explicit escape hatch in place of a built-in
 * mise CLI invocation (see this file's top doc comment for why no such
 * invocation is implemented): a caller-supplied function that returns raw
 * nodes from wherever it trusts mise's graph to come from (a real `mise
 * tasks graph --json` call once its shape is confirmed, a custom script,
 * a fixture), each mapped into cloud-ci's node shape — i.e. exactly the
 * `rawNodes`/`mapFn` pair `graph.fromGraph` already takes, not a new
 * parsing convention. Until `source` is supplied, `mise.plan` throws
 * `MisePlanNotImplementedError` rather than guessing. */
export interface MisePlanOptions<RawNode = GraphNode> {
  readonly tasks: readonly string[];
  readonly id?: string;
  readonly runner?: string;
  readonly check?: Check | null;
  /** Caller-supplied source of mise's raw task nodes, plus the mapping
   * function to cloud-ci's node shape — see this interface's doc comment.
   * Omitted means `mise.plan` has no real command to run and throws
   * `MisePlanNotImplementedError`. */
  readonly source?: {
    readonly rawNodes: () => Promise<readonly RawNode[]>;
    readonly mapFn: (rawNode: RawNode) => GraphNode;
  };
}

/**
 * `[blocked: unverified mise CLI contract]` — see this file's top doc
 * comment for the full, dated investigation. This round does not invoke
 * any mise command itself: `mise tasks deps` has no `--json` output at
 * all, and `mise tasks graph --json`'s real, confirmed output
 * (`{"projects": []}` against this repo, 2026-10-02) is a `projects`
 * array whose per-task field shape (if any) is undocumented and untested
 * against a non-empty project graph.
 *
 * If `opts.source` is supplied, this function uses it exactly the way
 * `turbo.plan` uses `graph.fromGraph` internally — fetching the caller's
 * raw nodes and mapping them into cloud-ci's shape — and does not touch
 * `ci.container` at all (there is no mise command this round trusts enough
 * to run in one). If `opts.source` is omitted, this throws
 * `MisePlanNotImplementedError` rather than silently returning an empty
 * graph or fabricating a command.
 */
export async function plan<RawNode = GraphNode>(
  _ci: CiContext,
  opts: MisePlanOptions<RawNode>,
): Promise<Graph> {
  if (!opts.source) {
    throw new MisePlanNotImplementedError();
  }
  const rawNodes = await opts.source.rawNodes();
  return graph.fromGraph(rawNodes, opts.source.mapFn);
}

export const mise = { plan };
