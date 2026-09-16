/**
 * The paste classifier the Devin paste page's primary button reads: what
 * keeps the button off (`empty`), what sends (`code`, `refused`), and that
 * a paste is never rewritten — the control plane is the parser of record.
 */
import { describe, expect, it } from "vitest";
import { parseDevinPaste } from "./devinOauth";

const REDIRECT =
  "http://127.0.0.1:59653/callback?code=the-code&state=the-state";

describe("parseDevinPaste", () => {
  it("reads the whole dead-redirect address as a code", () => {
    expect(parseDevinPaste(REDIRECT)).toEqual({ kind: "code" });
  });

  it("reads a bare code, and the surrounding whitespace does not matter", () => {
    expect(parseDevinPaste("the-code")).toEqual({ kind: "code" });
    expect(parseDevinPaste("  the-code  \n")).toEqual({ kind: "code" });
  });

  it("reads the query alone — the address the user trimmed — as a code", () => {
    expect(parseDevinPaste("code=the-code&state=the-state")).toEqual({
      kind: "code",
    });
    expect(parseDevinPaste("?code=the-code&state=the-state")).toEqual({
      kind: "code",
    });
  });

  it("drops a fragment the browser left on the address", () => {
    expect(parseDevinPaste(`${REDIRECT}#section`)).toEqual({ kind: "code" });
  });

  it("reads a refusal redirect as refused, not as a code", () => {
    expect(
      parseDevinPaste(
        "http://127.0.0.1:59653/callback?error=access_denied&error_description=Denied",
      ),
    ).toEqual({ kind: "refused" });
  });

  it("reads the dead address alone as empty — a page that cannot load", () => {
    expect(parseDevinPaste("")).toEqual({ kind: "empty" });
    expect(parseDevinPaste("   ")).toEqual({ kind: "empty" });
    expect(parseDevinPaste("http://127.0.0.1:59653/callback")).toEqual({
      kind: "empty",
    });
    expect(
      parseDevinPaste("http://127.0.0.1:59653/callback?state=the-state"),
    ).toEqual({ kind: "empty" });
    expect(parseDevinPaste("http://127.0.0.1:59653/callback?code=")).toEqual({
      kind: "empty",
    });
  });
});
