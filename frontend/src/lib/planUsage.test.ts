import { describe, expect, it } from "vitest";
import { orderedWindows, resetHint } from "./planUsage";
import type { UsageWindow } from "../api/wire";

/** One window, as the daemon files it. */
function window(overrides: Partial<UsageWindow> = {}): UsageWindow {
  return {
    label: "5-hour",
    used_percent: 26,
    resets_at_unix: 1789002000,
    window_minutes: 300,
    ...overrides,
  };
}

/** 2026-09-09T21:00:00Z in milliseconds, four hours before the reset. */
const NOW = 1788987600000;

describe("plan usage windows", () => {
  it("reads shortest window first, whatever order the harness sent", () => {
    const ordered = orderedWindows([
      window({ label: "Weekly", window_minutes: 10080 }),
      window({ label: "Monthly", window_minutes: 43200 }),
      window({ label: "5-hour", window_minutes: 300 }),
    ]);

    expect(ordered.map((w) => w.label)).toEqual(["5-hour", "Weekly", "Monthly"]);
  });

  it("puts a window whose length the harness never stated last", () => {
    const ordered = orderedWindows([
      window({ label: "Plan", window_minutes: null }),
      window({ label: "Weekly", window_minutes: 10080 }),
    ]);

    expect(ordered.map((w) => w.label)).toEqual(["Weekly", "Plan"]);
  });

  it("keeps the harness's own order between windows of the same length", () => {
    // Claude reports a whole-plan weekly window and per-model ones beside
    // it; nothing distinguishes them by length, so the list stands.
    const ordered = orderedWindows([
      window({ label: "Weekly", window_minutes: 10080 }),
      window({ label: "Weekly (Fable)", window_minutes: 10080 }),
    ]);

    expect(ordered.map((w) => w.label)).toEqual(["Weekly", "Weekly (Fable)"]);
  });

  it("says how long until the window turns over, the way a person says it", () => {
    expect(resetHint(window({ resets_at_unix: NOW / 1000 + 2 * 3600 + 10 * 60 }), NOW)).toBe(
      "Resets in 2h 10m",
    );
    expect(resetHint(window({ resets_at_unix: NOW / 1000 + 45 * 60 }), NOW)).toBe(
      "Resets in 45m",
    );
  });

  it("says a reset that is already due is due, rather than counting backwards", () => {
    expect(resetHint(window({ resets_at_unix: NOW / 1000 - 60 }), NOW)).toBe("Resets now");
  });

  it("says nothing at all when the harness named no reset", () => {
    expect(resetHint(window({ resets_at_unix: null }), NOW)).toBeUndefined();
  });
});
