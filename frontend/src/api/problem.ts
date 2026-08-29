/**
 * RFC 9457 problem handling for every REST call the client makes.
 *
 * flyco's API answers every error as `application/problem+json` (RFC 9457):
 * `{ type, title, status, detail }`. This module turns that document into a
 * typed error, and gives the single most important failure mode — a
 * capability that is documented but not built yet — its own class so the UI
 * can render a calm "not built yet" state instead of a crash.
 *
 * Most of this API is deliberately unimplemented right now: the backend has
 * dozens of handlers being filled in milestone by milestone. Until they
 * ship, they answer `501 Not Implemented` with a problem `type` ending in
 * `/not-implemented` (see docs/ARCHITECTURE.md's `problems/relay-unavailable`
 * for the one that ships today; the generic suffix covers every handler
 * that answers the same way once it exists). A response in that shape must
 * never be treated as a generic error.
 */
import type { components } from "./schema";

export type Problem = components["schemas"]["Problem"];

/** A non-2xx response that carried a well-formed RFC 9457 problem document. */
export class ApiProblem extends Error {
  readonly type: string;
  readonly title: string;
  readonly status: number;
  readonly detail: string;

  constructor(problem: Problem) {
    super(`${problem.title}: ${problem.detail}`);
    this.name = "ApiProblem";
    this.type = problem.type;
    this.title = problem.title;
    this.status = problem.status;
    this.detail = problem.detail;
  }
}

/**
 * A documented capability that this build of the control plane does not
 * implement yet.
 *
 * Distinguished from every other `ApiProblem` by the `type` URI's suffix,
 * so the UI can render "not built yet" instead of an error toast wherever
 * one of these surfaces.
 */
export class NotImplementedError extends ApiProblem {
  constructor(problem: Problem) {
    super(problem);
    this.name = "NotImplementedError";
  }
}

/**
 * Problem-type suffixes that mean "this build does not have this
 * capability yet", not "something went wrong".
 *
 * `/not-implemented` is the generic suffix the spec describes for a
 * handler that has not shipped. `/relay-unavailable` is the one concrete
 * 501 flyco emits today (`ApiError::RelayUnavailable` in
 * `crates/api/src/error.rs`) — the session relay is unbuildable on a
 * native/dev run of the control plane (no Durable Object simulator), which
 * is the same "not on this build" fact from the browser's point of view,
 * so it gets the same calm treatment rather than a distinct error class.
 */
const NOT_IMPLEMENTED_SUFFIXES = ["/not-implemented", "/relay-unavailable"];

/**
 * A non-2xx response whose body was not a problem document at all — a
 * framework-level failure, a proxy error page, or anything else that isn't
 * describing itself the way flyco's own API does. Deliberately distinct
 * from [`ApiProblem`]: pretending an unstructured body is a problem
 * document would be inventing fields that were never there.
 */
export class UnexpectedResponseError extends Error {
  readonly status: number;
  readonly bodyText: string;

  constructor(status: number, bodyText: string) {
    super(`Unexpected ${status} response (not a problem document)`);
    this.name = "UnexpectedResponseError";
    this.status = status;
    this.bodyText = bodyText;
  }
}

/** The request never reached a server: offline, DNS, CORS, a dead socket. */
export class NetworkError extends Error {
  constructor(cause: unknown) {
    super(cause instanceof Error ? cause.message : String(cause));
    this.name = "NetworkError";
    this.cause = cause;
  }
}

/**
 * Builds the right error class for a non-2xx response.
 *
 * A response whose media type is `application/problem+json` is parsed as a
 * [`Problem`]; anything ending in `/not-implemented` becomes a
 * [`NotImplementedError`] rather than a plain [`ApiProblem`], because the
 * UI treats "not built yet" as a first-class, calm state rather than an
 * error. Anything else that is not a problem document becomes an
 * [`UnexpectedResponseError`] — it is never coerced into looking like one.
 */
export async function problemFromResponse(response: Response): Promise<Error> {
  const contentType = response.headers.get("content-type") ?? "";
  if (!contentType.includes("application/problem+json")) {
    const bodyText = await response.text();
    return new UnexpectedResponseError(response.status, bodyText);
  }

  const problem = (await response.json()) as Problem;
  if (NOT_IMPLEMENTED_SUFFIXES.some((suffix) => problem.type.endsWith(suffix))) {
    return new NotImplementedError(problem);
  }
  return new ApiProblem(problem);
}
