import { describe, expect, it } from "vitest";
import { credentialExpiry } from "./expiry";
import { formatDate } from "./dates";

/** Midday, so that adding whole days never crosses a boundary by accident. */
const NOW = Date.UTC(2026, 8, 5, 12) / 1000;
const DAY = 86_400;

describe("credentialExpiry", () => {
  it("says nothing about a credential that does not expire", () => {
    expect(credentialExpiry(null, NOW)).toBeNull();
    expect(credentialExpiry(undefined, NOW)).toBeNull();
  });

  it("gives a distant expiry the date, and leaves the card reading Linked", () => {
    const far = NOW + 60 * DAY;
    expect(credentialExpiry(far, NOW)).toEqual({
      level: "fine",
      status: "Linked",
      sentence: `Expires ${formatDate(far)}`,
    });
  });

  it("counts the days a reader would otherwise have to subtract", () => {
    // The card that prompted this read `expires Sep 6, 2026` on Sep 5.
    expect(credentialExpiry(NOW + DAY, NOW)).toEqual({
      level: "soon",
      status: "Expires soon",
      sentence: "Expires tomorrow",
    });
    expect(credentialExpiry(NOW + 3 * DAY, NOW)?.sentence).toBe("Expires in 3 days");
    expect(credentialExpiry(NOW + 7 * DAY, NOW)?.level).toBe("soon");
    expect(credentialExpiry(NOW + 8 * DAY, NOW)?.level).toBe("fine");
  });

  it("rounds a part-day up, because it runs out tomorrow and not today", () => {
    expect(credentialExpiry(NOW + 20 * 3600, NOW)?.sentence).toBe("Expires tomorrow");
  });

  it("says an expired credential is expired, and when it went", () => {
    const gone = NOW - 2 * DAY;
    expect(credentialExpiry(gone, NOW)).toEqual({
      level: "gone",
      status: "Expired",
      sentence: `Expired ${formatDate(gone)}`,
    });
    expect(credentialExpiry(NOW, NOW)?.level).toBe("gone");
  });
});
