import { For, Show } from "solid-js";
import styles from "./ApprovalsPanel.module.css";

export interface ApprovalItem {
  id: string;
  summary: string;
  state: "pending" | "approved" | "denied";
}

export interface ApprovalsPanelProps {
  approvals: ApprovalItem[];
}

/** GitHub merges, history rewrites, and other actions the proposal requires a human decision on — never a model-prompted approval. */
export default function ApprovalsPanel(props: ApprovalsPanelProps) {
  return (
    <section class={styles.panel} aria-label="Approvals">
      <h2>Approvals</h2>
      <Show when={props.approvals.length > 0} fallback={<p class={styles.empty}>No approvals waiting.</p>}>
        <ul class={styles.list}>
          <For each={props.approvals}>
            {(approval) => (
              <li class={styles.item}>
                <span>{approval.summary}</span>
                <span class={styles.state} data-state={approval.state}>
                  {approval.state}
                </span>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </section>
  );
}
