/**
 * The GitHub sign-in flow.
 *
 * `POST /v1/auth/github/start` returns the vendor URL to send the browser
 * to; GitHub redirects back to `/v1/auth/github/callback`, which lands the
 * browser on `/auth/complete#token=fs_…` (see src/routes/AuthComplete.tsx).
 */
import { startGithubLogin } from "./client";

export async function beginGithubLogin(): Promise<void> {
  const { authorize_url: authorizeUrl } = await startGithubLogin();
  window.location.assign(authorizeUrl);
}
