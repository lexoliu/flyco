import { describe, expect, it } from "vitest";
import type { MemoryNode } from "../api/client";
import { ROOT_KEY, flattenTree, forgetSubtree, replaceNode } from "./memoryTree";

function node(id: string, title: string, parent: string | null = null): MemoryNode {
  return { id, title, content: "", parent, updated_at_unix: 0 };
}

const ROOTS = [node("a", "Rust"), node("b", "Deploys")];
const CHILDREN = {
  [ROOT_KEY]: ROOTS,
  a: [node("a1", "Lints", "a"), node("a2", "Testing", "a")],
  a1: [node("a1x", "Clippy", "a1")],
};

describe("flattenTree", () => {
  it("shows only the roots while nothing is expanded", () => {
    const rows = flattenTree(CHILDREN, new Set());

    expect(rows.map((row) => row.node.id)).toEqual(["a", "b"]);
    expect(rows.every((row) => row.depth === 0)).toBe(true);
  });

  it("puts an expanded node's children under it, one level deeper", () => {
    const rows = flattenTree(CHILDREN, new Set(["a"]));

    expect(rows.map((row) => [row.node.id, row.depth])).toEqual([
      ["a", 0],
      ["a1", 1],
      ["a2", 1],
      ["b", 0],
    ]);
  });

  it("nests further as deeper nodes are opened", () => {
    const rows = flattenTree(CHILDREN, new Set(["a", "a1"]));

    expect(rows.map((row) => [row.node.id, row.depth])).toEqual([
      ["a", 0],
      ["a1", 1],
      ["a1x", 2],
      ["a2", 1],
      ["b", 0],
    ]);
  });

  it("hides a subtree when its parent is collapsed, without discarding it", () => {
    const rows = flattenTree(CHILDREN, new Set(["a1"]));

    expect(rows.map((row) => row.node.id)).toEqual(["a", "b"]);
  });

  it("reports whether a node's children have been fetched", () => {
    const rows = flattenTree(CHILDREN, new Set(["a"]));

    expect(rows.find((row) => row.node.id === "a1")?.loaded).toBe(true);
    expect(rows.find((row) => row.node.id === "a2")?.loaded).toBe(false);
  });
});

describe("forgetSubtree", () => {
  it("drops the node from its parent's children", () => {
    const next = forgetSubtree(CHILDREN, "a1");

    expect((next["a"] ?? []).map((child) => child.id)).toEqual(["a2"]);
  });

  it("drops everything cached beneath it, so a stale subtree cannot reappear", () => {
    const next = forgetSubtree(CHILDREN, "a");

    expect(next["a"]).toBeUndefined();
    expect(next["a1"]).toBeUndefined();
    expect((next[ROOT_KEY] ?? []).map((child) => child.id)).toEqual(["b"]);
  });
});

describe("replaceNode", () => {
  it("swaps a renamed node in place and leaves its siblings alone", () => {
    const next = replaceNode(CHILDREN, { ...node("a1", "Lint rules", "a"), content: "be strict" });

    expect((next["a"] ?? []).map((child) => child.title)).toEqual(["Lint rules", "Testing"]);
    expect(next["a1"]).toEqual(CHILDREN.a1);
  });
});
