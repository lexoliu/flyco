import { describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import type { Readiness } from "../components/Readiness";
import Home from "./Home";

/**
 * Readiness under test control: the home page's first-run gate is a
 * function of `loading`, `ready` and `error`, so the three are signals the
 * test flips rather than a control plane it has to time.
 */
const [loading, setLoading] = createSignal(true);
const [ready, setReady] = createSignal(false);
const [error, setError] = createSignal<unknown>(undefined);

vi.mock("../components/Readiness", () => ({
  useReadiness: (): Readiness => ({
    harness: () => [],
    compute: () => [],
    ready,
    loading,
    error,
    refresh: () => Promise.resolve(),
  }),
}));

function mount() {
  const history = createMemoryHistory();
  history.set({ value: "/", replace: true, scroll: false });
  const result = render(() => (
    <MemoryRouter history={history}>
      <Route path="/" component={Home} />
      <Route path="/welcome" component={() => <h1>First run</h1>} />
    </MemoryRouter>
  ));
  return { ...result, history };
}

describe("Home's first-run gate", () => {
  it("paints nothing while readiness is still being read", () => {
    setLoading(true);
    setReady(false);
    setError(undefined);
    const { queryByText, history } = mount();

    // Neither the composer nor the first run: a frame of the home page
    // followed by a redirect is exactly the glitch this guards against.
    expect(queryByText(/What should we build|Welcome back/)).not.toBeInTheDocument();
    expect(queryByText("First run")).not.toBeInTheDocument();
    expect(history.get()).toBe("/");
  });

  it("goes to the first run once readiness says nothing is linked", async () => {
    setLoading(true);
    setReady(false);
    setError(undefined);
    const { findByText } = mount();

    setLoading(false);

    expect(await findByText("First run")).toBeInTheDocument();
  });

  it("shows the home page, not the first run, when readiness could not be read", async () => {
    setLoading(false);
    setReady(false);
    setError(new Error("the control plane is unreachable"));
    const { findByText, queryByText } = mount();

    expect(await findByText(/What should we build|Welcome back/)).toBeInTheDocument();
    expect(queryByText("First run")).not.toBeInTheDocument();
  });
});
