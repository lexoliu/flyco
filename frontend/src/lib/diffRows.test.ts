import { describe, expect, it } from "vitest";
import { applyFindReplace, diffRows } from "./diffRows";

describe("applyFindReplace", () => {
  it("replaces the first occurrence and leaves the rest alone", () => {
    expect(applyFindReplace("run tests\nrun tests\n", "run tests", "run cargo test")).toBe(
      "run cargo test\nrun tests\n",
    );
  });

  it("reports text that is no longer in the document rather than guessing", () => {
    expect(applyFindReplace("use bun\n", "use npm", "use bun")).toBeNull();
  });
});

describe("diffRows", () => {
  it("marks the replaced line as removed and the new one as added", () => {
    const rows = diffRows("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");

    expect(rows).toEqual([
      { kind: "context", text: "alpha" },
      { kind: "removed", text: "beta" },
      { kind: "added", text: "BETA" },
      { kind: "context", text: "gamma" },
    ]);
  });

  it("renders an addition with no removal", () => {
    const rows = diffRows("alpha\n", "alpha\nbeta\n");

    expect(rows).toEqual([
      { kind: "context", text: "alpha" },
      { kind: "added", text: "beta" },
    ]);
  });

  it("produces no rows at all when nothing changed", () => {
    expect(diffRows("same\n", "same\n")).toEqual([]);
  });

  it("elides the untouched middle of a long document into one gap", () => {
    const before = Array.from({ length: 30 }, (_, i) => `line ${i}`).join("\n");
    const after = before.replace("line 0", "LINE 0");

    const rows = diffRows(before, after, 2);

    expect(rows).toEqual([
      { kind: "removed", text: "line 0" },
      { kind: "added", text: "LINE 0" },
      { kind: "context", text: "line 1" },
      { kind: "context", text: "line 2" },
      { kind: "gap", hidden: 27 },
    ]);
  });

  it("keeps context around each change and gaps only between them", () => {
    const before = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"].join("\n");
    const after = before.replace("a", "A").replace("j", "J");

    const rows = diffRows(before, after, 1);

    expect(rows.filter((row) => row.kind === "gap")).toEqual([{ kind: "gap", hidden: 6 }]);
    expect(rows.flatMap((row) => (row.kind === "added" ? [row.text] : []))).toEqual(["A", "J"]);
  });
});
