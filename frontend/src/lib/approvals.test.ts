import { describe, expect, it } from "vitest";
import { operation } from "./approvals";

describe("approval cards", () => {
  it("names the branches a merge would move between", () => {
    expect(
      operation({
        kind: "merge",
        repo: "lexoliu/flyco",
        from_branch: "feat/issue-65",
        into_branch: "dev",
      }),
    ).toEqual({
      title: "Merge a branch",
      detail: { kind: "text", text: "lexoliu/flyco: feat/issue-65 → dev" },
    });
  });

  it("quotes a license-bound resize in dollars rather than in hours", () => {
    // 24 hours is a fact about a licence; $15.60 is what pressing Approve
    // costs, and it has to be on the card before anybody presses it.
    const asked = operation({
      kind: "machine_resize_license_bound",
      machine_type: "mac2.metal",
      minimum: { hours: 24, charge: 15_600_000 },
      reason: "the build needs a signed macOS toolchain",
    });

    expect(asked.title).toBe("Switch to mac2.metal");
    expect(asked.detail).toEqual({
      kind: "text",
      text: expect.stringContaining("Starts a 24-hour minimum charge of $15.60 the moment it boots."),
    });
  });

  it("shows the agent's own reason for wanting the machine", () => {
    const asked = operation({
      kind: "machine_resize_license_bound",
      machine_type: "mac2.metal",
      minimum: { hours: 24, charge: 15_600_000 },
      reason: "the build needs a signed macOS toolchain",
    });

    expect(asked.detail).toEqual({
      kind: "text",
      text: expect.stringContaining("The agent says: the build needs a signed macOS toolchain"),
    });
    expect(asked.detail).toEqual({
      kind: "text",
      text: expect.stringContaining("Resizing restarts the machine; the disk is kept."),
    });
  });

  it("names the repository, branch and reason an agent wants to clone", () => {
    const asked = operation({
      kind: "repo_add",
      repo: "lexoliu/aither",
      branch: "main",
      reason: "the fix needs the shared transport it defines",
    });

    // Fetching a repository the user never picked is the whole decision,
    // so the card names it before the reason that argued for it.
    expect(asked.title).toBe("Add a repository");
    expect(asked.detail).toEqual({
      kind: "text",
      text: "Clone lexoliu/aither on main into the session's workspace.\nThe agent says: the fix needs the shared transport it defines",
    });
  });

  it("reads a tool call's input out as its arguments, not as JSON", () => {
    // The decision is about the command; braces around it make the reader
    // find it first (issue #136).
    expect(
      operation({
        kind: "tool_use",
        tool: "Bash",
        input: { command: "git push --force origin fix/flaky-compaction" },
      }),
    ).toEqual({
      title: "Run Bash",
      detail: {
        kind: "fields",
        fields: [
          {
            name: "command",
            value: "git push --force origin fix/flaky-compaction",
            block: false,
          },
        ],
      },
    });
  });

  it("keeps a string exactly as it was written, quotes and all", () => {
    const asked = operation({
      kind: "tool_use",
      tool: "Write",
      input: { file_path: "notes.md", content: "line one\nline two" },
    });

    expect(asked.detail).toEqual({
      kind: "fields",
      fields: [
        { name: "file_path", value: "notes.md", block: false },
        // Over one line, so the card gives it a block rather than a line.
        { name: "content", value: "line one\nline two", block: true },
      ],
    });
  });

  it("falls back to JSON for a value that is not a string", () => {
    const asked = operation({
      kind: "tool_use",
      tool: "Grep",
      input: { pattern: "TODO", options: { glob: "*.rs", limit: 20 } },
    });

    expect(asked.detail).toEqual({
      kind: "fields",
      fields: [
        { name: "pattern", value: "TODO", block: false },
        {
          name: "options",
          value: JSON.stringify({ glob: "*.rs", limit: 20 }, null, 2),
          block: true,
        },
      ],
    });
  });

  it("shows an input with no keys as the one value it is", () => {
    expect(operation({ kind: "tool_use", tool: "Ping", input: {} }).detail).toEqual({
      kind: "text",
      text: "{}",
    });
    expect(operation({ kind: "tool_use", tool: "Say", input: "hello" }).detail).toEqual({
      kind: "text",
      text: "hello",
    });
  });
});
