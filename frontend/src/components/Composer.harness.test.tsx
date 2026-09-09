import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import type { Readiness } from "./Readiness";
import Composer from "./Composer";

/**
 * Readiness with one linked agent, so the harness chip is in its linked
 * form: the one docs/ux.md §5 says opens a picker rather than a page.
 */
vi.mock("./Readiness", () => ({
  useReadiness: (): Readiness => ({
    harness: () => [
      {
        id: "harness-1",
        harness: "claude_code",
        label: "lexo@flyco.dev",
        linked_at_unix: 1_787_000_000,
        expires_at_unix: null,
        models: [],
        usage: [],
      },
    ],
    compute: () => [],
    ready: () => false,
    loading: () => false,
    error: () => undefined,
    refresh: () => Promise.resolve(),
  }),
}));

function mount() {
  const history = createMemoryHistory();
  history.set({ value: "/", replace: true, scroll: false });
  const result = render(() => (
    <MemoryRouter history={history}>
      <Route path="/" component={() => <Composer onSend={vi.fn(() => Promise.resolve())} />} />
      <Route path="/connect/harness" component={() => <h1>Connect flow</h1>} />
    </MemoryRouter>
  ));
  return { ...result, history };
}

describe("the composer's agent chip", () => {
  it("opens a picker of the linked agents instead of leaving the page", async () => {
    const { findByRole, getByRole, queryByText, history } = mount();

    const chip = await findByRole("button", { name: /Claude Code/ });
    chip.click();

    const picker = getByRole("dialog", { name: /Claude Code/ });
    expect(picker.textContent).toContain("Your agents");
    expect(picker.textContent).toContain("lexo@flyco.dev");
    expect(getByRole("link", { name: /Connect another agent/ }).getAttribute("href")).toBe(
      "/connect/harness",
    );
    expect(queryByText("Connect flow")).toBeNull();
    expect(history.get()).toBe("/");
  });
});
