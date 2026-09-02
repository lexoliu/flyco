import { describe, expect, it, vi } from "vitest";
import { render, waitFor } from "@solidjs/testing-library";

import FileTree from "./FileTree";
import type { DirectoryListing } from "../api/client";

/** The listings the fake control plane answers with, by path. */
const LISTINGS: Record<string, DirectoryListing> = {
  "": {
    path: "",
    truncated: false,
    entries: [
      { name: "src", path: "src", kind: "directory", size_bytes: null, ignored: false },
      { name: "target", path: "target", kind: "directory", size_bytes: null, ignored: true },
      { name: "README.md", path: "README.md", kind: "file", size_bytes: 812, ignored: false },
    ],
  },
  src: {
    path: "src",
    truncated: false,
    entries: [{ name: "lib.rs", path: "src/lib.rs", kind: "file", size_bytes: 4_211, ignored: false }],
  },
};

const listSessionFiles = vi.fn(async (_session: string, path: string) => {
  const listing = LISTINGS[path];
  if (listing === undefined) {
    throw new Error(`no fixture for \`${path}\``);
  }
  return listing;
});

vi.mock("../api/client", async () => {
  const actual = await vi.importActual<typeof import("../api/client")>("../api/client");
  return {
    ...actual,
    listSessionFiles: (session: string, path: string) => listSessionFiles(session, path),
  };
});

function mount(onOpen: (path: string) => void = () => undefined) {
  return render(() => <FileTree sessionId="session-1" onOpen={onOpen} />);
}

describe("FileTree", () => {
  it("lists a directory with folders before files", async () => {
    const { findByText, getAllByRole } = mount();
    await findByText("src");

    const names = getAllByRole("button").map((row) => row.textContent);
    expect(names).toEqual(["src", "targetignored", "README.md"]);
  });

  it("marks what git ignores rather than hiding it", async () => {
    const { findByText } = mount();

    // A `target/` is exactly what a user goes looking for when something is
    // wrong; the tree shows it and says what it is.
    expect(await findByText("target")).toBeInTheDocument();
    expect(await findByText("ignored")).toBeInTheDocument();
  });

  it("reads a directory only once it is opened", async () => {
    listSessionFiles.mockClear();
    const { findByRole } = mount();

    const folder = await findByRole("button", { name: "src" });
    expect(listSessionFiles.mock.calls.map((call) => call[1])).toEqual([""]);
    expect(folder).toHaveAttribute("aria-expanded", "false");

    folder.click();

    await waitFor(() => {
      expect(listSessionFiles.mock.calls.map((call) => call[1])).toEqual(["", "src"]);
    });
    expect(await findByRole("button", { name: "lib.rs" })).toBeInTheDocument();
  });

  it("opens a file by its whole path", async () => {
    const opened: string[] = [];
    const { findByRole } = mount((path) => opened.push(path));

    (await findByRole("button", { name: "README.md" })).click();

    expect(opened).toEqual(["README.md"]);
  });
});
