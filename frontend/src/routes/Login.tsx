import { Show, createSignal, onMount } from "solid-js";
import { useLocation } from "@solidjs/router";
import Logomark, { GITHUB_MARK } from "../components/Logomark";
import { beginGithubLogin } from "../api/auth";
import { rememberPostLoginPath } from "../lib/postLoginPath";
import styles from "./Login.module.css";

export default function Login() {
  const location = useLocation();
  const [error, setError] = createSignal<string | null>(null);

  onMount(() => {
    const returnTo = new URLSearchParams(location.search).get("returnTo");
    if (returnTo !== null) {
      rememberPostLoginPath(returnTo);
    }
  });

  async function onSignIn(): Promise<void> {
    setError(null);
    try {
      await beginGithubLogin();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <div class={styles.page}>
      <div class={styles.card}>
        <h1 class={styles.wordmark}>flyco</h1>
        <p class={styles.lede}>Claude Code and Codex, on a machine you own.</p>
        <button
          type="button"
          class={styles.githubButton}
          onClick={() => {
            void onSignIn();
          }}
        >
          <Logomark mark={GITHUB_MARK} size={15} />
          Sign in with GitHub
        </button>
        <Show when={error()}>
          <p role="alert" class={styles.error}>
            {error()}
          </p>
        </Show>
      </div>
    </div>
  );
}
