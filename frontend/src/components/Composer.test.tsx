import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import Composer from "./Composer";
import { ReadinessProvider } from "./Readiness";
import { rememberRepo } from "../lib/localPreferences";

/**
 * The composer under the same two providers it has on the home page: a
 * router, because the missing-prerequisite chips are links, and readiness,
 * because every chip reads what is linked. The control plane behind both
 * is the in-memory one from src/test/setup.ts.
 */
function mount() {
  const history = createMemoryHistory();
  history.set({ value: "/", replace: true, scroll: false });
  return render(() => (
    <MemoryRouter history={history}>
      <Route
        path="/"
        component={() => (
          <ReadinessProvider enabled={() => true}>
            <Composer onSend={vi.fn(() => Promise.resolve())} />
          </ReadinessProvider>
        )}
      />
    </MemoryRouter>
  ));
}

describe("Composer", () => {
  it("shows no Branch chip until a repository is chosen", async () => {
    const { findByRole, queryByText } = mount();

    expect(await findByRole("button", { name: /Select repository/ })).toBeInTheDocument();
    // Not a disabled chip, nothing: a branch is a fact about one repository,
    // and there is no repository yet to have one.
    expect(queryByText("Branch")).not.toBeInTheDocument();
    expect(queryByText("Default branch")).not.toBeInTheDocument();
  });

  it("shows the Branch chip beside a chosen repository", async () => {
    // The most recent repository is preselected, which is how a returning
    // user lands on the page with one already chosen.
    rememberRepo("octocat/hello-world");
    const { findByRole } = mount();

    expect(await findByRole("button", { name: /octocat\/hello-world/ })).toBeInTheDocument();
    expect(await findByRole("button", { name: "Default branch" })).toBeInTheDocument();
  });
});
