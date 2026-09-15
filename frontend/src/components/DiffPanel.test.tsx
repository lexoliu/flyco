import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";

import DiffPanel, { type DiffPanelProps } from "./DiffPanel";
import { LARGE_DIFF_LINES } from "../lib/patch";
import type { FileDiff, SessionRepo, WorkdirDiff } from "../api/client";

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

const REPO: SessionRepo = {
  slug: "octocat/hello-world",
  branch: "main",
  dir: "hello-world",
  added_by: "user",
};

const SECOND: SessionRepo = {
  slug: "octocat/wiki",
  branch: "main",
  dir: "wiki",
  added_by: "user",
};

/** The `repo` argument every `getSessionDiff` call was made with. */
const diffCalls: (string | undefined)[] = [];

vi.mock("../api/client", async () => {
  const actual = await vi.importActual<typeof import("../api/client")>("../api/client");
  return {
    ...actual,
    getSessionDiff: async (_id: string, repo?: string) => {
      diffCalls.push(repo);
      return diff;
    },
    getRepoStatus: async () => ({
      checkouts: [{ dir: "hello-world", dirty: true, summary: " M src/lib.rs" }],
    }),
  };
});

function mount(overrides: Partial<DiffPanelProps> = {}) {
  return render(() => (
    <DiffPanel sessionId="session-1" repos={[REPO]} devMachine={false} {...overrides} />
  ));
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

  it("diffs the primary checkout on a provisioned session", async () => {
    diffCalls.length = 0;
    mount();

    // The workspace root is not a repository, so the request always names
    // a checkout's dir; the default is the primary.
    await vi.waitFor(() => expect(diffCalls).toContain("hello-world"));
    expect(diffCalls).not.toContain(undefined);
  });

  it("names no checkout on a developer machine, whose workdir is the checkout", async () => {
    diffCalls.length = 0;
    mount({ devMachine: true });

    await vi.waitFor(() => expect(diffCalls.length).toBeGreaterThan(0));
    expect(diffCalls).toEqual([undefined]);
  });

  it("switches checkouts from the row of tabs", async () => {
    diffCalls.length = 0;
    const { findByRole } = mount({ repos: [REPO, SECOND] });

    const tab = await findByRole("tab", { name: "octocat/wiki" });
    expect(tab).toHaveAttribute("aria-selected", "false");
    tab.click();

    expect(tab).toHaveAttribute("aria-selected", "true");
    await vi.waitFor(() => expect(diffCalls).toContain("wiki"));
  });
});
