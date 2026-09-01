import { Show, createSignal, onMount } from "solid-js";
import { useLocation } from "@solidjs/router";
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
        <h1>Flyco</h1>
        <p>Agentic coding on the web, with flexible cloud computing.</p>
        <button
          type="button"
          class={styles.githubButton}
          onClick={() => {
            void onSignIn();
          }}
        >
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
