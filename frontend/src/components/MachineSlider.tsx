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
 * Everything the slider needs is on the entry. The backend stamps each one
 * with the family, generation and architecture its provider published and
 * with the charge a billing minimum implies, so nothing here parses a type
 * name or multiplies a rate.
 *
 * Keyboard: it is a real `<input type="range">`, so arrows, Home and End work
 * without a line of code, and `aria-valuetext` reads out the machine rather
 * than the index.
 */
import { For, Show, createMemo, createSignal } from "solid-js";
import { AlertTriangle } from "lucide-solid";
import Disclosure from "./Disclosure";
import Toggle from "./Toggle";
import type { MachineCatalogEntry, MachineDefault, ProviderAccountView } from "../api/client";
import { cx } from "../lib/cx";
import {
  ARCHITECTURE_LABEL,
  OS_LABEL,
  billingMinimum,
  billingMinimumSentence,
  detentLabel,
  entryKey,
} from "../lib/machines";
import styles from "./MachineSlider.module.css";

/** What the leftmost detent means, in the words docs/ux.md §7.7 asks for. */
const AUTO_LABEL = "Auto — the cheapest Linux type with at least 4 vCPU and 16 GiB";

/** The value of "no filter" in a `<select>`, which cannot hold null. */
const ANY = "";

export interface MachineSliderProps {
  /** The whole curated catalog, every account and region. */
  catalog: MachineCatalogEntry[];
  /** The caller's linked accounts, for the account filter's labels. */
  accounts: ProviderAccountView[];
  /** What flyco would pick on its own, which the `Auto` detent describes. */
  automatic: MachineDefault | undefined;
  /** Whether prices are quoted against spot capacity. */
  spot: boolean;
  /** The chosen entry's key, or `null` while the choice is flyco's. */
  chosenKey: string | null;
  /** Chooses an entry, or `null` to hand the choice back to flyco. */
  onChoose: (key: string | null) => void;
  onSpot: (spot: boolean) => void;
}

export default function MachineSlider(props: MachineSliderProps) {
  // Absent filters follow the machine flyco picked, so the detents open on
  // the account and region the user is about to use rather than on every
  // region every account can reach.
  const [account, setAccount] = createSignal<string | null>(null);
  const [region, setRegion] = createSignal<string | null>(null);
  const [architecture, setArchitecture] = createSignal<string | null>(null);
  const [os, setOs] = createSignal("linux");

  const activeAccount = createMemo(() => account() ?? props.automatic?.entry.account ?? null);
  const activeRegion = createMemo(() => region() ?? props.automatic?.entry.region ?? null);

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
        entry.os === os() &&
        (architecture() === null || entry.lineage?.architecture === architecture()),
    ),
  );

  /** Where the thumb sits: 0 is `Auto`, 1…n are the detents. */
  const position = createMemo(() => {
    const key = props.chosenKey;
    if (key === null) {
      return 0;
    }
    const index = detents().findIndex((entry) => entryKey(entry) === key);
    return index < 0 ? 0 : index + 1;
  });

  const selected = createMemo<MachineCatalogEntry | undefined>(() =>
    position() === 0 ? undefined : detents()[position() - 1],
  );

  const label = createMemo(() => {
    const entry = selected();
    return entry === undefined ? AUTO_LABEL : detentLabel(entry, props.spot);
  });

  function move(next: number): void {
    const entry = detents()[next - 1];
    props.onChoose(next === 0 || entry === undefined ? null : entryKey(entry));
  }

  return (
    <div class={styles.slider}>
      <Disclosure summary="Advanced">
        <div class={styles.filters}>
          <Filter
            label="Account"
            value={activeAccount() ?? ANY}
            options={optionsFor("account").map((id) => ({
              value: id,
              label: props.accounts.find((row) => row.id === id)?.label ?? id,
            }))}
            onChange={setAccount}
          />
          <Filter
            label="Region"
            value={activeRegion() ?? ANY}
            options={optionsFor("region").map((value) => ({ value, label: value }))}
            onChange={setRegion}
          />
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
          <Filter
            label="Operating system"
            value={os()}
            options={[...new Set(props.catalog.map((entry) => entry.os))].sort().map((value) => ({
              value,
              label: OS_LABEL[value],
            }))}
            onChange={(next) => setOs(next ?? "linux")}
          />
          <label class={styles.toggleRow}>
            <Toggle
              label="Use spot capacity"
              checked={props.spot}
              onChange={(next) => props.onSpot(next)}
            />
            Spot capacity — cheaper, and flyco handles eviction
          </label>
        </div>
      </Disclosure>

      <p class={styles.reading}>
        <span class={styles.machine}>{label()}</span>
        <Show when={selected() !== undefined && billingMinimum(selected() as MachineCatalogEntry)}>
          <span class={styles.badge}>
            <AlertTriangle size={12} aria-hidden="true" />
            License-bound
          </span>
        </Show>
      </p>

      <Show
        when={detents().length > 0}
        fallback={
          <p class={styles.empty}>
            This account offers no machine in {activeRegion() ?? "any region"} that flyco can
            deploy. Try another region under Advanced.
          </p>
        }
      >
        <div class={styles.track}>
          <input
            class={styles.range}
            type="range"
            min={0}
            max={detents().length}
            step={1}
            value={position()}
            aria-label="Machine"
            aria-valuetext={label()}
            onInput={(event) => move(Number(event.currentTarget.value))}
          />
          <div class={styles.ticks} aria-hidden="true">
            <For each={[undefined, ...detents()]}>
              {(entry, index) => (
                <span
                  class={cx(
                    styles.tick,
                    entry !== undefined && billingMinimum(entry) !== null && styles.tickBound,
                    index() === position() && styles.tickActive,
                  )}
                  style={{
                    left: `${(index() / detents().length) * 100}%`,
                  }}
                />
              )}
            </For>
          </div>
          <div class={styles.ends}>
            <span>Auto</span>
            <span>
              {detents().length} {detents().length === 1 ? "machine" : "machines"}
            </span>
          </div>
        </div>
      </Show>

      <Show when={selected() !== undefined && billingMinimumSentence(selected() as MachineCatalogEntry)}>
        {(sentence) => <p class={styles.warning}>{sentence()}</p>}
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
