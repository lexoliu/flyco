/**
 * Enrolling a machine the user owns, as a state machine the wizard renders.
 *
 * The control plane runs on Cloudflare Workers and has no TCP sockets, so a
 * machine somebody owns is *enrolled* rather than dialled (docs/ux.md §7.5,
 * docs/host-enrollment.md): flyco mints a single-use command, the user runs
 * it, and the machine opens the connection. The wizard's whole job is the
 * waiting in between, and that waiting has more outcomes than a spinner —
 * the machine arrives, the command goes stale ten minutes after it was
 * minted, or the poll fails outright.
 *
 * All of it lives here rather than in the component, so it can be asserted
 * without a timer: the component owns the `setTimeout` and the fetch, and
 * asks this module what state it is in and how long to wait. This mirrors
 * `src/lib/codexDevice.ts`, which is the same shape of problem.
 */
import type { EnrollmentToken, HostView } from "../api/client";
import { ApiProblem, type Problem } from "../api/problem";

/**
 * How often the wizard asks whether the machine has arrived.
 *
 * Every few seconds rather than every few hundred milliseconds: the user is
 * copying a command into another terminal, and the first useful answer
 * cannot arrive before the installer has run.
 */
export const POLL_INTERVAL_SECONDS = 3;

/** Every state the wizard can be in. */
export type Enrollment =
  /** Nothing has been minted; the wizard mints as it opens. */
  | { step: "idle" }
  | { step: "minting" }
  /** A live command is on screen and the wizard is polling for the machine. */
  | { step: "waiting"; token: EnrollmentToken }
  | { step: "enrolled"; host: HostView }
  /** The command went stale. The token is kept so the card can say which. */
  | { step: "expired"; token: EnrollmentToken }
  | { step: "failed"; error: unknown };

/** Everything that moves the wizard from one state to the next. */
export type EnrollmentEvent =
  | { kind: "mint" }
  | { kind: "minted"; token: EnrollmentToken }
  | { kind: "pending" }
  | { kind: "arrived"; host: HostView }
  /** The clock passed the token's expiry with no machine on it. */
  | { kind: "expire" }
  | { kind: "failed"; error: unknown }
  | { kind: "reset" };

/** The state the wizard opens in. */
export const IDLE: Enrollment = { step: "idle" };

/**
 * The RFC 9457 document behind a failure, when there is one.
 *
 * `src/api/problem.ts` is the only thing that builds one of these: every
 * REST call goes through it, so an error either is an [`ApiProblem`] and
 * carries a document, or it is a network failure, an unparseable response,
 * or a bug — none of which this module has an opinion about.
 */
function problemOf(error: unknown): Problem | null {
  return error instanceof ApiProblem ? error.document : null;
}

/** The `type` URI of an error, when it carries one. */
function problemType(error: unknown): string | null {
  return problemOf(error)?.type ?? null;
}

/**
 * Whether `error` is the control plane saying this command is finished
 * with — expired, unknown, or already spent by some other machine.
 */
export function isEnrollmentOver(error: unknown): boolean {
  const type = problemType(error);
  return (
    type?.endsWith("/enrollment-token-expired") === true ||
    type?.endsWith("/enrollment-token-not-found") === true
  );
}

/**
 * The next state.
 *
 * A poll's result is only accepted where it still means something: one that
 * lands after the wizard closed, or after the machine already arrived, must
 * not put a stale screen back up.
 */
export function nextEnrollment(state: Enrollment, event: EnrollmentEvent): Enrollment {
  switch (event.kind) {
    case "reset":
      return IDLE;
    case "mint":
      return { step: "minting" };
    case "minted":
      return { step: "waiting", token: event.token };
    case "pending":
      // Nothing has happened yet, in every state. The wizard keeps saying
      // what it was saying and the component schedules the next poll.
      return state;
    case "arrived":
      // Accepted while expired as well as while waiting: expiry is this
      // browser's clock, and a machine that enrolled in the last second is
      // a machine that enrolled. The server is the authority on that.
      return state.step === "waiting" || state.step === "expired"
        ? { step: "enrolled", host: event.host }
        : state;
    case "expire":
      return state.step === "waiting" ? { step: "expired", token: state.token } : state;
    case "failed":
      return failure(state, event.error);
  }
}

/** How a failure lands, which depends on what kind of failure it is. */
function failure(state: Enrollment, error: unknown): Enrollment {
  if (isEnrollmentOver(error)) {
    // The command going stale is an outcome with an action, not an error:
    // the wizard offers a new one rather than a message.
    return state.step === "waiting" ? { step: "expired", token: state.token } : state;
  }
  return { step: "failed", error };
}

/**
 * How long to wait before the next poll, in milliseconds, or `null` when
 * there is nothing left to poll.
 *
 * `null` is what the component reads to stop its timer, so a wizard that
 * enrolled, expired, or was closed makes no further requests.
 */
export function pollDelayMs(state: Enrollment): number | null {
  return state.step === "waiting" ? POLL_INTERVAL_SECONDS * 1000 : null;
}

/** Whether the wizard is mid-flow, which is what disables `Mint`. */
export function isRunning(state: Enrollment): boolean {
  return state.step === "minting" || state.step === "waiting";
}

/**
 * Seconds left on the command, never below zero.
 *
 * `now` is a parameter rather than `Date.now()` so the countdown and the
 * expiry decision read one clock, and so both can be tested.
 */
export function secondsUntilExpiry(token: EnrollmentToken, now: number): number {
  return Math.max(0, token.expires_at_unix - Math.floor(now / 1000));
}

/** Whether a live command has run out at `now`, which raises `expire`. */
export function hasExpired(state: Enrollment, now: number): boolean {
  return state.step === "waiting" && secondsUntilExpiry(state.token, now) === 0;
}

/**
 * How many sessions a refused removal named, or `null` when the refusal did
 * not say.
 *
 * `host-has-active-sessions` states the count as the `active_sessions`
 * extension member of its problem document (RFC 9457 §3.2), so this reads a
 * number rather than parsing the sentence in `detail` — which is written for
 * a person and free to be reworded. A refusal that carries no member yields
 * `null` and the card says that sessions are running without inventing a
 * number.
 */
export function activeSessions(error: unknown): number | null {
  const problem = problemOf(error);
  if (problem === null || !problem.type.endsWith("/host-has-active-sessions")) {
    return null;
  }
  return problem.active_sessions ?? null;
}

/** Whether `error` is the control plane refusing to remove a busy machine. */
export function isBusyHost(error: unknown): boolean {
  return problemType(error)?.endsWith("/host-has-active-sessions") === true;
}
