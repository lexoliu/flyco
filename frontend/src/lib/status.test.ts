import { describe, expect, it } from "vitest";
import type { SessionState } from "../api/client";
import { STATUS_ORDER, deriveStatus, elapsedSince } from "./status";

/** A fixed instant, so nothing here depends on when the suite runs. */
const NOW_UNIX = 1_800_000_000;
const NOW = NOW_UNIX * 1000;

function session(state: SessionState, createdAgo = 0) {
  return {
    state,
    created_at_unix: NOW_UNIX - createdAgo,
    last_active_unix: NOW_UNIX - createdAgo,
  };
}

describe("deriveStatus", () => {
  it("counts the wait while a machine is being built", () => {
    const view = deriveStatus(session("provisioning", 125), NOW);
    expect(view.status).toBe("provisioning");
    expect(view.detail).toBe("2m");
    expect(view.breathing).toBe(true);
  });

  it("reads an active session with no live signals as idle", () => {
    // Telling `Working` from `Needs input` needs relay events the session
    // list does not have; `Idle` is the honest answer until it does.
    expect(deriveStatus(session("active"), NOW).status).toBe("idle");
    expect(deriveStatus(session("active"), NOW).label).toBe("Idle");
  });

  it("reads an active session with a turn in flight as working", () => {
    const view = deriveStatus(session("active"), NOW, { turnInFlight: true });
    expect(view.status).toBe("working");
    expect(view.tone).toBe("working");
    expect(view.breathing).toBe(true);
  });

  it("reads an active session blocked on the user as needing input", () => {
    const view = deriveStatus(session("active"), NOW, { awaitingUser: true });
    expect(view.status).toBe("needs_input");
    expect(view.tone).toBe("attention");
  });

  it("prefers a running turn over a stale awaiting-user signal", () => {
    const view = deriveStatus(session("active"), NOW, {
      turnInFlight: true,
      awaitingUser: true,
    });
    expect(view.status).toBe("working");
  });

  it("explains why a session stopped", () => {
    expect(deriveStatus(session("paused"), NOW).detail).toBe("budget exhausted");
    expect(deriveStatus(session("interrupted"), NOW).detail).toBe("spot reclaimed");
  });

  it("colours a failed session red and everything at rest quietly", () => {
    expect(deriveStatus(session("failed"), NOW).tone).toBe("failed");
    expect(deriveStatus(session("archived"), NOW).tone).toBe("quiet");
  });

  it("derives a status for every lifecycle state", () => {
    const states: SessionState[] = [
      "provisioning",
      "active",
      "paused",
      "interrupted",
      "archived",
      "failed",
    ];
    for (const state of states) {
      expect(STATUS_ORDER).toContain(deriveStatus(session(state), NOW).status);
    }
  });
});

describe("elapsedSince", () => {
  it("is coarse: seconds, then minutes, then hours", () => {
    expect(elapsedSince(NOW_UNIX, NOW)).toBe("0s");
    expect(elapsedSince(NOW_UNIX - 59, NOW)).toBe("59s");
    expect(elapsedSince(NOW_UNIX - 60, NOW)).toBe("1m");
    expect(elapsedSince(NOW_UNIX - 3599, NOW)).toBe("59m");
    expect(elapsedSince(NOW_UNIX - 7200, NOW)).toBe("2h");
  });

  it("never counts backwards from a clock that is behind the server", () => {
    expect(elapsedSince(NOW_UNIX + 30, NOW)).toBe("0s");
  });
});
