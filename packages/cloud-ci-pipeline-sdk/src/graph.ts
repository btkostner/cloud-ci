import {
  DuplicateGraphNodeIdError,
  UnknownGraphDependencyError,
  UnknownGraphNodeError,
} from "./errors.js";

/**
 * cloud-ci's generic graph-node shape. `docs/design/dynamic-pipelines.md`'s
 * "Graph helpers" table (`:377`) names this shape exactly:
 * `{ id, package, task, hash, dependencies, outputs }`. `turbo.plan`/
 * `mise.plan` each parse their own tool's output into an array of these
 * before calling `graph.fromJson`/`graph.fromGraph`.
 */
export interface GraphNode {
  readonly id: string;
  readonly package: string;
  readonly task: string;
  readonly hash: string;
  readonly dependencies: readonly string[];
  readonly outputs: readonly string[];
}

/**
 * A resolved, walkable dependency graph of `GraphNode`s — what `turbo.plan`/
 * `mise.plan`/`graph.fromJson`/`graph.fromGraph` all return, and what
 * `turbo.execute`'s dependency-ordered fan-out (`src/turbo.ts`) walks via
 * `ids()`/`node(id)`/`deps(id)`, matching the design doc's own
 * `execute(ci, graph, opts)` sketch (`:139-158`) exactly: `graph.deps(id)`,
 * `graph.node(id)`, `graph.ids()`.
 */
export interface Graph {
  /** Every node id, in the order `fromJson`/`fromGraph` received them. */
  ids(): readonly string[];
  /** The full node for `id`. Throws `UnknownGraphNodeError` if `id` is not
   * in this graph. */
  node(id: string): GraphNode;
  /** `id`'s dependency ids (same as `node(id).dependencies`, exposed as its
   * own method to match the design doc's `graph.deps(id)` call sites).
   * Throws `UnknownGraphNodeError` if `id` is not in this graph. */
  deps(id: string): readonly string[];
}

/**
 * Builds a `Graph` from an array already in cloud-ci's own node shape —
 * `docs/design/dynamic-pipelines.md`'s "Graph helpers" table (`:377`):
 * "Builds cloud-ci's generic graph from an array already in cloud-ci's own
 * node shape ...; the primitive `turbo.plan`/`mise.plan` call internally
 * after parsing their own tool's output." Rejects a duplicate `id` within
 * `nodes` (same "ids are a lookup key, duplicates make lookup ambiguous"
 * posture `DuplicateContainerIdError` already takes for container ids) and
 * a `dependencies` entry naming an id outside `nodes` (a graph whose edges
 * point outside its own node set cannot be walked).
 */
export function fromJson(nodes: readonly GraphNode[]): Graph {
  const byId = new Map<string, GraphNode>();
  for (const node of nodes) {
    if (byId.has(node.id)) {
      throw new DuplicateGraphNodeIdError(node.id);
    }
    byId.set(node.id, node);
  }
  for (const node of nodes) {
    for (const dependencyId of node.dependencies) {
      if (!byId.has(dependencyId)) {
        throw new UnknownGraphDependencyError(node.id, dependencyId);
      }
    }
  }

  const ids = nodes.map((node) => node.id);

  function requireNode(id: string): GraphNode {
    const node = byId.get(id);
    if (!node) {
      throw new UnknownGraphNodeError(id);
    }
    return node;
  }

  return {
    ids: () => ids,
    node: requireNode,
    deps: (id) => requireNode(id).dependencies,
  };
}

/**
 * Escape hatch for a tool without a built-in integration module (Nx,
 * Bazel, Pants, a custom script that prints JSON) — `docs/design/
 * dynamic-pipelines.md`'s "Graph helpers" table (`:378`): "calls `mapFn`
 * over each of the other tool's own raw nodes to produce cloud-ci's node
 * shape, then `graph.fromJson`s the result. Parsing the other tool's
 * output format is the caller's job via `mapFn` — cloud-ci does not
 * understand Nx/Bazel/Pants output itself." This is also what `turbo.plan`
 * (`src/turbo.ts`) calls internally to turn turbo's own dry-run task
 * objects into cloud-ci's node shape — not a separate, parallel mapping
 * mechanism.
 */
export function fromGraph<RawNode>(
  rawNodes: readonly RawNode[],
  mapFn: (rawNode: RawNode) => GraphNode,
): Graph {
  return fromJson(rawNodes.map(mapFn));
}

/** `import { graph } from "@cloud-ci/pipeline-sdk"` — matches the design
 * doc's call sites (`graph.fromJson(nodes)`, `graph.fromGraph(rawNodes,
 * mapFn)`) exactly: a namespace object, not two separate named exports,
 * since nothing in the design doc ever imports `fromJson`/`fromGraph`
 * unqualified. */
export const graph = { fromJson, fromGraph };
