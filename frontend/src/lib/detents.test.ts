import { describe, expect, it } from "vitest";
import { detentForKey } from "./detents";

describe("detentForKey", () => {
  it("moves one detent per arrow, on either axis", () => {
    expect(detentForKey("ArrowRight", 1, 4)).toBe(2);
    expect(detentForKey("ArrowUp", 1, 4)).toBe(2);
    expect(detentForKey("ArrowLeft", 1, 4)).toBe(0);
    expect(detentForKey("ArrowDown", 1, 4)).toBe(0);
  });

  it("puts the ends of the track on Home and End", () => {
    expect(detentForKey("Home", 3, 4)).toBe(0);
    expect(detentForKey("End", 0, 4)).toBe(4);
  });

  it("stops at the first and last detent rather than running off", () => {
    expect(detentForKey("ArrowLeft", 0, 4)).toBe(0);
    expect(detentForKey("ArrowRight", 4, 4)).toBe(4);
  });

  it("claims nothing else, so the browser keeps its own keys", () => {
    for (const key of ["Tab", "Enter", " ", "PageUp", "a"]) {
      expect(detentForKey(key, 2, 4)).toBeNull();
    }
  });
});
