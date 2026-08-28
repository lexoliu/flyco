import { createSignal, Show } from "solid-js";
import { registerSW } from "virtual:pwa-register";
import styles from "./UpdatePrompt.module.css";

/**
 * Banner for the `registerType: 'prompt'` service worker: Vite's
 * vite-plugin-pwa never auto-activates a new worker, so this is the
 * update-available affordance the user has to click through, plus an
 * offline-ready notice the first time the shell gets cached.
 */
export default function UpdatePrompt() {
  const [needsRefresh, setNeedsRefresh] = createSignal(false);
  const [offlineReady, setOfflineReady] = createSignal(false);

  const updateServiceWorker = registerSW({
    onNeedRefresh() {
      setNeedsRefresh(true);
    },
    onOfflineReady() {
      setOfflineReady(true);
    },
  });

  function dismiss(): void {
    setNeedsRefresh(false);
    setOfflineReady(false);
  }

  return (
    <Show when={needsRefresh() || offlineReady()}>
      <div class={styles.banner} role="status">
        <Show
          when={needsRefresh()}
          fallback={<span class={styles.message}>Flyco is ready to work offline.</span>}
        >
          <span class={styles.message}>A new version of Flyco is available.</span>
          <button
            type="button"
            class={styles.reload}
            onClick={() => {
              void updateServiceWorker(true);
            }}
          >
            Reload
          </button>
        </Show>
        <button type="button" class={styles.dismiss} aria-label="Dismiss" onClick={dismiss}>
          ×
        </button>
      </div>
    </Show>
  );
}
