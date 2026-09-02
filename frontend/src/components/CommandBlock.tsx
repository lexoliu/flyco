/**
 * A block of text the user is expected to take somewhere else: a shell
 * command, a policy document, a public key.
 *
 * Monospaced, selectable, and copyable in one press — docs/ux.md §2 reserves
 * mono for exactly this. The copy button sits inside the block rather than
 * beside it, so the thing being copied and the control that copies it are
 * one object on screen.
 */
import { Show } from "solid-js";
import CopyButton from "./CopyButton";
import { cx } from "../lib/cx";
import styles from "./CommandBlock.module.css";

export interface CommandBlockProps {
  /** The exact text, newlines and all. */
  value: string;
  /** What is being copied, for the button's label. */
  label?: string | undefined;
  /** A caption above the block, when the text needs naming. */
  caption?: string | undefined;
  /**
   * Whether a long line wraps rather than scrolling out of sight.
   *
   * Off by default, because a script written in lines is easier to read as
   * the lines it was written in. On where the text is one long line the
   * reader has to see all of — an installer command piped into `sudo sh`
   * above all, which nobody should run half-read.
   */
  wrap?: boolean | undefined;
}

export default function CommandBlock(props: CommandBlockProps) {
  return (
    <div class={styles.block}>
      <Show when={props.caption}>{(caption) => <p class={styles.caption}>{caption()}</p>}</Show>
      <pre class={cx(styles.text, props.wrap === true && styles.wrap)}>{props.value}</pre>
      <div class={styles.actions}>
        <CopyButton value={props.value} label={props.label} class={styles.copy} />
      </div>
    </div>
  );
}
