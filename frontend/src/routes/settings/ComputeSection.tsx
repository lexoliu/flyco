/**
 * Settings → Compute (docs/ux.md §10).
 *
 * One card per linked account, and the card is the same component every
 * wizard ends with (`components/ComputeCard.tsx`). One component rather than
 * two: the version a user reads the moment they finish linking and the
 * version they come back to a week later have to be the same card, or the
 * second one reads as a different account.
 *
 * Machines the user owns are listed in that same run of cards rather than
 * under a heading of their own — a host is a compute account like any other
 * (docs/host-enrollment.md), and a second section would say it is a
 * different kind of thing. They come from `GET /v1/hosts` rather than from
 * readiness because a host's state, facts and label live on the host row;
 * the provider account beside it carries only the link. That is also why
 * the machines' accounts are dropped from the cloud run here: they are
 * already on screen, as the machine itself.
 *
 * Which accounts those are is read off `host_id`, which names the machine an
 * account *is*. The kind would only say that some machine is behind it, and
 * a page holding both lists has to join them on an id — that is what lets a
 * card that needs its account (the compute chip's selector, which labels a
 * machine by the account's own label) find exactly one.
 *
 * Adding an account is a link to `/connect/compute` rather than a form here.
 * The wizards live on that route, and settings is not a second place to paste
 * a credential.
 */
import { For, Show, createMemo, createSignal } from "solid-js";
import { createQuery } from "../../lib/query";
import { A } from "@solidjs/router";
import { Plus } from "lucide-solid";
import ComputeCard from "../../components/ComputeCard";
import HostCard from "../../components/HostCard";
import ProblemNotice from "../../components/ProblemNotice";
import { useReadiness } from "../../components/Readiness";
import { listCloudUsage, listHosts, unlinkProvider } from "../../api/client";
import { setSpotPreference, spotPreference } from "../../lib/localPreferences";
import styles from "./Settings.module.css";

export default function ComputeSection() {
  const readiness = useReadiness();
  const [usage, { refetch: refetchUsage }] = createQuery(() => listCloudUsage());
  const [hosts, { refetch: refetchHosts }] = createQuery(() => listHosts());
  const [spot, setSpot] = createSignal(spotPreference());

  /** The machines on screen, by the account each one provisions through. */
  const machines = createMemo(() => new Set((hosts() ?? []).map((host) => host.id)));

  /**
   * The accounts a credential was pasted for, which is every account that is
   * not one of the machines already carded above.
   */
  const clouds = createMemo(() => {
    const carded = machines();
    return readiness.compute().filter((account) => {
      const host = account.host_id ?? null;
      return host === null || !carded.has(host);
    });
  });

  /**
   * Unlinks one account, and lets the failure through.
   *
   * The card asked before this ran and is the thing on screen holding the
   * question, so a refusal belongs in the dialog it opened rather than in a
   * notice at the top of the page (issue #139).
   */
  async function unlink(id: string): Promise<void> {
    await unlinkProvider(id);
    await readiness.refresh();
    void refetchUsage();
  }

  /** A machine was renamed or removed; both lists can have moved. */
  async function hostChanged(): Promise<void> {
    await Promise.all([refetchHosts(), readiness.refresh()]);
  }

  /** The default for the next session, which is what this page is for. */
  function chooseSpot(next: boolean): void {
    setSpot(next);
    setSpotPreference(next);
  }

  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Compute</h2>
        <p class={styles.lede}>
          Sessions run on machines in your own cloud accounts, or on hardware you enrolled, so you
          keep the bill, the region and the data. Spot capacity is the default for every new
          session.
        </p>
      </header>

      <ProblemNotice error={readiness.error() ?? hosts.error ?? usage.error} />

      <Show
        when={readiness.compute().length > 0}
        fallback={
          <div class={styles.empty}>
            <p class={styles.emptyLine}>No compute is linked, so no session can start yet.</p>
            <A href="/connect/compute" class={styles.pillPrimary}>
              <Plus size={14} aria-hidden="true" />
              Add compute
            </A>
          </div>
        }
      >
        <div class={styles.cards}>
          <For each={hosts() ?? []}>
            {(host) => <HostCard host={host} onChanged={() => void hostChanged()} />}
          </For>
          <For each={clouds()}>
            {(account) => (
              <ComputeCard
                account={account}
                usage={usage()?.find((row) => row.account === account.id)}
                spot={spot()}
                onSpot={chooseSpot}
                onUnlink={() => unlink(account.id)}
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
