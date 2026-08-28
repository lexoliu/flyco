import { describe, expect, it } from "vitest";
import { InvalidAuthFragmentError, parseSessionTokenFragment } from "./fragmentToken";

describe("parseSessionTokenFragment", () => {
  it("parses a valid token", () => {
    expect(parseSessionTokenFragment("#token=fs_abc123")).toBe("fs_abc123");
  });

  it("accepts the fragment without its leading #", () => {
    expect(parseSessionTokenFragment("token=fs_abc123")).toBe("fs_abc123");
  });

  it("throws when the fragment is empty", () => {
    expect(() => parseSessionTokenFragment("")).toThrow(InvalidAuthFragmentError);
    expect(() => parseSessionTokenFragment("#")).toThrow(InvalidAuthFragmentError);
  });

  it("throws when the fragment has no token field", () => {
    expect(() => parseSessionTokenFragment("#foo=bar")).toThrow(
      /missing its token field/,
    );
  });

  it("throws when the token doesn't have the fs_ prefix", () => {
    expect(() => parseSessionTokenFragment("#token=malformed")).toThrow(
      /fs_ prefix/,
    );
  });
});
