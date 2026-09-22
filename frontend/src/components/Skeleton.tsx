/**
 * The shape of content that has not arrived yet.
 *
 * A screen that renders its empty state while the first read is still in
 * flight says "there is nothing here" and then contradicts itself a
 * moment later — the flash the rail did on every load. A block the size
 * of the row that is coming says the true thing instead: something is on
 * its way, and it will land here.
 *
 * Static, not shimmering: the rail sits in the corner of the eye and a
 * pulse out there competes with the work in the middle of the screen. It
 * carries `aria-hidden`, because the region it stands in announces the
 * wait itself.
 */
import { For } from "solid-js";
import { cx } from "../lib/cx";
import styles from "./Skeleton.module.css";

export interface SkeletonProps {
  /** How many lines to draw. */
  lines?: number | undefined;
  /** Extra class on the wrapper, for a caller that needs its own spacing. */
  class?: string | undefined;
}

export default function Skeleton(props: SkeletonProps) {
  const lines = () => props.lines ?? 3;
  return (
    <div class={cx(styles.skeleton, props.class)} aria-hidden="true">
      <For each={Array.from({ length: lines() })}>
        {(_, index) => (
          <span
            class={styles.line}
            /* Three widths in rotation, so a column of them reads as text
               rather than as a bar chart. */
            style={{ width: `${[86, 62, 74][index() % 3] ?? 72}%` }}
          />
        )}
      </For>
    </div>
  );
}
