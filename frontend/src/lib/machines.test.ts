import { describe, expect, it } from "vitest";
import { MACHINE_STATE_LABEL, machineChip } from "./machines";
import type { MachineState, MachineView } from "../api/client";

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
