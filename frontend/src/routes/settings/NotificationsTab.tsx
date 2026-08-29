import { Show, createResource, createSignal, onMount } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import { getVapidPublicKey } from "../../api/client";
import { getPushSubscription, isPushSupported, subscribeToPush, unsubscribeFromPush } from "../../lib/push";
import styles from "../../components/Panel.module.css";

/**
 * Web push settings. `getVapidPublicKey` doubles as an availability check:
 * a deployment with no VAPID key configured answers 501, which
 * `ProblemNotice` already renders as a calm "not built yet" rather than an
 * error — exactly the "this deployment cannot send notifications" state
 * this tab needs, with no separate handling required.
 */
export default function NotificationsTab() {
  const [vapid] = createResource(getVapidPublicKey);
  const [subscribed, setSubscribed] = createSignal(false);
  const [checked, setChecked] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  onMount(() => {
    void getPushSubscription().then((subscription) => {
      setSubscribed(subscription !== null);
      setChecked(true);
    });
  });

  async function onEnable(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await subscribeToPush();
      setSubscribed(true);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function onDisable(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await unsubscribeFromPush();
      setSubscribed(false);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>Notifications</h2>
        <p class={styles.tabDescription}>
          Push notifications for approvals waiting on you and turns that finish while flyco isn't
          open in a tab.
        </p>
      </div>

      <ProblemNotice error={vapid.error} />
      <Show when={!vapid.loading && vapid.error === undefined}>
        <Show
          when={isPushSupported()}
          fallback={<p class={styles.empty}>This browser does not support web push.</p>}
        >
          <Show when={checked()}>
            <div class={styles.form}>
              <ProblemNotice error={error()} />
              <Show
                when={subscribed()}
                fallback={
                  <button type="button" class={styles.primaryButton} disabled={busy()} onClick={() => void onEnable()}>
                    {busy() ? "Enabling…" : "Enable notifications"}
                  </button>
                }
              >
                <button type="button" class={styles.dangerButton} disabled={busy()} onClick={() => void onDisable()}>
                  {busy() ? "Disabling…" : "Disable notifications"}
                </button>
              </Show>
            </div>
          </Show>
        </Show>
      </Show>
    </div>
  );
}
