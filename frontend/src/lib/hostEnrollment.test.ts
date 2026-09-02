import { describe, expect, it } from "vitest";
import type { EnrollmentToken, HostView } from "../api/client";
import {
  IDLE,
  POLL_INTERVAL_SECONDS,
  type Enrollment,
  activeSessions,
  hasExpired,
  isBusyHost,
  isEnrollmentOver,
  isRunning,
  nextEnrollment,
  pollDelayMs,
  secondsUntilExpiry,
} from "./hostEnrollment";

/** Ten minutes past the epoch second the token was minted at. */
const EXPIRES_AT = 1_790_000_600;

const TOKEN: EnrollmentToken = {
  id: "3f2b1c9d-6a4e-4d8b-9f21-7c5a0e3b8d14",
  token: "fh_2Qv8xLmR4pT7nWzKcYbA",
  expires_at_unix: EXPIRES_AT,
  command: "curl -fsSL https://dev.flyco.dev/install/flycod.sh | sudo sh -s -- host enroll fh_2Qv8xLmR4pT7nWzKcYbA",
};

const HOST: HostView = {
  id: "8d1a6f30-4b7c-4e21-b0f5-9c2d6a7e4b11",
  label: "mercury",
  state: "online",
  facts: {
    architecture: "x86_64",
    vcpus: 16,
    memory_mib: 64 * 1024,
    disk_free_gib: 812,
    podman_version: "5.4.0",
    kernel: "6.8.0-45-generic",
    hostname: "mercury",
  },
  last_seen_unix: EXPIRES_AT,
  created_at_unix: EXPIRES_AT - 600,
};

/** A problem document as the API answers with one. */
function problem(slug: string, detail: string): Record<string, unknown> {
  return {
    type: `https://flyco.dev/problems/${slug}`,
    title: "Conflict",
    status: 409,
    detail,
  };
}

/** The state the wizard is in once a command is on screen. */
function waiting(): Enrollment {
  return nextEnrollment(nextEnrollment(IDLE, { kind: "mint" }), {
    kind: "minted",
    token: TOKEN,
  });
}

describe("nextEnrollment", () => {
  it("walks from idle to a command the user can run", () => {
    expect(nextEnrollment(IDLE, { kind: "mint" })).toEqual({ step: "minting" });
    expect(waiting()).toEqual({ step: "waiting", token: TOKEN });
  });

  it("stays where it is while no machine has run the command", () => {
    const state = waiting();
    expect(nextEnrollment(state, { kind: "pending" })).toBe(state);
  });

  it("shows the machine the moment a poll finds it enrolled", () => {
    expect(nextEnrollment(waiting(), { kind: "arrived", host: HOST })).toEqual({
      step: "enrolled",
      host: HOST,
    });
  });

  it("keeps the stale command when it expires, so the wizard can say which", () => {
    expect(nextEnrollment(waiting(), { kind: "expire" })).toEqual({
      step: "expired",
      token: TOKEN,
    });
  });

  it("treats the control plane refusing a spent command as expiry, not an error", () => {
    for (const slug of ["enrollment-token-expired", "enrollment-token-not-found"]) {
      expect(
        nextEnrollment(waiting(), {
          kind: "failed",
          error: problem(slug, "that command is finished with"),
        }),
      ).toEqual({ step: "expired", token: TOKEN });
    }
  });

  it("takes a machine that arrived after the local clock gave up", () => {
    // Expiry is this browser's clock; the server is the authority on
    // whether the token was spent. A machine that got in under the wire is
    // a machine that enrolled.
    const expired = nextEnrollment(waiting(), { kind: "expire" });
    expect(nextEnrollment(expired, { kind: "arrived", host: HOST })).toEqual({
      step: "enrolled",
      host: HOST,
    });
  });

  it("keeps any other failure as itself, so the wizard can show what it was", () => {
    const error = new Error("the network went away");
    expect(nextEnrollment(waiting(), { kind: "failed", error })).toEqual({
      step: "failed",
      error,
    });
    expect(nextEnrollment({ step: "minting" }, { kind: "failed", error })).toEqual({
      step: "failed",
      error,
    });
  });

  it("ignores a poll that lands after the wizard closed", () => {
    expect(nextEnrollment(IDLE, { kind: "arrived", host: HOST })).toBe(IDLE);
    const enrolled = nextEnrollment(waiting(), { kind: "arrived", host: HOST });
    expect(nextEnrollment(enrolled, { kind: "arrived", host: HOST })).toBe(enrolled);
    expect(nextEnrollment(enrolled, { kind: "expire" })).toBe(enrolled);
  });

  it("returns to the beginning on reset", () => {
    expect(nextEnrollment(waiting(), { kind: "reset" })).toEqual(IDLE);
    expect(nextEnrollment({ step: "failed", error: null }, { kind: "reset" })).toEqual(IDLE);
  });
});

describe("pollDelayMs", () => {
  it("asks every few seconds while a command is live", () => {
    expect(pollDelayMs(waiting())).toBe(POLL_INTERVAL_SECONDS * 1000);
  });

  it("stops the timer once there is nothing left to poll", () => {
    for (const state of [
      IDLE,
      { step: "minting" } as const,
      { step: "enrolled", host: HOST } as const,
      { step: "expired", token: TOKEN } as const,
      { step: "failed", error: new Error("nope") } as const,
    ]) {
      expect(pollDelayMs(state)).toBeNull();
    }
  });
});

describe("expiry", () => {
  it("counts down to zero and no further", () => {
    expect(secondsUntilExpiry(TOKEN, (EXPIRES_AT - 600) * 1000)).toBe(600);
    expect(secondsUntilExpiry(TOKEN, (EXPIRES_AT - 1) * 1000)).toBe(1);
    expect(secondsUntilExpiry(TOKEN, EXPIRES_AT * 1000)).toBe(0);
    expect(secondsUntilExpiry(TOKEN, (EXPIRES_AT + 3600) * 1000)).toBe(0);
  });

  it("only calls a live command expired", () => {
    expect(hasExpired(waiting(), (EXPIRES_AT - 1) * 1000)).toBe(false);
    expect(hasExpired(waiting(), EXPIRES_AT * 1000)).toBe(true);
    // Nothing else has a command to run out.
    expect(hasExpired(IDLE, EXPIRES_AT * 1000)).toBe(false);
    expect(hasExpired({ step: "expired", token: TOKEN }, EXPIRES_AT * 1000)).toBe(false);
  });
});

describe("classifying a failure", () => {
  it("reads the problem type rather than the message", () => {
    expect(isEnrollmentOver(problem("enrollment-token-expired", "gone"))).toBe(true);
    expect(isEnrollmentOver(problem("enrollment-token-not-found", "gone"))).toBe(true);
    expect(isEnrollmentOver(problem("host-offline", "not here"))).toBe(false);
    expect(isBusyHost(problem("host-has-active-sessions", "3 session(s) still run"))).toBe(true);
    expect(isBusyHost(problem("host-not-found", "no"))).toBe(false);
  });

  it("treats anything that is not a problem document as neither", () => {
    for (const error of [null, undefined, new Error("offline"), "host-has-active-sessions", {}]) {
      expect(isEnrollmentOver(error)).toBe(false);
      expect(isBusyHost(error)).toBe(false);
      expect(activeSessions(error)).toBeNull();
    }
  });

  it("reads the session count off the refusal that carries it", () => {
    expect(
      activeSessions(
        problem("host-has-active-sessions", "3 session(s) still run on this host; pass force"),
      ),
    ).toBe(3);
  });

  it("invents no number when the refusal does not state one", () => {
    expect(activeSessions(problem("host-has-active-sessions", "sessions still run"))).toBeNull();
    expect(activeSessions(problem("host-not-found", "7 of them"))).toBeNull();
  });
});

describe("isRunning", () => {
  it("is true exactly while the wizard is mid-flow", () => {
    expect(isRunning(IDLE)).toBe(false);
    expect(isRunning({ step: "minting" })).toBe(true);
    expect(isRunning(waiting())).toBe(true);
    expect(isRunning({ step: "enrolled", host: HOST })).toBe(false);
    expect(isRunning({ step: "expired", token: TOKEN })).toBe(false);
  });
});
