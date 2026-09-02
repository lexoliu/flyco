/**
 * Settings → Compute (docs/ux.md §10).
 *
 * One card per linked account, each carrying what the user actually wants
 * to know about a cloud account they gave flyco a key to: whose it is, when
 * they linked it, and what it has cost this billing period. Spend is read
 * from the provider's own meter (`GET /v1/usage/cloud`), which is the only
 * number that matches the invoice.
 *
 * Adding an account is a link to `/connect/compute` rather than a form
 * here: the wizards live on that route (docs/ux.md §7) and settings is not
 * a second place to paste a credential.
 */
import { For, Show, createResource, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { Plus, Server } from "lucide-solid";
import Logomark, { PROVIDER_MARK } from "../../components/Logomark";
import ProblemNotice from "../../components/ProblemNotice";
import { useReadiness } from "../../components/Readiness";
import {
  listCloudUsage,
  unlinkProvider,
  type CloudUsageRow,
  type ProviderAccountView,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import { formatUsd } from "../../lib/money";
import { PROVIDER_LABEL } from "../../lib/providers";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

export default function ComputeSection() {
  const readiness = useReadiness();
  const [usage] = createResource(() => listCloudUsage());
  const [actionError, setActionError] = createSignal<unknown>(null);

  async function unlink(id: string): Promise<void> {
    setActionError(null);
    try {
      await unlinkProvider(id);
      await readiness.refresh();
    } catch (err) {
      setActionError(err);
    }
  }

  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Compute</h2>
        <p class={styles.lede}>
          Sessions run on machines in your own cloud accounts, so you keep the bill, the region and
          the data.
        </p>
      </header>

      <ProblemNotice error={readiness.error() ?? actionError()} />

      <Show
        when={readiness.compute().length > 0}
        fallback={
          <div class={styles.empty}>
            <p class={styles.emptyLine}>
              No compute is linked, so no session can start yet.
            </p>
            <A href="/connect/compute" class={styles.pillPrimary}>
              <Plus size={14} aria-hidden="true" />
              Add compute
            </A>
          </div>
        }
      >
        <div class={cx(styles.cards, styles.cardsPaired)}>
          <For each={readiness.compute()}>
            {(account) => (
              <ComputeCard
                account={account}
                usage={usage()?.find((row) => row.account === account.id)}
                onUnlink={() => void unlink(account.id)}
              />
            )}
          </For>
        </div>
        <div>
          <A href="/connect/compute" class={styles.pill}>
            <Plus size={14} aria-hidden="true" />
            Add compute
          </A>
        </div>
      </Show>
    </section>
  );
}

function ComputeCard(props: {
  account: ProviderAccountView;
  usage: CloudUsageRow | undefined;
  onUnlink: () => void;
}) {
  const mark = () => PROVIDER_MARK[props.account.kind];

  return (
    <article class={styles.card}>
      <div class={styles.cardTop}>
        <span class={styles.mark}>
          <Show
            when={mark()}
            /* A machine the user owns has no vendor behind it, so it gets
               the generic server glyph rather than borrowing a logo. */
            fallback={<Server size={16} aria-hidden="true" />}
          >
            {(vendor) => <Logomark mark={vendor()} size={16} />}
          </Show>
        </span>
        <div class={styles.identity}>
          <span class={styles.cardTitle}>{props.account.label}</span>
          <span class={styles.cardMeta}>
            {PROVIDER_LABEL[props.account.kind]} · linked {formatDate(props.account.linked_at_unix)}
          </span>
        </div>
        <div class={styles.actions}>
          <button type="button" class={styles.pillDanger} onClick={props.onUnlink}>
            Unlink
          </button>
        </div>
      </div>

      <Show
        when={props.usage}
        fallback={
          /* `GET /v1/usage/cloud` deliberately returns no row for hardware
             the user already owns; a `$0.00` there would read as "this is
             free", which it is not. */
          <p class={styles.cardMeta}>Flyco meters no spend on this account.</p>
        }
      >
        {(row) => (
          <p class={styles.cardMeta}>
            <strong>{formatUsd(row().spent)}</strong> since {formatDate(row().period_start_unix)}
            <Show when={row().remaining_credit !== null && row().remaining_credit !== undefined}>
              {" · "}
              {formatUsd(row().remaining_credit ?? 0)} credit left
            </Show>
          </p>
        )}
      </Show>
    </article>
  );
}
