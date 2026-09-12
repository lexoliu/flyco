/**
 * The composer's container, shared by the two places one appears.
 *
 * docs/ux.md §9.3 says the session page uses "the same component as the
 * home composer, without chips". This is the part that is literally the
 * same: one large, low-contrast, 16px-rounded box with a textarea and a row
 * of controls inside it, and the send button at the right of that row.
 * What differs — the home page's four chips and its `Start session`
 * button, the session's `Stop` and its command palette — is passed in.
 *
 * It exists so that "the same component" is true rather than aspirational:
 * a second composer written beside the first would have drifted by the next
 * change to either.
 */
import { type JSX, Show, onCleanup, onMount } from "solid-js";
import { cx } from "../lib/cx";
import styles from "./Composer.module.css";

export interface ComposerShellProps {
  /** The prompt text. Controlled by the caller. */
  value: string;
  onInput: (value: string) => void;
  /** Sends. Called by the submit key and by whatever `action` renders. */
  onSubmit: () => void;
  placeholder: string;
  /** What the field is, for assistive technology. */
  label: string;
  /**
   * Which key sends.
   *
   * `mod-enter` on the home page, where a prompt is prose and prose has
   * paragraphs; `enter` in a session, where a message is a message.
   */
  submitOn: "enter" | "mod-enter";
  disabled?: boolean | undefined;
  /** Extra key handling, e.g. a command palette. Returning `true` consumes the event. */
  onKeyDown?: ((event: KeyboardEvent) => boolean) | undefined;
  /** The chips at the left of the row under the field. */
  controls?: JSX.Element | undefined;
  /** What sits between the chips and the action: the model chip, a ring. */
  trailing?: JSX.Element | undefined;
  /** The right-hand action: a send button, or a stop button. */
  action: JSX.Element;
  /** A panel floating over the field, e.g. the command palette. */
  overlay?: JSX.Element | undefined;
  /** Anything under the container: a hint, an error. */
  children?: JSX.Element | undefined;
  /** Hands the textarea back, so a caller can focus it. */
  ref?: ((element: HTMLTextAreaElement) => void) | undefined;
}

export default function ComposerShell(props: ComposerShellProps) {
  function onKeyDown(event: KeyboardEvent): void {
    if (props.onKeyDown?.(event) === true) {
      return;
    }
    if (event.key !== "Enter" || event.shiftKey) {
      return;
    }
    const modified = event.metaKey || event.ctrlKey;
    if (props.submitOn === "mod-enter" ? modified : !modified) {
      event.preventDefault();
      props.onSubmit();
    }
  }

  /*
   * Codex's answer to "the row ran out of room": the chips leave the box
   * and float above it as an island, where they can stack layer on layer,
   * instead of squeezing the send row into a second line inside it.
   *
   * The move is a move, not a re-render: `chips` is one live DOM subtree
   * docked into whichever home is in force, so crossing the threshold —
   * the viewport (~900px) or the composer itself (~600px), which a drawer
   * shrinks without touching the viewport — carries an open popover, a
   * half-typed goal condition, and the focus with it. `display: contents`
   * keeps the carrier invisible to layout in both homes. It starts docked
   * in the row because `moveBefore`, the state-preserving move it is
   * carried by, only accepts a node that is already connected.
   */
  let stack: HTMLDivElement | undefined;
  let islandHome: HTMLDivElement | undefined;
  let controlsRow: HTMLDivElement | undefined;
  const chips = (
    <div class={styles.islandCarry}>{props.controls}</div>
  ) as HTMLDivElement;

  onMount(() => {
    const narrow = window.matchMedia("(max-width: 900px)");
    const dock = () => {
      const up = (stack?.clientWidth ?? 0) <= 600 || narrow.matches;
      const home = up ? islandHome : controlsRow;
      if (home === undefined) {
        return;
      }
      /* A no-op move would still remove and re-insert, dropping focus. */
      if (chips.parentElement !== home) {
        /*
         * `moveBefore` is the state-preserving move: no remove+insert,
         * so a focused or open chip inside keeps its focus — a plain
         * `prepend` detaches it for a moment and the popover's focus-out
         * dismissal reads that as "focus left".
         */
        if ("moveBefore" in home) {
          (home as Element & { moveBefore: (node: Node, child: Node | null) => void }).moveBefore(
            chips,
            home.firstChild,
          );
        } else {
          home.prepend(chips);
        }
      }
      if (up) {
        islandHome?.style.removeProperty("display");
      } else {
        islandHome?.style.setProperty("display", "none");
      }
    };
    const observer = new ResizeObserver(dock);
    if (stack !== undefined) {
      observer.observe(stack);
    }
    dock();
    narrow.addEventListener("change", dock);
    onCleanup(() => {
      observer.disconnect();
      narrow.removeEventListener("change", dock);
    });
  });

  return (
    <div ref={stack} class={styles.stack}>
      <div ref={islandHome} class={styles.island} style={{ display: "none" }} />
      <div class={cx(styles.composer, styles.composerRelative)}>
        {props.overlay}
        <textarea
          class={styles.prompt}
          placeholder={props.placeholder}
          aria-label={props.label}
          value={props.value}
          rows="3"
          disabled={props.disabled ?? false}
          ref={props.ref}
          onInput={(event) => props.onInput(event.currentTarget.value)}
          onKeyDown={onKeyDown}
        />
        <div ref={controlsRow} class={styles.controls}>
          {chips}
          {props.trailing}
          {props.action}
        </div>
      </div>
      <Show when={props.children}>{props.children}</Show>
    </div>
  );
}
