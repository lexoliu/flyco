/**
 * Settings → Compute (docs/ux.md §10).
 *
 * One card per linked account, and the card is the same component every
 * wizard ends with (`components/ComputeCard.tsx`). One component rather than
 * two: the version a user reads the moment they finish linking and the
 * version they come back to a week later have to be the same card, or the
 * second one reads as a different account.
 *
 * Adding an account is a link to `/connect/compute` rather than a form here.
 * The wizards live on that route, and settings is not a second place to paste
 * a credential.
 */
import { For, Show, createResource, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { Plus } from "lucide-solid";
import ComputeCard from "../../components/ComputeCard";
import ProblemNotice from "../../components/ProblemNotice";
import { useReadiness } from "../../components/Readiness";
import { listCloudUsage, unlinkProvider } from "../../api/client";
import { setSpotPreference, spotPreference } from "../../lib/localPreferences";
import styles from "./Settings.module.css";

export default function ComputeSection() {
  const readiness = useReadiness();
  const [usage, { refetch: refetchUsage }] = createResource(() => listCloudUsage());
  const [actionError, setActionError] = createSignal<unknown>(null);
  const [spot, setSpot] = createSignal(spotPreference());

  async function unlink(id: string): Promise<void> {
    setActionError(null);
    try {
      await unlinkProvider(id);
      await readiness.refresh();
      void refetchUsage();
    } catch (err) {
      setActionError(err);
    }
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
          Sessions run on machines in your own cloud accounts, so you keep the bill, the region and
          the data. Spot capacity is the default for every new session.
        </p>
      </header>

      <ProblemNotice error={readiness.error() ?? actionError()} />

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
          <For each={readiness.compute()}>
            {(account) => (
              <ComputeCard
                account={account}
                usage={usage()?.find((row) => row.account === account.id)}
                spot={spot()}
                onSpot={chooseSpot}
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
