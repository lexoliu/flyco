import { For, Show, createResource, createSignal } from "solid-js";
import SessionCard, { type SessionCardProps } from "../components/SessionCard";
import { requestNewSession } from "../api/sessions";
import { cx } from "../lib/cx";
import styles from "./Sessions.module.css";

type SessionSummary = Omit<SessionCardProps, "budgetSpentUsd" | "budgetLimitUsd"> & {
  budgetSpentUsd: number;
  budgetLimitUsd: number;
};

async function fetchSessions(): Promise<SessionSummary[]> {
  // TODO: replace with the generated client's GET /v1/sessions once it
  // lands (see src/api/types.ts).
  return [];
}

export default function Sessions() {
  const [showArchived, setShowArchived] = createSignal(false);
  const [sessions] = createResource(fetchSessions);
  const [error, setError] = createSignal<string | null>(null);

  const visible = () => (sessions() ?? []).filter((session) => session.archived === showArchived());

  async function onNewSession(): Promise<void> {
    setError(null);
    try {
      await requestNewSession();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <section class={styles.page}>
      <header class={styles.header}>
        <h1>Sessions</h1>
        <button
          type="button"
          class={styles.newButton}
          onClick={() => {
            void onNewSession();
          }}
        >
          New session
        </button>
      </header>

      <div class={styles.filterRow} role="group" aria-label="Filter sessions">
        <button
          type="button"
          class={cx(!showArchived() && styles.filterActive)}
          onClick={() => setShowArchived(false)}
        >
          Active
        </button>
        <button
          type="button"
          class={cx(showArchived() && styles.filterActive)}
          onClick={() => setShowArchived(true)}
        >
          Archived
        </button>
      </div>

      <Show when={error()}>
        <p class={styles.error} role="alert">
          {error()}
        </p>
      </Show>

      <Show
        when={!sessions.loading}
        fallback={<p class={styles.empty}>Loading sessions…</p>}
      >
        <Show
          when={visible().length > 0}
          fallback={
            <p class={styles.empty}>
              {showArchived()
                ? "No archived sessions."
                : "No sessions yet. Start one to put an agent to work in a repo."}
            </p>
          }
        >
          <ul class={styles.grid}>
            <For each={visible()}>
              {(session) => (
                <li>
                  <SessionCard {...session} />
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </section>
  );
}
