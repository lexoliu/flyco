/**
 * Stage C's last page: the compute card of docs/ux.md §7, for whatever
 * just linked (§4 C8).
 *
 * The same card Settings › Compute shows, so what the user reads the
 * moment they finish linking and what they come back to a week later are
 * the same account. A machine the user owns gets the host card, with the
 * facts it reported instead of a price.
 */
import { Show, createSignal } from "solid-js";
import ComputeCard from "../../ComputeCard";
import HostCard from "../../HostCard";
import ProblemNotice from "../../ProblemNotice";
import { listCloudUsage, type HostView, type ProviderAccountView } from "../../../api/client";
import { setSpotPreference, spotPreference } from "../../../lib/localPreferences";
import { PROVIDER_LABEL } from "../../../lib/providers";
import { createQuery } from "../../../lib/query";
import type { PageComponent, Primary } from "../page";

/** What a flow that reaches this page has linked: one or the other. */
type Linked = { kind: "cloud"; account: ProviderAccountView } | { kind: "host"; host: HostView };

function linkedOf(account: ProviderAccountView | null, host: HostView | null): Linked {
  if (host !== null) {
    return { kind: "host", host };
  }
  if (account !== null) {
    return { kind: "cloud", account };
  }
  throw new Error("the linked page was reached without a compute account or a machine");
}

export const ComputeLinked: PageComponent<{ id: "compute-linked" }> = (props) => {
  const answers = props.state().answers;
  const linked = linkedOf(answers.computeAccount, answers.host);
  const [usage] = createQuery(() => listCloudUsage());
  const [spot, setSpot] = createSignal(spotPreference());

  function chooseSpot(next: boolean): void {
    setSpot(next);
    setSpotPreference(next);
  }

  const primary = (): Primary => ({
    label: "Start building",
    disabled: null,
    onClick: () => props.advance(),
  });

  return {
    title:
      linked.kind === "host"
        ? `${linked.host.label} is linked`
        : `${PROVIDER_LABEL[linked.account.kind]} is linked`,
    body: (
      <>
        {/* Spend is secondary to the account being linked, so its failure is
            a line above the card rather than anything that hides it. */}
        <ProblemNotice error={usage.error} />
        <Show
          when={linked.kind === "cloud" ? linked.account : null}
          fallback={
            /* Renaming and removing live in Settings › Compute; this
               machine has been on screen for two seconds. */
            <Show when={linked.kind === "host" ? linked.host : null}>
              {(host) => <HostCard host={host()} onChanged={() => undefined} editable={false} />}
            </Show>
          }
        >
          {(account) => (
            <ComputeCard
              account={account()}
              usage={usage()?.find((row) => row.account === account().id)}
              spot={spot()}
              onSpot={chooseSpot}
            />
          )}
        </Show>
      </>
    ),
    primary,
  };
};
