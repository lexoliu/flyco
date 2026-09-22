import { describe, expect, it } from "vitest";
import { render } from "@solidjs/testing-library";
import HarnessUsage from "./HarnessUsage";
import type { UsageWindow } from "../api/wire";

/** One window, as the vendor stated it while the page was answered. */
function window(overrides: Partial<UsageWindow> = {}): UsageWindow {
  return {
    label: "5-hour",
    used_percent: 26,
    resets_at_unix: Math.floor(Date.now() / 1000) + 2 * 3600 + 10 * 60,
    window_minutes: 300,
    ...overrides,
  };
}

describe("HarnessUsage", () => {
  it("draws nothing at all for a credential with no plan behind it", () => {
    const { container } = render(() => (
      <HarnessUsage row={undefined} plan={{ state: "unmetered" }} />
    ));

    expect(container.querySelectorAll("[role='progressbar']")).toHaveLength(0);
    expect(container.textContent).toBe("");
  });

  it("says the plan could not be read rather than drawing it at zero", () => {
    const { container, getByText } = render(() => (
      <HarnessUsage row={undefined} plan={{ state: "unavailable", reason: "HTTP 401" }} />
    ));

    expect(getByText("Plan usage unavailable")).toBeInTheDocument();
    expect(container.querySelectorAll("[role='progressbar']")).toHaveLength(0);
  });

  it("reads the plan's windows shortest first, with how long each has left", () => {
    const { getAllByRole, getByText } = render(() => (
      <HarnessUsage
        row={undefined}
        plan={{
          state: "windows",
          windows: [
            window({
              label: "Weekly",
              window_minutes: 10080,
              used_percent: 15,
              resets_at_unix: null,
            }),
            window(),
          ],
        }}
      />
    ));

    const bars = getAllByRole("progressbar");
    expect(bars.map((bar) => bar.getAttribute("aria-label"))).toEqual(["5-hour", "Weekly"]);
    expect(bars[0]?.getAttribute("aria-valuenow")).toBe("26");

    expect(getByText("26% · Resets in 2h 10m")).toBeInTheDocument();
    // A window the vendor gave no reset for reads as the figure alone
    // rather than inventing a deadline.
    expect(getByText("15%")).toBeInTheDocument();
  });

  it("colours a window only once it is nearly spent", () => {
    const { getAllByRole } = render(() => (
      <HarnessUsage
        row={undefined}
        plan={{
          state: "windows",
          windows: [
            window({ used_percent: 26 }),
            window({ label: "Weekly", window_minutes: 10080, used_percent: 84 }),
            window({ label: "Monthly", window_minutes: 43200, used_percent: 97 }),
          ],
        }}
      />
    ));

    const tiers = getAllByRole("progressbar").map((bar) =>
      bar.querySelector("[data-tier]")?.getAttribute("data-tier"),
    );
    expect(tiers).toEqual(["ok", "warn", "final-warn"]);
  });
});
