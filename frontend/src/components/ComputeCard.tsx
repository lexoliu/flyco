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
 *
 * A machine the user owns is the same card with different facts, so the
 * frame lives here as [`CardShell`] and `components/HostCard.tsx` fills it:
 * a host has no price, no spot mode and no invoice, and giving it a second
 * frame of its own would make one machine look like a different kind of
 * thing from the account beside it.
 */
import { Show, createResource, type JSX } from "solid-js";
import { Server } from "lucide-solid";
import Logomark, { PROVIDER_MARK } from "./Logomark";
import Toggle from "./Toggle";
import { getDefaultMachine, type CloudUsageRow, type ProviderAccountView } from "../api/client";
import { formatDate } from "../lib/dates";
import { hourlyLabel } from "../lib/machines";
import { formatUsd } from "../lib/money";
import { PROVIDER_LABEL } from "../lib/providers";
import styles from "./ComputeCard.module.css";

export interface CardShellProps {
  /** The glyph in the square: a vendor's logo, or a plain server. */
  mark: JSX.Element;
  /** What the account or machine is called. */
  title: JSX.Element;
  /** The one line under the title. */
  meta: JSX.Element;
  /** A status pill on the right of the header, where there is a state. */
  status?: JSX.Element;
  /** The header's own controls, e.g. `Unlink` or `Rename`. */
  actions?: JSX.Element;
  /** Anything below the facts: a confirmation strip, an error. */
  footer?: JSX.Element;
  /** The facts, as `<Fact>` rows. */
  children: JSX.Element;
}

/**
 * The frame every compute card shares: a surface one step off the ground,
 * an identity, and a definition list of facts under a hairline.
 */
export function CardShell(props: CardShellProps) {
  return (
    <article class={styles.card}>
      <header class={styles.top}>
        <span class={styles.mark}>{props.mark}</span>
        <div class={styles.identity}>
          {/* A div rather than a span: renaming swaps the title for a form,
              and a form inside a span is not markup a browser will keep. */}
          <div class={styles.title}>{props.title}</div>
          <div class={styles.meta}>{props.meta}</div>
        </div>
        <div class={styles.headerEnd}>
          {props.status}
          {props.actions}
        </div>
      </header>

      <dl class={styles.facts}>{props.children}</dl>

      {props.footer}
    </article>
  );
}

/** One labelled fact: what it is, and the one value that names it. */
export function Fact(props: { label: string; children: JSX.Element }) {
  return (
    <div class={styles.fact}>
      <dt>{props.label}</dt>
      <dd>{props.children}</dd>
    </div>
  );
}

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
    <CardShell
      mark={
        <Show
          when={mark()}
          /* A machine the user owns has no vendor behind it, so it gets
             the generic server glyph rather than borrowing a logo. */
          fallback={<Server size={16} aria-hidden="true" />}
        >
          {(vendor) => <Logomark mark={vendor()} size={16} />}
        </Show>
      }
      title={props.account.label}
      meta={
        <>
          {PROVIDER_LABEL[props.account.kind]} · linked{" "}
          {formatDate(props.account.linked_at_unix)}
        </>
      }
      actions={
        <Show when={props.onUnlink}>
          {(unlink) => (
            <button type="button" class={styles.danger} onClick={() => unlink()()}>
              Unlink
            </button>
          )}
        </Show>
      }
    >
      <Fact label="Machine">
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
      </Fact>

      <Fact label="Spot capacity">
        <span class={styles.toggleCell}>
          <Toggle
            label={`Use spot capacity on ${props.account.label}`}
            checked={props.spot}
            onChange={props.onSpot}
          />
          <span class={styles.dim}>
            {props.spot ? "Cheaper; flyco handles eviction" : "Uninterruptible, and dearer"}
          </span>
        </span>
      </Fact>

      <Fact label="This billing period">
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
                <Show when={row().remaining_credit !== null && row().remaining_credit !== undefined}>
                  {" · "}
                  {formatUsd(row().remaining_credit ?? 0)} credit left
                </Show>
              </span>
            </>
          )}
        </Show>
      </Fact>
    </CardShell>
  );
}
