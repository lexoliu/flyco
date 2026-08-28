import { Show } from "solid-js";
import styles from "./Meter.module.css";

export interface UsageMeterProps {
  label: string;
  used?: number | undefined;
  total?: number | undefined;
  unit?: string | undefined;
}

/** Generic used/total meter, shared by LLM-usage and context-window displays. */
export default function UsageMeter(props: UsageMeterProps) {
  const hasData = () => props.used !== undefined && props.total !== undefined;
  const ratio = () => {
    const used = props.used;
    const total = props.total;
    if (used === undefined || total === undefined || total <= 0) return 0;
    return Math.min(used / total, 1);
  };

  return (
    <div class={styles.wrapper}>
      <div class={styles.labelRow}>
        <span>{props.label}</span>
        <Show when={hasData()} fallback={<span class={styles.muted}>Not loaded yet</span>}>
          <span>
            {props.used?.toLocaleString()} / {props.total?.toLocaleString()}
            {props.unit !== undefined ? ` ${props.unit}` : ""}
          </span>
        </Show>
      </div>
      <div
        class={styles.track}
        role="progressbar"
        aria-label={props.label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={hasData() ? Math.round(ratio() * 100) : 0}
      >
        <div
          class={styles.fill}
          data-tier={hasData() ? undefined : "unknown"}
          style={{ width: `${hasData() ? ratio() * 100 : 0}%` }}
        />
      </div>
    </div>
  );
}
