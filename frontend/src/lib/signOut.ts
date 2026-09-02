/**
 * Signing out, in one place.
 *
 * Two screens offer it — the account menu in the top bar and the Account
 * section of settings — and they must do exactly the same two things:
 * forget the token, then replace the current history entry with the sign-in
 * page so the back button cannot return to a screen the user no longer has
 * a credential for.
 */
import type { useNavigate } from "@solidjs/router";
import { clearSessionToken } from "./session";

export function signOut(navigate: ReturnType<typeof useNavigate>): void {
  clearSessionToken();
  navigate("/login", { replace: true });
}
