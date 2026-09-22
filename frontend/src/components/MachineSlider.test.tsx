import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import MachineSlider from "./MachineSlider";
import type { MachineCatalogEntry, MachineDefault, ProviderAccountView } from "../api/client";

const ACCOUNT = "0b4a1f2c-3d5e-4a6b-8c9d-0e1f2a3b4c5d";

const ACCOUNTS: ProviderAccountView[] = [
  { id: ACCOUNT, kind: "azure", label: "lexo — pay-as-you-go", linked_at_unix: 1785974400 },
];

function entry(
  machineType: string,
  vcpus: number,
  memoryGib: number,
  micros: number,
  extra: Partial<MachineCatalogEntry> = {},
): MachineCatalogEntry {
  return {
    provider: "azure",
    account: ACCOUNT,
    region: "eastus",
    machine_type: machineType,
    os: "linux",
    capacity: { vcpus, memory_mib: memoryGib * 1024 },
    lineage: { architecture: "x86_64", family: "dals", generation: 6 },
    pricing: {
      kind: "metered",
      on_demand_hourly: micros,
      spot_hourly: Math.round(micros * 0.4),
      minimum: null,
      storage: { kind: "per_gib_hourly", rate: 137 },
    },
    ...extra,
  };
}

const SMALL = entry("Standard_D4als_v6", 4, 16, 137_000);
const LARGE = entry("Standard_D8als_v6", 8, 32, 274_000);

const SMALL_KEY = `${ACCOUNT}/eastus/Standard_D4als_v6`;
const LARGE_KEY = `${ACCOUNT}/eastus/Standard_D8als_v6`;

/** A Mac: another OS, another architecture, and a 24-hour floor. */
const MAC = entry("mac2-m2.metal", 8, 24, 650_000, {
  os: "mac_os",
  lineage: { architecture: "arm64", family: "mac", generation: 2 },
  pricing: {
    kind: "metered",
    on_demand_hourly: 650_000,
    spot_hourly: null,
    minimum: { hours: 24, charge: 15_600_000 },
    storage: { kind: "per_gib_hourly", rate: 137 },
  },
});

/** An Azure Container Apps job, with the grant the subscription carries. */
const CONTAINER = entry("aca-4x8", 4, 8, 210_000, {
  runtime: "container",
  lineage: null,
  free_grant: { vcpu_seconds_per_month: 180_000, gib_seconds_per_month: 360_000 },
  pricing: {
    kind: "metered",
    on_demand_hourly: 210_000,
    spot_hourly: null,
    minimum: null,
    storage: { kind: "per_gib_hourly", rate: 0 },
  },
});

const CONTAINER_KEY = `${ACCOUNT}/eastus/aca-4x8`;

const GITHUB_ACCOUNT = "8f2e4a1b-6d3c-4e5f-9a8b-7c6d5e4f3a2b";

/** A codespace: GitHub's product on a VM-shaped machine, grant-covered. */
function codespace(machineType: string, vcpus: number, memoryGib: number): MachineCatalogEntry {
  return {
    provider: "codespaces",
    account: GITHUB_ACCOUNT,
    region: "WestEurope",
    machine_type: machineType,
    runtime: "vm",
    free_grant: { vcpu_seconds_per_month: 432_000, gib_seconds_per_month: 9_999_999 },
    os: "linux",
    capacity: { vcpus, memory_mib: memoryGib * 1024 },
    lineage: { architecture: "x86_64", family: "codespaces", generation: null },
    pricing: {
      kind: "metered",
      on_demand_hourly: 36_000 * vcpus,
      spot_hourly: null,
      minimum: null,
      storage: { kind: "per_gib_hourly", rate: 25 },
    },
  };
}

const CODESPACE = codespace("standardLinux32gb", 4, 16);
const CODESPACE_LARGE = codespace("premiumLinux", 8, 32);
const CODESPACE_KEY = `${GITHUB_ACCOUNT}/WestEurope/standardLinux32gb`;

const AUTOMATIC: MachineDefault = {
  choice: {
    provider_account: ACCOUNT,
    machine_type: "Standard_D4als_v6",
    region: "eastus",
    spot: true,
    disk_gib: 64,
  },
  entry: SMALL,
  pending_accounts: [],
};

/** The single catalog entry an enrolled machine contributes. */
const HOST_ACCOUNT = "6c1d9e77-1c2b-4c7f-9c65-3a8b1f2d4e60";
const HOST: MachineCatalogEntry = {
  provider: "host",
  account: HOST_ACCOUNT,
  // A host is its own region and its own machine type: there is nothing
  // else to call it that a person would recognise.
  region: "mercury",
  machine_type: "mercury",
  // A session on an enrolled machine is a Podman container, and it is the
  // one container that keeps its own name: `mercury` is what the user
  // called it.
  runtime: "container",
  os: "linux",
  capacity: { vcpus: 16, memory_mib: 64 * 1024 },
  lineage: null,
  pricing: { kind: "user_owned" },
};

const HOST_KEY = `${HOST_ACCOUNT}/mercury/mercury`;

const HOST_AUTOMATIC: MachineDefault = {
  choice: {
    provider_account: HOST_ACCOUNT,
    machine_type: "mercury",
    region: "mercury",
    spot: false,
    disk_gib: 64,
  },
  entry: HOST,
  pending_accounts: [],
};

function mount(
  catalog: MachineCatalogEntry[],
  chosenKey: string | null,
  onChoose = vi.fn(),
  automatic: MachineDefault = AUTOMATIC,
) {
  const result = render(() => (
    <MachineSlider
      catalog={catalog}
      accounts={ACCOUNTS}
      automatic={automatic}
      spot={false}
      chosenKey={chosenKey}
      onChoose={onChoose}
      onSpot={vi.fn()}
    />
  ));
  return { ...result, onChoose };
}

/**
 * The slider as its callers mount it: `chosenKey` is the caller's state,
 * so a form click only reaches the track once the answer comes back down
 * as a prop — which this harness reproduces with a signal.
 */
function mountControlled(
  catalog: MachineCatalogEntry[],
  initial: string | null = null,
  automatic: MachineDefault = AUTOMATIC,
) {
  const onChoose = vi.fn();
  const result = render(() => {
    const [chosenKey, setChosenKey] = createSignal<string | null>(initial);
    return (
      <MachineSlider
        catalog={catalog}
        accounts={ACCOUNTS}
        automatic={automatic}
        spot={false}
        chosenKey={chosenKey()}
        onChoose={(key) => {
          onChoose(key);
          setChosenKey(key);
        }}
        onSpot={vi.fn()}
      />
    );
  });
  return { ...result, onChoose };
}

describe("MachineSlider", () => {
  it("opens on Auto, and says which machine it picked and what it costs", () => {
    const { getByRole, getByText, queryByLabelText } = mount([SMALL, LARGE], null);

    const auto = getByRole("radio", { name: "Auto" });
    expect(auto).toHaveAttribute("aria-checked", "true");
    // No track under Auto — the choice flyco keeps is the size as well as
    // the form, so the panel owes the machine, not a slider.
    expect(queryByLabelText("Machine")).toBeNull();
    expect(getByText("Standard_D4als_v6 · 4 vCPU / 16 GiB")).toBeInTheDocument();
    expect(getByText("$0.14/hr")).toBeInTheDocument();
  });

  it("lists the forms the catalog offers, Auto first", () => {
    const { getByRole, queryByRole } = mount([CONTAINER, SMALL, CODESPACE], null);

    expect(getByRole("radio", { name: "Auto" })).toBeInTheDocument();
    expect(getByRole("radio", { name: "Container" })).toBeInTheDocument();
    expect(getByRole("radio", { name: "VM" })).toBeInTheDocument();
    expect(getByRole("radio", { name: "Codespace" })).toBeInTheDocument();
    expect(queryByRole("radio", { name: "Cheapest" })).toBeNull();
  });

  it("keeps a codespace out of the VM form even though it runs on one", () => {
    // `runtime: vm` is the bargain — the disk survives a stop — while the
    // product is GitHub's: provider decides the form, not the runtime.
    const { getByLabelText, getByRole, getByText } = mountControlled([
      SMALL,
      CODESPACE,
      CODESPACE_LARGE,
    ]);

    getByRole("radio", { name: "Codespace" }).click();
    // Both codespace sizes, and only they, are on the track.
    expect(getByLabelText("Machine")).toHaveAttribute("max", "1");
    expect(getByText("2 codespaces")).toBeInTheDocument();
    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "standardLinux32gb · 4 vCPU / 16 GiB",
    );

    getByRole("radio", { name: "VM" }).click();
    expect(getByLabelText("Machine")).toHaveAttribute("max", "0");
    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "Standard_D4als_v6 · 4 vCPU / 16 GiB",
    );
  });

  it("reads a machine documented before the runtime axis as a VM", () => {
    const legacy = entry("Standard_B2s", 2, 4, 41_000);
    // Rows written before the runtime axis carry no field at all.
    delete legacy.runtime;
    const { getByRole, queryByRole } = mount([legacy], `${ACCOUNT}/eastus/Standard_B2s`);

    expect(getByRole("radio", { name: "VM" })).toHaveAttribute("aria-checked", "true");
    expect(queryByRole("radio", { name: "Container" })).toBeNull();
  });

  it("choosing a form chooses its cheapest entry the scope reaches", () => {
    const { getByRole, onChoose } = mount([CONTAINER, SMALL, LARGE], null);

    getByRole("radio", { name: "VM" }).click();
    expect(onChoose).toHaveBeenLastCalledWith(SMALL_KEY);

    getByRole("radio", { name: "Container" }).click();
    expect(onChoose).toHaveBeenLastCalledWith(CONTAINER_KEY);
  });

  it("retargets the account and region when the form lives elsewhere", () => {
    // The catalog is ordered by price: the codespace is elsewhere than the
    // Azure account the anchor opens on, and clicking the segment moves
    // the scope onto it rather than showing an empty track.
    const { getByRole, onChoose } = mount([SMALL, CODESPACE], null);

    getByRole("radio", { name: "Codespace" }).click();
    expect(onChoose).toHaveBeenLastCalledWith(CODESPACE_KEY);
  });

  it("gives the detents one position each within the chosen form", () => {
    const { getByLabelText, getByText } = mount([SMALL, LARGE, CONTAINER], SMALL_KEY);

    expect(getByLabelText("Machine")).toHaveAttribute("max", "1");
    expect(getByText("2 VMs")).toBeInTheDocument();
  });

  it("marks every stop the thumb can take on the form's track", () => {
    // The dots are the reason the control is drawn rather than left to the
    // user agent: they are what says the positions can be counted.
    const { container } = mount([SMALL, LARGE], SMALL_KEY);

    expect(container.querySelectorAll('[class*="dot"]')).toHaveLength(2);
  });

  it("reads out the machine rather than the index", () => {
    const { getByLabelText } = mount([SMALL, LARGE], LARGE_KEY);

    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "Standard_D8als_v6 · 8 vCPU / 32 GiB",
    );
  });

  it("chooses the detent the thumb lands on, and hands the choice back via Auto", () => {
    const { getByLabelText, getByRole, onChoose } = mountControlled([SMALL, LARGE], SMALL_KEY);

    fireEvent.input(getByLabelText("Machine"), { target: { value: "1" } });
    expect(onChoose).toHaveBeenLastCalledWith(LARGE_KEY);

    getByRole("radio", { name: "Auto" }).click();
    expect(onChoose).toHaveBeenLastCalledWith(null);
  });

  it("steps one machine at a time under the arrow keys", () => {
    const { getByLabelText, onChoose } = mountControlled([SMALL, LARGE], SMALL_KEY);

    fireEvent.keyDown(getByLabelText("Machine"), { key: "ArrowRight" });
    expect(onChoose).toHaveBeenLastCalledWith(LARGE_KEY);

    fireEvent.keyDown(getByLabelText("Machine"), { key: "ArrowLeft" });
    expect(onChoose).toHaveBeenLastCalledWith(SMALL_KEY);
  });

  it("jumps to the cheapest and the largest machine with Home and End", () => {
    const { getByLabelText, onChoose } = mount([SMALL, LARGE], SMALL_KEY);

    fireEvent.keyDown(getByLabelText("Machine"), { key: "End" });
    expect(onChoose).toHaveBeenLastCalledWith(LARGE_KEY);

    fireEvent.keyDown(getByLabelText("Machine"), { key: "Home" });
    expect(onChoose).toHaveBeenLastCalledWith(SMALL_KEY);
  });

  it("leaves keys it does not claim to the browser", () => {
    const { getByLabelText, onChoose } = mount([SMALL, LARGE], SMALL_KEY);

    fireEvent.keyDown(getByLabelText("Machine"), { key: "Tab" });
    expect(onChoose).not.toHaveBeenCalled();
  });

  it("states the minimum charge of a license-bound machine as money", () => {
    // The Mac is only reachable once the OS filter names macOS: Linux is
    // what flyco picks on its own, so it is what the slider opens on.
    const { container, getByLabelText, getByText } = mount(
      [SMALL, MAC],
      `${ACCOUNT}/eastus/mac2-m2.metal`,
    );

    fireEvent.change(getByLabelText("Operating system"), { target: { value: "mac_os" } });

    expect(getByText("License-bound")).toBeInTheDocument();
    expect(
      getByText("Starts a 24-hour minimum charge of $15.60 the moment it boots."),
    ).toBeInTheDocument();
    // …and the detent itself is amber, before anybody stops on it.
    expect(container.querySelectorAll('[class*="dotBound"]')).toHaveLength(1);
  });

  it("names the form when a scope offers none of it", () => {
    // A form click lands on the cheapest entry that form has, so an empty
    // track only ever comes from a filter the user set — and the sentence
    // names the form they are looking at.
    const { getByLabelText, getByRole, getByText } = mountControlled([SMALL, MAC]);

    getByRole("radio", { name: "VM" }).click();
    fireEvent.change(getByLabelText("Architecture"), { target: { value: "arm64" } });

    expect(getByText(/offers no VM in eastus/)).toBeInTheDocument();
  });

  it("lets Auto land on a machine the user owns when that is the only compute", () => {
    // A host arrives through the curated catalog as one ordinary entry —
    // of the container form, since a session on it is a Podman container.
    const { getByRole, getByText } = mount([HOST], null, vi.fn(), HOST_AUTOMATIC);

    expect(getByRole("radio", { name: "Container" })).toBeInTheDocument();
    expect(getByText("mercury · 16 vCPU / 64 GiB")).toBeInTheDocument();
    expect(getByText("your hardware")).toBeInTheDocument();
  });

  it("prices a hand-picked host as the hardware it is", () => {
    const { getByLabelText } = mount([HOST], HOST_KEY, vi.fn(), HOST_AUTOMATIC);

    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "mercury · 16 vCPU / 64 GiB",
    );
  });

  it("never mixes architectures into one set of detents", () => {
    const arm = entry("Standard_D4pls_v6", 4, 8, 96_000, {
      lineage: { architecture: "arm64", family: "dpls", generation: 6 },
    });
    const { getByLabelText, getByText } = mount([SMALL, LARGE, arm], SMALL_KEY);

    // Three VMs while the architecture filter is open…
    expect(getByText("3 VMs")).toBeInTheDocument();
    // …and only the Arm one once it is not.
    fireEvent.change(getByLabelText("Architecture"), { target: { value: "arm64" } });
    expect(getByText("1 VM")).toBeInTheDocument();
  });

  it("only offers spot where a detent can quote a spot price", () => {
    // Codespaces carry no spot price — under that form the toggle would
    // reprice nothing, so it is not on the panel to flip.
    const { getByRole, queryByRole } = mountControlled([SMALL, CODESPACE]);

    getByRole("radio", { name: "Codespace" }).click();
    expect(queryByRole("switch", { name: "Use spot capacity" })).not.toBeInTheDocument();

    getByRole("radio", { name: "VM" }).click();
    expect(getByRole("switch", { name: "Use spot capacity" })).toBeInTheDocument();
  });
});

describe("MachineSlider without an Auto segment", () => {
  /** The resize case: a machine already exists, so one is always chosen. */
  function mountFixed(catalog: MachineCatalogEntry[], chosenKey: string | null, onChoose = vi.fn()) {
    const result = render(() => (
      <MachineSlider
        catalog={catalog}
        accounts={ACCOUNTS}
        automatic={undefined}
        anchor={catalog[0]}
        allowAuto={false}
        filters={["architecture", "os"]}
        spot={false}
        chosenKey={chosenKey}
        onChoose={onChoose}
      />
    ));
    return { ...result, onChoose };
  }

  it("names the one form a resize can express, and gives its machines the whole track", () => {
    const { getByLabelText, getByRole, getByText, queryByRole } = mountFixed(
      [SMALL, LARGE],
      SMALL_KEY,
    );

    // Two VMs, two positions: 0 and 1, and no segment for flyco to choose.
    expect(getByLabelText("Machine")).toHaveAttribute("max", "1");
    expect(getByLabelText("Machine")).toHaveValue("0");
    expect(getByText("Cheapest")).toBeInTheDocument();
    expect(getByRole("radio", { name: "VM" })).toHaveAttribute("aria-checked", "true");
    expect(queryByRole("radio", { name: "Auto" })).toBeNull();
  });

  it("opens on the machine it was given, not on the cheapest one", () => {
    const { getByLabelText } = mountFixed([SMALL, LARGE], LARGE_KEY);
    expect(getByLabelText("Machine")).toHaveValue("1");
    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "Standard_D8als_v6 · 8 vCPU / 32 GiB",
    );
  });

  it("chooses the detent the thumb lands on, and never hands the choice back", () => {
    const { getByLabelText, onChoose } = mountFixed([SMALL, LARGE], LARGE_KEY);

    fireEvent.input(getByLabelText("Machine"), { target: { value: "0" } });
    expect(onChoose).toHaveBeenCalledWith(SMALL_KEY);
    expect(onChoose).not.toHaveBeenCalledWith(null);
  });

  it("moves the choice when a filter drops the machine it was on", () => {
    // Otherwise the reading above the thumb and the machine the caller
    // holds would be two different machines.
    const onChoose = vi.fn();
    mountFixed([SMALL, LARGE], `${ACCOUNT}/eastus/Standard_D64als_v6`, onChoose);
    expect(onChoose).toHaveBeenCalledWith(SMALL_KEY);
  });

  it("offers only the dimensions the caller can act on", () => {
    // A resize carries a machine type and nothing else, so account, region
    // and capacity mode are not on the panel to be changed.
    const { getByText, queryByRole, queryByText } = mountFixed([SMALL, LARGE], SMALL_KEY);
    getByText("Advanced").click();

    expect(getByText("Operating system")).toBeInTheDocument();
    expect(getByText("Architecture")).toBeInTheDocument();
    expect(queryByText("Account")).not.toBeInTheDocument();
    expect(queryByText("Region")).not.toBeInTheDocument();
    expect(queryByRole("switch", { name: "Use spot capacity" })).not.toBeInTheDocument();
  });

  it("still warns about a license-bound machine before it is committed to", () => {
    const { getByLabelText, getByText } = mountFixed([MAC], `${ACCOUNT}/eastus/mac2-m2.metal`);

    // The Mac is the only machine its OS offers, so the track opens on it.
    expect(getByLabelText("Machine")).toHaveValue("0");
    expect(getByText("License-bound")).toBeInTheDocument();
    expect(
      getByText("Starts a 24-hour minimum charge of $15.60 the moment it boots."),
    ).toBeInTheDocument();
  });
});

describe("MachineSlider on a container", () => {
  it("reads a container detent by its size and says the grant covers it", () => {
    // `aca-4x8` is flyco's key for a size billed by the second; what the
    // user is choosing between is 4 vCPU and 8 GiB at $0.21/hr, free until
    // the subscription's monthly allowance runs out.
    const { getByLabelText } = mount([CONTAINER, LARGE], CONTAINER_KEY);

    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "Container · 4 vCPU · 8 GiB",
    );
  });

  it("keeps a container's sizes off the VM track rather than hiding VMs behind them", () => {
    // A session that needs a disk which survives a stop is not served by
    // an execution whose filesystem ends with it — so the two forms are
    // different segments, and neither's track carries the other's detents.
    const { getByLabelText, getByText } = mount([CONTAINER, SMALL], SMALL_KEY);

    expect(getByLabelText("Machine")).toHaveAttribute("max", "0");
    expect(getByText("1 VM")).toBeInTheDocument();
  });
});
