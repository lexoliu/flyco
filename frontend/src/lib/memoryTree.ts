/**
 * The shape of the memory outliner, kept apart from the component drawing it.
 *
 * `GET /v1/memory?parent=` answers one level at a time, so the tree the user
 * sees is assembled from however many levels have been opened so far. That
 * assembly — which rows exist, at what depth, in what order — is pure, and
 * lives here so it can be tested without a DOM or a network.
 */
import type { MemoryNode } from "../api/client";

/** Children keyed by their parent's id; `ROOT_KEY` holds the roots. */
export type LoadedChildren = Readonly<Record<string, readonly MemoryNode[]>>;

/** The key children of the tree's top level are stored under. */
export const ROOT_KEY = "";

/** One row of the outliner: a node, how deep it sits, and its disclosure state. */
export interface TreeRow {
  readonly node: MemoryNode;
  /** 0 for a root, 1 for its children, and so on. */
  readonly depth: number;
  /** Whether this node's children are showing. */
  readonly expanded: boolean;
  /**
   * Whether this node's children have been fetched at least once.
   *
   * A node that has never been opened is drawn with a disclosure arrow
   * regardless: the API does not report child counts, and refusing to offer
   * the arrow would make a populated subtree unreachable.
   */
  readonly loaded: boolean;
}

/**
 * Flattens the opened parts of the tree into the rows an outliner renders.
 *
 * Only expanded nodes contribute children, so collapsing a node hides its
 * whole subtree without discarding what was already fetched — reopening it
 * is instant and costs no request.
 */
export function flattenTree(
  children: LoadedChildren,
  expanded: ReadonlySet<string>,
  parent: string = ROOT_KEY,
  depth = 0,
): TreeRow[] {
  const rows: TreeRow[] = [];
  for (const node of children[parent] ?? []) {
    const isExpanded = expanded.has(node.id);
    rows.push({
      node,
      depth,
      expanded: isExpanded,
      loaded: children[node.id] !== undefined,
    });
    if (isExpanded) {
      rows.push(...flattenTree(children, expanded, node.id, depth + 1));
    }
  }
  return rows;
}

/**
 * Drops a node and everything cached beneath it.
 *
 * Deleting a node deletes its subtree server-side, so leaving the
 * descendants in the cache would let a later expansion redraw rows for
 * notes that no longer exist.
 */
export function forgetSubtree(children: LoadedChildren, id: string): LoadedChildren {
  const next: Record<string, readonly MemoryNode[]> = { ...children };
  const doomed = [id];
  while (doomed.length > 0) {
    const current = doomed.pop() ?? "";
    for (const child of next[current] ?? []) {
      doomed.push(child.id);
    }
    delete next[current];
  }
  for (const [parent, siblings] of Object.entries(next)) {
    if (siblings.some((node) => node.id === id)) {
      next[parent] = siblings.filter((node) => node.id !== id);
    }
  }
  return next;
}

/** Replaces one node in place, wherever it sits, leaving the rest of the cache alone. */
export function replaceNode(children: LoadedChildren, node: MemoryNode): LoadedChildren {
  const next: Record<string, readonly MemoryNode[]> = { ...children };
  for (const [parent, siblings] of Object.entries(next)) {
    if (siblings.some((sibling) => sibling.id === node.id)) {
      next[parent] = siblings.map((sibling) => (sibling.id === node.id ? node : sibling));
    }
  }
  return next;
}
