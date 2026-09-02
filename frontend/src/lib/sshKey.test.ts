import { describe, expect, it } from "vitest";
import { generateBreakGlassKey } from "./sshKey";

describe("generateBreakGlassKey", () => {
  it("mints a key OpenSSH would read", () => {
    const key = generateBreakGlassKey("flyco@lexo.cool");

    expect(key.publicKey.startsWith("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5")).toBe(true);
    expect(key.publicKey.endsWith(" flyco@lexo.cool")).toBe(true);
    expect(key.privateKey.startsWith("-----BEGIN OPENSSH PRIVATE KEY-----")).toBe(true);
    expect(key.privateKey.trimEnd().endsWith("-----END OPENSSH PRIVATE KEY-----")).toBe(true);
    expect(key.fingerprint.startsWith("SHA256:")).toBe(true);
  });

  it("mints a different key every time", () => {
    // A wizard that handed two users the same break-glass login would be a
    // shared password with extra steps.
    const first = generateBreakGlassKey("flyco@lexo.cool");
    const second = generateBreakGlassKey("flyco@lexo.cool");

    expect(first.publicKey).not.toBe(second.publicKey);
    expect(first.privateKey).not.toBe(second.privateKey);
  });
});
