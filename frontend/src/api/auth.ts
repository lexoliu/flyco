/**
 * The GitHub sign-in flow.
 *
 * `POST /v1/auth/github/start` returns the vendor URL to send the browser
 * to; GitHub redirects back to `/v1/auth/github/callback`, which lands the
 * browser on `/auth/complete#token=fs_…` (see src/routes/AuthComplete.tsx).
 */
import { ApiProblem } from "./problem";
import { startGithubLogin } from "./client";

/**
 * Whether `error` is GitHub refusing the token flyco holds for the user.
 *
 * The session is fine; the credential died. The way out is a new GitHub
 * authorization from wherever the user is, never a sign-out.
 */
export function githubTokenRevoked(error: unknown): boolean {
  return error instanceof ApiProblem && error.type.endsWith("/github-token-revoked");
}

export async function beginGithubLogin(): Promise<void> {
  const { authorize_url: authorizeUrl } = await startGithubLogin();
  window.location.assign(authorizeUrl);
}
