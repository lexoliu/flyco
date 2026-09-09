import { describe, expect, it } from "vitest";
import { render } from "@solidjs/testing-library";
import Ring from "./Ring";

describe("Ring", () => {
  it("names what it measures and what it reads", () => {
    const { getByRole } = render(() => (
      <Ring label="Context" value={41_000} total={200_000} readout="41k / 200k" />
    ));

    expect(getByRole("img", { name: "Context: 41k / 200k" })).toBeInTheDocument();
  });

  it("carries a plan window's reset in the same name as its reading", () => {
    // Both halves reachable by ear: a tooltip that replaced the percentage
    // with the deadline would make the number unreadable to a screen reader.
    const { getByRole } = render(() => (
      <Ring label="5-hour" value={26} total={100} readout="26%" hint="Resets in 2h 10m" />
    ));

    expect(
      getByRole("img", { name: "5-hour: 26% · Resets in 2h 10m" }),
    ).toBeInTheDocument();
  });

  it("draws no arc at all while the number is not known", () => {
    // An empty track and an invented fill look the same to a reader; only
    // one of them is honest.
    const { container } = render(() => (
      <Ring label="Budget" value={undefined} total={undefined} readout="—" />
    ));

    expect(container.querySelectorAll("circle")).toHaveLength(1);
  });
});
