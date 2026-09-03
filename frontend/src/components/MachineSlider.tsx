/**
 * Choosing a machine by hand, as a tiered slider (docs/ux.md §7.7).
 *
 * A table of machine types is a table of numbers nobody reads. A slider is
 * the shape of the decision the user is actually making — spend less, or get
 * more — and the curated catalog (`flyco_core::catalog`) is short enough and
 * strictly ordered by price, which is exactly what a slider's detents need to
 * be. The leftmost position is `Auto`, so the default is not a machine among
 * machines but the absence of a choice.
 *
 * The control is drawn rather than left to the user agent, because a bare
 * range input hides the one thing that matters here: that the positions are
 * *countable*. Every detent is a dot on the track, the thumb lands on one of
 * them and nowhere between, the reading rides above the thumb so the name and
 * the price are read where the eye already is, and a type its provider bills a
 * minimum for wears amber on its dot before the user ever stops there.
 *
 * Underneath it is still a real `<input type="range">`, transparent and laid
 * over the track: the pointer drag, the accessible role, the value semantics
 * and the focus ring are the platform's. Only the mapping from key to detent
 * is ours, in {@link detentForKey}, so that stepping is by detent rather than
 * by number and so that it can be tested.
 *
 * Everything the reading needs is on the entry. The backend stamps each one
 * with the family, generation and architecture its provider published and
 * with the charge a billing minimum implies, so nothing here parses a type
 * name or multiplies a rate.
 */
import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { AlertTriangle } from "lucide-solid";
import Disclosure from "./Disclosure";
import Toggle from "./Toggle";
import type { MachineCatalogEntry, MachineDefault, ProviderAccountView } from "../api/client";
import { cx } from "../lib/cx";
import {
  ARCHITECTURE_LABEL,
  OS_LABEL,
  autoSentence,
  billingMinimum,
  billingMinimumSentence,
  detentLabel,
  entryKey,
} from "../lib/machines";
import styles from "./MachineSlider.module.css";

/** The leftmost detent, which is flyco keeping the choice. */
const AUTO_NAME = "Auto";

/**
 * What the leftmost detent is called where flyco is not choosing.
 *
 * A resize is a move from one machine to another (docs/ux.md §9.5): there
 * is no "let flyco decide" in it, and the track's left end is simply the
 * cheapest thing on offer.
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

/**
 * The detent a key moves the thumb to, or `null` for a key the slider does
 * not claim.
 *
 * The browser would step this input on its own, and every step would land on
 * an integer — but the integers *are* the detents here, and which key means
 * which detent is a decision about the control rather than about numbers:
 * `Home` is `Auto`, `End` is the largest machine on offer, and both arrow
 * axes move by one machine so that a thumb reached by keyboard behaves like
 * the thumb reached by pointer. Owning it keeps that in one tested place;
 * the component prevents the default so nothing steps twice.
 */
export function detentForKey(key: string, position: number, count: number): number | null {
  const target = ((): number | null => {
    switch (key) {
      case "ArrowLeft":
      case "ArrowDown":
        return position - 1;
      case "ArrowRight":
      case "ArrowUp":
        return position + 1;
      case "Home":
        return 0;
      case "End":
        return count;
      default:
        return null;
    }
  })();
  return target === null ? null : Math.min(Math.max(target, 0), count);
}

export interface MachineSliderProps {
  /** The whole curated catalog, every account and region. */
  catalog: MachineCatalogEntry[];
  /** The caller's linked accounts, for the account filter's labels. */
  accounts: ProviderAccountView[];
  /** What flyco would pick on its own, which the `Auto` detent describes. */
  automatic: MachineDefault | undefined;
  /**
   * The entry the detents open around: whose account and region are shown
   * before the user touches a filter.
   *
   * Defaults to the machine flyco would pick, which is what a new session
   * is about to use. A resize passes the machine the session is *on*, so
   * the track it opens on is the one it can actually move along.
   */
  anchor?: MachineCatalogEntry | undefined;
  /**
   * Whether the leftmost detent hands the choice back to flyco.
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
  /** How far the detents are pushed right by an `Auto` at position 0. */
  const offset = createMemo(() => (autoOffered() ? 1 : 0));
  const offers = (filter: MachineFilter): boolean =>
    (props.filters ?? ALL_FILTERS).includes(filter);

  /** Every value one dimension takes, once the others have had their say. */
  function optionsFor(dimension: "account" | "region" | "architecture"): string[] {
    const seen = new Set<string>();
    for (const entry of props.catalog) {
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

  /** The detents: the selected account and region, ordered by price. */
  const detents = createMemo(() =>
    props.catalog.filter(
      (entry) =>
        entry.account === activeAccount() &&
        entry.region === activeRegion() &&
        entry.os === activeOs() &&
        (architecture() === null || entry.lineage?.architecture === architecture()),
    ),
  );

  /**
   * Where the thumb sits: 0 is `Auto` where it is offered, and the detents
   * follow it; without an `Auto` the detents start at 0 themselves.
   */
  const position = createMemo(() => {
    const key = props.chosenKey;
    if (key === null) {
      return 0;
    }
    const index = detents().findIndex((entry) => entryKey(entry) === key);
    return index < 0 ? 0 : index + offset();
  });

  /** The rightmost position, which is the last detent. */
  const lastPosition = createMemo(() => Math.max(0, detents().length - 1 + offset()));

  const selected = createMemo<MachineCatalogEntry | undefined>(() =>
    detents()[position() - offset()],
  );

  /**
   * Without an `Auto` detent something is always chosen, so a filter change
   * that drops the chosen machine out of the set has to move the choice
   * with it — otherwise the reading above the thumb and the machine the
   * caller holds would be two different machines.
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

  /** The name over the thumb: a machine, or the absence of a choice. */
  const reading = createMemo(() => {
    const entry = selected();
    return entry === undefined ? AUTO_NAME : detentLabel(entry, props.spot);
  });

  /**
   * What `Auto` resolved to, said as a sentence.
   *
   * Read off the machine flyco would actually provision rather than stated
   * as a constant: hardware the user enrolled has no price to be cheapest
   * at, so the rule about being cheapest would be a claim about a decision
   * that was never made.
   */
  const rule = createMemo(() => autoSentence(props.automatic?.entry));

  /** The same thing said in full, for a reader who cannot see the track. */
  const spoken = createMemo(() =>
    selected() === undefined ? `${AUTO_NAME}. ${rule()}` : reading(),
  );

  /** Whether the chosen machine starts billing the moment it boots. */
  const bound = createMemo(() => {
    const entry = selected();
    return entry !== undefined && billingMinimum(entry) !== null;
  });

  /**
   * The line under the track: what `Auto` picks, or what a floor costs.
   *
   * It holds its line whether or not it has something to say, because the
   * popover is anchored to a chip and a control that changes height as the
   * thumb passes a machine is a control that shrugs.
   */
  const note = createMemo(() => {
    const entry = selected();
    return entry === undefined ? rule() : billingMinimumSentence(entry);
  });

  function move(next: number): void {
    const entry = detents()[next - offset()];
    props.onChoose(entry === undefined ? null : entryKey(entry));
  }

  return (
    <div class={styles.slider}>
      {/*
        Only the dimensions the caller can act on: a filter whose change the
        request cannot carry is a control that lies.
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
            options={[...new Set(props.catalog.map((entry) => entry.os))].sort().map((value) => ({
              value,
              label: OS_LABEL[value],
            }))}
            onChange={setOs}
          />
          </Show>
          <Show when={offers("spot") && props.onSpot}>
            {(onSpot) => (
              <label class={styles.toggleRow}>
                <Toggle
                  label="Use spot capacity"
                  checked={props.spot}
                  onChange={(next) => onSpot()(next)}
                />
                Spot capacity — cheaper, and flyco handles eviction
              </label>
            )}
          </Show>
        </div>
      </Disclosure>
      </Show>

      <Show
        when={detents().length > 0}
        fallback={
          <p class={styles.empty}>
            This account offers no machine in {activeRegion() ?? "any region"} that flyco can
            deploy. Try another region under Advanced.
          </p>
        }
      >
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

          <div class={styles.track}>
            <div class={styles.rail}>
              <For each={autoOffered() ? [undefined, ...detents()] : detents()}>
                {(entry, index) => (
                  <span
                    aria-hidden="true"
                    class={cx(
                      styles.dot,
                      entry !== undefined && billingMinimum(entry) !== null && styles.dotBound,
                      index() === position() && styles.dotTaken,
                    )}
                    style={{
                      left: lastPosition() === 0 ? "0%" : `${(index() / lastPosition()) * 100}%`,
                    }}
                  />
                )}
              </For>
              <div class={styles.thumbTravel} aria-hidden="true">
                <span class={styles.thumb} />
              </div>
              <input
                class={styles.range}
                type="range"
                min={0}
                max={lastPosition()}
                step={1}
                value={position()}
                aria-label="Machine"
                aria-valuetext={spoken()}
                onInput={(event) => move(Number(event.currentTarget.value))}
                onKeyDown={(event) => {
                  const next = detentForKey(event.key, position(), lastPosition());
                  if (next !== null) {
                    event.preventDefault();
                    move(next);
                  }
                }}
              />
            </div>
          </div>

          <div class={styles.ends}>
            <span>{autoOffered() ? AUTO_NAME : CHEAPEST_NAME}</span>
            <span>
              {detents().length} {detents().length === 1 ? "machine" : "machines"}
            </span>
          </div>

          <p class={cx(styles.note, bound() && styles.noteBound)}>{note()}</p>
        </div>
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
