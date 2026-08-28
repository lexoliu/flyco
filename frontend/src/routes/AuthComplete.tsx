import { Show, createSignal, onMount } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { parseSessionTokenFragment } from "../lib/fragmentToken";
import { setSessionToken } from "../lib/session";
import styles from "./AuthComplete.module.css";

/**
 * The control plane 303-redirects here with the session token in the URL
 * fragment (`#token=fs_...`, see docs/ARCHITECTURE.md — "Auth"). This
 * reads it once on mount, stores it, and forwards to `/`.
 */
export default function AuthComplete() {
  const navigate = useNavigate();
  const [error, setError] = createSignal<string | null>(null);

  onMount(() => {
    try {
      const token = parseSessionTokenFragment(window.location.hash);
      setSessionToken(token);
      navigate("/", { replace: true });
    } catch (err) {
      if (!(err instanceof Error)) {
        throw err;
      }
      setError(err.message);
    }
  });

  return (
    <Show
      when={error()}
      fallback={
        <p class={styles.status} role="status">
          Signing you in…
        </p>
      }
    >
      <div class={styles.errorPage} role="alert">
        <h1>Sign-in failed</h1>
        <p>{error()}</p>
        <a href="/login">Back to sign-in</a>
      </div>
    </Show>
  );
}
