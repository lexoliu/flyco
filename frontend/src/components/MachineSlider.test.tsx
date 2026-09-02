import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
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

const AUTOMATIC: MachineDefault = {
  choice: {
    provider_account: ACCOUNT,
    machine_type: "Standard_D4als_v6",
    region: "eastus",
    spot: true,
    disk_gib: 64,
  },
  entry: SMALL,
};

function mount(
  catalog: MachineCatalogEntry[],
  chosenKey: string | null,
  onChoose = vi.fn(),
) {
  const result = render(() => (
    <MachineSlider
      catalog={catalog}
      accounts={ACCOUNTS}
      automatic={AUTOMATIC}
      spot={false}
      chosenKey={chosenKey}
      onChoose={onChoose}
      onSpot={vi.fn()}
    />
  ));
  return { ...result, onChoose };
}

describe("MachineSlider", () => {
  it("opens on Auto, and says what Auto means", () => {
    const { getByLabelText, getByText } = mount([SMALL, LARGE], null);

    const slider = getByLabelText("Machine");
    expect(slider).toHaveValue("0");
    expect(getByText(/Auto — the cheapest Linux type/)).toBeInTheDocument();
  });

  it("gives the detents one position each, above Auto", () => {
    const { getByLabelText, getByText } = mount([SMALL, LARGE], null);

    expect(getByLabelText("Machine")).toHaveAttribute("max", "2");
    expect(getByText("2 machines")).toBeInTheDocument();
  });

  it("reads out the machine rather than the index", () => {
    const { getByLabelText } = mount([SMALL, LARGE], `${ACCOUNT}/eastus/Standard_D8als_v6`);

    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "Standard_D8als_v6 · 8 vCPU / 32 GiB · $0.27/hr",
    );
  });

  it("chooses the detent the thumb lands on, and Auto at the left end", () => {
    const { getByLabelText, onChoose } = mount([SMALL, LARGE], null);
    const slider = getByLabelText("Machine");

    fireEvent.input(slider, { target: { value: "2" } });
    expect(onChoose).toHaveBeenCalledWith(`${ACCOUNT}/eastus/Standard_D8als_v6`);

    fireEvent.input(slider, { target: { value: "0" } });
    expect(onChoose).toHaveBeenLastCalledWith(null);
  });

  it("states the minimum charge of a license-bound machine as money", () => {
    // The Mac is only reachable once the OS filter names macOS: Linux is
    // what flyco picks on its own, so it is what the slider opens on.
    const { getByLabelText, getByText } = mount(
      [SMALL, MAC],
      `${ACCOUNT}/eastus/mac2-m2.metal`,
    );

    fireEvent.change(getByLabelText("Operating system"), { target: { value: "mac_os" } });

    expect(getByText("License-bound")).toBeInTheDocument();
    expect(
      getByText("Starts a 24-hour minimum charge of $15.60 the moment it boots."),
    ).toBeInTheDocument();
  });

  it("says so rather than showing an empty track when a region offers nothing", () => {
    const { getByText, queryByLabelText } = mount([], null);

    expect(queryByLabelText("Machine")).toBeNull();
    expect(getByText(/offers no machine in eastus/)).toBeInTheDocument();
  });

  it("never mixes architectures into one set of detents", () => {
    const arm = entry("Standard_D4pls_v6", 4, 8, 96_000, {
      lineage: { architecture: "arm64", family: "dpls", generation: 6 },
    });
    const { getByLabelText, getByText } = mount([SMALL, LARGE, arm], null);

    // Three machines while the architecture filter is open…
    expect(getByText("3 machines")).toBeInTheDocument();
    // …and only the Arm one once it is not.
    fireEvent.change(getByLabelText("Architecture"), { target: { value: "arm64" } });
    expect(getByText("1 machine")).toBeInTheDocument();
  });
});
