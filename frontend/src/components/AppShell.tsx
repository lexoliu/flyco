import {
  type JSX,
  Show,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  onCleanup,
} from "solid-js";
import { A, Navigate, useLocation, useNavigate } from "@solidjs/router";
import { Check, LogOut, Monitor, Moon, Sun } from "lucide-solid";
import Popover from "./Popover";
import UpdatePrompt from "./UpdatePrompt";
import { ReadinessProvider } from "./Readiness";
import { getMe } from "../api/client";
import { clearSessionToken, isSignedIn, onSessionChanged } from "../lib/session";
import { type ThemePreference, readStoredThemePreference, setTheme } from "../lib/theme";
import { cx } from "../lib/cx";
import styles from "./AppShell.module.css";

/** Routes that render their own full-page layout, with no top bar. */
const BARE_ROUTES = new Set(["/login", "/auth/complete", "/welcome"]);

const THEMES: readonly { value: ThemePreference; label: string; icon: typeof Sun }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

/**
 * Up to two letters standing in for an avatar.
 *
 * A GitHub login is the only name flyco has for its user, so the initials
 * come from it: `lexo-liu` and `lexoLiu` both read as `LL`, and a
 * single-word login keeps its first letter rather than inventing a second.
 */
export function initialsOf(login: string): string {
  const parts = login.split(/[^a-zA-Z0-9]+|(?=[A-Z])/u).filter((part) => part.length > 0);
  const letters = parts.slice(0, 2).map((part) => part.charAt(0));
  return (letters.length > 0 ? letters.join("") : login.charAt(0)).toUpperCase();
}

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
        <ReadinessProvider enabled={signedIn}>
          <div class={styles.shell}>
            <UpdatePrompt />
            <Show when={!isBareRoute()}>
              <header class={styles.header}>
                <A href="/" class={styles.brand}>
                  flyco
                </A>
                <nav class={styles.nav} aria-label="Primary">
                  <A href="/" end activeClass={styles.navActive}>
                    Sessions
                  </A>
                  <A href="/settings" activeClass={styles.navActive}>
                    Settings
                  </A>
                </nav>
                <AccountMenu onSignOut={signOut} />
              </header>
            </Show>
            <main class={styles.main}>{props.children}</main>
          </div>
        </ReadinessProvider>
      </Show>
    </Show>
  );
}

/**
 * The avatar and everything behind it.
 *
 * The top bar carries navigation and identity and nothing else; appearance
 * and signing out are account business, so they live under the avatar
 * rather than as two more buttons competing with `Sessions`.
 */
function AccountMenu(props: { onSignOut: () => void }) {
  const [me] = createResource(getMe);
  const [theme, setPreference] = createSignal<ThemePreference>(readStoredThemePreference());
  const initials = createMemo(() => {
    const login = me()?.login;
    return login === undefined ? "" : initialsOf(login);
  });

  function choose(preference: ThemePreference): void {
    setTheme(preference);
    setPreference(preference);
  }

  return (
    <Popover
      label="Account"
      align="end"
      panelClass={styles.menu}
      trigger={(attrs) => (
        <button
          {...attrs}
          type="button"
          class={styles.avatar}
          aria-label={me() === undefined ? "Account" : `Account: ${me()?.login ?? ""}`}
        >
          <Show when={initials()} fallback={<span class={styles.avatarBlank} />}>
            {initials()}
          </Show>
        </button>
      )}
    >
      {(close) => (
        <>
          <Show when={me()}>
            {(user) => (
              <p class={styles.menuIdentity}>
                Signed in as <strong>{user().login}</strong>
              </p>
            )}
          </Show>
          <p class={styles.menuLabel} id="appearance-label">
            Appearance
          </p>
          <div class={styles.themeRow} role="group" aria-labelledby="appearance-label">
            {THEMES.map((option) => (
              <button
                type="button"
                class={cx(styles.themeOption, theme() === option.value && styles.themeChosen)}
                aria-pressed={theme() === option.value}
                onClick={() => choose(option.value)}
              >
                <option.icon size={14} aria-hidden="true" />
                {option.label}
                <Show when={theme() === option.value}>
                  <Check size={13} aria-hidden="true" class={cx(styles.themeCheck)} />
                </Show>
              </button>
            ))}
          </div>
          <button
            type="button"
            class={styles.menuAction}
            onClick={() => {
              close();
              props.onSignOut();
            }}
          >
            <LogOut size={14} aria-hidden="true" />
            Sign out
          </button>
        </>
      )}
    </Popover>
  );
}
