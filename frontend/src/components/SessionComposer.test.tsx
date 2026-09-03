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

  it("says why it will not take one, rather than sitting silently disabled", () => {
    // A disabled field with a disabled button beside it and nothing saying
    // why is the page refusing without saying so (issue #133).
    const { getByLabelText, getByRole, getByText } = mount({
      refusal: "This session is archived and read-only.",
    });

    expect(getByText("This session is archived and read-only.")).toBeInTheDocument();
    expect((getByLabelText("Message the agent") as HTMLTextAreaElement).disabled).toBe(true);
    expect(getByRole("button", { name: "Send" })).toHaveAttribute(
      "title",
      "This session is archived and read-only.",
    );
  });

  it("sends nothing while it is refusing, whatever is in the field", () => {
    const { props, getByLabelText, getByRole } = mount({
      refusal: "This session failed. Resume it to pick the conversation back up.",
    });
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    field.value = "carry on";
    field.dispatchEvent(new Event("input", { bubbles: true }));
    getByRole("button", { name: "Send" }).click();

    expect(props.onSend).not.toHaveBeenCalled();
  });

  it("keeps the shell hint out of the way of the refusal", () => {
    const { getByText, queryByText } = mount({
      refusal: "This session is paused: its budget is spent. Raise it to continue.",
    });
    const field = getByText("This session is paused: its budget is spent. Raise it to continue.");
    expect(field).toBeInTheDocument();
    expect(queryByText("Runs in the machine's bash")).not.toBeInTheDocument();
  });
});
