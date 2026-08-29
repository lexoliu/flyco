import { Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import { getAgentsMd, putAgentsMd } from "../../api/client";
import styles from "../../components/Panel.module.css";

/** Single shared AGENTS.md document, provisioned onto every session's machine. */
export default function AgentsMdTab() {
  const [doc, { refetch }] = createResource(getAgentsMd);
  const [content, setContent] = createSignal("");
  const [loadedAt, setLoadedAt] = createSignal<number | null>(null);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  const current = doc();
  if (current !== undefined && loadedAt() !== current.updated_at_unix) {
    setContent(current.content);
    setLoadedAt(current.updated_at_unix);
  }

  async function onSave(): Promise<void> {
    setSaving(true);
    setError(null);
    try {
      const saved = await putAgentsMd(content());
      setLoadedAt(saved.updated_at_unix);
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setSaving(false);
    }
  }

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>AGENTS.md</h2>
        <p class={styles.tabDescription}>
          Shared instructions provisioned onto every session's machine, alongside whatever
          AGENTS.md the repository itself carries.
        </p>
      </div>

      <ProblemNotice error={doc.error} />
      <Show when={!doc.loading}>
        <div class={styles.form}>
          <div class={styles.field}>
            <label for="agents-md-content">Content</label>
            <textarea
              id="agents-md-content"
              rows="16"
              style={{ "font-family": "var(--font-mono)" }}
              value={content()}
              onInput={(event) => setContent(event.currentTarget.value)}
            />
          </div>
          <ProblemNotice error={error()} />
          <button type="button" class={styles.primaryButton} disabled={saving()} onClick={() => void onSave()}>
            {saving() ? "Saving…" : "Save"}
          </button>
        </div>
      </Show>
    </div>
  );
}
