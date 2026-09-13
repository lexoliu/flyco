import { For, Show, createSignal } from "solid-js";
import { Plus, X } from "lucide-solid";
import { createQuery } from "../lib/query";
import ProblemNotice from "./ProblemNotice";
import { getSessionEnv, putSessionEnv, type EnvEntry } from "../api/client";
import styles from "./EnvEditor.module.css";

/**
 * The `.env` editor for one session. Lives on the session detail page
 * rather than settings, since a `.env` file belongs to a session, not the
 * account — see `EnvDocument.warning`, which this always renders verbatim
 * rather than hard-coding the network-control caveat on the frontend.
 */
export default function EnvEditor(props: { sessionId: string }) {
  const [doc, { refetch }] = createQuery(() => props.sessionId, getSessionEnv);
  const [entries, setEntries] = createSignal<EnvEntry[]>([]);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [loadedFor, setLoadedFor] = createSignal<string | null>(null);

  // Seed the editable draft from the fetched document, once per session switch.
  const current = doc();
  if (current !== undefined && loadedFor() !== props.sessionId) {
    setEntries(current.entries);
    setLoadedFor(props.sessionId);
  }

  function updateEntry(index: number, field: "key" | "value", value: string): void {
    setEntries((prev) => prev.map((entry, i) => (i === index ? { ...entry, [field]: value } : entry)));
  }

  function removeEntry(index: number): void {
    setEntries((prev) => prev.filter((_, i) => i !== index));
  }

  function addEntry(): void {
    setEntries((prev) => [...prev, { key: "", value: "" }]);
  }

  async function onSave(): Promise<void> {
    setSaving(true);
    setError(null);
    try {
      await putSessionEnv(
        props.sessionId,
        entries().filter((entry) => entry.key.trim() !== ""),
      );
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setSaving(false);
    }
  }

  return (
    <div class={styles.tab}>
      <ProblemNotice error={doc.error} />
      <Show when={!doc.loading}>
        <Show when={doc()} fallback={<p class={styles.empty}>No .env file yet for this session.</p>}>
          {(document) => (
            <>
              <p class={styles.tabDescription}>{document().warning}</p>
              <div class={styles.form}>
                <For each={entries()}>
                  {(entry, index) => (
                    <div class={styles.row}>
                      <input
                        class={styles.input}
                        aria-label="Variable name"
                        value={entry.key}
                        onInput={(event) => updateEntry(index(), "key", event.currentTarget.value)}
                        placeholder="KEY"
                        autocomplete="off"
                        autocapitalize="off"
                        spellcheck={false}
                      />
                      <input
                        class={styles.input}
                        aria-label="Variable value"
                        value={entry.value}
                        onInput={(event) => updateEntry(index(), "value", event.currentTarget.value)}
                        placeholder="value"
                        autocomplete="off"
                        autocapitalize="off"
                        spellcheck={false}
                      />
                      <button
                        type="button"
                        class={styles.remove}
                        aria-label={entry.key ? `Remove ${entry.key}` : "Remove variable"}
                        onClick={() => removeEntry(index())}
                      >
                        <X size={14} aria-hidden="true" />
                      </button>
                    </div>
                  )}
                </For>
                <ProblemNotice error={error()} />
                <div class={styles.footer}>
                  <button type="button" class={styles.add} onClick={addEntry}>
                    <Plus size={14} aria-hidden="true" />
                    Add variable
                  </button>
                  <button type="button" class={styles.save} disabled={saving()} onClick={() => void onSave()}>
                    {saving() ? "Saving…" : "Save"}
                  </button>
                </div>
              </div>
            </>
          )}
        </Show>
      </Show>
    </div>
  );
}
