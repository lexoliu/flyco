import { describe, expect, it } from "vitest";
import { render } from "@solidjs/testing-library";
import HarnessUsage from "./HarnessUsage";
import type { LlmUsageRow } from "../api/client";
import type { UsageWindow } from "../api/wire";

/** The account's own record: the vendor refused a call an hour ago. */
function limited(): LlmUsageRow {
  const now = Math.floor(Date.now() / 1000);
  return {
    account: "c0ffee00-1111-4222-8333-444455556666",
    harness: "claude_code",
    label: "me@lexo.cool",
    observed_cost: null,
    period_start_unix: now - 7 * 86_400,
    rate_limited_at_unix: now - 3600,
    resets_at_unix: now + 3600,
  };
}

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

  it("waits out a refusal only where the vendor would not state the plan", () => {
    // The wait names no window. Beside stated windows it is a second,
    // contradictory answer to what they already say per window, so it
    // renders only in their absence.
    const stated = render(() => (
      <HarnessUsage row={limited()} plan={{ state: "windows", windows: [window()] }} />
    ));
    expect(
      stated.getAllByRole("progressbar").map((bar) => bar.getAttribute("aria-label")),
    ).toEqual(["5-hour"]);

    const unstated = render(() => (
      <HarnessUsage row={limited()} plan={{ state: "unavailable", reason: "HTTP 401" }} />
    ));
    expect(
      unstated.getAllByRole("progressbar").map((bar) => bar.getAttribute("aria-label")),
    ).toEqual(["Usage limit"]);
  });
});
