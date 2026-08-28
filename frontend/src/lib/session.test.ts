import { beforeEach, describe, expect, it } from "vitest";
import {
  clearSessionToken,
  getSessionToken,
  isSignedIn,
  resetSessionTokenCacheForTests,
  setSessionToken,
} from "./session";

describe("session token storage", () => {
  beforeEach(() => {
    resetSessionTokenCacheForTests();
  });

  it("round-trips a valid token", () => {
    setSessionToken("fs_abc123");
    expect(getSessionToken()).toBe("fs_abc123");
    expect(isSignedIn()).toBe(true);
  });

  it("rejects a token without the fs_ prefix", () => {
    expect(() => setSessionToken("bad_token")).toThrow(/unexpected shape/);
  });

  it("reports null and signed-out when nothing is stored", () => {
    expect(getSessionToken()).toBeNull();
    expect(isSignedIn()).toBe(false);
  });

  it("survives a simulated reload by falling back to localStorage", () => {
    setSessionToken("fs_abc123");
    resetSessionTokenCacheForTests();
    expect(getSessionToken()).toBe("fs_abc123");
  });

  it("clears both the memory cache and localStorage", () => {
    setSessionToken("fs_abc123");
    clearSessionToken();
    resetSessionTokenCacheForTests();
    expect(getSessionToken()).toBeNull();
  });
});
