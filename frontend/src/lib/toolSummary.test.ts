import { describe, expect, it } from "vitest";
import { summarizeTool } from "./toolSummary";

/** A call the harness reported as finished and fine, which most of these are. */
function worked(tool: string, input: unknown): string {
  return summarizeTool(tool, input, true);
}

describe("summarizeTool", () => {
  it("writes the three summaries docs/ux.md §9.2 names", () => {
    expect(worked("Read", { file_path: "src/main.rs" })).toBe("Read src/main.rs");
    expect(worked("Bash", { command: "cargo test" })).toBe("Ran cargo test");
    expect(
      worked("MultiEdit", {
        file_path: "src/lib.rs",
        edits: [{ old_string: "a" }, { old_string: "b" }, { old_string: "c" }],
      }),
    ).toBe("Edited 3 files");
  });

  it("matches a tool name however the harness cases it", () => {
    expect(worked("bash", { command: "ls" })).toBe("Ran ls");
    expect(worked("BASH", { command: "ls" })).toBe("Ran ls");
  });

  it("summarises Codex's tools by the same rules", () => {
    expect(worked("shell", { command: "cargo fmt" })).toBe("Ran cargo fmt");
    expect(worked("view_image", { path: "shot.png" })).toBe("Viewed shot.png");
  });

  it("prefers the command over a description that merely narrates it", () => {
    expect(worked("Bash", { command: "cargo build", description: "Build the crate" })).toBe(
      "Ran cargo build",
    );
  });

  it("names a single-edit call by its file rather than counting to one", () => {
    expect(worked("MultiEdit", { file_path: "src/lib.rs", edits: [{ old_string: "a" }] })).toBe(
      "Edited src/lib.rs",
    );
  });

  it("drops the object for a tool that is entirely its own verb", () => {
    expect(worked("TodoWrite", { todos: [{ content: "ship it" }] })).toBe("Updated the plan");
    expect(worked("update_plan", { plan: [] })).toBe("Updated the plan");
  });

  it("leads with the tool's own name when it is not one we know", () => {
    // An MCP server's tool. Guessing a verb for it would be an invention;
    // its name plus its argument is everything the transcript knows.
    expect(worked("linear__create_issue", { title: "Relay drops frames" })).toBe(
      "linear__create_issue",
    );
    expect(worked("deploy", { name: "dev.flyco.dev" })).toBe("deploy dev.flyco.dev");
  });

  it("falls back to the verb alone when the input names nothing", () => {
    expect(worked("Read", {})).toBe("Read");
    expect(worked("Bash", null)).toBe("Ran");
    expect(worked("Bash", { command: "   " })).toBe("Ran");
  });

  it("survives an input that is not an object at all", () => {
    expect(worked("Bash", "ls -la")).toBe("Ran ls -la");
    expect(worked("Bash", 42)).toBe("Ran");
    expect(worked("Bash", ["ls"])).toBe("Ran");
  });

  it("collapses a multi-line command onto one line", () => {
    expect(worked("Bash", { command: "cargo test \\\n  --workspace" })).toBe(
      "Ran cargo test \\ --workspace",
    );
  });

  it("truncates a command too long for a one-line row", () => {
    const long = "x".repeat(200);
    const summary = worked("Bash", { command: long });
    expect(summary.length).toBeLessThanOrEqual("Ran ".length + 72);
    expect(summary.endsWith("…")).toBe(true);
  });
});

describe("summarizeTool, on a call that failed", () => {
  it("says the tool did not do the thing, rather than that it did", () => {
    // The row used to read `Edited crates/daemon/src/compaction.rs` with a
    // small ✕ beside it, which reads as a success to anybody who does not
    // study the icon (issue #136).
    expect(summarizeTool("Edit", { file_path: "crates/daemon/src/compaction.rs" }, false)).toBe(
      "Could not edit crates/daemon/src/compaction.rs",
    );
    expect(summarizeTool("Bash", { command: "cargo test" }, false)).toBe("Could not run cargo test");
    expect(summarizeTool("Read", { file_path: "src/main.rs" }, false)).toBe(
      "Could not read src/main.rs",
    );
  });

  it("names a tool whose whole meaning is its verb", () => {
    expect(summarizeTool("TodoWrite", { todos: [] }, false)).toBe("Could not update the plan");
  });

  it("counts the files an edit did not make", () => {
    expect(
      summarizeTool("MultiEdit", { edits: [{ old_string: "a" }, { old_string: "b" }] }, false),
    ).toBe("Could not edit 2 files");
  });

  it("keeps an unknown tool's own name, and still says the call failed", () => {
    expect(summarizeTool("deploy", { name: "dev.flyco.dev" }, false)).toBe(
      "Could not call deploy dev.flyco.dev",
    );
  });

  it("reads a call still running the way a finished one reads", () => {
    // Nothing has failed yet, and a row that hedged would be motion.
    expect(summarizeTool("Bash", { command: "cargo test" }, null)).toBe("Ran cargo test");
  });
});
