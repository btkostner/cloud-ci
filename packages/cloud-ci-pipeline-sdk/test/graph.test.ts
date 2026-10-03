import { describe, expect, it } from "vitest";
import {
  DuplicateGraphNodeIdError,
  UnknownGraphDependencyError,
  UnknownGraphNodeError,
} from "../src/errors.js";
import type { GraphNode } from "../src/graph.js";
import { graph } from "../src/graph.js";

const nodeA: GraphNode = {
  id: "pkg-a#build",
  package: "pkg-a",
  task: "build",
  hash: "hash-a",
  dependencies: [],
  outputs: ["dist/**"],
};

const nodeB: GraphNode = {
  id: "pkg-b#build",
  package: "pkg-b",
  task: "build",
  hash: "hash-b",
  dependencies: ["pkg-a#build"],
  outputs: ["dist/**"],
};

describe("graph.fromJson", () => {
  it("exposes ids(), node(id), and deps(id)", () => {
    const g = graph.fromJson([nodeA, nodeB]);

    expect(g.ids()).toEqual(["pkg-a#build", "pkg-b#build"]);
    expect(g.node("pkg-b#build")).toEqual(nodeB);
    expect(g.deps("pkg-b#build")).toEqual(["pkg-a#build"]);
    expect(g.deps("pkg-a#build")).toEqual([]);
  });

  it("ids() preserves the input array's order", () => {
    const g = graph.fromJson([nodeB, nodeA]);
    expect(g.ids()).toEqual(["pkg-b#build", "pkg-a#build"]);
  });

  it("throws DuplicateGraphNodeIdError for two nodes sharing an id", () => {
    expect(() => graph.fromJson([nodeA, { ...nodeA, hash: "different" }])).toThrow(
      DuplicateGraphNodeIdError,
    );
  });

  it("throws UnknownGraphDependencyError when a dependency id is not in the array", () => {
    const danglingDep: GraphNode = {
      id: "pkg-c#build",
      package: "pkg-c",
      task: "build",
      hash: "hash-c",
      dependencies: ["pkg-does-not-exist#build"],
      outputs: [],
    };
    expect(() => graph.fromJson([danglingDep])).toThrow(UnknownGraphDependencyError);
  });

  it("node(id) throws UnknownGraphNodeError for an id outside the graph", () => {
    const g = graph.fromJson([nodeA]);
    expect(() => g.node("not-a-real-id")).toThrow(UnknownGraphNodeError);
  });

  it("deps(id) throws UnknownGraphNodeError for an id outside the graph", () => {
    const g = graph.fromJson([nodeA]);
    expect(() => g.deps("not-a-real-id")).toThrow(UnknownGraphNodeError);
  });

  it("accepts an empty node array", () => {
    const g = graph.fromJson([]);
    expect(g.ids()).toEqual([]);
  });
});

describe("graph.fromGraph", () => {
  interface RawNxNode {
    readonly nodeId: string;
    readonly project: string;
    readonly target: string;
    readonly computedHash: string;
    readonly deps: readonly string[];
  }

  it("maps raw nodes through mapFn before delegating to fromJson", () => {
    const rawNodes: RawNxNode[] = [
      { nodeId: "a:build", project: "a", target: "build", computedHash: "h1", deps: [] },
      { nodeId: "b:build", project: "b", target: "build", computedHash: "h2", deps: ["a:build"] },
    ];

    const g = graph.fromGraph(rawNodes, (raw) => ({
      id: raw.nodeId,
      package: raw.project,
      task: raw.target,
      hash: raw.computedHash,
      dependencies: raw.deps,
      outputs: [],
    }));

    expect(g.ids()).toEqual(["a:build", "b:build"]);
    expect(g.node("b:build")).toEqual({
      id: "b:build",
      package: "b",
      task: "build",
      hash: "h2",
      dependencies: ["a:build"],
      outputs: [],
    });
    expect(g.deps("b:build")).toEqual(["a:build"]);
  });

  it("surfaces fromJson's validation errors (duplicate id) through the mapped output", () => {
    const rawNodes: RawNxNode[] = [
      { nodeId: "a:build", project: "a", target: "build", computedHash: "h1", deps: [] },
      { nodeId: "a:build", project: "a", target: "build", computedHash: "h1-again", deps: [] },
    ];

    expect(() =>
      graph.fromGraph(rawNodes, (raw) => ({
        id: raw.nodeId,
        package: raw.project,
        task: raw.target,
        hash: raw.computedHash,
        dependencies: raw.deps,
        outputs: [],
      })),
    ).toThrow(DuplicateGraphNodeIdError);
  });
});
