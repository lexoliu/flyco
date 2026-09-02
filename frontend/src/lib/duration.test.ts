import { describe, expect, it } from "vitest";
import { formatDuration } from "./duration";

describe("formatDuration", () => {
  it("keeps the seconds a short turn is measured in", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(12)).toBe("12s");
    expect(formatDuration(59)).toBe("59s");
  });

  it("reads minutes and seconds together, which is what a turn takes", () => {
    expect(formatDuration(503)).toBe("8m 23s");
    expect(formatDuration(61)).toBe("1m 1s");
  });

  it("drops the seconds only when there are none", () => {
    expect(formatDuration(60)).toBe("1m");
    expect(formatDuration(180)).toBe("3m");
  });

  it("drops seconds entirely past an hour, where they are noise", () => {
    expect(formatDuration(3600)).toBe("1h");
    expect(formatDuration(3600 + 4 * 60 + 30)).toBe("1h 4m");
  });

  it("never renders a negative duration from a clock that ran backwards", () => {
    expect(formatDuration(-30)).toBe("0s");
  });

  it("floors a fractional second rather than showing one", () => {
    expect(formatDuration(12.9)).toBe("12s");
  });
});
