import { describe, expect, it } from "vitest";
import { render } from "@solidjs/testing-library";
import Transcript from "./Transcript";
import type { TranscriptItem } from "../lib/transcript";

const T0 = 1_800_000_000;
const RUN = "6f1c8e2a-1111-4b3a-9e1a-4c2f8b6d7a10";

/** One `!` command block, with everything overridable. */
function shell(overrides: Partial<Extract<TranscriptItem, { kind: "shell" }>> = {}) {
  const item: TranscriptItem = {
    kind: "shell",
    key: `shell-${RUN}`,
    run: RUN,
    command: "cargo test -p flyco-core",
    output: [],
    outcome: null,
    truncated: false,
    atUnix: T0,
    endedAtUnix: null,
    ...overrides,
  };
  return item;
}

function show(item: TranscriptItem, stoppedAtUnix: number | null = null) {
  return render(() => (
    <Transcript
      items={[item]}
      repo="lexoliu/flyco"
      provider="Azure"
      models={[]}
      plan={[]}
      now={T0 * 1000}
      stoppedAtUnix={stoppedAtUnix}
    />
  ));
}

describe("Transcript provisioning timeline", () => {
  /** A first machine still being reserved, ten minutes before `now`. */
  const reserving: TranscriptItem = {
    kind: "provisioning",
    key: "provisioning-0",
    steps: [{ key: `reserving-${T0 - 600}`, stage: "reserving", atUnix: T0 - 600 }],
    recovery: false,
    attempt: 1,
    endedAtUnix: null,
  };

  it("counts a stage that is still in progress against the clock", () => {
    const { container, getByLabelText } = show(reserving);

    getByLabelText("Provisioning");
    expect(container.querySelector('[data-active="true"]')).not.toBeNull();
    expect(container.querySelector('[data-failed="true"]')).toBeNull();
    expect(container.textContent).toContain("10m");
  });

  it("stops at the stage the build was on when the session stopped, with the time it had run", () => {
    // The session failed two minutes in; eight more have passed since.
    const { container, getByLabelText } = show(reserving, T0 - 480);

    getByLabelText("Provisioning stopped");
    expect(container.querySelector('[data-active="true"]')).toBeNull();
    expect(container.querySelector('[data-failed="true"]')).not.toBeNull();
    expect(container.textContent).toContain("Reserving a machine on Azure");
    expect(container.textContent).toContain("2m");
    expect(container.textContent).not.toContain("10m");
  });

  it("leaves a finished episode alone when a later one failed", () => {
    const ready: TranscriptItem = {
      ...reserving,
      steps: [
        { key: `reserving-${T0 - 600}`, stage: "reserving", atUnix: T0 - 600 },
        { key: `ready-${T0 - 540}`, stage: "ready", atUnix: T0 - 540 },
      ],
    };
    const { container, getByLabelText } = show(ready, T0 - 480);

    getByLabelText("Provisioning");
    expect(container.querySelector('[data-failed="true"]')).toBeNull();
  });
});

describe("Transcript shell block", () => {
  it("shows the command, its output and its exit status", () => {
    const { getByRole } = show(
      shell({
        output: [
          { key: 0, stream: "stdout", data: "running 1 test\n" },
          { key: 1, stream: "stderr", data: "warning: unused\n" },
        ],
        outcome: { kind: "exited", code: 0 },
        endedAtUnix: T0 + 12,
      }),
    );

    const block = getByRole("region", { name: "Shell command" });
    expect(block.textContent).toContain("cargo test -p flyco-core");
    expect(block.textContent).toContain("running 1 test");
    expect(block.textContent).toContain("Exit 0");
    // How long it took, beside the status, the way a turn says how long it
    // worked for.
    expect(block.textContent).toContain("12s");
  });

  it("marks stderr apart from stdout without hiding either", () => {
    const { getByRole } = show(
      shell({
        output: [
          { key: 0, stream: "stdout", data: "out\n" },
          { key: 1, stream: "stderr", data: "err\n" },
        ],
        outcome: { kind: "exited", code: 0 },
        endedAtUnix: T0 + 1,
      }),
    );

    const streams = [...getByRole("region").querySelectorAll("pre span")].map((chunk) => [
      chunk.getAttribute("data-stream"),
      chunk.textContent,
    ]);
    expect(streams).toEqual([
      ["stdout", "out\n"],
      ["stderr", "err\n"],
    ]);
  });

  it("says a command is still running rather than showing an empty block", () => {
    const { getByRole } = show(shell({ command: "sleep 60" }));
    const block = getByRole("region", { name: "Shell command" });
    expect(block.textContent).toContain("Running");
  });

  it("colours a failed command apart from one that worked", () => {
    const failed = show(shell({ outcome: { kind: "exited", code: 1 }, endedAtUnix: T0 + 2 }));
    expect(
      failed.getByRole("region").querySelector("[data-ok]")?.getAttribute("data-ok"),
    ).toBe("false");

    const worked = show(shell({ outcome: { kind: "exited", code: 0 }, endedAtUnix: T0 + 2 }));
    expect(
      worked.getByRole("region").querySelector("[data-ok]")?.getAttribute("data-ok"),
    ).toBe("true");
  });

  it("says when output was dropped rather than eliding it silently", () => {
    const { getByRole } = show(
      shell({
        output: [{ key: 0, stream: "stdout", data: "a lot of output\n" }],
        truncated: true,
        outcome: { kind: "exited", code: 0 },
        endedAtUnix: T0 + 3,
      }),
    );
    expect(getByRole("region").textContent).toContain("dropped");
  });

  it("explains a command that never ran", () => {
    const { getByRole } = show(
      shell({ outcome: { kind: "offline" }, endedAtUnix: T0 }),
    );
    expect(getByRole("region").textContent).toContain("the machine was not connected");
  });
});

/** One turn, with everything overridable. */
function turn(overrides: Partial<Extract<TranscriptItem, { kind: "turn" }>> = {}) {
  const item: TranscriptItem = {
    kind: "turn",
    key: "turn-t1",
    turnId: "t1",
    parts: [{ kind: "text", key: 0, text: "I'll rebase the branch and push it." }],
    status: "completed",
    error: null,
    usage: null,
    startedAtUnix: T0,
    endedAtUnix: T0 + 30,
    ...overrides,
  };
  return item;
}

describe("Transcript turn", () => {
  it("says a failed turn stopped, and that the session is still the user's to continue", () => {
    // The provider's own words used to stand alone, which tells a
    // first-time reader neither of those things (issue #136).
    const { getByText } = show(
      turn({ status: "failed", error: "stream closed by the provider: overloaded_error" }),
    );

    expect(
      getByText("The turn stopped before it finished. Send a message to continue it."),
    ).toBeInTheDocument();
    expect(getByText("stream closed by the provider: overloaded_error")).toBeInTheDocument();
  });

  it("says a tool call failed in the line, not only in the glyph beside it", () => {
    const { getByText } = show(
      turn({
        parts: [
          {
            kind: "tools",
            key: 0,
            calls: [
              {
                key: "c1",
                callId: "c1",
                tool: "Edit",
                input: { file_path: "crates/daemon/src/compaction.rs" },
                ok: false,
                startedAtUnix: T0,
                endedAtUnix: T0 + 2,
              },
            ],
          },
        ],
      }),
    );

    expect(getByText("Could not edit crates/daemon/src/compaction.rs")).toBeInTheDocument();
  });
});

describe("Transcript command output and context card", () => {
  it("renders output the harness printed on its own as a block, not a bubble", async () => {
    // Markdown paints on the animation frame, so the text arrives a tick
    // after the block does.
    const { getByRole, findByText } = show({
      kind: "command_output",
      key: "output-0",
      text: "Context window: 42k of 200k",
      atUnix: T0,
    });
    expect(getByRole("region", { name: "Command output" })).toBeInTheDocument();
    expect(await findByText("Context window: 42k of 200k")).toBeInTheDocument();
  });

  it("shows the window's fill, what fills it, and the plan beside it", () => {
    const { getByRole, getByText } = render(() => (
      <Transcript
        items={[
          {
            kind: "context",
            key: "context-0",
            atUnix: T0,
            usage: {
              model: "claude-opus-4-8",
              window: { used_tokens: 84_000, size_tokens: 200_000 },
              autoCompact: 160_000,
              categories: [
                { key: "category-0", name: "System prompt", tokens: 3_000, deferred: false },
              ],
              mcpTools: [{ key: "mcp-0", name: "mcp__github", tokens: 1_200, deferred: true }],
              memoryFiles: [],
              agents: [],
              skills: [],
            },
          },
        ]}
        repo="lexoliu/flyco"
        provider="Azure"
        models={[]}
        plan={[
          {
            label: "5-hour",
            used_percent: 62,
            resets_at_unix: T0 + 3600,
            window_minutes: 300,
          },
        ]}
        now={T0 * 1000}
        stoppedAtUnix={null}
      />
    ));

    const card = getByRole("region", { name: "Context usage" });
    expect(card.textContent).toContain("claude-opus-4-8");
    expect(card.textContent).toContain("84k of 200k");
    expect(card.textContent).toContain("42%");
    expect(card.textContent).toContain("System prompt");
    expect(card.textContent).toContain("Compacts on its own at 160k");
    // The MCP list is behind its disclosure, with the total on the summary.
    expect(card.textContent).toContain("MCP tools");
    // And the plan's windows sit in the same card, the way the header's two
    // rings sit beside each other.
    expect(getByText("Plan")).toBeInTheDocument();
    expect(card.textContent).toContain("62%");
  });
});

describe("Transcript approval card", () => {
  /** One approval, as the room raises it. */
  function approval(payload: Extract<TranscriptItem, { kind: "approval" }>["payload"]) {
    const item: TranscriptItem = {
      kind: "approval",
      key: "approval-ap-1",
      id: "ap-1",
      payload,
      state: "pending",
      atUnix: T0,
    };
    return item;
  }

  it("reads a tool call's arguments out, one row each, instead of printing JSON", () => {
    const { getByRole, getByText, queryByText } = show(
      approval({
        kind: "tool_use",
        tool: "Bash",
        input: { command: "git push --force origin fix/flaky-compaction" },
      }),
    );

    const card = getByRole("region", { name: "Approval" });
    expect(card.textContent).toContain("Run Bash");
    expect(getByText("command")).toBeInTheDocument();
    // The command exactly as it would run: no quotes, no braces, no
    // re-indentation of the text the user is being asked to allow.
    expect(getByText("git push --force origin fix/flaky-compaction")).toBeInTheDocument();
    expect(queryByText(/^\{$/)).not.toBeInTheDocument();
    expect(card.querySelector("dl")).not.toBeNull();
  });

  it("keeps writing the cards it wrote itself as sentences", () => {
    const { getByRole } = show(
      approval({
        kind: "merge",
        repo: "lexoliu/flyco",
        from_branch: "fix/flaky-compaction",
        into_branch: "dev",
      }),
    );

    const card = getByRole("region", { name: "Approval" });
    expect(card.textContent).toContain("lexoliu/flyco: fix/flaky-compaction → dev");
    expect(card.querySelector("dl")).toBeNull();
  });
});

describe("Transcript user messages", () => {
  /** One message in the conversation, from whoever wrote it. */
  function said(origin: "user" | "flyco"): TranscriptItem {
    return {
      kind: "user_message",
      key: "user-0",
      text: "usage limit reset, please continue",
      atUnix: T0,
      origin,
    };
  }

  it("marks the message flyco sent on the user's behalf", () => {
    // The continuation sent when a plan window turns over belongs in the
    // user's half of the conversation, because that is what it is — but a
    // reader coming back to a session that carried on overnight has to be
    // able to tell it from a sentence they typed themselves.
    const { getByText } = show(said("flyco"));
    expect(getByText("Sent by flyco")).toBeInTheDocument();
    expect(getByText(/usage limit reset/)).toBeInTheDocument();
  });

  it("says nothing about the author of a message the user typed", () => {
    const { queryByText, getByText } = show(said("user"));
    expect(queryByText("Sent by flyco")).toBeNull();
    expect(getByText(/usage limit reset/)).toBeInTheDocument();
  });
});
