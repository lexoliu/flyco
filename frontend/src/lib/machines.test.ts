import { describe, expect, it } from "vitest";
import {
  MACHINE_STATE_LABEL,
  autoSentence,
  capacityLabel,
  chipLabel,
  detentLabel,
  entryChipName,
  entryName,
  hasFreeGrant,
  machineChip,
  runtimeOf,
} from "./machines";
import type { MachineCatalogEntry, MachineState, MachineView } from "../api/client";

/** One machine as the API serves it, with everything overridable. */
function machine(overrides: Partial<MachineView> = {}): MachineView {
  return {
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
    ...overrides,
  };
}

describe("machineChip", () => {
  it("quotes the rate and which price it is while the machine runs", () => {
    expect(machineChip(machine())).toBe("m7i-flex.xlarge · $0.15/hr · spot");
  });

  it("says on-demand capacity by leaving the spot clause off", () => {
    expect(machineChip(machine({ spot: false }))).toBe("m7i-flex.xlarge · $0.15/hr");
  });

  it("quotes no price on hardware flyco meters nothing on", () => {
    // `$0.00/hr` on a machine the user owns would read as "this is free".
    expect(machineChip(machine({ hourly: null, spot: false }))).toBe("m7i-flex.xlarge");
  });

  it("says the machine is stopped rather than quoting a rate nobody is paying", () => {
    // A paused session's machine is deallocated: an hourly price there
    // reads as "still billing" (issue #135).
    expect(machineChip(machine({ state: "deallocated" }))).toBe("m7i-flex.xlarge · stopped");
  });

  it("says the machine was released once the session let its disk go", () => {
    expect(machineChip(machine({ state: "destroyed" }))).toBe("m7i-flex.xlarge · released");
  });

  it("says a machine still being built is starting", () => {
    expect(machineChip(machine({ state: "provisioning" }))).toBe("m7i-flex.xlarge · starting");
  });
});

describe("MACHINE_STATE_LABEL", () => {
  it("writes out every state the control plane can report", () => {
    const states: MachineState[] = ["provisioning", "running", "deallocated", "destroyed"];
    expect(states.map((state) => MACHINE_STATE_LABEL[state])).toEqual([
      "starting",
      "running",
      "stopped",
      "released",
    ]);
  });
});

/** One Azure VM entry, as the curated catalog serves it. */
function vm(overrides: Partial<MachineCatalogEntry> = {}): MachineCatalogEntry {
  return {
    provider: "azure",
    account: "1b2c3d4e-5f60-4718-8293-a4b5c6d7e8f9",
    region: "westeurope",
    machine_type: "Standard_D4s_v6",
    runtime: "vm",
    os: "linux",
    capacity: { vcpus: 4, memory_mib: 16 * 1024 },
    lineage: { architecture: "x86_64", family: "ds", generation: 6 },
    pricing: {
      kind: "metered",
      on_demand_hourly: 190_000,
      spot_hourly: 95_000,
      minimum: null,
      storage: { kind: "per_gib_hourly", rate: 110 },
    },
    ...overrides,
  };
}

/** An Azure Container Apps job at the shape the driver PR will publish. */
function container(overrides: Partial<MachineCatalogEntry> = {}): MachineCatalogEntry {
  return vm({
    machine_type: "aca-4x8",
    runtime: "container",
    capacity: { vcpus: 4, memory_mib: 8 * 1024 },
    lineage: null,
    free_grant: { vcpu_seconds_per_month: 180_000, gib_seconds_per_month: 360_000 },
    pricing: {
      kind: "metered",
      on_demand_hourly: 210_000,
      spot_hourly: null,
      minimum: null,
      storage: { kind: "per_gib_hourly", rate: 0 },
    },
    ...overrides,
  });
}

/** The one entry an enrolled host publishes: itself, as a container. */
function ownMachine(): MachineCatalogEntry {
  return vm({
    provider: "host",
    region: "build.lexo.cool",
    machine_type: "build.lexo.cool",
    runtime: "container",
    capacity: null,
    lineage: null,
    pricing: { kind: "user_owned" },
  });
}

describe("runtimeOf", () => {
  it("reads a document written before the axis existed as a virtual machine", () => {
    expect(runtimeOf({})).toBe("vm");
    expect(runtimeOf({ runtime: "container" })).toBe("container");
  });
});

describe("entryName", () => {
  it("calls a virtual machine by the type the provider sells", () => {
    expect(entryName(vm())).toBe("Standard_D4s_v6");
    expect(entryChipName(vm())).toBe("D4s_v6");
  });

  it("calls a managed container a container, because its type is a key", () => {
    // `aca-4x8` is flyco's own name for a size billed by the second, not
    // something the user picked or would recognise.
    expect(entryName(container())).toBe("Container");
    expect(entryChipName(container())).toBe("Container");
  });

  it("keeps the name of the machine the user enrolled", () => {
    // Also a container, and the one whose type is a name a person chose.
    expect(entryName(ownMachine())).toBe("build.lexo.cool");
  });
});

describe("detentLabel", () => {
  it("reads a virtual machine as its type, its size and its price", () => {
    expect(detentLabel(vm(), false)).toBe("Standard_D4s_v6 · 4 vCPU / 16 GiB · $0.19/hr");
  });

  it("reads a container as its size, and says the grant covers it", () => {
    expect(detentLabel(container(), false)).toBe(
      "Container · 4 vCPU · 8 GiB · $0.21/hr · Free this month",
    );
  });

  it("says nothing about a grant on a container the provider bills for", () => {
    expect(detentLabel(container({ free_grant: null }), false)).toBe(
      "Container · 4 vCPU · 8 GiB · $0.21/hr",
    );
  });

  it("quotes no price on the machine the user owns", () => {
    expect(detentLabel(ownMachine(), true)).toBe("build.lexo.cool · your hardware");
  });
});

describe("chipLabel", () => {
  it("drops a virtual machine's size, because its type is the answer", () => {
    expect(chipLabel(vm(), true)).toBe("D4s_v6 · $0.10/hr");
  });

  it("keeps a container's size, because `Container` alone names no machine", () => {
    expect(chipLabel(container(), true)).toBe(
      "Container · 4 vCPU · 8 GiB · $0.21/hr · Free this month",
    );
  });
});

describe("capacityLabel", () => {
  it("has nothing to say about a machine flyco has never measured", () => {
    expect(capacityLabel(ownMachine())).toBeNull();
  });
});

describe("hasFreeGrant", () => {
  it("is true only where the provider publishes an allowance", () => {
    expect(hasFreeGrant(container())).toBe(true);
    expect(hasFreeGrant(vm())).toBe(false);
    expect(hasFreeGrant(undefined)).toBe(false);
  });
});

describe("autoSentence", () => {
  it("names the grant when Auto landed on a container the provider gives away", () => {
    expect(autoSentence(container())).toBe(
      "A container your provider gives away this month, which flyco spends before it spends money.",
    );
  });

  it("still names the machine the user enrolled when that is what Auto picked", () => {
    expect(autoSentence(ownMachine())).toBe(
      "build.lexo.cool — the machine you enrolled, which flyco meters no spend on.",
    );
  });

  it("otherwise quotes the rule that decided", () => {
    expect(autoSentence(vm())).toBe(
      "The cheapest curated Linux type with at least 4 vCPU and 16 GiB.",
    );
  });
});

describe("machineChip on a container", () => {
  it("says what the machine is rather than the key its size is filed under", () => {
    const running = machine({
      spec: {
        provider: "azure",
        machine_type: "aca-4x8",
        runtime: "container",
        region: "westeurope",
        disk_gib: 64,
        spot: false,
      },
      spot: false,
      hourly: 210_000,
    });
    expect(machineChip(running)).toBe("Container · $0.21/hr");
  });

  it("keeps calling the machine the user enrolled by its own name", () => {
    const own = machine({
      spec: {
        provider: "host",
        machine_type: "build.lexo.cool",
        runtime: "container",
        region: "build.lexo.cool",
        disk_gib: 0,
        spot: false,
      },
      spot: false,
      hourly: null,
    });
    expect(machineChip(own)).toBe("build.lexo.cool");
  });
});
