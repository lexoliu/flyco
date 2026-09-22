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
import { Show, createEffect, createSignal, onCleanup, type JSX } from "solid-js";
import { createQuery } from "../lib/query";
import { A } from "@solidjs/router";
import { Server } from "lucide-solid";
import ConfirmDialog from "./ConfirmDialog";
import Logomark, { PROVIDER_MARK } from "./Logomark";
import ProblemNotice from "./ProblemNotice";
import Toggle from "./Toggle";
import {
  getDefaultMachine,
  type CloudProviderKind,
  type CloudUsageRow,
  type MachineCatalogEntry,
  type ProviderAccountView,
} from "../api/client";
import { ApiProblem } from "../api/problem";
import { formatDate } from "../lib/dates";
import { inUseRefusal, sessionsStillRunning, type InUseRefusal } from "../lib/inUse";
import {
  CATALOG_POLL_SECONDS,
  catalogNotReady,
  hourlyLabel,
  readingMachines,
} from "../lib/machines";
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
  /**
   * Unlinks the account, and rejects the way the request did.
   *
   * The card asks before calling it and reads the refusal afterwards: an
   * account with machines still on it is refused with `provider-in-use`,
   * which is not an error but the control plane saying what has to happen
   * first (issue #139). Omitted where unlinking is not on offer.
   */
  onUnlink?: (() => Promise<void>) | undefined;
}

export default function ComputeCard(props: ComputeCardProps) {
  const [confirming, setConfirming] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  /** What a refused unlink said, when the account is still in use. */
  const [refused, setRefused] = createSignal<InUseRefusal | null>(null);

  async function unlink(): Promise<void> {
    const request = props.onUnlink;
    if (request === undefined) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await request();
      setConfirming(false);
      setRefused(null);
    } catch (failure) {
      const inUse = inUseRefusal(failure);
      if (inUse === null) {
        setError(failure);
      } else {
        // There is no forcing this one: the machines are the user's, in
        // their own account, and flyco will not strand them. The dialog
        // keeps its place and says what ends the refusal instead.
        setRefused(inUse);
      }
    } finally {
      setBusy(false);
    }
  }

  function stopConfirming(): void {
    setConfirming(false);
    setRefused(null);
    setError(null);
  }

  // Keyed on both, because the machine flyco would pick and the price it
  // would pay both move with the capacity mode.
  const [machine, { refetch: reask }] = createQuery(
    () => ({ account: props.account.id, spot: props.spot }),
    ({ account, spot }) => getDefaultMachine(spot, account),
  );

  /**
   * Whether flyco is still reading what this account can deploy.
   *
   * A card opened moments after the account was linked is the ordinary case:
   * reading a cloud account's machines happens on the provisioning queue and
   * lands seconds later, so the cell says so and asks again rather than
   * showing a skeleton that never resolves.
   */
  const reading = () => catalogNotReady(machine.error);

  createEffect(() => {
    if (!reading()) {
      return;
    }
    const timer = setInterval(() => void reask(), CATALOG_POLL_SECONDS * 1000);
    onCleanup(() => clearInterval(timer));
  });

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
        // Nothing else while the question is up: the dialog under the card
        // is the one thing being answered, and a second `Unlink` beside it
        // would be two buttons with one meaning.
        <Show when={props.onUnlink !== undefined && !confirming()}>
          <button
            type="button"
            class={styles.danger}
            disabled={busy()}
            onClick={() => setConfirming(true)}
          >
            Unlink
          </button>
        </Show>
      }
      footer={
        <Show when={confirming()}>
          <ConfirmDialog
            title={`Unlink ${props.account.label}?`}
            body={
              <Show
                when={refused()}
                fallback={
                  <>
                    Flyco stops provisioning in this {PROVIDER_LABEL[props.account.kind]} account
                    and forgets the credential; you would paste it again to link it back. Nothing
                    in the account itself is deleted.
                  </>
                }
              >
                {(inUse) => (
                  <>
                    {sessionsStillRunning(inUse())} Unlinking would leave their machines running
                    with nothing to stop them, so archive those sessions first.
                  </>
                )}
              </Show>
            }
            confirmLabel="Unlink"
            cancelLabel={refused() === null ? "Keep it" : "Close"}
            busy={busy()}
            {...(refused() === null ? { onConfirm: () => void unlink() } : {})}
            onCancel={stopConfirming}
          >
            <ProblemNotice error={error()} />
            <Show when={refused()}>
              <A href="/" class={styles.sessionsLink}>
                Go to Sessions
              </A>
            </Show>
          </ConfirmDialog>
        </Show>
      }
    >
      <Fact label="Machine">
        {/*
          The error is checked before the value is read: a Solid resource
          that rejected throws from its accessor, and a refusal read the
          other way round would take the card down and leave the cell blank.
        */}
        <Show
          when={machine.error === undefined}
          fallback={
            <span class={styles.absent}>
              {machineAbsence(machine.error, props.account.kind)}
            </span>
          }
        >
          <Show
            when={machine()}
            fallback={<span class={styles.skeleton} aria-label="Reading the catalog" />}
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
        </Show>
      </Fact>

      {/*
        What the provider gives away before it bills anything, where it
        gives away something: a codespace's included core-hours are the
        reason a new user pays nothing at all for weeks, and a page about
        what compute costs that did not say so would be missing the first
        number a reader looks for.
      */}
      <Show when={freeHours(machine()?.entry)}>
        {(hours) => <Fact label="Free each month">{hours()} vCPU-hours</Fact>}
      </Show>

      {/*
        Only where the provider sells interruptible capacity. GitHub bills
        a codespace one way, and hardware the user enrolled is already
        theirs, so on those the toggle was a switch that repriced nothing.
      */}
      <Show when={SELLS_SPOT.has(props.account.kind)}>
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
      </Show>

      <Fact label="This billing period">
        <Show
          when={props.account.kind !== "host"}
          /* `GET /v1/usage/cloud` deliberately returns no row for hardware
             the user already owns; a `$0.00` there would read as "this is
             free", which it is not. */
          fallback={<YourHardware />}
        >
          <Show
            when={metered(props.usage)}
            /* A cloud account that has not been billed yet is a different
               fact from one flyco cannot meter: the meter is running, it
               has just read nothing so far. */
            fallback={
              <span class={styles.absent}>
                No metered spend yet this period.
                <CreditLeft usage={props.usage} />
              </span>
            }
          >
            {(row) => (
              <>
                <strong>{formatUsd(row().spent)}</strong>
                <span class={styles.dim}>
                  {" since "}
                  {formatDate(row().period_start_unix)}
                  <CreditLeft usage={row()} />
                </span>
              </>
            )}
          </Show>
        </Show>
      </Fact>
    </CardShell>
  );
}

/**
 * The vCPU-hours a provider gives away each month, where it gives any.
 *
 * The grant is stated in the provider's own meters — vCPU-seconds, and on
 * some providers a memory meter beside it — because that is how it is
 * actually spent. Hours are what a reader counts in, and the vCPU meter is
 * the one that runs out first on every machine flyco picks.
 */
function freeHours(entry: MachineCatalogEntry | undefined): number | undefined {
  const seconds = entry?.free_grant?.vcpu_seconds_per_month;
  return seconds === undefined || seconds === 0 ? undefined : Math.round(seconds / 3600);
}

/** The usage row, when it carries any spend at all. */
function metered(usage: CloudUsageRow | undefined): CloudUsageRow | undefined {
  return usage !== undefined && usage.spent > 0 ? usage : undefined;
}

/** `· $80.00 credit left`, on whichever spend line has a credit to report. */
function CreditLeft(props: { usage: CloudUsageRow | undefined }) {
  const credit = () => props.usage?.remaining_credit ?? null;
  return (
    <Show when={credit() !== null}>
      {" · "}
      {formatUsd(credit() ?? 0)} credit left
    </Show>
  );
}

/**
 * What a machine the user owns costs: nothing flyco meters.
 *
 * Not `$0.00`, and not silence either — a zero would tell a budget it can
 * run forever, and an empty cell would look like a number that failed to
 * load. Shared with `HostCard`, so the host's own card and the compute card
 * that stands in for it before the host's facts arrive say the same words.
 */
export function YourHardware() {
  return (
    <>
      <strong>your hardware</strong>
      <span class={styles.dim}> · a session here spends no budget</span>
    </>
  );
}

/**
 * Why there is no machine to name, in words.
 *
 * Three different answers, and collapsing any two of them would say
 * something untrue. `catalog-not-ready` is a wait that ends by itself, so it
 * describes what flyco is doing; `no-deployable-linux-machine` is a fact
 * about an account that *was* read, so it is stated as one; anything else is
 * a failure, and its own message is the most honest thing to show.
 */
/**
 * The providers that sell interruptible capacity at a lower price.
 *
 * GitHub bills a codespace at one rate, and a machine the user enrolled is
 * already theirs; a spot toggle on either is a switch with nothing behind
 * it.
 */
const SELLS_SPOT: ReadonlySet<CloudProviderKind> = new Set(["azure", "aws", "gcp"]);

function machineAbsence(error: unknown, kind: CloudProviderKind): string {
  if (catalogNotReady(error)) {
    return readingMachines([kind]);
  }
  if (error instanceof ApiProblem && error.type.endsWith("/no-deployable-linux-machine")) {
    return "No deployable Linux machine in this account yet.";
  }
  return error instanceof Error ? error.message : String(error);
}
