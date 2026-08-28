import { beforeEach, describe, expect, it } from "vitest";
import {
  applyThemePreference,
  readStoredThemePreference,
  setTheme,
  storeThemePreference,
} from "./theme";

describe("theme preference", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute("data-theme");
  });

  it("defaults to system when nothing is stored", () => {
    expect(readStoredThemePreference()).toBe("system");
  });

  it("round-trips a stored preference", () => {
    storeThemePreference("dark");
    expect(readStoredThemePreference()).toBe("dark");
  });

  it("throws on a corrupted stored value", () => {
    localStorage.setItem("flyco.theme", "purple");
    expect(() => readStoredThemePreference()).toThrow(/Invalid theme preference/);
  });

  it("applies dark and light by setting data-theme", () => {
    applyThemePreference("dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    applyThemePreference("light");
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
  });

  it("applies system by clearing data-theme", () => {
    applyThemePreference("dark");
    applyThemePreference("system");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
  });

  it("setTheme persists and applies together", () => {
    setTheme("light");
    expect(localStorage.getItem("flyco.theme")).toBe("light");
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
  });
});
