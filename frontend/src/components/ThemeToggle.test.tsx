import { describe, expect, it } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import ThemeToggle from "./ThemeToggle";

describe("ThemeToggle", () => {
  it("cycles system -> light -> dark and persists each step", () => {
    const { getByRole } = render(() => <ThemeToggle />);
    const button = getByRole("button");

    expect(button.textContent).toBe("System");

    fireEvent.click(button);
    expect(button.textContent).toBe("Light");
    expect(localStorage.getItem("flyco.theme")).toBe("light");
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");

    fireEvent.click(button);
    expect(button.textContent).toBe("Dark");
    expect(localStorage.getItem("flyco.theme")).toBe("dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");

    fireEvent.click(button);
    expect(button.textContent).toBe("System");
    expect(localStorage.getItem("flyco.theme")).toBe("system");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
  });
});
