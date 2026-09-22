/**
 * Settings → API keys.
 *
 * The credentials that call the flyco API without a browser session: the
 * CLI on a machine, a script, CI. Its own section because it is the one
 * page here that is about a credential rather than about the person.
 */
import { For, Show, createSignal } from "solid-js";
import { createQuery } from "../../lib/query";
import { Plus } from "lucide-solid";
import ProblemNotice from "../../components/ProblemNotice";
import CopyButton from "../../components/CopyButton";
import {
  createApiKey,
  listApiKeys,
  revokeApiKey,
  type CreatedApiKey,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import styles from "./Settings.module.css";

export default function ApiKeysSection() {
  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>API keys</h2>
      </header>

      <ApiKeys />
    </section>
  );
}

function ApiKeys() {
  const [keys, { refetch }] = createQuery(listApiKeys);
  const [label, setLabel] = createSignal("");
  const [creating, setCreating] = createSignal(false);
  const [created, setCreated] = createSignal<CreatedApiKey | null>(null);
  const [error, setError] = createSignal<unknown>(null);

  async function create(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setCreating(true);
    setError(null);
    try {
      setCreated(await createApiKey(label()));
      setLabel("");
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setCreating(false);
    }
  }

  async function revoke(id: string): Promise<void> {
    setError(null);
    try {
      await revokeApiKey(id);
      await refetch();
    } catch (err) {
      setError(err);
    }
  }

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>API keys</p>
      <ProblemNotice error={keys.error ?? error()} />

      <Show when={created()}>
        {(key) => (
          <article class={styles.card}>
            <div class={styles.identity}>
              <span class={styles.cardTitle}>{key().label}</span>
              <span class={styles.cardMeta}>
                This is the only time this key is shown. Copy it before you leave the page.
              </span>
            </div>
            <div class={styles.secret}>
              <span class={styles.secretValue}>{key().token}</span>
              <div class={styles.actions}>
                <CopyButton value={key().token} class={styles.pill} label="Copy key" />
                <button type="button" class={styles.pill} onClick={() => setCreated(null)}>
                  Done
                </button>
              </div>
            </div>
          </article>
        )}
      </Show>

      <Show when={(keys() ?? []).length > 0}>
        <div class={styles.cards}>
          <For each={keys()}>
            {(key) => (
              <article class={styles.card}>
                <div class={styles.cardTop}>
                  <div class={styles.identity}>
                    <span class={styles.cardTitle}>{key.label}</span>
                    <span class={styles.cardMeta}>
                      Created {formatDate(key.created_at_unix)} ·{" "}
                      {key.last_used_unix === null || key.last_used_unix === undefined
                        ? "never used"
                        : `last used ${formatDate(key.last_used_unix)}`}
                    </span>
                  </div>
                  <div class={styles.actions}>
                    <button
                      type="button"
                      class={styles.pillDanger}
                      onClick={() => void revoke(key.id)}
                    >
                      Revoke
                    </button>
                  </div>
                </div>
              </article>
            )}
          </For>
        </div>
      </Show>

      <article class={styles.card}>
        <form class={styles.form} onSubmit={(event) => void create(event)}>
          <div class={styles.field}>
            <label for="api-key-label">New key for</label>
            <input
              id="api-key-label"
              value={label()}
              onInput={(event) => setLabel(event.currentTarget.value)}
              placeholder="CI"
              required
            />
          </div>
          <div class={styles.formActions}>
            <button type="submit" class={styles.pillPrimary} disabled={creating()}>
              <Plus size={14} aria-hidden="true" />
              {creating() ? "Creating…" : "Create key"}
            </button>
            <span class={styles.note}>Keys call the flyco API without a browser session.</span>
          </div>
        </form>
      </article>
    </div>
  );
}

