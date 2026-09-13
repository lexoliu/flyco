import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import ModeChip, { type ModeChipProps } from "./ModeChip";
import { modesFor } from "../lib/modes";

function mount(overrides: Partial<ModeChipProps> = {}) {
  const props: ModeChipProps = {
    modes: modesFor("claude_code"),
    mode: "auto",
    onChoose: vi.fn(),
    ...overrides,
  };
  return { props, ...render(() => <ModeChip {...props} />) };
}

describe("ModeChip", () => {
  it("is a chip named for the mode the session runs under", () => {
    const { getByRole } = mount({ mode: "plan" });

    const trigger = getByRole("button", { name: /Plan/ });
    expect(trigger.getAttribute("aria-haspopup")).toBe("dialog");
  });

  it("lists the modes the harness offers and marks the current one", () => {
    const { getByRole, getAllByRole } = mount({ mode: "acceptEdits" });

    getByRole("button", { name: /Accept edits/ }).click();

    const options = getAllByRole("option");
    expect(options.map((option) => option.textContent)).toEqual([
      expect.stringContaining("Auto"),
      expect.stringContaining("Default"),
      expect.stringContaining("Plan"),
      expect.stringContaining("Accept edits"),
      expect.stringContaining("Yolo"),
      expect.stringContaining("Don't ask"),
    ]);
    expect(
      options.find((option) => option.textContent?.includes("Accept edits"))?.getAttribute(
        "aria-selected",
      ),
    ).toBe("true");
  });

  it("offers Codex no dontAsk row — it is plan's mapping under another name", () => {
    const { getByRole, queryAllByRole } = mount({ modes: modesFor("codex") });

    getByRole("button", { name: /Auto/ }).click();

    const options = queryAllByRole("option");
    expect(options).toHaveLength(5);
    expect(options.some((option) => option.textContent?.includes("Don't ask"))).toBe(false);
  });

  it("commits the choice and closes", () => {
    const { props, getByRole, queryByRole } = mount();

    getByRole("button", { name: /Auto/ }).click();
    getByRole("option", { name: /Yolo/ }).click();

    expect(props.onChoose).toHaveBeenCalledWith("bypassPermissions");
    expect(queryByRole("dialog")).toBeNull();
  });
});
