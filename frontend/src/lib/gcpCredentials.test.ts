import { describe, expect, it } from "vitest";
import { parseGcpServiceAccount } from "./gcpCredentials";

const KEY = JSON.stringify({
  type: "service_account",
  project_id: "flyco-sessions",
  private_key_id: "6c1f0b93a4d2e7185fbb2c04e93d7a1f5c8e2b60",
  private_key: "-----BEGIN PRIVATE KEY-----\nMIIEv…\n-----END PRIVATE KEY-----\n",
  client_email: "flyco@flyco-sessions.iam.gserviceaccount.com",
  client_id: "104829173650284917362",
});

describe("parseGcpServiceAccount", () => {
  it("names the project and the account the key opens", () => {
    const parsed = parseGcpServiceAccount(KEY);

    expect(parsed.ok).toBe(true);
    if (!parsed.ok) {
      return;
    }
    expect(parsed.account.projectId).toBe("flyco-sessions");
    expect(parsed.account.clientEmail).toBe("flyco@flyco-sessions.iam.gserviceaccount.com");
  });

  it("refuses the OAuth client secret from the same console page", () => {
    const parsed = parseGcpServiceAccount(
      JSON.stringify({ installed: { client_id: "x", client_secret: "y" } }),
    );

    expect(parsed.ok).toBe(false);
    if (parsed.ok) {
      return;
    }
    expect(parsed.error).toContain("not an OAuth client");
  });

  it("refuses a key with nothing to sign with", () => {
    const parsed = parseGcpServiceAccount(
      JSON.stringify({ type: "service_account", project_id: "p", client_email: "e" }),
    );

    expect(parsed.ok).toBe(false);
    if (parsed.ok) {
      return;
    }
    expect(parsed.error).toContain("no private key");
  });

  it("refuses a file that is not JSON", () => {
    expect(parseGcpServiceAccount("not json at all").ok).toBe(false);
  });
});
