import { For, Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import { getSessionEnv, listSessions, putSessionEnv, type EnvEntry } from "../../api/client";
import styles from "./Tab.module.css";

function EnvEditor(props: { sessionId: string }) {
  const [doc, { refetch }] = createResource(() => props.sessionId, getSessionEnv);
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
                    <div class={styles.field} style={{ "flex-direction": "row", gap: "var(--space-2)" }}>
                      <input
                        aria-label="Variable name"
                        value={entry.key}
                        onInput={(event) => updateEntry(index(), "key", event.currentTarget.value)}
                        placeholder="KEY"
                      />
                      <input
                        aria-label="Variable value"
                        value={entry.value}
                        onInput={(event) => updateEntry(index(), "value", event.currentTarget.value)}
                        placeholder="value"
                      />
                      <button type="button" class={styles.dangerButton} onClick={() => removeEntry(index())}>
                        Remove
                      </button>
                    </div>
                  )}
                </For>
                <button type="button" onClick={addEntry}>
                  Add variable
                </button>
                <ProblemNotice error={error()} />
                <button type="button" class={styles.primaryButton} disabled={saving()} onClick={() => void onSave()}>
                  {saving() ? "Saving…" : "Save"}
                </button>
              </div>
            </>
          )}
        </Show>
      </Show>
    </div>
  );
}

export default function EnvTab() {
  const [sessions] = createResource(listSessions);
  const [sessionId, setSessionId] = createSignal<string | null>(null);

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>.env</h2>
        <p class={styles.tabDescription}>
          Agents can read this file but not edit it; you can. Flyco doesn't yet control which
          hosts a session can reach, so secrets placed here may still leak over the network —
          keep that in mind until network control ships. The .env file is per session, so pick
          one below.
        </p>
      </div>

      <ProblemNotice error={sessions.error} />
      <Show when={!sessions.loading}>
        <Show
          when={sessions.error !== undefined || (sessions() ?? []).length > 0}
          fallback={<p class={styles.empty}>No sessions yet — create one to edit its .env.</p>}
        >
          <div class={styles.field}>
            <label for="env-session">Session</label>
            <select
              id="env-session"
              value={sessionId() ?? ""}
              onChange={(event) => setSessionId(event.currentTarget.value === "" ? null : event.currentTarget.value)}
            >
              <option value="">Choose a session…</option>
              <For each={sessions()}>{(session) => <option value={session.id}>{session.repo} ({session.id})</option>}</For>
            </select>
          </div>
        </Show>
      </Show>

      <Show when={sessionId()}>{(id) => <EnvEditor sessionId={id()} />}</Show>
    </div>
  );
}
