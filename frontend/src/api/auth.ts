/**
 * Seam for the GitHub sign-in flow.
 *
 * The control plane exposes `POST /v1/auth/github/start`, which returns the
 * URL to send the browser to (see /openapi.json). The generated client
 * (src/api/types.ts) will provide the typed request; until it lands this
 * throws rather than guessing the request/response shape or hand-rolling a
 * fetch call against an API surface that may still change.
 */
export async function beginGithubLogin(): Promise<never> {
  throw new Error(
    "GitHub sign-in is not wired yet: beginGithubLogin() needs the generated client for POST /v1/auth/github/start (see src/api/types.ts).",
  );
}
