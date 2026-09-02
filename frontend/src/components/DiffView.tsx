/**
 * A unified diff, rendered so the change is the only thing carrying colour.
 *
 * Added and removed lines are tinted with the success and danger tokens —
 * the two semantic colours this system already spends on "done" and
 * "failed" — and every unchanged line stays ink on the ground. A `+`/`-`
 * gutter carries the same information for anyone who cannot see the tint.
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
            ) : (
              <p class={styles.line} data-kind={row.kind}>
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
