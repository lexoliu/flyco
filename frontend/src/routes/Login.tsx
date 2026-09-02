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
        <p class={styles.lede}>
          The official Claude Code and Codex, on a computer you own. You bring the agent and the
          machine; flyco runs the session and keeps the budget.
        </p>
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
        <p class={styles.footnote}>
          Flyco reads your repositories so an agent can work in them. It never holds your cloud
          bill or your model tokens — both stay on your own accounts.
        </p>
        <Show when={error()}>
          <p role="alert" class={styles.error}>
            {error()}
          </p>
        </Show>
      </div>
    </div>
  );
}
