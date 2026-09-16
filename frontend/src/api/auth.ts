/**
 * The GitHub sign-in flow.
 *
 * `POST /v1/auth/github/start` returns the vendor URL to send the browser
 * to; GitHub redirects back to `/v1/auth/github/callback`, which lands the
 * browser on `/auth/complete#token=fs_…` (see src/routes/AuthComplete.tsx).
 */
import { ApiProblem } from "./problem";
import { getPublicConfig, startGithubLogin } from "./client";
import { acquireTurnstileToken } from "../lib/turnstile";

/**
 * Whether `error` is GitHub refusing the token flyco holds for the user.
 *
 * The session is fine; the credential died. The way out is a new GitHub
 * authorization from wherever the user is, never a sign-out.
 */
export function githubTokenRevoked(error: unknown): boolean {
  return error instanceof ApiProblem && error.type.endsWith("/github-token-revoked");
}

export async function beginGithubLogin(turnstileToken?: string): Promise<void> {
  let token = turnstileToken;
  if (token === undefined) {
    // Callers without a widget of their own — the "Reconnect GitHub"
    // affordances — prove the human through a hidden one on demand.
    const { turnstile_sitekey: sitekey } = await getPublicConfig();
    token = await acquireTurnstileToken(sitekey);
  }
  const { authorize_url: authorizeUrl } = await startGithubLogin(token);
  window.location.assign(authorizeUrl);
}
