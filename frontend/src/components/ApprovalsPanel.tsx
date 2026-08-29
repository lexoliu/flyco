import { For, Show } from "solid-js";
import type { ApprovalPayload } from "../api/wire";
import type { ApprovalState } from "../api/client";
import styles from "./ApprovalsPanel.module.css";

export interface ApprovalItem {
  id: string;
  payload: ApprovalPayload;
  state: ApprovalState;
}

export interface ApprovalsPanelProps {
  approvals: ApprovalItem[];
  /** Absent when the panel is read-only (no live session to decide against). */
  onDecide?: ((id: string, decision: "approved" | "denied") => void) | undefined;
}

/** Turns one of the four approval shapes into what a human needs to read to decide it. */
function summarize(payload: ApprovalPayload): string {
  switch (payload.kind) {
    case "merge":
      return `Merge ${payload.repo}: ${payload.from_branch} → ${payload.into_branch}`;
    case "history_rewrite":
      return `Rewrite history on ${payload.repo}/${payload.branch}: ${payload.description}`;
    case "agents_md_change":
      return `Change AGENTS.md: replace "${payload.find}" with "${payload.replace}"`;
    case "tool_use":
      return `Run tool "${payload.tool}"`;
  }
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
                <span>{summarize(approval.payload)}</span>
                <span class={styles.itemRight}>
                  <span class={styles.state} data-state={approval.state}>
                    {approval.state}
                  </span>
                  <Show when={approval.state === "pending" && props.onDecide}>
                    {(onDecide) => (
                      <span class={styles.decideButtons}>
                        <button type="button" class={styles.approve} onClick={() => onDecide()(approval.id, "approved")}>
                          Approve
                        </button>
                        <button type="button" class={styles.deny} onClick={() => onDecide()(approval.id, "denied")}>
                          Deny
                        </button>
                      </span>
                    )}
                  </Show>
                </span>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </section>
  );
}
