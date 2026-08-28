import { Show } from "solid-js";
import styles from "./Meter.module.css";

export interface BudgetBarProps {
  label?: string | undefined;
  /** Already converted to whole dollars; the wire format is microdollars. */
  spentUsd?: number | undefined;
  limitUsd?: number | undefined;
}

/** Matches the budget engine's thresholds (flyco_core::budget): notice 50%, warn 80%, final warn 90%, pause 100%. */
const NOTICE_RATIO = 0.5;
const WARN_RATIO = 0.8;
const FINAL_WARN_RATIO = 0.9;

type Tier = "ok" | "notice" | "warn" | "final-warn" | "paused";

function tierFor(ratio: number): Tier {
  if (ratio >= 1) return "paused";
  if (ratio >= FINAL_WARN_RATIO) return "final-warn";
  if (ratio >= WARN_RATIO) return "warn";
  if (ratio >= NOTICE_RATIO) return "notice";
  return "ok";
}

export default function BudgetBar(props: BudgetBarProps) {
  const hasData = () => props.spentUsd !== undefined && props.limitUsd !== undefined;
  const ratio = () => {
    const spent = props.spentUsd;
    const limit = props.limitUsd;
    if (spent === undefined || limit === undefined || limit <= 0) return 0;
    return Math.min(spent / limit, 1);
  };

  return (
    <div class={styles.wrapper}>
      <div class={styles.labelRow}>
        <span>{props.label ?? "Budget"}</span>
        <Show
          when={hasData()}
          fallback={<span class={styles.muted}>Not loaded yet</span>}
        >
          <span>
            ${props.spentUsd?.toFixed(2)} / ${props.limitUsd?.toFixed(2)}
          </span>
        </Show>
      </div>
      <div
        class={styles.track}
        role="progressbar"
        aria-label={props.label ?? "Budget"}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={hasData() ? Math.round(ratio() * 100) : 0}
      >
        <div
          class={styles.fill}
          data-tier={hasData() ? tierFor(ratio()) : "unknown"}
          style={{ width: `${hasData() ? ratio() * 100 : 0}%` }}
        />
      </div>
    </div>
  );
}
