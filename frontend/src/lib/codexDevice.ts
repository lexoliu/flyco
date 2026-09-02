/**
 * The Codex device sign-in, as a state machine the card renders.
 *
 * `codex login --device-auth` prints a code and a link and then waits;
 * flyco does the same thing in a card, and the waiting is a poll of
 * `GET /v1/harness-accounts/codex/oauth/{attempt_id}` every
 * `interval_seconds`. That loop has more states than a button: the code can
 * expire, OpenAI can refuse to start a device sign-in at all because the
 * account has the flow switched off, and a poll can land after the user has
 * already closed the card.
 *
 * All of that lives here rather than in the component, so it can be
 * asserted without a timer: the component owns the `setTimeout` and the
 * fetch, and asks this module what state it is in and how long to wait.
 */

/** Where a person turns device-code authorization on for their account. */
export const CHATGPT_SECURITY_SETTINGS_URL = "https://chatgpt.com/#settings/Security";

/**
 * The floor on how often the card polls.
 *
 * OpenAI states an interval and flyco obeys it, but a value it never sent —
 * or sent as zero — must not turn the card into a request loop.
 */
export const MIN_POLL_INTERVAL_SECONDS = 1;

/** What `POST .../codex/oauth/start` handed the card to show. */
export interface CodexAttempt {
  /** Names the attempt when polling. */
  attemptId: string;
  /** The one-time code the user types. */
  userCode: string;
  /** Where they type it. */
  verificationUrl: string;
  /** How often OpenAI says to ask. */
  intervalSeconds: number;
}

/** Every state the Codex card can be in. */
export type CodexSignIn =
  | { step: "idle" }
  | { step: "starting" }
  | { step: "waiting"; attempt: CodexAttempt }
  | { step: "linked" }
  /** The code was never approved in time; the attempt is kept so the card
   *  can say *which* code went stale rather than just "try again". */
  | { step: "expired"; attempt: CodexAttempt }
  /** OpenAI will not start a device sign-in for this account at all. */
  | { step: "blocked" }
  | { step: "failed"; error: unknown };

/** Everything that moves the card from one state to the next. */
export type CodexEvent =
  | { kind: "start" }
  | { kind: "started"; attempt: CodexAttempt }
  | { kind: "pending" }
  | { kind: "linked" }
  | { kind: "failed"; error: unknown }
  | { kind: "reset" };

/** The state a card opens in. */
export const IDLE: CodexSignIn = { step: "idle" };

/** A problem document, as far as this module needs to read one. */
interface ProblemLike {
  type?: unknown;
}

/** The `type` URI of an error, when it carries one. */
function problemType(error: unknown): string | null {
  const type = (error as ProblemLike | null)?.type;
  return typeof type === "string" ? type : null;
}

/** Whether `error` is the control plane saying this attempt is over. */
export function isAttemptExpired(error: unknown): boolean {
  return problemType(error)?.endsWith("/codex-oauth-attempt-expired") ?? false;
}

/**
 * Whether `error` is OpenAI refusing to start a device sign-in.
 *
 * The one failure with a fix the user can apply themselves, which is why
 * the card gives it a state of its own rather than an error message.
 */
export function isDeviceAuthDisabled(error: unknown): boolean {
  return problemType(error)?.endsWith("/codex-device-auth-disabled") ?? false;
}

/**
 * The next state.
 *
 * Poll results are only accepted while the card is waiting: a response that
 * arrives after the user cancelled, or after an earlier poll already
 * finished the sign-in, must not put the card back on screen.
 */
export function nextSignIn(state: CodexSignIn, event: CodexEvent): CodexSignIn {
  switch (event.kind) {
    case "reset":
      return IDLE;
    case "start":
      return { step: "starting" };
    case "started":
      return { step: "waiting", attempt: event.attempt };
    case "pending":
      // Nothing has happened yet, in every state. The card keeps saying
      // what it was saying and the component schedules the next poll.
      return state;
    case "linked":
      return state.step === "waiting" ? { step: "linked" } : state;
    case "failed":
      return failure(state, event.error);
  }
}

/** How a failure lands, which depends on what kind of failure it is. */
function failure(state: CodexSignIn, error: unknown): CodexSignIn {
  if (isDeviceAuthDisabled(error)) {
    return { step: "blocked" };
  }
  if (isAttemptExpired(error)) {
    return state.step === "waiting" ? { step: "expired", attempt: state.attempt } : state;
  }
  return { step: "failed", error };
}

/**
 * How long to wait before the next poll, in milliseconds, or `null` when
 * there is nothing to poll.
 *
 * `null` is what the component reads to stop its timer, so a card that has
 * linked, expired, or been closed makes no further requests.
 */
export function pollDelayMs(state: CodexSignIn): number | null {
  if (state.step !== "waiting") {
    return null;
  }
  const seconds = Math.max(state.attempt.intervalSeconds, MIN_POLL_INTERVAL_SECONDS);
  return seconds * 1000;
}

/** Whether the card is mid-flow, which is what disables the start button. */
export function isRunning(state: CodexSignIn): boolean {
  return state.step === "starting" || state.step === "waiting";
}
