/**
 * The question asked before something cannot be undone.
 *
 * Archiving a session with uncommitted work, unlinking the account every
 * session's agent runs on, unlinking the cloud account the machines are in,
 * removing a machine somebody is still working on: four irreversible moves
 * that were four different pieces of markup, three of which asked nothing at
 * all before firing their `DELETE` (issue #139). They are one shape — what
 * will happen, what it costs, and two ways out — so they are one component.
 *
 * Presentational on purpose: the caller owns whether the dialog is open,
 * what the request is, and what a refusal means. What is fixed here is the
 * shape of the question, so that "the button that changes nothing" is in the
 * same place every time.
 */
import { type JSX, Show } from "solid-js";
import styles from "./ConfirmDialog.module.css";

export interface ConfirmDialogProps {
  /** What is about to happen, in a few words. */
  title: string;
  /** What it means, in the interface's voice. */
  body: JSX.Element;
  /** The word on the button that does it. */
  confirmLabel: string;
  /** The word on the button that does not. Defaults to `Cancel`. */
  cancelLabel?: string | undefined;
  /** Whether the request is in flight, which disables the confirmation. */
  busy?: boolean | undefined;
  /**
   * Whether confirming destroys something.
   *
   * `danger` is for a move that ends work — an archive that drops
   * uncommitted changes, a removal that stops running sessions. An unlink
   * the user can undo by linking again is not one.
   */
  /**
   * What confirming does to the reader's world.
   *
   * `danger` and `quiet` are a question whose safe answer is to cancel —
   * the cancel is the filled button and the confirm is outlined in the
   * danger colour. `affirm` is a question whose expected answer is yes —
   * starting a machine to deliver a message — so the confirm is the
   * filled primary and the cancel the quiet one.
   */
  tone?: "danger" | "quiet" | "affirm" | undefined;
  /** Does the thing. Absent while there is nothing to confirm — see below. */
  onConfirm?: (() => void) | undefined;
  onCancel: () => void;
  /** Anything between the body and the buttons: a summary, a refusal. */
  children?: JSX.Element | undefined;
}

/**
 * `onConfirm` is optional because a refusal can take the confirmation away
 * while leaving the dialog up: a cloud account with machines still on it has
 * nothing to press until those sessions are archived, and a dialog that kept
 * offering the button would be offering the request the server just refused.
 */
export default function ConfirmDialog(props: ConfirmDialogProps) {
  const titleId = `confirm-${props.title.replace(/\W+/g, "-").toLowerCase()}`;

  return (
    <div
      class={styles.dialog}
      data-tone={props.tone ?? "quiet"}
      role="alertdialog"
      aria-labelledby={titleId}
    >
      <h2 id={titleId} class={styles.title}>
        {props.title}
      </h2>
      <p class={styles.body}>{props.body}</p>
      {props.children}
      <div class={styles.actions}>
        <button type="button" class={styles.cancel} onClick={() => props.onCancel()}>
          {props.cancelLabel ?? "Cancel"}
        </button>
        <Show when={props.onConfirm}>
          {(confirm) => (
            <button
              type="button"
              class={styles.confirm}
              disabled={props.busy === true}
              onClick={() => confirm()()}
            >
              {props.confirmLabel}
            </button>
          )}
        </Show>
      </div>
    </div>
  );
}
