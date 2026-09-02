/**
 * A unified diff, rendered so the change is the only thing carrying colour.
 *
 * Added and removed lines are tinted with the success and danger tokens —
 * the two semantic colours this system already spends on "done" and
 * "failed" — and every unchanged line stays ink on the ground. A `+`/`-`
 * gutter carries the same information for anyone who cannot see the tint.
 *
 * One component for both diffs in the product: the approval diff, which has
 * no line numbers because it compares two documents, and the session diff,
 * which has git's own. The gutter renders whatever the rows carry.
 */
import { For, Show } from "solid-js";
import type { DiffRow } from "../lib/diffRows";
import styles from "./DiffView.module.css";

export interface DiffViewProps {
  rows: readonly DiffRow[];
  /** What the diff is of, for assistive technology. */
  label: string;
}

const SIGN: Record<"context" | "added" | "removed", string> = {
  context: " ",
  added: "+",
  removed: "-",
};

/** A line number, or the blank the other side of a change leaves. */
function gutter(value: number | undefined): string {
  return value === undefined ? "" : String(value);
}

export default function DiffView(props: DiffViewProps) {
  return (
    <Show
      when={props.rows.length > 0}
      fallback={<p class={styles.identical}>This change would leave the document as it is.</p>}
    >
      <div class={styles.diff} role="group" aria-label={props.label}>
        <For each={props.rows}>
          {(row) =>
            row.kind === "gap" ? (
              <p class={styles.gap}>
                {row.hidden} unchanged {row.hidden === 1 ? "line" : "lines"}
              </p>
            ) : row.kind === "hunk" ? (
              <p class={styles.hunk}>{row.text}</p>
            ) : (
              <p class={styles.line} data-kind={row.kind}>
                <Show when={row.oldNumber !== undefined || row.newNumber !== undefined}>
                  <span class={styles.numbers} aria-hidden="true">
                    <span class={styles.number}>{gutter(row.oldNumber)}</span>
                    <span class={styles.number}>{gutter(row.newNumber)}</span>
                  </span>
                </Show>
                <span class={styles.sign} aria-hidden="true">
                  {SIGN[row.kind]}
                </span>
                <span class={styles.text}>{row.text}</span>
              </p>
            )
          }
        </For>
      </div>
    </Show>
  );
}
