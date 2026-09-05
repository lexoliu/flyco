import { describe, expect, it } from "vitest";
import { ApiProblem } from "./problem";
import { githubTokenRevoked } from "./auth";

describe("githubTokenRevoked", () => {
  it("recognises GitHub refusing the stored token, and nothing else", () => {
    const revoked = new ApiProblem({
      type: "https://flyco.dev/problems/github-token-revoked",
      title: "Failed Dependency",
      status: 424,
      detail: "GitHub no longer accepts flyco's access to your account; reconnect GitHub",
    });
    const outage = new ApiProblem({
      type: "https://flyco.dev/problems/github-unavailable",
      title: "Bad Gateway",
      status: 502,
      detail: "GitHub could not be reached",
    });

    expect(githubTokenRevoked(revoked)).toBe(true);
    expect(githubTokenRevoked(outage)).toBe(false);
    expect(githubTokenRevoked(new Error("network"))).toBe(false);
    expect(githubTokenRevoked(undefined)).toBe(false);
  });
});
