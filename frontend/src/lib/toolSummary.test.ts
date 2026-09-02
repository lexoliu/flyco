import { describe, expect, it } from "vitest";
import { summarizeTool } from "./toolSummary";

describe("summarizeTool", () => {
  it("writes the three summaries docs/ux.md §9.2 names", () => {
    expect(summarizeTool("Read", { file_path: "src/main.rs" })).toBe("Read src/main.rs");
    expect(summarizeTool("Bash", { command: "cargo test" })).toBe("Ran cargo test");
    expect(
      summarizeTool("MultiEdit", {
        file_path: "src/lib.rs",
        edits: [{ old_string: "a" }, { old_string: "b" }, { old_string: "c" }],
      }),
    ).toBe("Edited 3 files");
  });

  it("matches a tool name however the harness cases it", () => {
    expect(summarizeTool("bash", { command: "ls" })).toBe("Ran ls");
    expect(summarizeTool("BASH", { command: "ls" })).toBe("Ran ls");
  });

  it("summarises Codex's tools by the same rules", () => {
    expect(summarizeTool("shell", { command: "cargo fmt" })).toBe("Ran cargo fmt");
    expect(summarizeTool("view_image", { path: "shot.png" })).toBe("Viewed shot.png");
  });

  it("prefers the command over a description that merely narrates it", () => {
    expect(summarizeTool("Bash", { command: "cargo build", description: "Build the crate" })).toBe(
      "Ran cargo build",
    );
  });

  it("names a single-edit call by its file rather than counting to one", () => {
    expect(summarizeTool("MultiEdit", { file_path: "src/lib.rs", edits: [{ old_string: "a" }] })).toBe(
      "Edited src/lib.rs",
    );
  });

  it("drops the object for a tool that is entirely its own verb", () => {
    expect(summarizeTool("TodoWrite", { todos: [{ content: "ship it" }] })).toBe("Updated the plan");
    expect(summarizeTool("update_plan", { plan: [] })).toBe("Updated the plan");
  });

  it("leads with the tool's own name when it is not one we know", () => {
    // An MCP server's tool. Guessing a verb for it would be an invention;
    // its name plus its argument is everything the transcript knows.
    expect(summarizeTool("linear__create_issue", { title: "Relay drops frames" })).toBe(
      "linear__create_issue",
    );
    expect(summarizeTool("deploy", { name: "dev.flyco.dev" })).toBe("deploy dev.flyco.dev");
  });

  it("falls back to the verb alone when the input names nothing", () => {
    expect(summarizeTool("Read", {})).toBe("Read");
    expect(summarizeTool("Bash", null)).toBe("Ran");
    expect(summarizeTool("Bash", { command: "   " })).toBe("Ran");
  });

  it("survives an input that is not an object at all", () => {
    expect(summarizeTool("Bash", "ls -la")).toBe("Ran ls -la");
    expect(summarizeTool("Bash", 42)).toBe("Ran");
    expect(summarizeTool("Bash", ["ls"])).toBe("Ran");
  });

  it("collapses a multi-line command onto one line", () => {
    expect(summarizeTool("Bash", { command: "cargo test \\\n  --workspace" })).toBe(
      "Ran cargo test \\ --workspace",
    );
  });

  it("truncates a command too long for a one-line row", () => {
    const long = "x".repeat(200);
    const summary = summarizeTool("Bash", { command: long });
    expect(summary.length).toBeLessThanOrEqual("Ran ".length + 72);
    expect(summary.endsWith("…")).toBe(true);
  });
});
