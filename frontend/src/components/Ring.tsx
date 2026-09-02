/**
 * A small progress ring: the budget and the context window, in the session
 * header (docs/ux.md §9.1).
 *
 * A ring rather than a bar because the header is a row of chips and a bar
 * would be the only thing in it wanting horizontal room. Two of them sit
 * side by side, so they are one component with one geometry — a second
 * ring drawn by hand somewhere else would be a different size within a
 * week.
 *
 * The ring carries no colour of its own. It takes the ink of its
 * surroundings and only turns amber, then red, as the thing it measures
 * runs out, which is the whole of the colour policy in docs/ux.md §2:
 * colour means something is wrong, never that something exists.
 */
import { Show } from "solid-js";
import styles from "./Ring.module.css";

/** How full a ring has to be before it says so in colour. */
const WARN_RATIO = 0.8;
const DANGER_RATIO = 0.95;

export interface RingProps {
  /** What is being measured, e.g. `Budget`. Announced, and used in the tooltip. */
  label: string;
  /** How much is used. `undefined` while the number is not known yet. */
  value: number | undefined;
  /** The total. `undefined` while it is not known yet. */
  total: number | undefined;
  /** The reading beside the ring, already formatted (`$1.20 / $10`). */
  readout: string;
}

function tone(ratio: number): "ok" | "warn" | "danger" {
  if (ratio >= DANGER_RATIO) {
    return "danger";
  }
  return ratio >= WARN_RATIO ? "warn" : "ok";
}

const SIZE = 22;
const STROKE = 2.5;
const RADIUS = (SIZE - STROKE) / 2;
const CIRCUMFERENCE = 2 * Math.PI * RADIUS;

export default function Ring(props: RingProps) {
  const known = () => props.value !== undefined && props.total !== undefined && props.total > 0;
  const ratio = () => {
    if (props.value === undefined || props.total === undefined || props.total <= 0) {
      return 0;
    }
    return Math.min(props.value / props.total, 1);
  };

  return (
    <span
      class={styles.ring}
      title={`${props.label}: ${props.readout}`}
      role="img"
      aria-label={`${props.label}: ${props.readout}`}
    >
      <svg width={SIZE} height={SIZE} viewBox={`0 0 ${SIZE} ${SIZE}`} aria-hidden="true">
        <circle
          class={styles.track}
          cx={SIZE / 2}
          cy={SIZE / 2}
          r={RADIUS}
          fill="none"
          stroke-width={STROKE}
        />
        <Show when={known()}>
          <circle
            class={styles.value}
            data-tone={tone(ratio())}
            cx={SIZE / 2}
            cy={SIZE / 2}
            r={RADIUS}
            fill="none"
            stroke-width={STROKE}
            stroke-linecap="round"
            stroke-dasharray={`${CIRCUMFERENCE}`}
            stroke-dashoffset={`${CIRCUMFERENCE * (1 - ratio())}`}
            transform={`rotate(-90 ${SIZE / 2} ${SIZE / 2})`}
          />
        </Show>
      </svg>
      <span class={styles.readout}>{props.readout}</span>
    </span>
  );
}
