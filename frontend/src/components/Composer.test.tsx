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
  it("asks for repositories while none is chosen", async () => {
    const { findByRole, queryByText } = mount();

    expect(
      await findByRole("button", { name: /Select repositories/ }),
    ).toBeInTheDocument();
    // No branch picker anywhere: a branch is a fact about one repository,
    // and there is no repository yet to have one.
    expect(queryByText("Default branch")).not.toBeInTheDocument();
  });

  it("names the chosen repository, and its branch inside the picker", async () => {
    // The most recent repository is preselected, which is how a returning
    // user lands on the page with one already chosen.
    rememberRepo("octocat/hello-world");
    const { findByRole } = mount();

    const chip = await findByRole("button", { name: /octocat\/hello-world/ });
    chip.click();

    // The picked row's branch is read as soon as it is known, so the picker
    // names it rather than saying "Default branch".
    expect(await findByRole("button", { name: "main" })).toBeInTheDocument();
  });
});
