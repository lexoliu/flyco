/**
 * The paste reader the Devin paste page's primary button reads: what
 * keeps the button off (`null`), and that a paste is never rewritten
 * beyond its surrounding whitespace — the control plane is the parser of
 * record.
 */
import { describe, expect, it } from "vitest";
import { pastedDevinCode } from "./devinOauth";

describe("pastedDevinCode", () => {
  it("reads the code, and the surrounding whitespace does not matter", () => {
    expect(pastedDevinCode("the-code")).toBe("the-code");
    expect(pastedDevinCode("  the-code  \n")).toBe("the-code");
  });

  it("reads a blank field as nothing to send", () => {
    expect(pastedDevinCode("")).toBeNull();
    expect(pastedDevinCode("   ")).toBeNull();
  });
});
