import { render } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { describe, expect, it } from "vitest";
import YesNo from "./YesNo";

describe("YesNo", () => {
  it("starts unanswered and records the choice", () => {
    const [value, setValue] = createSignal<boolean | null>(null);
    const { getByRole } = render(() => (
      <YesNo question="Are you a student?" value={value()} onChange={setValue} />
    ));

    const group = getByRole("radiogroup", { name: "Are you a student?" });
    expect(group).toBeInTheDocument();
    expect(getByRole("radio", { name: "Yes" })).not.toBeChecked();
    expect(getByRole("radio", { name: "No" })).not.toBeChecked();

    getByRole("radio", { name: "No" }).click();
    expect(value()).toBe(false);
    expect(getByRole("radio", { name: "No" })).toBeChecked();

    getByRole("radio", { name: "Yes" }).click();
    expect(value()).toBe(true);
    expect(getByRole("radio", { name: "Yes" })).toBeChecked();
  });
});
