import { Show } from "solid-js";
import { createQuery } from "../lib/query";
import ProblemNotice from "./ProblemNotice";
import { getRepoStatus } from "../api/client";
import styles from "./MachinePanel.module.css";

/**
 * Working-tree status of a session's checkout. Dirtiness is load-bearing on
 * the backend (an agent may not stop while the tree is dirty), so this is
 * shown plainly rather than folded into another panel.
 */
export default function RepoStatusPanel(props: {
  sessionId: string;
  /** Latest `repo_dirty` summary from the live relay, when one has arrived. */
  liveSummary?: string | null;
}) {
  const [status] = createQuery(() => props.sessionId, getRepoStatus);
  const view = () => {
    if (props.liveSummary !== undefined && props.liveSummary !== null) {
      return { dirty: props.liveSummary.trim() !== "", summary: props.liveSummary };
    }
    return status();
  };

  return (
    <section class={styles.panel} aria-label="Repository status">
      <h2>Repository</h2>
      <ProblemNotice error={status.error} />
      <Show when={view()}>
        {(tree) => (
          <>
            <span class={styles.state} data-state={tree().dirty ? "dirty" : "running"}>
              {tree().dirty ? "Dirty" : "Clean"}
            </span>
            <Show when={tree().summary !== ""}>
              <pre class={styles.facts}>{tree().summary}</pre>
            </Show>
          </>
        )}
      </Show>
    </section>
  );
}
