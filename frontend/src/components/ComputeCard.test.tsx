import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import ComputeCard from "./ComputeCard";
import { ApiProblem } from "../api/problem";
import type { CloudUsageRow, MachineDefault, ProviderAccountView } from "../api/client";

const DEFAULT_MACHINE_PATH = "/v1/machines/default";

const AWS: ProviderAccountView = {
  id: "0f6c2a4e-8b13-4c5d-9e7f-2a1b3c4d5e6f",
  kind: "aws",
  label: "AWS",
  linked_at_unix: 1_789_991_000,
};

/**
 * A machine the user owns, as `GET /v1/providers` lists it: a provider
 * account like any other, which is how it reaches this card before the
 * host's own facts have arrived to give it a card of its own.
 */
const HOST: ProviderAccountView = {
  id: "6c1d9e77-1c2b-4c7f-9c65-3a8b1f2d4e60",
  kind: "host",
  label: "mercury",
  linked_at_unix: 1_789_991_000,
  host_id: "8d1a6f30-4b7c-4e21-b0f5-9c2d6a7e4b11",
};

const DEFAULT: MachineDefault = {
  choice: {
    machine_type: "t3.large",
    provider_account: AWS.id,
    region: "us-east-1",
    spot: true,
  },
  entry: {
    provider: "aws",
    account: AWS.id,
    region: "us-east-1",
    machine_type: "t3.large",
    os: "linux",
    capacity: { vcpus: 2, memory_mib: 8 * 1024 },
    lineage: { architecture: "x86_64", family: "t3", generation: 3 },
    pricing: {
      kind: "metered",
      on_demand_hourly: 83_200,
      spot_hourly: 25_000,
      minimum: null,
      storage: { kind: "per_gib_hourly", rate: 137 },
    },
  },
  pending_accounts: [],
};

/** A meter that is running and has read nothing yet. */
const UNBILLED: CloudUsageRow = {
  account: AWS.id,
  provider: "aws",
  period_start_unix: 1_790_000_000,
  period_end_unix: 1_792_592_000,
  spent: 0,
  remaining_credit: null,
};

function machineResponse(): Response {
  return new Response(JSON.stringify(DEFAULT), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

/** The control plane refusing to pick, as `flyco_api::machines::automatic` does. */
function noMachineResponse(): Response {
  return new Response(
    JSON.stringify({
      type: "https://flyco.dev/problems/no-deployable-linux-machine",
      title: "No deployable Linux machine",
      status: 422,
      detail: "No linked account offers a Linux type big enough to choose on its own.",
    }),
    { status: 422, headers: { "content-type": "application/problem+json" } },
  );
}

/**
 * The control plane saying it has not read this account yet, as
 * `flyco_api::machines::automatic` does while a refresh is on the queue.
 */
function notReadyResponse(): Response {
  return new Response(
    JSON.stringify({
      type: "https://flyco.dev/problems/catalog-not-ready",
      title: "Conflict",
      status: 409,
      detail: "flyco is still reading what your linked accounts can deploy.",
    }),
    { status: 409, headers: { "content-type": "application/problem+json" } },
  );
}

function answerMachineWith(answer: () => Response): void {
  const base = vi.mocked(fetch).getMockImplementation();
  vi.mocked(fetch).mockImplementation((input, init) => {
    const url = new URL(String(input instanceof Request ? input.url : input));
    if (url.pathname === DEFAULT_MACHINE_PATH) {
      return Promise.resolve(answer());
    }
    if (base === undefined) {
      throw new Error("the shared fetch stand-in from src/test/setup.ts is not installed");
    }
    return base(input, init);
  });
}

function mount(account: ProviderAccountView, usage: CloudUsageRow | undefined) {
  return render(() => (
    <ComputeCard account={account} usage={usage} spot={true} onSpot={vi.fn()} />
  ));
}

/**
 * The card with an unlink on offer, under a router: a refused unlink links
 * to the sessions that have to be archived first.
 */
function mountUnlinkable(onUnlink: () => Promise<void>) {
  const history = createMemoryHistory();
  history.set({ value: "/settings/compute", replace: true, scroll: false });
  return render(() => (
    <MemoryRouter history={history}>
      <Route
        path="/settings/compute"
        component={() => (
          <ComputeCard
            account={AWS}
            usage={undefined}
            spot={true}
            onSpot={vi.fn()}
            onUnlink={onUnlink}
          />
        )}
      />
    </MemoryRouter>
  ));
}

describe("ComputeCard", () => {
  it("names the default machine and says a cloud meter has read nothing yet", async () => {
    answerMachineWith(machineResponse);
    const { findByText, getByText, queryByText } = mount(AWS, undefined);

    expect(await findByText("t3.large")).toBeInTheDocument();
    // A cloud account with no bill yet is not an account flyco cannot
    // meter; the meter is running and the period is young.
    expect(getByText("No metered spend yet this period.")).toBeInTheDocument();
    expect(queryByText(/meters no spend/)).not.toBeInTheDocument();
  });

  it("states what the provider gives away each month, where it gives any", async () => {
    // A codespace's included core-hours are why a new user pays nothing at
    // all for weeks; a page about what compute costs has to say so.
    answerMachineWith(() =>
      new Response(
        JSON.stringify({
          ...DEFAULT,
          entry: {
            ...DEFAULT.entry,
            free_grant: { vcpu_seconds_per_month: 432_000, gib_seconds_per_month: 0 },
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
    const { findByText } = mount(AWS, undefined);

    expect(await findByText("120 vCPU-hours")).toBeInTheDocument();
  });

  it("reads a zero row the same way as no row", async () => {
    answerMachineWith(machineResponse);
    const { findByText, queryByText } = mount(AWS, UNBILLED);

    expect(await findByText("No metered spend yet this period.")).toBeInTheDocument();
    expect(queryByText("$0.00")).not.toBeInTheDocument();
  });

  it("keeps a machine the user owns priced as their hardware", async () => {
    answerMachineWith(machineResponse);
    const { findByText, queryByText } = mount(HOST, undefined);

    expect(await findByText("your hardware")).toBeInTheDocument();
    expect(queryByText("No metered spend yet this period.")).not.toBeInTheDocument();
  });

  it("says in words when the account has no machine flyco would pick", async () => {
    answerMachineWith(noMachineResponse);
    const { findByText } = mount(AWS, undefined);

    expect(
      await findByText("No deployable Linux machine in this account yet."),
    ).toBeInTheDocument();
  });

  it("says what it is doing while the account has not been read yet", async () => {
    // The two are not the same fact, and the card must not tell a user who
    // linked an account seconds ago that it can deploy nothing: reading a
    // cloud account's machines happens on the provisioning queue and lands
    // seconds later.
    answerMachineWith(notReadyResponse);
    const { findByText, queryByText } = mount(AWS, undefined);

    expect(await findByText("Reading your AWS account\u2019s machines\u2026")).toBeInTheDocument();
    expect(
      queryByText("No deployable Linux machine in this account yet."),
    ).not.toBeInTheDocument();
  });

  it("asks before it forgets a credential", async () => {
    // `Unlink` used to fire the DELETE on the first click (issue #139).
    answerMachineWith(machineResponse);
    const onUnlink = vi.fn(() => Promise.resolve());
    const { getByRole, findByRole } = mountUnlinkable(onUnlink);

    getByRole("button", { name: "Unlink" }).click();
    const dialog = await findByRole("alertdialog");

    expect(dialog).toHaveTextContent("Unlink AWS?");
    expect(dialog).toHaveTextContent("you would paste it again to link it back");
    expect(onUnlink).not.toHaveBeenCalled();

    getByRole("button", { name: "Unlink" }).click();
    expect(onUnlink).toHaveBeenCalled();
  });

  it("says what has to happen first when the account is still in use", async () => {
    answerMachineWith(machineResponse);
    const refusal = new ApiProblem({
      type: "https://flyco.dev/problems/provider-in-use",
      title: "Conflict",
      status: 409,
      detail: "2 session(s) still run on this account; archive them before unlinking",
      active_sessions: 2,
    });
    const onUnlink = vi.fn(() => Promise.reject(refusal));
    const { getByRole, findByText, queryByRole } = mountUnlinkable(onUnlink);

    getByRole("button", { name: "Unlink" }).click();
    getByRole("button", { name: "Unlink" }).click();

    // Counted from the `active_sessions` member the refusal now carries
    // (issue #152), not from the sentence it also states it in.
    expect(await findByText(/2 sessions are still running there/)).toBeInTheDocument();
    // Nothing left to press: the sessions have to be archived first, and
    // the way to them is on the dialog.
    expect(queryByRole("button", { name: "Unlink" })).toBeNull();
    expect(getByRole("link", { name: "Go to Sessions" })).toHaveAttribute("href", "/");
  });
});
