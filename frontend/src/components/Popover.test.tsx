import { createSignal } from "solid-js";
import { fireEvent, render } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import Popover from "./Popover";

describe("Popover", () => {
  it("keeps the body it built when a value the body's setup read changes", () => {
    // A body that reads a signal while it is being built, the way the
    // model picker seeds its slider from the current choice — and then
    // changes that signal from inside, the way a slider move does.
    const [choice, setChoice] = createSignal("low");
    const { getByRole, getByTestId } = render(() => (
      <Popover
        label="Effort"
        trigger={(attrs) => (
          <button id={attrs.id} type="button" onClick={attrs.onClick}>
            Open
          </button>
        )}
      >
        {() => {
          const seeded = choice();
          return (
            <div data-testid="body" data-seeded={seeded}>
              <button type="button" onClick={() => setChoice("high")}>
                Raise
              </button>
            </div>
          );
        }}
      </Popover>
    ));

    fireEvent.click(getByRole("button", { name: "Open" }));
    const body = getByTestId("body");
    expect(body.dataset["seeded"]).toBe("low");

    fireEvent.click(getByRole("button", { name: "Raise" }));

    // The same element is still in the document: the panel was not torn
    // down and rebuilt because its setup happened to read `choice`.
    expect(getByTestId("body")).toBe(body);
    expect(body.isConnected).toBe(true);
    expect(getByRole("dialog")).toBeTruthy();
  });
});
