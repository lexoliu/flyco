import { A } from "@solidjs/router";
import type { HarnessKind, SessionState } from "../api/client";
import styles from "./SessionCard.module.css";

const HARNESS_LABEL: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
};

const STATE_LABEL: Record<SessionState, string> = {
  provisioning: "Provisioning",
  active: "Active",
  paused: "Paused",
  interrupted: "Interrupted",
  archived: "Archived",
  failed: "Failed",
};

export interface SessionCardProps {
  id: string;
  repo: string;
  harness: HarnessKind;
  state: SessionState;
}

export default function SessionCard(props: SessionCardProps) {
  return (
    <A href={`/sessions/${props.id}`} class={styles.card}>
      <div class={styles.top}>
        <span class={styles.repo}>{props.repo}</span>
        <span class={styles.harness}>{HARNESS_LABEL[props.harness]}</span>
      </div>
      <span class={styles.stateBadge} data-state={props.state}>
        {STATE_LABEL[props.state]}
      </span>
    </A>
  );
}
