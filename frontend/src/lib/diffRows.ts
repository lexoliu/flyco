/**
 * Turning two versions of a document into the rows a reviewer reads.
 *
 * An agent that wants to change the shared `AGENTS.md` raises an approval
 * carrying a find/replace pair, not a document. The user is being asked to
 * approve a *change*, so the UI has to show the change: this module applies
 * the pair to the document the user actually has, diffs before against
 * after, and trims the untouched middle so a two-line edit to a
 * two-hundred-line file reads as a two-line edit.
 *
 * The line diff itself comes from `diff`, which is the reference
 * implementation of Myers' algorithm on npm — hand-rolling one here would
 * be strictly worse code doing strictly the same job.
 */
import { diffLines } from "diff";

/**
 * One rendered row of a diff.
 *
 * Shared by the two diffs in the product: the approval diff below, which
 * compares two documents the browser holds, and the session diff
 * (`src/lib/patch.ts`), which reads git's own patch text. The line numbers
 * are optional because only the second one has any: an approval is a
 * proposed change to a document, not a change at a position in a file.
 */
export type DiffRow =
  | {
      readonly kind: "context" | "added" | "removed";
      readonly text: string;
      /** Line number on the left, when the row exists there. */
      readonly oldNumber?: number;
      /** Line number on the right, when the row exists there. */
      readonly newNumber?: number;
    }
  /** A hunk header, verbatim, marking a jump to another part of the file. */
  | { readonly kind: "hunk"; readonly text: string }
  /** A run of unchanged lines that was elided, and how many it stood for. */
  | { readonly kind: "gap"; readonly hidden: number };

/** How many unchanged lines are kept on each side of a change. */
export const DEFAULT_CONTEXT_LINES = 3;

/**
 * Applies an agent's find/replace to a document.
 *
 * Returns `null` when `find` no longer appears — the document moved on
 * since the agent read it, and the honest thing to tell the user is that
 * the change no longer applies rather than to show a diff of nothing.
 * Only the first occurrence is replaced, matching what the daemon does.
 */
export function applyFindReplace(
  content: string,
  find: string,
  replace: string,
): string | null {
  const at = content.indexOf(find);
  if (at === -1) {
    return null;
  }
  return content.slice(0, at) + replace + content.slice(at + find.length);
}

/** Splits a chunk of text into lines, dropping the empty tail a trailing newline leaves. */
function linesOf(text: string): string[] {
  const lines = text.split("\n");
  if (lines.length > 1 && lines.at(-1) === "") {
    lines.pop();
  }
  return lines;
}

/**
 * Diffs `before` against `after`, line by line, keeping `context` unchanged
 * lines around every change and eliding the rest into `gap` rows.
 *
 * Two identical documents produce no rows at all, which is what lets a
 * caller say "this changes nothing" instead of rendering an empty frame.
 */
export function diffRows(
  before: string,
  after: string,
  context: number = DEFAULT_CONTEXT_LINES,
): DiffRow[] {
  const changes = diffLines(before, after);
  const flat: DiffRow[] = [];
  for (const change of changes) {
    const kind = change.added === true ? "added" : change.removed === true ? "removed" : "context";
    for (const text of linesOf(change.value)) {
      flat.push({ kind, text });
    }
  }
  if (!flat.some((row) => row.kind === "added" || row.kind === "removed")) {
    return [];
  }
  return elideContext(flat, context);
}

/** Replaces every run of more than `2 * context` unchanged rows with a gap. */
function elideContext(rows: readonly DiffRow[], context: number): DiffRow[] {
  const keep = new Array<boolean>(rows.length).fill(false);
  rows.forEach((row, index) => {
    if (row.kind === "context") {
      return;
    }
    for (let i = Math.max(0, index - context); i <= Math.min(rows.length - 1, index + context); i += 1) {
      keep[i] = true;
    }
  });

  const out: DiffRow[] = [];
  let hidden = 0;
  rows.forEach((row, index) => {
    if (keep[index] === true) {
      if (hidden > 0) {
        out.push({ kind: "gap", hidden });
        hidden = 0;
      }
      out.push(row);
    } else {
      hidden += 1;
    }
  });
  if (hidden > 0) {
    out.push({ kind: "gap", hidden });
  }
  return out;
}
