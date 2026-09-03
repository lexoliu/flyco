import { describe, expect, it } from "vitest";
import { ApiProblem, NetworkError, type Problem } from "../api/problem";
import { inUseRefusal, sessionsStillRunning } from "./inUse";

function problem(slug: string, detail: string, extensions: Partial<Problem> = {}): ApiProblem {
  return new ApiProblem({
    type: `https://flyco.dev/problems/${slug}`,
    title: "Conflict",
    status: 409,
    detail,
    ...extensions,
  });
}

describe("inUseRefusal", () => {
  it("reads both refusals that mean something is still running there", () => {
    expect(
      inUseRefusal(
        problem("host-has-active-sessions", "3 session(s) still run on this host; pass force", {
          active_sessions: 3,
        }),
      ),
    ).toEqual({ sessions: 3, detail: "3 session(s) still run on this host; pass force" });

    expect(
      inUseRefusal(problem("provider-in-use", "2 session(s) still run on this account")),
    ).toEqual({ sessions: null, detail: "2 session(s) still run on this account" });
  });

  it("reads the member rather than the sentence around it", () => {
    // `detail` is written for a person and free to be reworded; the number
    // a client acts on is the extension member and nothing else.
    expect(
      inUseRefusal(problem("host-has-active-sessions", "plenty of them", { active_sessions: 12 }))
        ?.sessions,
    ).toBe(12);
  });

  it("has no opinion about any other failure", () => {
    expect(inUseRefusal(problem("host-not-found", "7 of them", { active_sessions: 7 }))).toBeNull();
    expect(inUseRefusal(problem("dirty-archive", "1 file changed"))).toBeNull();
    for (const error of [null, undefined, new Error("offline"), "provider-in-use", {}]) {
      expect(inUseRefusal(error)).toBeNull();
    }
    expect(inUseRefusal(new NetworkError(new Error("offline")))).toBeNull();
  });
});

describe("sessionsStillRunning", () => {
  it("counts in the interface's own words when the refusal counted", () => {
    expect(sessionsStillRunning({ sessions: 2, detail: "…" })).toBe(
      "2 sessions are still running there.",
    );
    expect(sessionsStillRunning({ sessions: 1, detail: "…" })).toBe(
      "1 session is still running there.",
    );
  });

  it("repeats what the control plane said when it counted nothing out loud", () => {
    // Better the server's own sentence than a number parsed out of it:
    // `provider-in-use` carries its count only in prose. It is closed as a
    // sentence, because the guidance after it is another one.
    expect(
      sessionsStillRunning({ sessions: null, detail: "2 session(s) still run on this account" }),
    ).toBe("2 session(s) still run on this account.");
    expect(sessionsStillRunning({ sessions: null, detail: "They are still running!" })).toBe(
      "They are still running!",
    );
  });
});
