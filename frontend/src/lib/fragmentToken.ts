/**
 * Parses the session token out of the OAuth-complete redirect fragment.
 *
 * The control plane 303-redirects to `/auth/complete#token=fs_...` (see
 * docs/ARCHITECTURE.md, "Auth"): the token rides in the URL fragment so it
 * never reaches the server, access logs, or `Referer` headers. This is
 * pure and synchronous so it can be unit-tested without a DOM location.
 */

export class InvalidAuthFragmentError extends Error {}

/**
 * @param fragment `location.hash`, with or without its leading `#`.
 * @throws {InvalidAuthFragmentError} if the fragment is empty, has no
 *   `token` field, or the token doesn't have the `fs_` session-token shape.
 */
export function parseSessionTokenFragment(fragment: string): string {
  const withoutHash = fragment.startsWith("#") ? fragment.slice(1) : fragment;
  if (withoutHash.length === 0) {
    throw new InvalidAuthFragmentError(
      "Auth redirect had no fragment; expected #token=fs_...",
    );
  }

  const params = new URLSearchParams(withoutHash);
  const token = params.get("token");
  if (token === null || token.length === 0) {
    throw new InvalidAuthFragmentError(
      "Auth redirect fragment is missing its token field",
    );
  }
  if (!token.startsWith("fs_")) {
    throw new InvalidAuthFragmentError(
      `Auth redirect token does not have the expected fs_ prefix: ${token}`,
    );
  }

  return token;
}
