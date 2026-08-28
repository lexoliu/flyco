import { type JSX, Show, createEffect, createSignal } from "solid-js";
import { A, useLocation, useNavigate } from "@solidjs/router";
import ThemeToggle from "./ThemeToggle";
import UpdatePrompt from "./UpdatePrompt";
import { clearSessionToken, isSignedIn } from "../lib/session";
import styles from "./AppShell.module.css";

/** Routes that render their own full-page layout, with no top nav. */
const BARE_ROUTES = new Set(["/login", "/auth/complete"]);

export default function AppShell(props: { children?: JSX.Element }) {
  const location = useLocation();
  const navigate = useNavigate();
  const [signedIn, setSignedIn] = createSignal(isSignedIn());

  // Re-check on every navigation: sign-in (from /auth/complete) and
  // sign-out both end in a navigation, so this is the one signal the
  // shell needs without standing up a global auth store.
  createEffect(() => {
    location.pathname;
    setSignedIn(isSignedIn());
  });

  function signOut(): void {
    clearSessionToken();
    setSignedIn(false);
    navigate("/login", { replace: true });
  }

  return (
    <div class={styles.shell}>
      <UpdatePrompt />
      <Show when={!BARE_ROUTES.has(location.pathname)}>
        <header class={styles.header}>
          <A href="/" class={styles.brand}>
            Flyco
          </A>
          <nav class={styles.nav} aria-label="Primary">
            <A href="/" end activeClass={styles.navActive}>
              Sessions
            </A>
            <A href="/settings" activeClass={styles.navActive}>
              Settings
            </A>
          </nav>
          <div class={styles.actions}>
            <ThemeToggle />
            <Show when={signedIn()}>
              <button type="button" class={styles.signOut} onClick={signOut}>
                Sign out
              </button>
            </Show>
          </div>
        </header>
      </Show>
      <main class={styles.main}>{props.children}</main>
    </div>
  );
}
