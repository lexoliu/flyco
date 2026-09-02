/**
 * Turning one file's unified diff into the rows a reviewer reads.
 *
 * The daemon answers `GET /v1/sessions/{id}/diff` with git's own patch text
 * per file, hunk headers included. Parsing it is `diff`'s job — the same
 * library `src/lib/diffRows.ts` uses for the other direction — because a
 * unified diff has more corners than it looks like it has (`\ No newline at
 * end of file`, empty context lines written as `""`, hunk counts that are
 * omitted when they are 1) and a hand-rolled parser would get two of them
 * right.
 *
 * The rows are the same [`DiffRow`](./diffRows) the approval diff renders,
 * so both diffs in the product are one component with one set of colours.
 */
import { parsePatch } from "diff";

import type { DiffRow } from "./diffRows";

/**
 * How many lines a file's diff may have before it is gated behind a button
 * (docs/ux.md §9).
 *
 * A review of a two-thousand-line diff in a drawer is not a review, and
 * rendering one costs a browser more than the user got out of it. The gate
 * is per file, so a change that touched one generated lockfile and six
 * source files still opens the six.
 */
export const LARGE_DIFF_LINES = 900;

/**
 * Reads one file's unified diff into rows.
 *
 * A patch git wrote but `diff` cannot read produces no rows rather than an
 * exception: the diff is one file of many, and one unreadable patch must
 * not take the whole tab down with it.
 */
export function patchRows(patch: string): DiffRow[] {
  return rowsOf(patch);
}

/**
 * The start line as *git* wrote it.
 *
 * `diff` normalizes a side with no lines — a new or deleted file — so that
 * its start points at the line after which content is inserted, which is
 * one past what the header said. Undoing that keeps the header this view
 * shows identical to the one `git diff` prints, so a hunk can be found
 * again in a terminal.
 */
function headerStart(start: number, lines: number): number {
  return lines === 0 ? start - 1 : start;
}

function rowsOf(patch: string): DiffRow[] {
  let files;
  try {
    files = parsePatch(patch);
  } catch {
    return [];
  }

  const rows: DiffRow[] = [];
  for (const file of files) {
    for (const hunk of file.hunks) {
      rows.push({
        kind: "hunk",
        text: `@@ -${headerStart(hunk.oldStart, hunk.oldLines)},${hunk.oldLines} ` +
          `+${headerStart(hunk.newStart, hunk.newLines)},${hunk.newLines} @@`,
      });
      let oldLine = hunk.oldStart;
      let newLine = hunk.newStart;
      for (const line of hunk.lines) {
        // "\ No newline at end of file" is git talking about the patch, not
        // a line of the file, and showing it as context would put a line in
        // the view that is not in the file.
        if (line.startsWith("\\")) {
          continue;
        }
        const text = line.slice(1);
        if (line.startsWith("+")) {
          rows.push({ kind: "added", text, newNumber: newLine });
          newLine += 1;
        } else if (line.startsWith("-")) {
          rows.push({ kind: "removed", text, oldNumber: oldLine });
          oldLine += 1;
        } else {
          rows.push({ kind: "context", text, oldNumber: oldLine, newNumber: newLine });
          oldLine += 1;
          newLine += 1;
        }
      }
    }
  }
  return rows;
}
