import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import MachinePicker, { MachineResize } from "./MachinePicker";
import type { MachineCatalogEntry, MachineView, ProviderAccountView } from "../api/client";

const ACCOUNT = "0b4a1f2c-3d5e-4a6b-8c9d-0e1f2a3b4c5d";

const ACCOUNTS: ProviderAccountView[] = [
  { id: ACCOUNT, kind: "aws", label: "flyco — us-east-1", linked_at_unix: 1_785_974_400 },
];

function entry(machineType: string, micros: number): MachineCatalogEntry {
  return {
    provider: "aws",
    account: ACCOUNT,
    region: "us-east-1",
    machine_type: machineType,
    os: "linux",
    capacity: { vcpus: 4, memory_mib: 16 * 1024 },
    lineage: { architecture: "x86_64", family: "m7i", generation: 7 },
    pricing: {
      kind: "metered",
      on_demand_hourly: micros,
      spot_hourly: Math.round(micros * 0.4),
      minimum: null,
      storage: { kind: "per_gib_hourly", rate: 137 },
    },
  };
}

const SMALL = entry("m7i-flex.xlarge", 151_200);
const LARGE = entry("m7i.2xlarge", 403_200);

const MACHINE: MachineView = {
  id: "6f0d2c1e-8a9b-4c3d-9e2f-1a2b3c4d5e6f",
  session: "3f2b1c9d-6a4e-4d8b-9f21-7c5a0e3b8d14",
  region: "us-east-1",
  spot: true,
  state: "running",
  hourly: 151_200,
  storage_hourly: 7_040,
  created_at_unix: 1_790_000_000,
  spec: {
    provider: "aws",
    machine_type: "m7i-flex.xlarge",
    region: "us-east-1",
    disk_gib: 64,
    spot: true,
  },
};

function mount(overrides: Partial<Parameters<typeof MachineResize>[0]> = {}) {
  const onResize = vi.fn();
  const onCancel = vi.fn();
  const result = render(() => (
    <MachineResize
      catalog={[SMALL, LARGE]}
      accounts={ACCOUNTS}
      current={MACHINE}
      saving={false}
      onResize={onResize}
      onCancel={onCancel}
      {...overrides}
    />
  ));
  return { ...result, onResize, onCancel };
}

describe("MachineResize", () => {
  it("opens on the machine the session is already on", () => {
    // Not on `Choose a new type…`: the control's first reading should be
    // the truth about the session (issue #138).
    const { getByLabelText } = mount();

    expect(getByLabelText("Machine")).toHaveValue("0");
    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "m7i-flex.xlarge · 4 vCPU / 16 GiB · $0.06/hr",
    );
  });

  it("states what a resize does before it offers to do it", () => {
    const { getByText } = mount();
    expect(getByText("Restarts the machine; the disk is kept.")).toBeInTheDocument();
  });

  it("names the machine it would move to, and asks for nothing until it moves", () => {
    const { getByLabelText, getByRole, onResize } = mount();

    // The thumb has not moved, so there is no resize to ask for.
    expect(getByRole("button", { name: "Already on m7i-flex.xlarge" })).toBeDisabled();

    fireEvent.input(getByLabelText("Machine"), { target: { value: "1" } });
    const commit = getByRole("button", { name: "Resize to m7i.2xlarge" });
    expect(commit).not.toBeDisabled();

    commit.click();
    expect(onResize).toHaveBeenCalledWith("m7i.2xlarge");
  });

  it("says a resize is running rather than offering it twice", () => {
    const { getByRole } = mount({ saving: true });
    expect(getByRole("button", { name: "Resizing…" })).toBeDisabled();
  });

  it("gives the resize a way out", () => {
    const { getByRole, onCancel } = mount();
    getByRole("button", { name: "Cancel" }).click();
    expect(onCancel).toHaveBeenCalled();
  });

  it("prices the detents the way the machine is actually billed", () => {
    // The machine holds spot capacity, so the slider quotes spot prices.
    const { getByLabelText } = mount({ current: { ...MACHINE, spot: false } });
    expect(getByLabelText("Machine")).toHaveAttribute(
      "aria-valuetext",
      "m7i-flex.xlarge · 4 vCPU / 16 GiB · $0.15/hr",
    );
  });
});

describe("MachinePicker, before the catalog has been read", () => {
  function mountPicker(pending: string | undefined, catalog: MachineCatalogEntry[]) {
    return render(() => (
      <MachinePicker
        catalog={catalog}
        accounts={ACCOUNTS}
        spot={true}
        chosenKey={null}
        onChoose={vi.fn()}
        pending={pending}
      />
    ));
  }

  it("says what flyco is doing instead of showing empty controls", () => {
    // An account nobody has read offers no machine, no region and no
    // architecture — so every `Advanced` select would be empty, and the
    // empty-track sentence would tell the user to try another region that
    // is not offered either. Both are claims about an account that has not
    // been read (owner's screenshot, 2026-09-05).
    const { getByText, queryByText, queryByLabelText } = mountPicker(
      "Reading your AWS account\u2019s machines\u2026",
      [],
    );

    expect(getByText("Reading your AWS account\u2019s machines\u2026")).toBeInTheDocument();
    expect(queryByText(/offers no machine/)).not.toBeInTheDocument();
    expect(queryByLabelText("Machine")).not.toBeInTheDocument();
    for (const filter of ["Account", "Region", "Architecture", "OS"]) {
      expect(queryByText(filter)).not.toBeInTheDocument();
    }
  });

  it("states the refusal once the account has been read and offers nothing", () => {
    const { getByText } = mountPicker(undefined, []);
    expect(getByText(/offers no machine/)).toBeInTheDocument();
  });
});
