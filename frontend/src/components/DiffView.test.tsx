import { describe, expect, it } from "vitest";
import { render } from "@solidjs/testing-library";
import DiffView from "./DiffView";
import { diffRows } from "../lib/diffRows";

describe("DiffView", () => {
  it("signs every line so the change reads without colour", () => {
    const { getByRole } = render(() => (
      <DiffView label="Proposed change" rows={diffRows("alpha\nbeta\n", "alpha\nBETA\n")} />
    ));

    const diff = getByRole("group", { name: "Proposed change" });
    const lines = [...diff.querySelectorAll("p")].map((line) => line.textContent);
    expect(lines).toEqual([" alpha", "-beta", "+BETA"]);
  });

  it("marks added and removed lines apart from context", () => {
    const { getByRole } = render(() => (
      <DiffView label="Proposed change" rows={diffRows("alpha\nbeta\n", "alpha\nBETA\n")} />
    ));

    const kinds = [...getByRole("group").querySelectorAll("p")].map((line) =>
      line.getAttribute("data-kind"),
    );
    expect(kinds).toEqual(["context", "removed", "added"]);
  });

  it("names the elided run rather than hiding that anything was elided", () => {
    const before = Array.from({ length: 20 }, (_, i) => `line ${i}`).join("\n");
    const { getByText } = render(() => (
      <DiffView label="Proposed change" rows={diffRows(before, before.replace("line 0", "X"), 1)} />
    ));

    expect(getByText(/unchanged lines/)).toBeInTheDocument();
  });

  it("says so when a change would leave the document as it is", () => {
    const { getByText, queryByRole } = render(() => (
      <DiffView label="Proposed change" rows={diffRows("same\n", "same\n")} />
    ));

    expect(getByText("This change would leave the document as it is.")).toBeInTheDocument();
    expect(queryByRole("group")).toBeNull();
  });
});
