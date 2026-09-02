/**
 * A collapsed section that opens in place.
 *
 * Built on `<details>`/`<summary>`: the platform already gets the keyboard,
 * the accessible name and in-page find for free, and printing a settings
 * page reveals the closed content rather than losing it. All this adds is
 * the chevron and the app's own type.
 */
import type { JSX } from "solid-js";
import { ChevronRight } from "lucide-solid";
import styles from "./Disclosure.module.css";

export interface DisclosureProps {
  /** The line the user clicks to open the section. */
  summary: string;
  /** Whether it starts open. Defaults to closed. */
  open?: boolean | undefined;
  children: JSX.Element;
}

export default function Disclosure(props: DisclosureProps) {
  return (
    <details class={styles.details} open={props.open === true}>
      <summary class={styles.summary}>
        <ChevronRight size={14} aria-hidden="true" class={styles.chevron ?? ""} />
        {props.summary}
      </summary>
      <div class={styles.body}>{props.children}</div>
    </details>
  );
}
