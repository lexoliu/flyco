import { describe, expect, it } from "vitest";
import { parseClaudeSecret, parsePastedCode, pastedCodeForExchange } from "./claudeCode";

describe("parsePastedCode", () => {
  it("reads the CODE#STATE string Anthropic shows", () => {
    expect(parsePastedCode("ac_the-code#the-state")).toEqual({
      code: "ac_the-code",
      state: "the-state",
    });
  });

  it("accepts the bare code, because half the string is still a code", () => {
    expect(parsePastedCode("ac_the-code")).toEqual({ code: "ac_the-code", state: null });
  });

  it("tidies what a copy brought with it", () => {
    for (const pasted of [
      "  ac_the-code#the-state\n",
      '"ac_the-code#the-state"',
      "ac_the-code # the-state",
      "`ac_the-code#the-state`",
    ]) {
      expect(parsePastedCode(pasted)).toEqual({ code: "ac_the-code", state: "the-state" });
    }
  });

  it("reads a trailing separator as a code with no state", () => {
    expect(parsePastedCode("ac_the-code#")).toEqual({ code: "ac_the-code", state: null });
  });

  it("has nothing to redeem for an empty or state-only paste", () => {
    expect(parsePastedCode("")).toBeNull();
    expect(parsePastedCode("   \n ")).toBeNull();
    expect(parsePastedCode("#the-state")).toBeNull();
  });
});

describe("pastedCodeForExchange", () => {
  it("sends the halves back joined, so the server checks the state", () => {
    expect(pastedCodeForExchange({ code: "ac_the-code", state: "the-state" })).toBe(
      "ac_the-code#the-state",
    );
  });

  it("sends a bare code as itself", () => {
    expect(pastedCodeForExchange({ code: "ac_the-code", state: null })).toBe("ac_the-code");
  });
});

describe("parseClaudeSecret", () => {
  it("reads a setup token by its prefix", () => {
    expect(parseClaudeSecret("sk-ant-oat01-abc")).toEqual({
      kind: "claude_setup_token",
      token: "sk-ant-oat01-abc",
    });
  });

  it("reads an API key by its prefix", () => {
    expect(parseClaudeSecret("sk-ant-api03-abc")).toEqual({
      kind: "claude_api_key",
      key: "sk-ant-api03-abc",
    });
  });

  it("tidies what a terminal or a clipboard added", () => {
    expect(parseClaudeSecret('  "sk-ant-api03-abc"\n')).toEqual({
      kind: "claude_api_key",
      key: "sk-ant-api03-abc",
    });
  });

  it("refuses to guess at anything else", () => {
    expect(parseClaudeSecret("")).toBeNull();
    expect(parseClaudeSecret("sk-proj-openai")).toBeNull();
    expect(parseClaudeSecret("sk-ant-")).toBeNull();
  });
});
