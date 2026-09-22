import { createSignal } from "solid-js";
import { fireEvent, render } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import Popover from "./Popover";

/** One animation frame, which is when the panel re-measures what it holds. */
function frame(): Promise<void> {
  return new Promise((resolve) => {
    requestAnimationFrame(() => {
      requestAnimationFrame(() => resolve());
    });
  });
}

/** Puts an element where the test says it is, jsdom having no layout. */
function place(element: Element, top: number, bottom: number): void {
  element.getBoundingClientRect = () =>
    ({ top, bottom, left: 0, right: 0, width: 0, height: bottom - top }) as DOMRect;
}

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

  it("grows in place when its contents do, instead of flipping over the trigger", async () => {
    // A trigger near the foot of the window, the way every composer chip
    // is: room enough below for the panel it opened with, not enough for
    // the taller view a click inside it swaps in.
    const [tall, setTall] = createSignal(false);
    window.innerHeight = 800;
    const { getByRole } = render(() => (
      <Popover
        label="Model"
        trigger={(attrs) => (
          <button id={attrs.id} type="button" onClick={attrs.onClick}>
            Open
          </button>
        )}
      >
        {() => (
          <div>
            <button type="button" onClick={() => setTall(true)}>
              Models
            </button>
            {tall() ? "a taller view" : "a short rail"}
          </div>
        )}
      </Popover>
    ));

    fireEvent.click(getByRole("button", { name: "Open" }));
    const panel = getByRole("dialog");
    const anchor = panel.parentElement;
    expect(anchor).not.toBeNull();
    place(anchor as Element, 600, 630);
    // 154px below the trigger, 584px above it, and a view that wants 400.
    Object.defineProperty(panel, "scrollHeight", { configurable: true, value: 100 });
    await frame();
    expect(panel.className).not.toMatch(/sideTop/);
    expect(panel.style.maxHeight).toBe("160px");

    fireEvent.click(getByRole("button", { name: "Models" }));
    Object.defineProperty(panel, "scrollHeight", { configurable: true, value: 400 });
    await frame();

    // The taller view fits above and not below, and the panel stays put
    // anyway: the rows the pointer is reaching for must not jump to the
    // other side of the trigger.
    expect(panel.className).not.toMatch(/sideTop/);
    expect(panel.style.maxHeight).toBe("160px");
  });
});
