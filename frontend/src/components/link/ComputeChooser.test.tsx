import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import ComputeChooser, { ENROLLMENT_ISSUE } from "./ComputeChooser";
import { ReadinessProvider } from "../Readiness";

vi.mock("../../api/client", async () => {
  const actual = await vi.importActual<typeof import("../../api/client")>("../../api/client");
  return {
    ...actual,
    listProviders: async () => [],
    listHarnessAccounts: async () => [],
    listCloudUsage: async () => [],
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

describe("ComputeChooser", () => {
  it("offers the four choices docs/ux.md §7 names", () => {
    const { getByText } = mount();

    for (const title of ["Azure", "AWS", "Google Cloud", "Your own machine"]) {
      expect(getByText(title)).toBeInTheDocument();
    }
  });

  it("lists a machine the user owns without offering a form for it", () => {
    // The Worker has no sockets, so there is nothing a credential form here
    // could do but strand a session. The card is a link to the issue where
    // enrollment is being designed.
    const { getByRole, getByText } = mount();

    const card = getByRole("link", { name: /Your own machine/ });
    expect(card).toHaveAttribute("href", ENROLLMENT_ISSUE);
    expect(getByText("Enroll a Linux machine you own with one command.")).toBeInTheDocument();
  });

  it("opens the bonus questions before asking for a credential", () => {
    const { getByRole, getByText } = mount();

    getByRole("button", { name: /Azure/ }).click();

    expect(getByText("New to this provider?")).toBeInTheDocument();
    expect(getByText("Are you a student?")).toBeInTheDocument();
  });
});
