/**
 * A drawn slider whose stops are countable (docs/ux.md §7.7).
 *
 * A bare range input hides the one thing that matters in a choice like a
 * machine tier or an effort level: that the positions are *countable*.
 * Every stop is a dot on the rail, the thumb lands on one of them and
 * nowhere between, and the stop a caller wants flagged — a machine that
 * bills a minimum — can wear its own class via {@link DetentsProps.dotClass}.
 *
 * Underneath it is still a real `<input type="range">`, transparent and
 * laid over the rail: the pointer drag, the accessible role, the value
 * semantics and the focus ring are the platform's. Only the mapping from
 * key to detent is ours, in {@link detentForKey}, so that stepping is by
 * detent rather than by number.
 */
import { For } from "solid-js";
import { detentForKey } from "../lib/detents";
import { cx } from "../lib/cx";
import styles from "./Detents.module.css";

export interface DetentsProps {
  /** How many stops the rail carries, first to last. */
  count: number;
  /** The stop the thumb sits on. */
  position: number;
  /** The stop a pointer or key asked for, as an index. */
  onMove: (next: number) => void;
  /** The control's accessible name. */
  ariaLabel: string;
  /** What the current stop is called, for the value announcement. */
  ariaValueText?: string | undefined;
  /** An extra class for a stop's dot, by index — the caller's flag on a stop. */
  dotClass?: ((index: number) => string | undefined) | undefined;
}

export default function Detents(props: DetentsProps) {
  const last = () => Math.max(0, props.count - 1);
  /** How far along the rail the thumb rides, as a share of its width. */
  const travelled = () =>
    last() === 0 ? "0%" : `${(props.position / last()) * 100}%`;

  return (
    <div class={styles.detents}>
      <div class={styles.rail}>
        <For each={Array.from({ length: props.count })}>
          {(_, index) => (
            <span
              aria-hidden="true"
              class={cx(
                styles.dot,
                props.dotClass?.(index()),
                index() === props.position && styles.dotTaken,
              )}
              style={{ left: last() === 0 ? "0%" : `${(index() / last()) * 100}%` }}
            />
          )}
        </For>
        <div class={styles.thumbTravel} aria-hidden="true" style={{ "--travelled": travelled() }}>
          <span class={styles.thumb} />
        </div>
        <input
          class={styles.range}
          type="range"
          min={0}
          max={last()}
          step={1}
          value={props.position}
          aria-label={props.ariaLabel}
          aria-valuetext={props.ariaValueText}
          onInput={(event) => props.onMove(Number(event.currentTarget.value))}
          onKeyDown={(event) => {
            const next = detentForKey(event.key, props.position, last());
            if (next !== null) {
              event.preventDefault();
              props.onMove(next);
            }
          }}
        />
      </div>
    </div>
  );
}
