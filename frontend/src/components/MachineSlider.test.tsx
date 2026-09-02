import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import MachineSlider, { detentForKey } from "./MachineSlider";
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

/** The single catalog entry an enrolled machine contributes. */
const HOST_ACCOUNT = "6c1d9e77-1c2b-4c7f-9c65-3a8b1f2d4e60";
const HOST: MachineCatalogEntry = {
  provider: "host",
  account: HOST_ACCOUNT,
  // A host is its own region and its own machine type: there is nothing
  // else to call it that a person would recognise.
  region: "mercury",
  machine_type: "mercury",
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

describe("MachineSlider", () => {
  it("opens on Auto, and says what Auto means", () => {
    const { getByLabelText, getByText } = mount([SMALL, LARGE], null);

    expect(getByLabelText("Machine")).toHaveValue("0");
    expect(
      getByText("The cheapest curated Linux type with at least 4 vCPU and 16 GiB."),
    ).toBeInTheDocument();
  });

  it("gives the detents one position each, above Auto", () => {
    const { getByLabelText, getByText } = mount([SMALL, LARGE], null);

    expect(getByLabelText("Machine")).toHaveAttribute("max", "2");
    expect(getByText("2 machines")).toBeInTheDocument();
  });

  it("marks every stop the thumb can take, Auto included", () => {
    // The dots are the reason the control is drawn rather than left to the
    // user agent: they are what says the positions can be counted.
    const { container } = mount([SMALL, LARGE], null);

    expect(container.querySelectorAll('[class*="dot"]')).toHaveLength(3);
  });

  it("reads out the machine rather than the index", () => {
    const { getByLabelText } = mount([SMALL, LARGE], LARGE_KEY);

    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "Standard_D8als_v6 · 8 vCPU / 32 GiB · $0.27/hr",
    );
  });

  it("says what Auto is rather than reading out a bare word", () => {
    const { getByLabelText } = mount([SMALL, LARGE], null);

    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "Auto. The cheapest curated Linux type with at least 4 vCPU and 16 GiB.",
    );
  });

  it("chooses the detent the thumb lands on, and Auto at the left end", () => {
    const { getByLabelText, onChoose } = mount([SMALL, LARGE], null);
    const slider = getByLabelText("Machine");

    fireEvent.input(slider, { target: { value: "1" } });
    expect(onChoose).toHaveBeenLastCalledWith(SMALL_KEY);

    fireEvent.input(slider, { target: { value: "2" } });
    expect(onChoose).toHaveBeenLastCalledWith(LARGE_KEY);

    fireEvent.input(slider, { target: { value: "0" } });
    expect(onChoose).toHaveBeenLastCalledWith(null);
  });

  it("steps one machine at a time under the arrow keys", () => {
    const fromAuto = mount([SMALL, LARGE], null);
    fireEvent.keyDown(fromAuto.getByLabelText("Machine"), { key: "ArrowRight" });
    expect(fromAuto.onChoose).toHaveBeenLastCalledWith(SMALL_KEY);

    const fromSmall = mount([SMALL, LARGE], SMALL_KEY);
    fireEvent.keyDown(fromSmall.getByLabelText("Machine"), { key: "ArrowUp" });
    expect(fromSmall.onChoose).toHaveBeenLastCalledWith(LARGE_KEY);

    fireEvent.keyDown(fromSmall.getByLabelText("Machine"), { key: "ArrowLeft" });
    expect(fromSmall.onChoose).toHaveBeenLastCalledWith(null);
  });

  it("jumps to Auto and to the largest machine with Home and End", () => {
    const { getByLabelText, onChoose } = mount([SMALL, LARGE], SMALL_KEY);

    fireEvent.keyDown(getByLabelText("Machine"), { key: "End" });
    expect(onChoose).toHaveBeenLastCalledWith(LARGE_KEY);

    fireEvent.keyDown(getByLabelText("Machine"), { key: "Home" });
    expect(onChoose).toHaveBeenLastCalledWith(null);
  });

  it("leaves keys it does not claim to the browser", () => {
    const { getByLabelText, onChoose } = mount([SMALL, LARGE], null);

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

  it("says so rather than showing an empty track when a region offers nothing", () => {
    const { getByText, queryByLabelText } = mount([], null);

    expect(queryByLabelText("Machine")).toBeNull();
    expect(getByText(/offers no machine in eastus/)).toBeInTheDocument();
  });

  it("lets Auto land on a machine the user owns when that is the only compute", () => {
    // A host arrives through the curated catalog as one ordinary entry, so
    // the slider needs no case for it: Auto resolves to it and the detent is
    // there beside it. What the line under the track must not do is quote the
    // rule about being cheapest, because hardware somebody owns has no price
    // to be cheapest at.
    const { getByLabelText, getByText } = mount([HOST], null, vi.fn(), HOST_AUTOMATIC);

    expect(getByLabelText("Machine")).toHaveValue("0");
    expect(getByLabelText("Machine")).toHaveAttribute("max", "1");
    expect(getByText("1 machine")).toBeInTheDocument();
    expect(
      getByText("mercury — the machine you enrolled, which flyco meters no spend on."),
    ).toBeInTheDocument();
    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "Auto. mercury — the machine you enrolled, which flyco meters no spend on.",
    );
  });

  it("prices a hand-picked host as the hardware it is", () => {
    const { getByLabelText } = mount([HOST], HOST_KEY, vi.fn(), HOST_AUTOMATIC);

    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "mercury · 16 vCPU / 64 GiB · your hardware",
    );
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

describe("detentForKey", () => {
  it("moves one detent per arrow, on either axis", () => {
    expect(detentForKey("ArrowRight", 1, 4)).toBe(2);
    expect(detentForKey("ArrowUp", 1, 4)).toBe(2);
    expect(detentForKey("ArrowLeft", 1, 4)).toBe(0);
    expect(detentForKey("ArrowDown", 1, 4)).toBe(0);
  });

  it("puts the ends of the track on Home and End", () => {
    expect(detentForKey("Home", 3, 4)).toBe(0);
    expect(detentForKey("End", 0, 4)).toBe(4);
  });

  it("stops at Auto and at the largest machine rather than running off", () => {
    expect(detentForKey("ArrowLeft", 0, 4)).toBe(0);
    expect(detentForKey("ArrowRight", 4, 4)).toBe(4);
  });

  it("claims nothing else, so the browser keeps its own keys", () => {
    for (const key of ["Tab", "Enter", " ", "PageUp", "a"]) {
      expect(detentForKey(key, 2, 4)).toBeNull();
    }
  });
});
