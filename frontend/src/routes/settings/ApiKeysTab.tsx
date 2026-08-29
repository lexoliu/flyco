import { For, Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import { createApiKey, listApiKeys, revokeApiKey, type CreatedApiKey } from "../../api/client";
import styles from "../../components/Panel.module.css";

function formatUnix(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "Never";
  return new Date(seconds * 1000).toLocaleString();
}

export default function ApiKeysTab() {
  const [keys, { refetch }] = createResource(listApiKeys);
  const [label, setLabel] = createSignal("");
  const [creating, setCreating] = createSignal(false);
  const [created, setCreated] = createSignal<CreatedApiKey | null>(null);
  const [error, setError] = createSignal<unknown>(null);

  async function onCreate(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setCreating(true);
    setError(null);
    try {
      const key = await createApiKey(label());
      setCreated(key);
      setLabel("");
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setCreating(false);
    }
  }

  async function onRevoke(id: string): Promise<void> {
    setError(null);
    try {
      await revokeApiKey(id);
      await refetch();
    } catch (err) {
      setError(err);
    }
  }

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>API keys</h2>
        <p class={styles.tabDescription}>
          Keys let scripts and CI call the flyco API without a browser session. Each key is shown
          exactly once, right after creation — store it somewhere safe.
        </p>
      </div>

      <form class={styles.form} onSubmit={(event) => void onCreate(event)}>
        <div class={styles.field}>
          <label for="api-key-label">Label</label>
          <input id="api-key-label" value={label()} onInput={(event) => setLabel(event.currentTarget.value)} required />
        </div>
        <ProblemNotice error={error()} />
        <button type="submit" class={styles.primaryButton} disabled={creating()}>
          {creating() ? "Creating…" : "Create key"}
        </button>
      </form>

      <Show when={created()}>
        {(key) => (
          <div>
            <p class={styles.tabDescription}>
              This is the only time "{key().label}" will be shown. Copy it now.
            </p>
            <p class={styles.plaintext}>{key().token}</p>
          </div>
        )}
      </Show>

      <ProblemNotice error={keys.error} />
      <Show when={!keys.loading}>
        <Show
          when={keys.error !== undefined || (keys() ?? []).length > 0}
          fallback={<p class={styles.empty}>No API keys yet.</p>}
        >
          <ul class={styles.list}>
            <For each={keys()}>
              {(key) => (
                <li class={styles.listItem}>
                  <div>
                    <strong>{key.label}</strong>
                    <p class={styles.itemDetail}>
                      Created {formatUnix(key.created_at_unix)} · Last used {formatUnix(key.last_used_unix)}
                    </p>
                  </div>
                  <div class={styles.itemActions}>
                    <button type="button" class={styles.dangerButton} onClick={() => void onRevoke(key.id)}>
                      Revoke
                    </button>
                  </div>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </div>
  );
}
