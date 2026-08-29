import { Show } from "solid-js";
import { NotImplementedError } from "../api/problem";
import styles from "./ProblemNotice.module.css";

export interface ProblemNoticeProps {
  /** Whatever a resource's `.error` accessor returned; `undefined`/`null` renders nothing. */
  error: unknown;
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
              {(() => {
                const value = error();
                return value instanceof Error ? value.message : String(value);
              })()}
            </p>
          }
        >
          <p class={styles.notImplemented} role="status">
            Not built yet — this part of flyco is still on its way.
          </p>
        </Show>
      )}
    </Show>
  );
}
