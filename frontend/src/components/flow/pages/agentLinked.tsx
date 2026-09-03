/**
 * Stage B's last page: the account, as the settings card will show it
 * (docs/ux.md §4 B4).
 */
import { Show } from "solid-js";
import Logomark, { HARNESS_MARK } from "../../Logomark";
import { formatDate } from "../../../lib/dates";
import { HARNESS_LABEL } from "../../../lib/harnesses";
import { NEXT, type PageComponent } from "../page";
import styles from "./pages.module.css";

export const AgentLinked: PageComponent<{ id: "agent-linked" }> = (props) => {
  const account = props.state().answers.agentAccount;
  if (account === null) {
    throw new Error("the linked page was reached without an agent account");
  }
  const harness = HARNESS_LABEL[account.harness];

  return {
    title: `${harness} is linked`,
    body: (
      <>
        <div class={styles.linkedCard}>
          <span class={styles.linkedMark}>
            <Logomark mark={HARNESS_MARK[account.harness]} size={17} />
          </span>
          <span class={styles.linkedText}>
            <span class={styles.linkedTitle}>{account.label}</span>
            <span class={styles.linkedMeta}>
              {harness} · linked {formatDate(account.linked_at_unix)}
              <Show when={account.expires_at_unix}>
                {(expires) => <> · expires {formatDate(expires())}</>}
              </Show>
            </span>
          </span>
          <span class={styles.status}>Linked</span>
        </div>
        {/* A grant with an end is renewed before a session is given it, so
            the date above is a fact rather than a deadline to act on. */}
        <Show when={account.expires_at_unix}>
          <p class={styles.hint}>Renewed automatically before a session uses it.</p>
        </Show>
      </>
    ),
    primary: () => NEXT(() => props.advance()),
  };
};
