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

function show(item: TranscriptItem) {
  return render(() => (
    <Transcript items={[item]} repo="lexoliu/flyco" provider="Azure" now={T0 * 1000} />
  ));
}

describe("Transcript shell block", () => {
  it("shows the command, its output and its exit status", () => {
    const { getByRole } = show(
      shell({
        output: [
          { stream: "stdout", data: "running 1 test\n" },
          { stream: "stderr", data: "warning: unused\n" },
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
          { stream: "stdout", data: "out\n" },
          { stream: "stderr", data: "err\n" },
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
        output: [{ stream: "stdout", data: "a lot of output\n" }],
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
