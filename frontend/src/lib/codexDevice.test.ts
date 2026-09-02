import { describe, expect, it } from "vitest";
import {
  CHATGPT_SECURITY_SETTINGS_URL,
  IDLE,
  MIN_POLL_INTERVAL_SECONDS,
  type CodexAttempt,
  type CodexSignIn,
  isAttemptExpired,
  isDeviceAuthDisabled,
  isRunning,
  nextSignIn,
  pollDelayMs,
} from "./codexDevice";

const ATTEMPT: CodexAttempt = {
  attemptId: "11111111-2222-4333-8444-555555555555",
  userCode: "FLYC-8QK2",
  verificationUrl: "https://auth.openai.com/codex/device",
  intervalSeconds: 5,
};

/** A problem document as the API answers with one. */
function problem(slug: string): { type: string; title: string; status: number; detail: string } {
  return {
    type: `https://flyco.dev/problems/${slug}`,
    title: "Conflict",
    status: 409,
    detail: `see ${CHATGPT_SECURITY_SETTINGS_URL}`,
  };
}

/** The state a card is in once OpenAI has issued a code. */
function waiting(): CodexSignIn {
  return nextSignIn(nextSignIn(IDLE, { kind: "start" }), {
    kind: "started",
    attempt: ATTEMPT,
  });
}

describe("nextSignIn", () => {
  it("walks from idle to a code the user can type", () => {
    expect(nextSignIn(IDLE, { kind: "start" })).toEqual({ step: "starting" });
    expect(waiting()).toEqual({ step: "waiting", attempt: ATTEMPT });
  });

  it("stays where it is while nobody has approved the code", () => {
    const state = waiting();
    expect(nextSignIn(state, { kind: "pending" })).toBe(state);
  });

  it("links the account when a poll finds the code approved", () => {
    expect(nextSignIn(waiting(), { kind: "linked" })).toEqual({ step: "linked" });
  });

  it("keeps the stale code when the attempt expires, so the card can name it", () => {
    expect(nextSignIn(waiting(), { kind: "failed", error: problem("codex-oauth-attempt-expired") }))
      .toEqual({ step: "expired", attempt: ATTEMPT });
  });

  it("gives a switched-off device flow a state of its own", () => {
    expect(
      nextSignIn(
        { step: "starting" },
        { kind: "failed", error: problem("codex-device-auth-disabled") },
      ),
    ).toEqual({ step: "blocked" });
  });

  it("keeps any other failure as itself, so the card can show what it was", () => {
    const error = new Error("the network went away");
    expect(nextSignIn(waiting(), { kind: "failed", error })).toEqual({ step: "failed", error });
  });

  it("ignores a poll that lands after the card closed", () => {
    // The component clears its timer on close, but a request already in
    // flight still resolves. It must not put a linked card back on screen.
    expect(nextSignIn(IDLE, { kind: "linked" })).toBe(IDLE);
    const linked = nextSignIn(waiting(), { kind: "linked" });
    expect(nextSignIn(linked, { kind: "linked" })).toBe(linked);
  });

  it("returns to the beginning on reset", () => {
    expect(nextSignIn(waiting(), { kind: "reset" })).toEqual(IDLE);
    expect(nextSignIn({ step: "blocked" }, { kind: "reset" })).toEqual(IDLE);
  });
});

describe("pollDelayMs", () => {
  it("obeys the interval OpenAI stated", () => {
    expect(pollDelayMs(waiting())).toBe(5000);
  });

  it("never polls faster than the floor, whatever the server said", () => {
    const impatient = nextSignIn(IDLE, {
      kind: "started",
      attempt: { ...ATTEMPT, intervalSeconds: 0 },
    });
    expect(pollDelayMs(impatient)).toBe(MIN_POLL_INTERVAL_SECONDS * 1000);
  });

  it("stops the timer once there is nothing left to poll", () => {
    for (const state of [
      IDLE,
      { step: "starting" } as const,
      { step: "linked" } as const,
      { step: "blocked" } as const,
      { step: "expired", attempt: ATTEMPT } as const,
      { step: "failed", error: new Error("nope") } as const,
    ]) {
      expect(pollDelayMs(state)).toBeNull();
    }
  });
});

describe("classifying a failure", () => {
  it("reads the problem type rather than the message", () => {
    expect(isDeviceAuthDisabled(problem("codex-device-auth-disabled"))).toBe(true);
    expect(isDeviceAuthDisabled(problem("codex-oauth-rejected"))).toBe(false);
    expect(isAttemptExpired(problem("codex-oauth-attempt-expired"))).toBe(true);
    expect(isAttemptExpired(problem("codex-device-auth-disabled"))).toBe(false);
  });

  it("treats anything that is not a problem document as neither", () => {
    for (const error of [null, undefined, new Error("offline"), "codex-device-auth-disabled", {}]) {
      expect(isDeviceAuthDisabled(error)).toBe(false);
      expect(isAttemptExpired(error)).toBe(false);
    }
  });
});

describe("isRunning", () => {
  it("is true exactly while the card is mid-flow", () => {
    expect(isRunning(IDLE)).toBe(false);
    expect(isRunning({ step: "starting" })).toBe(true);
    expect(isRunning(waiting())).toBe(true);
    expect(isRunning({ step: "linked" })).toBe(false);
    expect(isRunning({ step: "blocked" })).toBe(false);
  });
});
