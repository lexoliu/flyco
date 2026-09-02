/**
 * One linked compute account, as docs/ux.md §7 describes it.
 *
 * The same card ends every wizard on `/connect/compute` and fills
 * Settings → Compute. One component rather than two, because the two would
 * drift: the wizard's version is what a user reads the moment they finish
 * linking, and the settings one is what they come back to, and those have to
 * be the same card or the second one reads as a different account.
 *
 * Everything on it is a fact read from the server. The machine line is
 * `GET /v1/machines/default?account=`, which runs the same choice
 * `POST /v1/sessions` runs, so the card cannot promise a machine a session
 * would not get. Spend is the provider's own meter, which is the only number
 * that matches the invoice — and an account flyco meters nothing on says so
 * rather than showing `$0.00`.
 */
import { Show, createResource } from "solid-js";
import { Server } from "lucide-solid";
import Logomark, { PROVIDER_MARK } from "./Logomark";
import Toggle from "./Toggle";
import { getDefaultMachine, type CloudUsageRow, type ProviderAccountView } from "../api/client";
import { formatDate } from "../lib/dates";
import { hourlyLabel } from "../lib/machines";
import { formatUsd } from "../lib/money";
import { PROVIDER_LABEL } from "../lib/providers";
import styles from "./ComputeCard.module.css";

export interface ComputeCardProps {
  /** The linked account this card is about. */
  account: ProviderAccountView;
  /** Its month-to-date spend, when the provider meters any. */
  usage: CloudUsageRow | undefined;
  /** Whether sessions on this account ask for interruptible capacity. */
  spot: boolean;
  onSpot: (spot: boolean) => void;
  /** Unlinks the account. Omitted where unlinking is not on offer. */
  onUnlink?: (() => void) | undefined;
}

export default function ComputeCard(props: ComputeCardProps) {
  // Keyed on both, because the machine flyco would pick and the price it
  // would pay both move with the capacity mode.
  const [machine] = createResource(
    () => ({ account: props.account.id, spot: props.spot }),
    ({ account, spot }) => getDefaultMachine(spot, account),
  );

  const mark = () => PROVIDER_MARK[props.account.kind];

  return (
    <article class={styles.card}>
      <header class={styles.top}>
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
          <span class={styles.title}>{props.account.label}</span>
          <span class={styles.meta}>
            {PROVIDER_LABEL[props.account.kind]} · linked {formatDate(props.account.linked_at_unix)}
          </span>
        </div>
        <Show when={props.onUnlink}>
          {(unlink) => (
            <button type="button" class={styles.unlink} onClick={() => unlink()()}>
              Unlink
            </button>
          )}
        </Show>
      </header>

      <dl class={styles.facts}>
        <div class={styles.fact}>
          <dt>Machine</dt>
          <dd>
            <Show
              when={machine()}
              fallback={
                <Show
                  when={machine.error === undefined || machine.error === null}
                  fallback={
                    <span class={styles.absent}>
                      No machine here is big enough for flyco to pick on its own.
                    </span>
                  }
                >
                  <span class={styles.skeleton} aria-label="Reading the catalog" />
                </Show>
              }
            >
              {(chosen) => (
                <>
                  {chosen().entry.machine_type}
                  <span class={styles.dim}>
                    {" · "}
                    {chosen().entry.region}
                    {" · "}
                    {hourlyLabel(chosen().entry, props.spot)}
                  </span>
                </>
              )}
            </Show>
          </dd>
        </div>

        <div class={styles.fact}>
          <dt>Spot capacity</dt>
          <dd class={styles.toggleCell}>
            <Toggle
              label={`Use spot capacity on ${props.account.label}`}
              checked={props.spot}
              onChange={props.onSpot}
            />
            <span class={styles.dim}>
              {props.spot ? "Cheaper; flyco handles eviction" : "Uninterruptible, and dearer"}
            </span>
          </dd>
        </div>

        <div class={styles.fact}>
          <dt>This billing period</dt>
          <dd>
            <Show
              when={props.usage}
              fallback={
                /* `GET /v1/usage/cloud` deliberately returns no row for
                   hardware the user already owns; a `$0.00` there would read
                   as "this is free", which it is not. */
                <span class={styles.absent}>Flyco meters no spend on this account.</span>
              }
            >
              {(row) => (
                <>
                  <strong>{formatUsd(row().spent)}</strong>
                  <span class={styles.dim}>
                    {" since "}
                    {formatDate(row().period_start_unix)}
                    <Show
                      when={row().remaining_credit !== null && row().remaining_credit !== undefined}
                    >
                      {" · "}
                      {formatUsd(row().remaining_credit ?? 0)} credit left
                    </Show>
                  </span>
                </>
              )}
            </Show>
          </dd>
        </div>
      </dl>
    </article>
  );
}
