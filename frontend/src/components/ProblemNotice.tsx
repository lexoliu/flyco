import { Show } from "solid-js";
import { NotImplementedError } from "../api/problem";
import styles from "./ProblemNotice.module.css";

export interface ProblemNoticeProps {
  /** Whatever a resource's `.error` accessor returned; `undefined`/`null` renders nothing. */
  error: unknown;
  /**
   * One thing the reader can do about it, e.g. `Retry`.
   *
   * On the notice rather than beside it, so the failure and the way out of
   * it are one line and never drift apart on a page that has several.
   */
  action?: { label: string; onClick: () => void } | undefined;
}

/** The action, when there is one, as a quiet pill at the end of the line. */
function Action(props: Pick<ProblemNoticeProps, "action">) {
  return (
    <Show when={props.action}>
      {(action) => (
        <button type="button" class={styles.action} onClick={() => action().onClick()}>
          {action().label}
        </button>
      )}
    </Show>
  );
}

/**
 * Renders the one thing every fetching page needs beyond loading/empty: what
 * to show when the request failed.
 *
 * A [`NotImplementedError`] gets its own calm, first-class treatment — most
 * of this API is deliberately unbuilt right now, and that is not the same
 * fact as something being broken. Everything else (a real `ApiProblem`, an
 * `UnexpectedResponseError`, a `NetworkError`) renders as a plain error, with
 * whatever message it carries.
 */
export default function ProblemNotice(props: ProblemNoticeProps) {
  return (
    <Show when={props.error}>
      {(error) => (
        <Show
          when={error() instanceof NotImplementedError}
          fallback={
            <p class={styles.error} role="alert">
              <span>
                {(() => {
                  const value = error();
                  return value instanceof Error ? value.message : String(value);
                })()}
              </span>
              <Action action={props.action} />
            </p>
          }
        >
          <p class={styles.notImplemented} role="status">
            <span>Not built yet — this part of flyco is still on its way.</span>
            <Action action={props.action} />
          </p>
        </Show>
      )}
    </Show>
  );
}
