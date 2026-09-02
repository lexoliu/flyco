import { describe, expect, it } from "vitest";
import { parseAzureServicePrincipal } from "./azureCredentials";

/** What `az ad sp create-for-rbac --json-auth` prints, trimmed. */
const JSON_AUTH = `{
  "clientId": "0d0d1a0f-2b6c-4f3a-9a1f-6c2c5b7f9a11",
  "clientSecret": "kV8Q~fake-secret-value-for-a-test",
  "subscriptionId": "3f2a91c4-7b0e-4c58-9e2a-1d4b8f60cc73",
  "tenantId": "9c7e2b41-55da-4f0c-8a3d-2e6f1b90a4d5",
  "activeDirectoryEndpointUrl": "https://login.microsoftonline.com"
}`;

/** What the same command prints without the flag. */
const CLI_DEFAULT = `{
  "appId": "0d0d1a0f-2b6c-4f3a-9a1f-6c2c5b7f9a11",
  "displayName": "flyco",
  "password": "kV8Q~fake-secret-value-for-a-test",
  "tenant": "9c7e2b41-55da-4f0c-8a3d-2e6f1b90a4d5"
}`;

describe("parseAzureServicePrincipal", () => {
  it("reads the --json-auth shape, subscription included", () => {
    const parsed = parseAzureServicePrincipal(JSON_AUTH);

    expect(parsed.ok).toBe(true);
    if (!parsed.ok) {
      return;
    }
    expect(parsed.principal.clientId).toBe("0d0d1a0f-2b6c-4f3a-9a1f-6c2c5b7f9a11");
    expect(parsed.principal.clientSecret).toBe("kV8Q~fake-secret-value-for-a-test");
    expect(parsed.principal.tenantId).toBe("9c7e2b41-55da-4f0c-8a3d-2e6f1b90a4d5");
    expect(parsed.principal.subscriptionId).toBe("3f2a91c4-7b0e-4c58-9e2a-1d4b8f60cc73");
  });

  it("reads the CLI's default shape, and says the subscription is missing", () => {
    const parsed = parseAzureServicePrincipal(CLI_DEFAULT);

    expect(parsed.ok).toBe(true);
    if (!parsed.ok) {
      return;
    }
    expect(parsed.principal.clientId).toBe("0d0d1a0f-2b6c-4f3a-9a1f-6c2c5b7f9a11");
    expect(parsed.principal.clientSecret).toBe("kV8Q~fake-secret-value-for-a-test");
    expect(parsed.principal.tenantId).toBe("9c7e2b41-55da-4f0c-8a3d-2e6f1b90a4d5");
    // Not an error: the wizard asks for it with one more command.
    expect(parsed.principal.subscriptionId).toBeNull();
  });

  it("tolerates the whitespace a terminal copy brings with it", () => {
    expect(parseAzureServicePrincipal(`\n\n  ${CLI_DEFAULT}  \n`).ok).toBe(true);
  });

  it("names the field that is missing rather than saying the JSON is wrong", () => {
    const parsed = parseAzureServicePrincipal('{"appId": "a", "password": "b"}');

    expect(parsed.ok).toBe(false);
    if (parsed.ok) {
      return;
    }
    expect(parsed.error).toContain("tenant id");
  });

  it("rejects text that is not JSON at all, and says what to paste", () => {
    const parsed = parseAzureServicePrincipal("Creating 'Contributor' role assignment...");

    expect(parsed.ok).toBe(false);
    if (parsed.ok) {
      return;
    }
    expect(parsed.error).toContain("braces included");
  });

  it("rejects an empty paste with an instruction, not a parse error", () => {
    const parsed = parseAzureServicePrincipal("   ");

    expect(parsed.ok).toBe(false);
    if (parsed.ok) {
      return;
    }
    expect(parsed.error).toBe("Paste the JSON block the command printed.");
  });

  it("refuses a JSON array, which holds no credentials", () => {
    expect(parseAzureServicePrincipal("[1, 2, 3]").ok).toBe(false);
  });

  it("treats a blank field as absent rather than as a credential", () => {
    const parsed = parseAzureServicePrincipal('{"appId": "a", "password": "  ", "tenant": "c"}');

    expect(parsed.ok).toBe(false);
    if (parsed.ok) {
      return;
    }
    expect(parsed.error).toContain("client secret");
  });
});
