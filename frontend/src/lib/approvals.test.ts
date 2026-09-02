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
      detail: "lexoliu/flyco: feat/issue-65 → dev",
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
    expect(asked.detail).toContain(
      "Starts a 24-hour minimum charge of $15.60 the moment it boots.",
    );
  });

  it("shows the agent's own reason for wanting the machine", () => {
    const asked = operation({
      kind: "machine_resize_license_bound",
      machine_type: "mac2.metal",
      minimum: { hours: 24, charge: 15_600_000 },
      reason: "the build needs a signed macOS toolchain",
    });

    expect(asked.detail).toContain("The agent says: the build needs a signed macOS toolchain");
    expect(asked.detail).toContain("Resizing restarts the machine; the disk is kept.");
  });

  it("shows a tool call's input as it would run", () => {
    expect(operation({ kind: "tool_use", tool: "Bash", input: { command: "ls" } })).toEqual({
      title: "Run Bash",
      detail: JSON.stringify({ command: "ls" }, null, 2),
    });
  });
});
