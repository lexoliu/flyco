/**
 * Choosing a machine by hand, in two steps (docs/ux.md §7.7): first the
 * product form, then the size on a tiered slider.
 *
 * A container, a VM and a codespace are different products, not different
 * prices of one — a container's filesystem ends with the run, a VM's disk
 * survives a stop, and a codespace is bought from GitHub in core-hours —
 * so the choice between them is a row of segments above the track rather
 * than more detents on it. `Auto` leads the row, still the absence of a
 * choice rather than one machine among machines; under it there is no
 * track, only the machine flyco would pick and the rule it picked by.
 *
 * The slider underneath is drawn rather than left to the user agent,
 * because a bare range input hides the one thing that matters here: that
 * the positions are *countable*. Every detent is a dot on the track, the
 * thumb lands on one of them and nowhere between, the reading rides above
 * the thumb so the name and the price are read where the eye already is,
 * and a type its provider bills a minimum for wears amber on its dot
 * before the user ever stops there.
 *
 * Underneath it is still a real `<input type="range">`, transparent and
 * laid over the track: the pointer drag, the accessible role, the value
 * semantics and the focus ring are the platform's. Only the mapping from
 * key to detent is ours, in {@link detentForKey}, so that stepping is by
 * detent rather than by number and so that it can be tested.
 *
 * Everything the reading needs is on the entry. The backend stamps each
 * one with the family, generation and architecture its provider published
 * and with the charge a billing minimum implies, so nothing here parses a
 * type name or multiplies a rate.
 */
import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { AlertTriangle } from "lucide-solid";
import Detents from "./Detents";
import Disclosure from "./Disclosure";
import Toggle from "./Toggle";
import type { MachineCatalogEntry, MachineDefault, ProviderAccountView } from "../api/client";
import { cx } from "../lib/cx";
import {
  ARCHITECTURE_LABEL,
  FORM_LABEL,
  FORM_NOUN,
  FORM_PLURAL,
  FORM_TITLE,
  MACHINE_FORMS,
  OS_LABEL,
  billingMinimum,
  billingMinimumSentence,
  detentLabel,
  priceLabel,
  entryKey,
  formOf,
  type MachineForm,
} from "../lib/machines";
import styles from "./MachineSlider.module.css";

/** The first segment, which is flyco keeping the choice. */
const AUTO_NAME = "Auto";

/**
 * What the leftmost detent is called.
 *
 * The track is one form's machines ordered by price, so its left end is
 * the cheapest thing the form offers — flyco choosing is a position on the
 * form row above, never a detent.
 */
const CHEAPEST_NAME = "Cheapest";

/**
 * A dimension the `Advanced` disclosure lets the user change.
 *
 * Named rather than a row of booleans, because which ones are offered is a
 * fact about the *caller*: a new session can be put in any account, any
 * region and either capacity mode, while a resize moves a machine that
 * already exists — `POST /v1/sessions/{id}/machine/resize` carries a type
 * and nothing else — so offering those three there would be offering
 * choices the request cannot express.
 */
export type MachineFilter = "account" | "region" | "architecture" | "os" | "spot";

/** Every dimension, which is what the composer's chip offers. */
const ALL_FILTERS: readonly MachineFilter[] = [
  "account",
  "region",
  "architecture",
  "os",
  "spot",
];

/** The value of "no filter" in a `<select>`, which cannot hold null. */
const ANY = "";

export interface MachineSliderProps {
  /** The whole curated catalog, every account and region. */
  catalog: MachineCatalogEntry[];
  /** The caller's linked accounts, for the account filter's labels. */
  accounts: ProviderAccountView[];
  /** What flyco would pick on its own, which the `Auto` segment describes. */
  automatic: MachineDefault | undefined;
  /**
   * The entry the untouched filters follow: whose account and region are
   * shown before the user touches a filter.
   *
   * Defaults to the machine flyco would pick, which is what a new session
   * is about to use. A resize passes the machine the session is *on*, so
   * the track it opens on is the one it can actually move along.
   */
  anchor?: MachineCatalogEntry | undefined;
  /**
   * Whether the first segment hands the choice back to flyco.
   *
   * `false` for a machine that already exists: there is nothing to hand
   * back, and something is always chosen.
   */
  allowAuto?: boolean | undefined;
  /** Which dimensions `Advanced` offers. Defaults to all of them. */
  filters?: readonly MachineFilter[] | undefined;
  /** Whether prices are quoted against spot capacity. */
  spot: boolean;
  /** The chosen entry's key, or `null` while the choice is flyco's. */
  chosenKey: string | null;
  /** Chooses an entry, or `null` to hand the choice back to flyco. */
  onChoose: (key: string | null) => void;
  /** Changes the capacity mode. Absent where `spot` is not offered. */
  onSpot?: ((spot: boolean) => void) | undefined;
  /**
   * What to say instead of the control, while there is nothing to choose
   * from *yet*.
   *
   * An account whose catalog flyco is still reading has no machines, no
   * regions and no architectures — so every `Advanced` select would be
   * empty and the empty-track sentence would tell the user to try another
   * region that is not offered either. Both are claims about an account
   * that has not been read, so while this is set the slider says this one
   * sentence and nothing else.
   */
  pending?: string | undefined;
}

export default function MachineSlider(props: MachineSliderProps) {
  // Absent filters follow the machine flyco picked, so the detents open on
  // the account and region the user is about to use rather than on every
  // region every account can reach.
  const [account, setAccount] = createSignal<string | null>(null);
  const [region, setRegion] = createSignal<string | null>(null);
  const [architecture, setArchitecture] = createSignal<string | null>(null);
  const [os, setOs] = createSignal<string | null>(null);

  /** The entry the untouched filters follow. */
  const anchor = createMemo(() => props.anchor ?? props.automatic?.entry);
  const activeAccount = createMemo(() => account() ?? anchor()?.account ?? null);
  const activeRegion = createMemo(() => region() ?? anchor()?.region ?? null);
  // Linux where nothing says otherwise, which is what a new session opens
  // on; a resize of a Mac opens on macOS, because the machine it is moving
  // is one and an empty track would be the control's own doing.
  const activeOs = createMemo(() => os() ?? anchor()?.os ?? "linux");
  const autoOffered = createMemo(() => props.allowAuto !== false);
  const offers = (filter: MachineFilter): boolean =>
    (props.filters ?? ALL_FILTERS).includes(filter);

  /** The catalog entry behind the caller's key, when it is still listed. */
  const chosenEntry = createMemo(() =>
    props.catalog.find((entry) => entryKey(entry) === props.chosenKey),
  );

  /**
   * The forms this picker can offer, in the row's own order.
   *
   * A dimension the caller locked narrows the list: a resize keeps the
   * machine's account and region, so the row lists the forms that scope
   * holds and a form it cannot reach is not a choice the row should offer.
   * The dimensions the user can still change do not narrow it — a segment
   * that disappears because a select moved is a control fighting itself.
   */
  const offeredForms = createMemo<MachineForm[]>(() => {
    const reachable = props.catalog.filter(
      (entry) =>
        (offers("account") || entry.account === activeAccount()) &&
        (offers("region") || entry.region === activeRegion()) &&
        (offers("os") || entry.os === activeOs()),
    );
    return MACHINE_FORMS.filter((form) =>
      reachable.some((entry) => formOf(entry) === form),
    );
  });

  /**
   * The form the track is showing, read off the caller's choice rather
   * than held in a signal of its own: `null` is `Auto` — flyco's choice —
   * and a chosen entry's own form decides the rest. Where `Auto` is not
   * offered and nothing is chosen yet, the first offered form stands in,
   * which is also the form the snap effect below lands on.
   */
  const activeForm = createMemo<MachineForm | null>(() => {
    const chosen = chosenEntry();
    if (chosen !== undefined) {
      return formOf(chosen);
    }
    return autoOffered() ? null : (offeredForms()[0] ?? null);
  });

  /** The detents: the chosen account, region and form, ordered by price. */
  const detents = createMemo(() => {
    const form = activeForm();
    return props.catalog.filter(
      (entry) =>
        formOf(entry) === form &&
        entry.account === activeAccount() &&
        entry.region === activeRegion() &&
        entry.os === activeOs() &&
        (architecture() === null || entry.lineage?.architecture === architecture()),
    );
  });

  /**
   * Where the thumb sits: the chosen detent's index, or the first detent
   * while the choice is one the current filters do not reach.
   */
  const position = createMemo(() => {
    const index = detents().findIndex((entry) => entryKey(entry) === props.chosenKey);
    return index < 0 ? 0 : index;
  });

  /** The rightmost position, which is the last detent. */
  const lastPosition = createMemo(() => Math.max(0, detents().length - 1));

  const selected = createMemo<MachineCatalogEntry | undefined>(() => detents()[position()]);

  /**
   * Without an `Auto` segment something is always chosen, so a filter
   * change that drops the chosen machine out of the set has to move the
   * choice with it — otherwise the reading above the thumb and the machine
   * the caller holds would be two different machines.
   */
  createEffect(() => {
    if (autoOffered()) {
      return;
    }
    const entries = detents();
    const first = entries[0];
    if (first !== undefined && !entries.some((entry) => entryKey(entry) === props.chosenKey)) {
      props.onChoose(entryKey(first));
    }
  });

  /** How far along the track the thumb is, which the reading rides on too. */
  const travelled = createMemo(() =>
    lastPosition() === 0 ? "0%" : `${(position() / lastPosition()) * 100}%`,
  );

  /** The name over the thumb. */
  const reading = createMemo(() => {
    const entry = selected();
    return entry === undefined ? "" : detentLabel(entry);
  });


  /** Whether the chosen machine starts billing the moment it boots. */
  const bound = createMemo(() => {
    const entry = selected();
    return entry !== undefined && billingMinimum(entry) !== null;
  });

  /**
   * The line under the track: what a floor costs.
   *
   * It holds its line whether or not it has something to say, because the
   * popover is anchored to a chip and a control that changes height as the
   * thumb passes a machine is a control that shrugs.
   */
  const note = createMemo(() => {
    const entry = selected();
    if (entry === undefined) {
      return null;
    }
    const floor = billingMinimumSentence(entry);
    return floor === null ? priceLabel(entry, props.spot) : floor;
  });

  /**
   * Whether `Use spot capacity` has anything to act on.
   *
   * A form whose machines carry no spot price makes the toggle a no-op —
   * so it appears only where some detent can quote one, and under `Auto`
   * only where the catalog holds one at all.
   */
  const spotOffered = createMemo(() => {
    if (!offers("spot") || props.onSpot === undefined) {
      return false;
    }
    const scope = activeForm() === null ? props.catalog : detents();
    return scope.some(
      (entry) =>
        entry.pricing.kind === "metered" &&
        entry.pricing.spot_hourly !== null &&
        entry.pricing.spot_hourly !== undefined,
    );
  });

  /**
   * What the track's absence says.
   *
   * The form's own noun — "no codespace", not "no machine" — because the
   * user just chose the form and the sentence is about that choice, and
   * the hint names a filter that can actually move: a region only where
   * one is offered.
   */
  const emptyNotice = createMemo(() => {
    const form = activeForm();
    const noun = form === null ? "machine" : FORM_NOUN[form];
    let hint = "";
    if ((props.filters ?? ALL_FILTERS).length > 0 && form !== null) {
      hint = offers("region")
        ? " Try another region under Advanced."
        : " Try widening the filters under Advanced.";
    }
    return `This account offers no ${noun} in ${activeRegion() ?? "any region"} that flyco can deploy.${hint}`;
  });

  function move(next: number): void {
    const entry = detents()[next];
    props.onChoose(entry === undefined ? null : entryKey(entry));
  }

  /**
   * Picks a form: the choice lands on the cheapest entry of it the current
   * scope still reaches, and the scope moves onto the pick when it holds
   * none — a segment that only opened an empty track would be a control
   * that lies. The within-form refinements reset with the move: an OS or
   * an architecture named under another form says nothing about this one,
   * and an architecture filter can otherwise empty a track whose entries
   * publish no lineage at all (a managed container's is null).
   */
  function chooseForm(form: MachineForm): void {
    if (form === activeForm()) {
      return;
    }
    const entries = props.catalog.filter((entry) => formOf(entry) === form);
    const pick =
      entries.find(
        (entry) =>
          entry.account === activeAccount() &&
          entry.region === activeRegion() &&
          entry.os === activeOs() &&
          (architecture() === null || entry.lineage?.architecture === architecture()),
      ) ??
      entries.find(
        (entry) => entry.account === activeAccount() && entry.region === activeRegion(),
      ) ??
      entries[0];
    if (pick === undefined) {
      return;
    }
    setAccount(pick.account ?? null);
    setRegion(pick.region);
    setOs(pick.os);
    setArchitecture(null);
    props.onChoose(entryKey(pick));
  }

  /** Every value one dimension takes, once the form and the others have had their say. */
  function optionsFor(dimension: "account" | "region" | "architecture"): string[] {
    const seen = new Set<string>();
    const form = activeForm();
    for (const entry of props.catalog) {
      if (form !== null && formOf(entry) !== form) {
        continue;
      }
      if (dimension !== "account" && entry.account !== activeAccount()) {
        continue;
      }
      if (dimension === "architecture" && entry.region !== activeRegion()) {
        continue;
      }
      const value =
        dimension === "account"
          ? entry.account
          : dimension === "region"
            ? entry.region
            : entry.lineage?.architecture;
      if (value !== null && value !== undefined) {
        seen.add(value);
      }
    }
    return [...seen].sort();
  }

  /** Every operating system the chosen form comes in, wherever it is. */
  const osOptions = createMemo(() => {
    const form = activeForm();
    return [
      ...new Set(
        props.catalog
          .filter((entry) => form === null || formOf(entry) === form)
          .map((entry) => entry.os),
      ),
    ].sort();
  });

  return (
    <div class={styles.slider}>
      <Show when={props.pending === undefined} fallback={<p class={styles.empty}>{props.pending}</p>}>
      {/*
        The product form comes first, as a choice of its own rather than a
        detent among machine types: a container, a VM and a codespace are
        different bargains, and the slider that follows is the size within
        one of them. Only forms the catalog can actually offer are listed.
      */}
      <Show when={autoOffered() || offeredForms().length > 0}>
        <div class={styles.forms} role="radiogroup" aria-label="Kind of machine">
          <Show when={autoOffered()}>
            <button
              type="button"
              role="radio"
              aria-checked={activeForm() === null}
              title="flyco picks the machine"
              class={styles.form}
              onClick={() => props.onChoose(null)}
            >
              {AUTO_NAME}
            </button>
          </Show>
          <For each={offeredForms()}>
            {(form) => (
              <button
                type="button"
                role="radio"
                aria-checked={activeForm() === form}
                title={FORM_TITLE[form]}
                class={styles.form}
                onClick={() => chooseForm(form)}
              >
                {FORM_LABEL[form]}
              </button>
            )}
          </For>
        </div>
      </Show>

      <Show
        when={activeForm()}
        fallback={
          /*
            `Auto` has no track: the choice flyco is keeping is the size as
            well as the form, so what the panel owes the user is the
            machine it would pick and the rule it picked by.
          */
          <Show
            when={props.automatic}
            fallback={<p class={styles.empty}>{emptyNotice()}</p>}
          >
            {(automatic) => (
              <div class={styles.auto}>
                <p class={styles.autoName}>{AUTO_NAME}</p>
                <p class={styles.autoPick}>{detentLabel(automatic().entry)}</p>
                <p class={styles.note}>{priceLabel(automatic().entry, props.spot)}</p>
                <Show when={spotOffered() && props.onSpot}>
                  {(onSpot) => (
                    <label class={styles.toggleRow}>
                      <Toggle
                        label="Use spot capacity"
                        checked={props.spot}
                        onChange={(next) => onSpot()(next)}
                      />
                      Spot
                    </label>
                  )}
                </Show>
              </div>
            )}
          </Show>
        }
      >
        {(form) => (
          <>
          {/*
            Only the dimensions the caller can act on: a filter whose change
            the request cannot carry is a control that lies. Each select is
            scoped to the chosen form — an account that offers no codespace
            is not an answer to "which account's codespaces".
          */}
          <Show when={(props.filters ?? ALL_FILTERS).length > 0}>
          <Disclosure summary="Advanced">
            <div class={styles.filters}>
              <Show when={offers("account")}>
              <Filter
                label="Account"
                value={activeAccount() ?? ANY}
                options={optionsFor("account").map((id) => ({
                  value: id,
                  label: props.accounts.find((row) => row.id === id)?.label ?? id,
                }))}
                onChange={setAccount}
              />
              </Show>
              <Show when={offers("region")}>
              <Filter
                label="Region"
                value={activeRegion() ?? ANY}
                options={optionsFor("region").map((value) => ({ value, label: value }))}
                onChange={setRegion}
              />
              </Show>
              <Show when={offers("architecture")}>
              <Filter
                label="Architecture"
                value={architecture() ?? ANY}
                anyLabel="Either"
                options={optionsFor("architecture").map((value) => ({
                  value,
                  label: ARCHITECTURE_LABEL[value as keyof typeof ARCHITECTURE_LABEL] ?? value,
                }))}
                onChange={setArchitecture}
              />
              </Show>
              <Show when={offers("os")}>
              <Filter
                label="Operating system"
                value={activeOs()}
                options={osOptions().map((value) => ({
                  value,
                  label: OS_LABEL[value],
                }))}
                onChange={setOs}
              />
              </Show>
              <Show when={spotOffered() && props.onSpot}>
                {(onSpot) => (
                  <label class={styles.toggleRow}>
                    <Toggle
                      label="Use spot capacity"
                      checked={props.spot}
                      onChange={(next) => onSpot()(next)}
                    />
                    Spot
                  </label>
                )}
              </Show>
            </div>
          </Disclosure>
          </Show>

          <Show when={detents().length > 0} fallback={<p class={styles.empty}>{emptyNotice()}</p>}>
            {/* One number drives the whole control: the thumb's travel, and the
                reading that rides above it. The reading is carried the same
                distance and then pulled back by that share of its own width, so
                it centres on the thumb in the middle and tucks inside the track
                at either end without measuring anything. */}
            <div class={styles.control} style={{ "--travelled": travelled() }}>
              <div class={styles.readingRow}>
                <div class={styles.readingTravel}>
                  <p class={styles.reading}>
                    <span class={styles.machine}>{reading()}</span>
                    <Show when={bound()}>
                      <span class={styles.badge}>
                        <AlertTriangle size={12} aria-hidden="true" />
                        License-bound
                      </span>
                    </Show>
                  </p>
                </div>
              </div>

              <Detents
                count={detents().length}
                position={position()}
                ariaLabel="Machine"
                ariaValueText={reading()}
                dotClass={(index) => {
                  const entry = detents()[index];
                  return entry !== undefined && billingMinimum(entry) !== null
                    ? styles.dotBound
                    : undefined;
                }}
                onMove={move}
              />

              <div class={styles.ends}>
                <span>{CHEAPEST_NAME}</span>
                <span>
                  {detents().length}{" "}
                  {detents().length === 1 ? FORM_NOUN[form()] : FORM_PLURAL[form()]}
                </span>
              </div>

              <p class={cx(styles.note, bound() && styles.noteBound)}>{note()}</p>
            </div>
          </Show>
          </>
        )}
      </Show>
      </Show>
    </div>
  );
}

/** One `<select>` under `Advanced`, with an "any" option where that is real. */
function Filter(props: {
  label: string;
  value: string;
  anyLabel?: string | undefined;
  options: { value: string; label: string }[];
  onChange: (next: string | null) => void;
}) {
  return (
    <label class={styles.filter}>
      <span>{props.label}</span>
      <select
        value={props.value}
        onChange={(event) =>
          props.onChange(event.currentTarget.value === ANY ? null : event.currentTarget.value)
        }
      >
        <Show when={props.anyLabel !== undefined}>
          <option value={ANY}>{props.anyLabel}</option>
        </Show>
        <For each={props.options}>
          {(option) => <option value={option.value}>{option.label}</option>}
        </For>
      </select>
    </label>
  );
}
