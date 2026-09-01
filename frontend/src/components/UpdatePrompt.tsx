import { createSignal, Show } from "solid-js";
import { registerSW } from "virtual:pwa-register";
import styles from "./UpdatePrompt.module.css";

type UpdateServiceWorker = (reloadPage?: boolean) => Promise<void>;

interface ServiceWorkerControllerEvents {
  addEventListener(type: "controllerchange", listener: () => void, options: { once: true }): void;
  removeEventListener(type: "controllerchange", listener: () => void): void;
}

/**
 * Activate the waiting worker and reload once the browser confirms that the
 * new worker controls this page.
 */
export function activateWaitingWorker(
  updateServiceWorker: UpdateServiceWorker,
  serviceWorker: ServiceWorkerControllerEvents,
  reload: () => void,
): Promise<void> {
  return new Promise((resolve, reject) => {
    const onControllerChange = () => {
      serviceWorker.removeEventListener("controllerchange", onControllerChange);
      reload();
      resolve();
    };

    serviceWorker.addEventListener("controllerchange", onControllerChange, { once: true });
    void updateServiceWorker(false).catch((error: unknown) => {
      serviceWorker.removeEventListener("controllerchange", onControllerChange);
      reject(error);
    });
  });
}

/**
 * Banner for the `registerType: 'prompt'` service worker: Vite's
 * vite-plugin-pwa never auto-activates a new worker, so this is the
 * update-available affordance the user has to click through, plus an
 * offline-ready notice the first time the shell gets cached.
 */
export default function UpdatePrompt() {
  const [needsRefresh, setNeedsRefresh] = createSignal(false);
  const [offlineReady, setOfflineReady] = createSignal(false);
  const [updating, setUpdating] = createSignal(false);
  const [updateFailed, setUpdateFailed] = createSignal(false);

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

  function activateUpdate(): void {
    setUpdating(true);
    setUpdateFailed(false);
    void activateWaitingWorker(
      updateServiceWorker,
      navigator.serviceWorker,
      window.location.reload.bind(window.location),
    ).catch(() => {
      setUpdating(false);
      setUpdateFailed(true);
    });
  }

  return (
    <Show when={needsRefresh() || offlineReady()}>
      <div class={styles.banner} role="status">
        <Show
          when={needsRefresh()}
          fallback={<span class={styles.message}>Flyco is ready to work offline.</span>}
        >
          <span class={styles.message}>
            {updateFailed()
              ? "Flyco could not activate the update."
              : "A new version of Flyco is available."}
          </span>
          <button
            type="button"
            class={styles.reload}
            disabled={updating()}
            onClick={activateUpdate}
          >
            {updating() ? "Updating…" : updateFailed() ? "Retry" : "Reload"}
          </button>
        </Show>
        <button type="button" class={styles.dismiss} aria-label="Dismiss" onClick={dismiss}>
          ×
        </button>
      </div>
    </Show>
  );
}
