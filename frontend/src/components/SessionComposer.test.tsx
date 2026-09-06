import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import SessionComposer, { type SessionComposerProps } from "./SessionComposer";

function mount(overrides: Partial<SessionComposerProps> = {}) {
  const props: SessionComposerProps = {
    turnInFlight: false,
    onSend: vi.fn(),
    onStop: vi.fn(),
    onCommand: vi.fn(),
    ...overrides,
  };
  return { props, ...render(() => <SessionComposer {...props} />) };
}

describe("SessionComposer", () => {
  it("takes a message when the session is running", () => {
    const { props, getByLabelText, getByRole } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    expect(field.disabled).toBe(false);
    field.value = "Also add a regression test.";
    field.dispatchEvent(new Event("input", { bubbles: true }));
    getByRole("button", { name: "Send" }).click();

    expect(props.onSend).toHaveBeenCalledWith("Also add a regression test.");
  });

  it("says a `!` line runs in the machine's shell", () => {
    const { getByLabelText, getByText } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    field.value = "!git status";
    field.dispatchEvent(new Event("input", { bubbles: true }));

    expect(getByText("Runs in the machine's bash")).toBeInTheDocument();
  });
});
