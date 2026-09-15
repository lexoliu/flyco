/**
 * Reading a curated catalog entry out loud.
 *
 * The backend curates the catalog (`flyco_core::catalog`) and stamps each
 * entry with the family, generation and architecture its provider published,
 * so nothing here parses a machine type name. What is left is presentation:
 * the four facts the slider's label states, and the one sentence a
 * license-bound type has to say before anybody commits to it.
 */
import type {
  CloudProviderKind,
  MachineCatalogEntry,
  MachineState,
  MachineView,
  Runtime,
} from "../api/client";
import { ApiProblem } from "../api/problem";
import { formatUsd } from "./money";
import { PROVIDER_LABEL } from "./providers";

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

/**
 * The machine type as the composer chip reads it.
 *
 * Azure's `Standard_` prefix says nothing on a chip that already names the
 * provider, and it is the part that pushed the chip row onto a second line.
 */
export function shortMachineType(machineType: string): string {
  return machineType.replace(/^Standard_/u, "");
}

/** What each runtime is called where a person reads it. */
export const RUNTIME_LABEL: Record<Runtime, string> = {
  vm: "VM",
  container: "Container",
};

/**
 * What kind of machine this is, with the default the API applies.
 *
 * `runtime` is optional on the wire because a document written before the
 * axis existed describes a virtual machine and reads back as one. Resolving
 * it here means nothing downstream has to remember that.
 */
export function runtimeOf(entry: { runtime?: Runtime | null }): Runtime {
  return entry.runtime ?? "vm";
}

/**
 * The three products a session can run on, as the user picks between them.
 *
 * They differ in the bargain, not the size: a container's filesystem ends
 * when the run does, a virtual machine's disk survives a stop, and a
 * codespace is GitHub's own product — bought in core-hours against a
 * monthly allowance rather than from a cloud account. The picker offers
 * them as a choice of their own, before the slider's sizes (docs/ux.md
 * §7.7).
 */
export type MachineForm = "container" | "vm" | "codespace";

/** The order the form picker lists the forms in. */
export const MACHINE_FORMS: readonly MachineForm[] = ["container", "vm", "codespace"];

/** What each form is called where a person reads it. */
export const FORM_LABEL: Record<MachineForm, string> = {
  container: "Container",
  vm: "VM",
  codespace: "Codespace",
};

/** The same names as nouns, for `This account offers no …`. */
export const FORM_NOUN: Record<MachineForm, string> = {
  container: "container",
  vm: "VM",
  codespace: "codespace",
};

/** …and as plurals, for `3 containers` under the track's right end. */
export const FORM_PLURAL: Record<MachineForm, string> = {
  container: "containers",
  vm: "VMs",
  codespace: "codespaces",
};

/** One line on what a form is, for the segment that offers it. */
export const FORM_TITLE: Record<MachineForm, string> = {
  container: "Runs the session without a disk to keep",
  vm: "A virtual machine whose disk survives a stop",
  codespace: "A GitHub codespace — free hours every month",
};

/**
 * Which product an entry is.
 *
 * The provider is read before the runtime: a codespace is provisioned on a
 * VM-shaped machine — `runtime` says `vm` because its disk survives a stop
 * — but it is a different product from a cloud VM, so the provider, not
 * the runtime, is what tells it apart.
 */
export function formOf(entry: {
  provider: CloudProviderKind;
  runtime?: Runtime | null;
}): MachineForm {
  if (entry.provider === "codespaces") {
    return "codespace";
  }
  return runtimeOf(entry) === "container" ? "container" : "vm";
}

/**
 * Whether this machine has a name worth showing, or is described by its
 * size.
 *
 * A virtual machine is its type: `D4s_v6` is a thing the provider sells,
 * it is what the user picked, and it is what they will look for again. A
 * managed container's type name (`aca-4x8`) is flyco's own key for a size
 * the provider bills by the second — nobody typed it and nothing outside
 * the driver reads it — so a container row says what it *is* and lets the
 * size beside it say how big.
 *
 * A machine the user enrolled is the exception, and it is a container too:
 * its type is the machine's own hostname, which is the name they gave it
 * and the only thing that tells two of their machines apart.
 */
function namesItself(provider: CloudProviderKind, runtime: Runtime): boolean {
  return runtime === "vm" || provider === "host";
}

/**
 * What a catalog entry is called where there is room for its whole name:
 * the slider's label, which is the one place the provider's own spelling is
 * worth reading in full (docs/ux.md §7.7).
 */
export function entryName(entry: MachineCatalogEntry): string {
  const runtime = runtimeOf(entry);
  return namesItself(entry.provider, runtime)
    ? entry.machine_type
    : RUNTIME_LABEL[runtime];
}

/** The same name on a chip, where `Standard_` is the part that wraps. */
export function entryChipName(entry: MachineCatalogEntry): string {
  return shortMachineType(entryName(entry));
}

/**
 * How big a machine is, as the parts a label joins.
 *
 * Two parts rather than one string, because the two runtimes spend them
 * differently: a VM row is anchored by its type name and carries the size
 * as one parenthetical fact (`4 vCPU / 16 GiB`), while a container row has
 * no name to anchor it and the size *is* the identity, so each half stands
 * on its own (`Container · 4 vCPU · 8 GiB`).
 *
 * Empty for a machine flyco has not measured — hardware the user enrolled,
 * whose size flyco does not learn until a daemon runs on it.
 */
export function capacityParts(entry: MachineCatalogEntry): string[] {
  const capacity = entry.capacity;
  if (capacity === null || capacity === undefined) {
    return [];
  }
  const gib = Math.round(capacity.memory_mib / MIB_PER_GIB);
  return [`${capacity.vcpus} vCPU`, `${gib} GiB`];
}

/** `4 vCPU / 16 GiB`, or nothing for a machine flyco has not measured. */
export function capacityLabel(entry: MachineCatalogEntry): string | null {
  const parts = capacityParts(entry);
  return parts.length === 0 ? null : parts.join(" / ");
}

/**
 * What a provider gives away every month, where it gives anything away.
 *
 * The grant is per subscription and per calendar month, and how much of it
 * this account has already spent is a number only the provider's own meter
 * has — so the label says the machine is covered, not how much of the cover
 * is left.
 */
export const FREE_GRANT_LABEL = "Free this month";

/** Whether the provider covers this entry out of a monthly allowance. */
export function hasFreeGrant(entry: MachineCatalogEntry | undefined): boolean {
  return entry?.free_grant !== null && entry?.free_grant !== undefined;
}

/**
 * The slider label of docs/ux.md §7.7:
 * `Standard_D4s_v6 · 4 vCPU / 16 GiB · $0.19/hr`, and for a container
 * `Container · 4 vCPU · 8 GiB · $0.21/hr · Free this month`.
 */
export function detentLabel(entry: MachineCatalogEntry, spot: boolean): string {
  const size = namesItself(entry.provider, runtimeOf(entry))
    ? [capacityLabel(entry)].filter((part) => part !== null)
    : capacityParts(entry);
  return [entryName(entry), ...size, hourlyLabel(entry, spot), ...grantParts(entry)].join(" · ");
}

/** ` · Free this month`, on the entries that earn it and nothing else. */
function grantParts(entry: MachineCatalogEntry): string[] {
  return hasFreeGrant(entry) ? [FREE_GRANT_LABEL] : [];
}

/**
 * The same entry, sized for the composer's compute chip.
 *
 * A VM drops its size — the type name is what the user picked, and this is
 * the chip that gives way when the row runs out of room. A container keeps
 * it, because `Container · $0.21/hr` would be a chip that says nothing
 * about the machine it names.
 */
export function chipLabel(entry: MachineCatalogEntry, spot: boolean): string {
  const size = namesItself(entry.provider, runtimeOf(entry)) ? [] : capacityParts(entry);
  return [entryChipName(entry), ...size, hourlyLabel(entry, spot), ...grantParts(entry)].join(
    " · ",
  );
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

/** Whether an entry is hardware the user already owns and already pays for. */
export function isUserOwned(entry: MachineCatalogEntry | undefined): boolean {
  return entry?.pricing.kind === "user_owned";
}

/**
 * What the leftmost detent picked, in one sentence.
 *
 * The rule docs/ux.md §7.7 states — the cheapest curated Linux type with at
 * least 4 vCPU and 16 GiB — describes a choice made across prices. A machine
 * the user enrolled has no price to be cheapest at, so when that is what
 * `Auto` resolved to the sentence names the machine rather than quoting a
 * rule that did not decide anything.
 *
 * A container the provider covers out of a monthly grant wins over both,
 * because that allowance expires unspent at the end of the month and the
 * machine at home does not — so the sentence names the grant rather than a
 * price nobody is paying.
 *
 * It follows the word `Auto` where the slider speaks it, so it opens as a
 * sentence of its own rather than as a clause.
 */
export function autoSentence(entry: MachineCatalogEntry | undefined): string {
  if (entry !== undefined && hasFreeGrant(entry)) {
    return "A container your provider gives away this month, which flyco spends before it spends money.";
  }
  return isUserOwned(entry) && entry !== undefined
    ? `${entry.machine_type} — the machine you enrolled, which flyco meters no spend on.`
    : "The cheapest curated Linux type with at least 4 vCPU and 16 GiB.";
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

/**
 * How a machine's lifecycle state is written where a person reads it.
 *
 * The wire words are the control plane's: `deallocated` is a provider's
 * term for a machine that is off but keeps its disk, and `destroyed` is one
 * for a machine whose disk has been let go. Neither is what a user calls
 * it, and both appear on screen — in the header's chip and on the drawer's
 * machine tab — so they are written out in one place.
 */
export const MACHINE_STATE_LABEL: Record<MachineState, string> = {
  provisioning: "starting",
  running: "running",
  deallocated: "stopped",
  destroyed: "released",
};

/**
 * The header's machine chip (docs/ux.md §9.1):
 * `m7i-flex.xlarge · $0.15/hr · spot`.
 *
 * The hourly rate is what a *running* machine costs, so it is quoted only
 * while one is running: a failed, archived or paused session whose machine
 * is stopped or released reads `m7i-flex.xlarge · stopped`, because a price
 * on a machine that is not there is a bill the user is not being sent
 * (issue #135). `spot` goes with the rate for the same reason — it says
 * which of two prices this one is.
 *
 * A running machine flyco meters nothing on (hardware the user enrolled)
 * quotes no price either: `$0.00/hr` would read as "this is free", which is
 * a different claim.
 *
 * A managed container reads `Container · $0.21/hr`, for the reason
 * {@link entryName} gives: `aca-4x8` is flyco's key for a size, not a name
 * anybody would recognise in a header. The size is not repeated here — the
 * header has one line and the drawer's machine tab has the whole entry.
 */
export function machineChip(machine: MachineView): string {
  const parts = [
    namesItself(machine.spec.provider, runtimeOf(machine.spec))
      ? shortMachineType(machine.spec.machine_type)
      : RUNTIME_LABEL.container,
  ];
  if (machine.state !== "running") {
    parts.push(MACHINE_STATE_LABEL[machine.state]);
    return parts.join(" · ");
  }
  if (machine.hourly !== null && machine.hourly !== undefined) {
    parts.push(`${formatUsd(machine.hourly)}/hr`);
  }
  if (machine.spot) {
    parts.push("spot");
  }
  return parts.join(" · ");
}

/**
 * How often a catalog that is still being read is asked about again.
 *
 * Reading a cloud account's machines is thousands of machine types, their
 * quotas and their published prices, and it happens on the control plane's
 * provisioning queue rather than in the request that asked for it — so the
 * answer arrives seconds after an account is linked. Five seconds is often
 * enough that the wait reads as a wait, and rare enough that a screen left
 * open is not asking every frame.
 */
export const CATALOG_POLL_SECONDS = 5;

/**
 * Whether a refusal means "flyco has not finished reading this yet".
 *
 * The one refusal that is not a problem with the account: it ends by itself,
 * so the screen says what is happening and asks again rather than telling
 * the user to act on something that is about to change.
 */
export function catalogNotReady(error: unknown): boolean {
  return error instanceof ApiProblem && error.type.endsWith("/catalog-not-ready");
}

/**
 * What a screen says while an account's machines are still being read.
 *
 * The vendor is named where exactly one account is waiting, because that is
 * the account the user just linked and is looking at. Where several are,
 * naming them all would be a list nobody needs to read to understand that
 * the waiting is flyco's rather than theirs.
 */
export function readingMachines(kinds: readonly CloudProviderKind[]): string {
  const named = [...new Set(kinds)];
  const only = named.length === 1 ? named[0] : undefined;
  return only === undefined
    ? "Reading your linked accounts’ machines…"
    : `Reading your ${PROVIDER_LABEL[only]} account’s machines…`;
}

/**
 * The same wait as [`readingMachines`], sized for a chip: `Reading Azure…`.
 *
 * The chip has room for a few words and the picker under it carries the
 * whole sentence, so the chip names only what is being read.
 */
export function readingMachinesShort(kinds: readonly CloudProviderKind[]): string {
  const named = [...new Set(kinds)];
  const only = named.length === 1 ? named[0] : undefined;
  return only === undefined ? "Reading accounts…" : `Reading ${PROVIDER_LABEL[only]}…`;
}
