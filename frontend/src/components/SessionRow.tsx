/**
 * One session, as a row in the home list.
 *
 * Status dot and label, title, `repo · relative time`, and the harness mark
 * on the right (docs/ux.md §5). A row is a link, so the whole thing is the
 * target rather than a word inside it.
 */
import { Show } from "solid-js";
import { A } from "@solidjs/router";
import Logomark, { HARNESS_MARK } from "./Logomark";
import type { SessionSummary } from "../api/client";
import { cx } from "../lib/cx";
import { relativeTime } from "../lib/relativeTime";
import { deriveStatus } from "../lib/status";
import styles from "./SessionRow.module.css";

export interface SessionRowProps {
  session: SessionSummary;
  /** The instant the whole list is rendered against. */
  now: number;
}

export default function SessionRow(props: SessionRowProps) {
  const status = () => deriveStatus(props.session, props.now);

  return (
    <A href={`/sessions/${props.session.id}`} class={styles.row}>
      <span
        class={cx(styles.dot, status().breathing && styles.breathing)}
        data-tone={status().tone}
        aria-hidden="true"
      />
      <span class={styles.status}>
        {status().label}
        <Show when={status().detail}>
          {(detail) => <span class={styles.statusDetail}> · {detail()}</span>}
        </Show>
      </span>
      <span class={styles.title}>{props.session.title}</span>
      <span class={styles.meta}>
        {props.session.repo} · {relativeTime(props.session.last_active_unix, props.now)}
      </span>
      <Logomark mark={HARNESS_MARK[props.session.harness]} size={13} />
    </A>
  );
}
