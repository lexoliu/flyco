import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import GoalChip, { type GoalChipProps } from "./GoalChip";

function mount(overrides: Partial<GoalChipProps> = {}) {
  const props: GoalChipProps = {
    description: "Keep working until the condition is met",
    onSet: vi.fn(),
    ...overrides,
  };
  return { props, ...render(() => <GoalChip {...props} />) };
}

describe("GoalChip", () => {
  it("is a chip that opens the goal panel", () => {
    const { getByRole } = mount();

    const trigger = getByRole("button", { name: /Goal/ });
    expect(trigger.getAttribute("aria-haspopup")).toBe("dialog");

    trigger.click();
    expect(getByRole("dialog").textContent).toContain(
      "Keep working until the condition is met",
    );
  });

  it("commits the condition as the goal", () => {
    const { props, getByRole, getByLabelText, queryByRole } = mount();

    getByRole("button", { name: /Goal/ }).click();
    const field = getByLabelText("Goal condition") as HTMLInputElement;
    field.value = "the tests pass";
    field.dispatchEvent(new InputEvent("input", { bubbles: true }));
    getByRole("button", { name: "Set the goal" }).click();

    expect(props.onSet).toHaveBeenCalledWith("the tests pass");
    expect(queryByRole("dialog")).toBeNull();
  });

  it("will not set an empty condition", () => {
    const { props, getByRole } = mount();

    getByRole("button", { name: /Goal/ }).click();

    expect(getByRole("button", { name: "Set the goal" })).toBeDisabled();
    expect(props.onSet).not.toHaveBeenCalled();
  });
});
