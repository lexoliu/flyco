/**
 * Reading a curated catalog entry out loud.
 *
 * The backend curates the catalog (`flyco_core::catalog`) and stamps each
 * entry with the family, generation and architecture its provider published,
 * so nothing here parses a machine type name. What is left is presentation:
 * the four facts the slider's label states, and the one sentence a
 * license-bound type has to say before anybody commits to it.
 */
import type { MachineCatalogEntry } from "../api/client";
import { formatUsd } from "./money";

/** How many MiB are in a GiB. Catalog capacities are stated in MiB. */
const MIB_PER_GIB = 1024;

/**
 * Identifies one catalog entry across the account, region and type it names.
 *
 * All three, because one machine type is offered by several accounts in
 * several regions and those are different machines with different prices.
 */
export function entryKey(entry: MachineCatalogEntry): string {
  return `${entry.account ?? ""}/${entry.region}/${entry.machine_type}`;
}

/**
 * What one hour costs, at the capacity mode being asked for.
 *
 * `null` where flyco meters nothing, which is hardware the user owns — a
 * `$0.00` there would read as "this is free", which it is not.
 */
export function hourlyMicros(entry: MachineCatalogEntry, spot: boolean): number | null {
  if (entry.pricing.kind === "user_owned") {
    return null;
  }
  const spotHourly = entry.pricing.spot_hourly;
  return spot && spotHourly !== null && spotHourly !== undefined
    ? spotHourly
    : entry.pricing.on_demand_hourly;
}

/** `$0.19/hr`, or the honest absence of a price on hardware the user owns. */
export function hourlyLabel(entry: MachineCatalogEntry, spot: boolean): string {
  const hourly = hourlyMicros(entry, spot);
  return hourly === null ? "your hardware" : `${formatUsd(hourly)}/hr`;
}

/** `4 vCPU / 16 GiB`, or nothing for a machine flyco has not measured. */
export function capacityLabel(entry: MachineCatalogEntry): string | null {
  const capacity = entry.capacity;
  if (capacity === null || capacity === undefined) {
    return null;
  }
  const gib = Math.round(capacity.memory_mib / MIB_PER_GIB);
  return `${capacity.vcpus} vCPU / ${gib} GiB`;
}

/**
 * The slider label of docs/ux.md §7.7:
 * `Standard_D4s_v6 · 4 vCPU / 16 GiB · $0.19/hr`.
 */
export function detentLabel(entry: MachineCatalogEntry, spot: boolean): string {
  return [entry.machine_type, capacityLabel(entry), hourlyLabel(entry, spot)]
    .filter((part) => part !== null)
    .join(" · ");
}

/**
 * The floor a provider bills the moment this machine boots, if it imposes
 * one.
 *
 * Both halves come off the entry rather than being multiplied here: the
 * backend computes the charge alongside the hours so the price and the
 * minimum cannot disagree.
 */
export function billingMinimum(
  entry: MachineCatalogEntry,
): { hours: number; charge: number } | null {
  if (entry.pricing.kind === "user_owned") {
    return null;
  }
  return entry.pricing.minimum ?? null;
}

/**
 * The sentence that has to be on screen before a license-bound machine can
 * be sent (docs/ux.md §7.7).
 */
export function billingMinimumSentence(entry: MachineCatalogEntry): string | null {
  const minimum = billingMinimum(entry);
  return minimum === null
    ? null
    : `Starts a ${minimum.hours}-hour minimum charge of ${formatUsd(minimum.charge)} the moment it boots.`;
}

/** How an architecture is written where a person reads it. */
export const ARCHITECTURE_LABEL: Record<
  NonNullable<MachineCatalogEntry["lineage"]>["architecture"],
  string
> = {
  x86_64: "x86-64",
  arm64: "arm64",
};

/** How an operating system family is written where a person reads it. */
export const OS_LABEL: Record<MachineCatalogEntry["os"], string> = {
  linux: "Linux",
  mac_os: "macOS",
  windows: "Windows",
};
