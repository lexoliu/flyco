import {
  type JSX,
  Show,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
} from "solid-js";
import { createQuery } from "../lib/query";
import { A, Navigate, useLocation, useNavigate } from "@solidjs/router";
import {
  LogOut,
  Menu,
  PanelLeftClose,
  Settings as SettingsIcon,
  SquarePen,
} from "lucide-solid";
import Logomark, { FLYCO_MARK } from "./Logomark";
import Popover from "./Popover";
import ProblemNotice from "./ProblemNotice";
import { ReadinessProvider } from "./Readiness";
import SessionNav from "./SessionNav";
import SettingsNav from "./SettingsNav";
import { getMe } from "../api/client";
import { isSignedIn, onSessionChanged } from "../lib/session";
import { signOut } from "../lib/signOut";
import { cx } from "../lib/cx";
import styles from "./AppShell.module.css";

/**
 * Routes a signed-out visitor may see: the sign-in itself and its return.
 *
 * Everything else — the first run included — belongs to a flyco account,
 * so a visit without a session goes to sign-in first and comes back.
 */
const PUBLIC_ROUTES = new Set(["/login", "/auth/complete"]);

/** Routes that render their own full-page layout, with no rail. */
const BARE_ROUTES = new Set([
  ...PUBLIC_ROUTES,
  "/welcome",
  "/connect/return",
  // The CLI's sign-in page is a single decision card; the session rail it
  // would sit beside is the product it is not yet a part of.
  "/cli/authorize",
]);

/**
 * Up to two letters standing in for an avatar.
 *
 * A GitHub login is the only name flyco has for its user, so the initials
 * come from it: `lexo-liu` and `lexoLiu` both read as `LL`, and a
 * single-word login keeps its first letter rather than inventing a second.
 */
export function initialsOf(login: string): string {
  const parts = login
    .split(/[^a-zA-Z0-9]+|(?=[A-Z])/u)
    .filter((part) => part.length > 0);
  const letters = parts.slice(0, 2).map((part) => part.charAt(0));
  return (
    letters.length > 0 ? letters.join("") : login.charAt(0)
  ).toUpperCase();
}

export default function AppShell(props: { children?: JSX.Element }) {
  const location = useLocation();
  const navigate = useNavigate();
  const [signedIn, setSignedIn] = createSignal(isSignedIn());
  const [railOpen, setRailOpen] = createSignal(false);
  const isBareRoute = createMemo(() => BARE_ROUTES.has(location.pathname));
  /**
   * Settings takes the rail over rather than standing a second nav column
   * beside it. A person in settings is not switching sessions; they came to
   * change one thing and leave, and the list they left is one click away at
   * the top of the same column.
   */
  const inSettings = createMemo(() => location.pathname.startsWith("/settings"));
  const isPublicRoute = createMemo(() => PUBLIC_ROUTES.has(location.pathname));
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
    setRailOpen(false);
  });

  const stopListening = onSessionChanged(() => setSignedIn(isSignedIn()));
  onCleanup(stopListening);

  return (
    <Show
      when={location.pathname !== "/login" || !signedIn()}
      fallback={<Navigate href="/" />}
    >
      <Show
        when={isPublicRoute() || signedIn()}
        fallback={<Navigate href={loginHref()} />}
      >
        <ReadinessProvider enabled={signedIn}>
          <div class={styles.shell}>
            <Show when={!isBareRoute()}>
              {/* The opener floats over the page rather than sitting in a
                  band of its own, so a phone spends its height on the work
                  and not on a bar that only holds a hamburger. */}
              <button
                type="button"
                class={styles.railOpen}
                aria-label="Open navigation"
                aria-expanded={railOpen()}
                onClick={() => setRailOpen(true)}
              >
                <Menu size={16} aria-hidden="true" />
              </button>
              <Show when={railOpen()}>
                <button
                  type="button"
                  class={styles.scrim}
                  aria-label="Close navigation"
                  onClick={() => setRailOpen(false)}
                />
              </Show>
              <nav
                class={cx(styles.rail, railOpen() && styles.railOpened)}
                aria-label="Primary"
              >
                <div class={styles.brandRow}>
                  <A href="/" class={styles.brand}>
                    <Logomark mark={FLYCO_MARK} class={styles.brandMark} />
                    flyco
                  </A>
                  <button
                    type="button"
                    class={styles.railClose}
                    aria-label="Close navigation"
                    onClick={() => setRailOpen(false)}
                  >
                    <PanelLeftClose size={16} aria-hidden="true" />
                  </button>
                </div>
                <Show
                  when={inSettings()}
                  fallback={
                    <>
                      <A href="/" end class={styles.newSession}>
                        <SquarePen size={15} aria-hidden="true" />
                        New session
                      </A>
                      <SessionNav onNavigate={() => setRailOpen(false)} />
                    </>
                  }
                >
                  <SettingsNav onNavigate={() => setRailOpen(false)} />
                </Show>
                <AccountMenu onSignOut={() => signOut(navigate)} />
              </nav>
            </Show>
            <main class={styles.main}>{props.children}</main>
          </div>
        </ReadinessProvider>
      </Show>
    </Show>
  );
}

/**
 * The account row at the foot of the rail, and everything behind it.
 *
 * Settings live here rather than beside the session list because they are
 * not a place the work goes: a person opens them to link an account or
 * change a default and then leaves. Putting them in the list would give a
 * page visited monthly the same standing as the sessions visited hourly.
 */
function AccountMenu(props: { onSignOut: () => void }) {
  const [me] = createQuery(getMe);
  const initials = createMemo(() => {
    const login = me()?.login;
    return login === undefined ? "" : initialsOf(login);
  });

  return (
    <Popover
      label="Account"
      align="start"
      side="top"
      panelClass={styles.menu}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={styles.account}
          aria-label={
            me() === undefined ? "Account" : `Account: ${me()?.login ?? ""}`
          }
        >
          <span class={styles.avatar} aria-hidden="true">
            <Show
              when={initials()}
              fallback={<span class={styles.avatarBlank} />}
            >
              {initials()}
            </Show>
          </span>
          <span class={styles.accountName}>{me()?.login ?? "Account"}</span>
        </button>
      )}
    >
      {(close) => (
        <>
          {/* The shell renders on every route, so a failed `GET /v1/me` must
              cost the identity line and nothing else; the menu says what went
              wrong where the name would have been. */}
          <ProblemNotice error={me.error} />
          <Show when={me()}>
            {(user) => (
              <p class={styles.menuIdentity}>
                Signed in as <strong>{user().login}</strong>
              </p>
            )}
          </Show>
          <A href="/settings" class={styles.menuLink} onClick={() => close()}>
            <SettingsIcon size={14} aria-hidden="true" />
            Settings
          </A>
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
