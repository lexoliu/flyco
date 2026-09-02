import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";

import DiffPanel from "./DiffPanel";
import { LARGE_DIFF_LINES } from "../lib/patch";
import type { FileDiff, WorkdirDiff } from "../api/client";

/** A patch of `lines` added lines, as git would write it. */
function patchOf(path: string, lines: number): string {
  const added = Array.from({ length: lines }, (_, index) => `+line ${index}`).join("\n");
  return [
    `diff --git a/${path} b/${path}`,
    "new file mode 100644",
    "--- /dev/null",
    `+++ b/${path}`,
    `@@ -0,0 +1,${lines} @@`,
    added,
    "",
  ].join("\n");
}

function file(overrides: Partial<FileDiff> & { path: string }): FileDiff {
  return {
    previous_path: null,
    change: "modified",
    added_lines: 0,
    removed_lines: 0,
    binary: false,
    patch: null,
    ...overrides,
  };
}

let diff: WorkdirDiff = {
  base: "origin/main",
  files: [],
  added_lines: 0,
  removed_lines: 0,
  truncated: false,
};

vi.mock("../api/client", async () => {
  const actual = await vi.importActual<typeof import("../api/client")>("../api/client");
  return {
    ...actual,
    getSessionDiff: async () => diff,
    getRepoStatus: async () => ({ dirty: true, summary: " M src/lib.rs" }),
  };
});

function mount() {
  return render(() => <DiffPanel sessionId="session-1" liveRepoSummary={null} />);
}

describe("DiffPanel", () => {
  it("says nothing has changed when nothing has", async () => {
    diff = { base: "origin/main", files: [], added_lines: 0, removed_lines: 0, truncated: false };
    const { findByText } = mount();

    expect(await findByText(/Nothing has changed/)).toBeInTheDocument();
  });

  it("heads every file with what it cost in lines", async () => {
    diff = {
      base: "origin/main",
      files: [
        file({
          path: "src/lib.rs",
          change: "modified",
          added_lines: 12,
          removed_lines: 3,
          patch: patchOf("src/lib.rs", 2),
        }),
        file({ path: "logo.png", change: "added", binary: true }),
      ],
      added_lines: 12,
      removed_lines: 3,
      truncated: false,
    };
    const { findByRole, getByText } = mount();

    const header = await findByRole("button", { name: /src\/lib\.rs/ });
    expect(header.textContent).toContain("Modified");
    expect(header.textContent).toContain("+12");
    expect(header.textContent).toContain("−3");
    expect(getByText(/2 files against/)).toBeInTheDocument();
    expect(getByText("origin/main")).toBeInTheDocument();
  });

  it("keeps a file collapsed until it is asked for", async () => {
    diff = {
      base: "origin/main",
      files: [
        file({
          path: "src/lib.rs",
          added_lines: 2,
          removed_lines: 0,
          patch: patchOf("src/lib.rs", 2),
        }),
      ],
      added_lines: 2,
      removed_lines: 0,
      truncated: false,
    };
    const { findByRole, queryByRole, getByRole } = mount();

    const header = await findByRole("button", { name: /src\/lib\.rs/ });
    expect(header).toHaveAttribute("aria-expanded", "false");
    expect(queryByRole("group", { name: "Diff of src/lib.rs" })).toBeNull();

    header.click();

    const view = getByRole("group", { name: "Diff of src/lib.rs" });
    expect(view.textContent).toContain("+line 0");
    expect(view.textContent).toContain("@@ -0,0 +1,2 @@");
  });

  it("gates a large diff behind its line count", async () => {
    const lines = LARGE_DIFF_LINES + 10;
    diff = {
      base: "origin/main",
      files: [
        file({
          path: "bun.lock",
          change: "added",
          added_lines: lines,
          removed_lines: 0,
          patch: patchOf("bun.lock", lines),
        }),
      ],
      added_lines: lines,
      removed_lines: 0,
      truncated: false,
    };
    const { findByRole, getByRole, getByText, queryByRole } = mount();

    (await findByRole("button", { name: /bun\.lock/ })).click();

    // The hunk header counts too, which is why the gate quotes the rows it
    // would render rather than the file's own line count.
    expect(getByText(`Large diff · ${(lines + 1).toLocaleString()} lines`)).toBeInTheDocument();
    expect(queryByRole("group", { name: "Diff of bun.lock" })).toBeNull();

    getByRole("button", { name: "Load diff" }).click();

    expect(getByRole("group", { name: "Diff of bun.lock" })).toBeInTheDocument();
  });

  it("says why a binary file has no diff to show", async () => {
    diff = {
      base: "origin/main",
      files: [file({ path: "logo.png", change: "added", binary: true })],
      added_lines: 0,
      removed_lines: 0,
      truncated: false,
    };
    const { findByRole, getByText } = mount();

    (await findByRole("button", { name: /logo\.png/ })).click();

    expect(getByText(/Binary file/)).toBeInTheDocument();
  });
});
