import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, waitFor } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import ComputeChooser from "./ComputeChooser";
import { ReadinessProvider } from "../Readiness";
import * as client from "../../api/client";
import type { EnrollmentToken, HostView } from "../../api/client";

const COMMAND =
  "curl -fsSL https://dev.flyco.dev/install/flycod.sh | sudo sh -s -- host enroll fh_2Qv8xLmR4pT7nWzKcYbA";

const TOKEN: EnrollmentToken = {
  id: "3f2b1c9d-6a4e-4d8b-9f21-7c5a0e3b8d14",
  token: "fh_2Qv8xLmR4pT7nWzKcYbA",
  // Ten minutes out from whatever "now" the test runs at, so the command is
  // live rather than stale the moment it renders.
  expires_at_unix: Math.floor(Date.now() / 1000) + 600,
  command: COMMAND,
};

const HOST: HostView = {
  id: "8d1a6f30-4b7c-4e21-b0f5-9c2d6a7e4b11",
  label: "mercury",
  state: "online",
  facts: {
    architecture: "arm64",
    vcpus: 12,
    memory_mib: 32 * 1024,
    disk_free_gib: 401,
    podman_version: "5.4.0",
    kernel: "6.8.0-45-generic",
    hostname: "mercury",
  },
  last_seen_unix: 1_790_000_600,
  created_at_unix: 1_790_000_000,
};

vi.mock("../../api/client", async () => {
  const actual = await vi.importActual<typeof import("../../api/client")>("../../api/client");
  return {
    ...actual,
    listProviders: async () => [],
    listHarnessAccounts: async () => [],
    listCloudUsage: async () => [],
    mintEnrollmentToken: vi.fn(async () => TOKEN),
    getEnrollment: vi.fn(async () => ({ status: "pending" as const })),
  };
});

function mount() {
  const history = createMemoryHistory();
  history.set({ value: "/", replace: true, scroll: false });
  return render(() => (
    <MemoryRouter history={history}>
      <Route
        path="/"
        component={() => (
          <ReadinessProvider enabled={() => false}>
            <ComputeChooser />
          </ReadinessProvider>
        )}
      />
    </MemoryRouter>
  ));
}

beforeEach(() => {
  vi.mocked(client.mintEnrollmentToken).mockResolvedValue(TOKEN);
  vi.mocked(client.getEnrollment).mockResolvedValue({ status: "pending" });
});

describe("ComputeChooser", () => {
  it("offers the four choices docs/ux.md §7 names", () => {
    const { getByText } = mount();

    for (const title of ["Azure", "AWS", "Google Cloud", "Your own machine"]) {
      expect(getByText(title)).toBeInTheDocument();
    }
  });

  it("opens the bonus questions before asking for a credential", () => {
    const { getByRole, getByText } = mount();

    getByRole("button", { name: /Azure/ }).click();

    expect(getByText("New to this provider?")).toBeInTheDocument();
    expect(getByText("Are you a student?")).toBeInTheDocument();
  });

  it("mints a command for a machine the user owns, and waits for it", async () => {
    // No bonus questions on the way in: there is no free credit for
    // hardware somebody already bought.
    const { getByRole, findByText, getByText, queryByText } = mount();

    getByRole("button", { name: /Your own machine/ }).click();

    expect(await findByText(COMMAND)).toBeInTheDocument();
    expect(queryByText("Are you a student?")).toBeNull();
    expect(getByRole("button", { name: "Copy command" })).toBeInTheDocument();
    expect(getByText(/needs Podman/)).toBeInTheDocument();
    expect(getByText(/expires in/)).toBeInTheDocument();
    expect(getByRole("status")).toHaveTextContent("Waiting for the machine…");
    expect(client.getEnrollment).not.toHaveBeenCalledWith("");
  });

  it("shows the machine's own compute card once it arrives", async () => {
    vi.mocked(client.getEnrollment).mockResolvedValue({ status: "enrolled", host: HOST });

    const { getByRole, findByText, getByText } = mount();
    getByRole("button", { name: /Your own machine/ }).click();

    // The first poll is a few seconds out: nobody installs a daemon faster
    // than that, and a tighter loop would be a request per keystroke of
    // waiting.
    expect(await findByText("Enrolled", {}, { timeout: 6000 })).toBeInTheDocument();
    expect(getByText("mercury")).toBeInTheDocument();
    expect(getByText(/^arm64 · 12 vCPU \/ 32 GiB$/)).toBeInTheDocument();
    expect(getByText("Online")).toBeInTheDocument();
    expect(getByText("your hardware")).toBeInTheDocument();
  });

  it("offers a new command once the old one has run out", async () => {
    vi.mocked(client.mintEnrollmentToken).mockResolvedValue({
      ...TOKEN,
      expires_at_unix: Math.floor(Date.now() / 1000) - 1,
    });

    const { getByRole, findByText } = mount();
    getByRole("button", { name: /Your own machine/ }).click();

    await waitFor(
      () => {
        expect(getByRole("button", { name: "Mint a new command" })).toBeInTheDocument();
      },
      { timeout: 3000 },
    );
    expect(await findByText(/The command expired/)).toBeInTheDocument();
  });
});
