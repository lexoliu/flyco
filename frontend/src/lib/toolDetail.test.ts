import { describe, expect, it } from "vitest";
import { detailOfTool } from "./toolDetail";

describe("detailOfTool", () => {
  it("opens a shell call into the command, as shell", () => {
    // What the row used to show was the wire format: braces, quoted keys,
    // and `\\"` around every shell quote the command actually contained.
    const detail = detailOfTool("Bash", {
      command: 'basename "$(pwd)"; basename /srv/flyco/work',
      description: "Show top-level directory name",
    });
    expect(detail.code).toEqual({
      language: "bash",
      text: 'basename "$(pwd)"; basename /srv/flyco/work',
    });
  });

  it("never repeats the sentence the collapsed row already read", () => {
    const detail = detailOfTool("Bash", {
      command: "cargo test",
      description: "Run the workspace tests",
    });
    expect(detail.fields).toEqual([]);
  });

  it("joins the argv Codex sends a shell call as", () => {
    expect(detailOfTool("shell", { command: ["bash", "-lc", "cargo fmt"] }).code).toEqual({
      language: "bash",
      text: "bash -lc cargo fmt",
    });
  });

  it("reads a patch as a diff", () => {
    const patch = "--- a/x\n+++ b/x\n-old\n+new";
    expect(detailOfTool("apply_patch", { patch }).code).toEqual({
      language: "diff",
      text: patch,
    });
  });

  it("names the values of a call that carries no source", () => {
    const detail = detailOfTool("Edit", {
      file_path: "src/main.rs",
      old_string: "a",
      new_string: "b",
    });
    expect(detail.code).toBeNull();
    expect(detail.fields).toEqual([
      { label: "File", value: "src/main.rs" },
      { label: "Replacing", value: "a" },
      { label: "With", value: "b" },
    ]);
  });

  it("gives an unnamed key a label rather than showing the key", () => {
    expect(detailOfTool("Task", { subagent_type: "Explore" }).fields).toEqual([
      { label: "Agent", value: "Explore" },
    ]);
    expect(detailOfTool("deploy", { targetEnvironment: "dev" }).fields).toEqual([
      { label: "Target environment", value: "dev" },
    ]);
  });

  it("joins a list of scalars and counts a list of anything else", () => {
    expect(detailOfTool("Grep", { paths: ["src", "tests"] }).fields).toEqual([
      { label: "Paths", value: "src, tests" },
    ]);
    expect(
      detailOfTool("MultiEdit", { edits: [{ old_string: "a" }, { old_string: "b" }] }).fields,
    ).toEqual([{ label: "Edits", value: "2 items" }]);
  });

  it("counts a structure too deep to read rather than unfolding it", () => {
    const detail = detailOfTool("mcp__thing__call", {
      request: { body: { deep: { deeper: 1 } } },
    });
    expect(detail.fields).toEqual([
      { label: "Request · Body · Deep", value: "1 field" },
    ]);
  });

  it("says so plainly when a call carried nothing", () => {
    expect(detailOfTool("TodoWrite", {})).toEqual({ code: null, fields: [] });
    expect(detailOfTool("Bash", null)).toEqual({ code: null, fields: [] });
  });

  it("treats a bare string as the source of a tool that takes source", () => {
    expect(detailOfTool("Bash", "ls -la").code).toEqual({
      language: "bash",
      text: "ls -la",
    });
    expect(detailOfTool("Read", "src/main.rs").fields).toEqual([
      { label: "Read", value: "src/main.rs" },
    ]);
  });
});
