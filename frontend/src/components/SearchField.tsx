/**
 * The one search box every list in a popover or header opens with.
 *
 * A `type="search"` input on its own comes with the browser's idea of a
 * search box — Safari's is a thin pill in a smaller type than anything
 * around it — so the native look is reset here and every list gets the
 * same field at the same size, rather than each popover re-styling the
 * input by hand and drifting.
 */
import type { JSX } from "solid-js";
import { splitProps } from "solid-js";
import { cx } from "../lib/cx";
import styles from "./SearchField.module.css";

export interface SearchFieldProps extends Omit<JSX.InputHTMLAttributes<HTMLInputElement>, "type" | "class"> {
  /** Extra class for the field, when a caller has to size it. */
  class?: string | undefined;
}

export default function SearchField(props: SearchFieldProps) {
  const [own, rest] = splitProps(props, ["class"]);
  return <input type="search" class={cx(styles.field, own.class)} {...rest} />;
}
