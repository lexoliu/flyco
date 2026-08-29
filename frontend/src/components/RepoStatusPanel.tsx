import { Show, createResource } from "solid-js";
import ProblemNotice from "./ProblemNotice";
import { getRepoStatus } from "../api/client";
import styles from "./MachinePanel.module.css";

/**
 * Working-tree status of a session's checkout. Dirtiness is load-bearing on
 * the backend (an agent may not stop while the tree is dirty), so this is
 * shown plainly rather than folded into another panel.
 */
export default function RepoStatusPanel(props: { sessionId: string }) {
  const [status] = createResource(() => props.sessionId, getRepoStatus);

  return (
    <section class={styles.panel} aria-label="Repository status">
      <h2>Repository</h2>
      <ProblemNotice error={status.error} />
      <Show when={!status.loading}>
        <Show when={status()}>
          {(view) => (
            <>
              <span class={styles.state} data-state={view().dirty ? "dirty" : "running"}>
                {view().dirty ? "Dirty" : "Clean"}
              </span>
              <Show when={view().summary !== ""}>
                <pre class={styles.facts}>{view().summary}</pre>
              </Show>
            </>
          )}
        </Show>
      </Show>
    </section>
  );
}
