/**
 * "Something is still running there", as the control plane says it.
 *
 * Two refusals mean it — `provider-in-use` when unlinking a cloud account
 * would strand machines, and `host-has-active-sessions` when removing an
 * enrolled machine would stop containers on it — and the browser answers
 * both the same way: say how much is still running, and offer the thing
 * that ends it. So the two are read here once rather than at each of the
 * places that has to explain one (issue #139).
 *
 * The count is read as an RFC 9457 §3.2 extension member where the refusal
 * carries one, never parsed out of `detail`: that sentence is written for a
 * person and is free to be reworded. A refusal that states no member yields
 * `null`, and the caller says that sessions are running without inventing a
 * number.
 */
import { ApiProblem } from "../api/problem";

/** Problem-type suffixes that mean "still in use", not "went wrong". */
const IN_USE_SUFFIXES = ["/provider-in-use", "/host-has-active-sessions"];

/** What a refusal of that kind carries. */
export interface InUseRefusal {
  /** How many sessions are still running there, when the refusal said. */
  sessions: number | null;
  /** The control plane's own sentence, for a refusal that counted nothing. */
  detail: string;
}

/**
 * Reads a failure as an in-use refusal, or `null` when it is something else
 * — a network failure, a 500, a problem type this has no opinion about.
 */
export function inUseRefusal(error: unknown): InUseRefusal | null {
  if (!(error instanceof ApiProblem)) {
    return null;
  }
  if (!IN_USE_SUFFIXES.some((suffix) => error.type.endsWith(suffix))) {
    return null;
  }
  const counted = error.document.active_sessions;
  return {
    sessions: typeof counted === "number" ? counted : null,
    detail: error.detail,
  };
}

/** Whether a sentence already ends as one. */
const ENDS_A_SENTENCE = /[.!?]$/;

/**
 * How much is still running there, as a sentence the guidance can follow.
 *
 * `2 sessions are still running there.` where the refusal counted, and the
 * control plane's own words where it did not — closed with a full stop if
 * it did not bring one, because what comes after it in the dialog is
 * another sentence and the two would otherwise run together.
 */
export function sessionsStillRunning(refusal: InUseRefusal): string {
  const count = refusal.sessions;
  if (count === null) {
    const said = refusal.detail.trim();
    return ENDS_A_SENTENCE.test(said) ? said : `${said}.`;
  }
  return `${count} ${count === 1 ? "session is" : "sessions are"} still running there.`;
}
