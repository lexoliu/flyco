import { Show, createSignal, onCleanup, onMount } from "solid-js";
import { useLocation } from "@solidjs/router";
import Logomark, { FLYCO_MARK, GITHUB_MARK } from "../components/Logomark";
import { beginGithubLogin } from "../api/auth";
import { getPublicConfig } from "../api/client";
import { TURNSTILE_ACTION, type TurnstileApi, loadTurnstile } from "../lib/turnstile";
import { rememberPostLoginPath } from "../lib/postLoginPath";
import styles from "./Login.module.css";

export default function Login() {
  const location = useLocation();
  const [error, setError] = createSignal<string | null>(null);
  const [token, setToken] = createSignal<string | null>(null);
  const [starting, setStarting] = createSignal(false);
  let challengeHost!: HTMLDivElement;
  let turnstile: TurnstileApi | undefined;
  let widgetId: string | undefined;

  onMount(async () => {
    const returnTo = new URLSearchParams(location.search).get("returnTo");
    if (returnTo !== null) {
      rememberPostLoginPath(returnTo);
    }

    try {
      const { turnstile_sitekey: sitekey } = await getPublicConfig();
      turnstile = await loadTurnstile();
      widgetId = turnstile.render(challengeHost, {
        sitekey,
        action: TURNSTILE_ACTION,
        callback: setToken,
        // The widget refreshes itself after expiring and calls `callback`
        // again with the new token, so expiry only drops the dead one.
        "expired-callback": () => setToken(null),
        "error-callback": () => {
          setToken(null);
          setError("the human check could not run — reload the page");
        },
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  });

  onCleanup(() => {
    if (widgetId !== undefined) {
      turnstile?.remove(widgetId);
    }
  });

  async function onSignIn(): Promise<void> {
    const proof = token();
    if (proof === null || starting()) {
      return;
    }
    setStarting(true);
    setError(null);
    try {
      await beginGithubLogin(proof);
    } catch (err) {
      // The token was spent proving nothing — a fresh widget answers a
      // fresh one, or sign-in is impossible either way.
      setStarting(false);
      setToken(null);
      turnstile?.reset(widgetId);
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <div class={styles.page}>
      <div class={styles.card}>
        <h1 class={styles.wordmark}>
          <Logomark mark={FLYCO_MARK} class={styles.wordmarkMark} />
          flyco
        </h1>
        <p class={styles.lede}>Claude Code and Codex, on a machine you own.</p>
        <button
          type="button"
          class={styles.githubButton}
          disabled={token() === null || starting()}
          onClick={() => {
            void onSignIn();
          }}
        >
          <Logomark mark={GITHUB_MARK} size={15} />
          Sign in with GitHub
        </button>
        <div ref={challengeHost} class={styles.challenge} />
        <Show when={error()}>
          <p role="alert" class={styles.error}>
            {error()}
          </p>
        </Show>
      </div>
    </div>
  );
}
