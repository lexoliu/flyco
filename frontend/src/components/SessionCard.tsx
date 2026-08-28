import { Show } from "solid-js";
import { A } from "@solidjs/router";
import BudgetBar from "./BudgetBar";
import styles from "./SessionCard.module.css";

/** Mirrors the wire enum `HarnessKind` in openapi.json. */
export type HarnessKind = "claude_code" | "codex";

const HARNESS_LABEL: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
};

export interface SessionCardProps {
  id: string;
  repo: string;
  harness: HarnessKind;
  archived: boolean;
  budgetSpentUsd?: number | undefined;
  budgetLimitUsd?: number | undefined;
}

export default function SessionCard(props: SessionCardProps) {
  return (
    <A href={`/sessions/${props.id}`} class={styles.card}>
      <div class={styles.top}>
        <span class={styles.repo}>{props.repo}</span>
        <span class={styles.harness}>{HARNESS_LABEL[props.harness]}</span>
      </div>
      <Show when={props.archived}>
        <span class={styles.archivedBadge}>Archived</span>
      </Show>
      <div class={styles.budget}>
        <BudgetBar spentUsd={props.budgetSpentUsd} limitUsd={props.budgetLimitUsd} />
      </div>
    </A>
  );
}
