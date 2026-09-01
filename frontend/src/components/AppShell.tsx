import { type JSX, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import { A, Navigate, useLocation, useNavigate } from "@solidjs/router";
import ThemeToggle from "./ThemeToggle";
import UpdatePrompt from "./UpdatePrompt";
import { clearSessionToken, isSignedIn, onSessionChanged } from "../lib/session";
import styles from "./AppShell.module.css";

/** Routes that render their own full-page layout, with no top nav. */
const BARE_ROUTES = new Set(["/login", "/auth/complete"]);

export default function AppShell(props: { children?: JSX.Element }) {
  const location = useLocation();
  const navigate = useNavigate();
  const [signedIn, setSignedIn] = createSignal(isSignedIn());
  const isBareRoute = createMemo(() => BARE_ROUTES.has(location.pathname));
  const loginHref = createMemo(() => {
    const destination = `${location.pathname}${location.search}${location.hash}`;
    return `/login?returnTo=${encodeURIComponent(destination)}`;
  });

  // Re-check on every navigation: sign-in (from /auth/complete) and
  // sign-out both end in a navigation, so this is the one signal the
  // shell needs without standing up a global auth store.
  createEffect(() => {
    location.pathname;
    setSignedIn(isSignedIn());
  });

  const stopListening = onSessionChanged(() => setSignedIn(isSignedIn()));
  onCleanup(stopListening);

  function signOut(): void {
    clearSessionToken();
    navigate("/login", { replace: true });
  }

  return (
    <Show when={location.pathname !== "/login" || !signedIn()} fallback={<Navigate href="/" />}>
      <Show when={isBareRoute() || signedIn()} fallback={<Navigate href={loginHref()} />}>
        <div class={styles.shell}>
          <UpdatePrompt />
          <Show when={!isBareRoute()}>
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
                <button type="button" class={styles.signOut} onClick={signOut}>
                  Sign out
                </button>
              </div>
            </header>
          </Show>
          <main class={styles.main}>{props.children}</main>
        </div>
      </Show>
    </Show>
  );
}
