/**
 * A form that takes the page over while it is open.
 *
 * The alternative flyco used was expanding a card in place: the row the
 * user clicked grew a form, the rows below it jumped, and the page behind
 * it stayed live. That is in-place editing used as a dialog — it works for
 * a single field the reader is already looking at, and it is the wrong
 * shape for a form with a transport, a command line and a set of headers.
 *
 * Native `<dialog>`, opened with `showModal()`: the focus trap, the
 * Escape key, the inert page behind and the top layer are the platform's,
 * and reimplementing any of them would be a worse version of what the
 * browser already does. The backdrop is styled here; the dismissal on a
 * click outside is ours, because `<dialog>` has no notion of it.
 */
import { type JSX, onCleanup, onMount } from "solid-js";
import { X } from "lucide-solid";
import styles from "./Modal.module.css";

export interface ModalProps {
  /** What the form is for, as its heading. */
  title: string;
  /** Closes it: Escape, the backdrop, and the corner all end here. */
  onClose: () => void;
  /** The form itself. */
  children: JSX.Element;
}

export default function Modal(props: ModalProps) {
  let dialog!: HTMLDialogElement;

  onMount(() => {
    dialog.showModal();
  });
  onCleanup(() => {
    if (dialog.open) {
      dialog.close();
    }
  });

  return (
    <dialog
      ref={dialog}
      class={styles.modal}
      aria-label={props.title}
      /* Escape fires `cancel` before `close`; both end at the caller, so
         the open state lives in one place rather than two that can
         disagree about whether the dialog is up. */
      onCancel={(event) => {
        event.preventDefault();
        props.onClose();
      }}
      onClick={(event) => {
        // A click that lands on the dialog element itself is a click on
        // the backdrop: everything inside is a child.
        if (event.target === dialog) {
          props.onClose();
        }
      }}
    >
      <div class={styles.head}>
        <h2 class={styles.title}>{props.title}</h2>
        <button type="button" class={styles.close} aria-label="Close" onClick={props.onClose}>
          <X size={16} aria-hidden="true" />
        </button>
      </div>
      <div class={styles.body}>{props.children}</div>
    </dialog>
  );
}
