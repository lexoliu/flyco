/**
 * The one bar in the app: a label, a readout, and a track filled to a ratio.
 *
 * Budget, context window and usage all render this; the callers differ only
 * in what they put in the readout and which tier they colour the fill with,
 * so the markup and the `progressbar` semantics live here once.
 *
 * A bar whose ratio is unknown draws an empty track and says so in the
 * readout rather than guessing at a width — an invented fill is a lie the
 * user has no way to detect.
 */
import { Show } from "solid-js";
import styles from "./Meter.module.css";

/** How the fill is coloured. `ok` is ink; the rest are the status palette. */
export type MeterTier = "ok" | "notice" | "warn" | "final-warn" | "paused";

export interface RatioBarProps {
  /** What the bar measures. Doubles as its accessible name. */
  label: string;
  /** Filled fraction, 0–1. `undefined` means "not known", not "zero". */
  ratio?: number | undefined;
  /** The figure shown at the right of the label row. */
  value?: string | undefined;
  /** Shown in place of the figure when `ratio` is unknown. */
  unknownValue?: string | undefined;
  tier?: MeterTier | undefined;
}

export default function RatioBar(props: RatioBarProps) {
  const known = () => props.ratio !== undefined;
  const clamped = () => Math.min(Math.max(props.ratio ?? 0, 0), 1);

  return (
    <div class={styles.wrapper}>
      <div class={styles.labelRow}>
        <span>{props.label}</span>
        <Show
          when={known()}
          fallback={<span class={styles.muted}>{props.unknownValue ?? "Not loaded yet"}</span>}
        >
          <span>{props.value}</span>
        </Show>
      </div>
      <div
        class={styles.track}
        role="progressbar"
        aria-label={props.label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={known() ? Math.round(clamped() * 100) : 0}
      >
        <div
          class={styles.fill}
          data-tier={known() ? (props.tier ?? "ok") : "unknown"}
          style={{ width: `${known() ? clamped() * 100 : 0}%` }}
        />
      </div>
    </div>
  );
}
