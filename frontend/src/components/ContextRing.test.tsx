import { describe, expect, it, vi } from "vitest";
import { render, within } from "@solidjs/testing-library";
import ContextRing, { type ContextRingProps } from "./ContextRing";
import type { ContextUsage, ContextWindow, UsageWindow } from "../api/wire";

/** A window an eighth full, as a `context_usage` answer reports it. */
const CONTEXT: ContextWindow = { used_tokens: 16000, size_tokens: 200000 };

/** 2026-09-09T21:00:00Z in milliseconds, the instant the panel's clock shows. */
const NOW = 1788987600000;

function usage(overrides: Partial<ContextUsage> = {}): ContextUsage {
  return {
    model: "claude-sonnet-4-6",
    window: CONTEXT,
    auto_compact: 160000,
    categories: [
      { name: "System prompt", tokens: 3200, deferred: false },
      { name: "MCP tools", tokens: 4000, deferred: true },
      { name: "Messages", tokens: 8800, deferred: false },
      // A remainder the harness lists as a category; it must never draw.
      { name: "Free space", tokens: 184000, deferred: false },
    ],
    mcp_tools: [{ name: "mcp__flyco__budget_status", tokens: 130, deferred: false }],
    memory_files: [],
    agents: [],
    skills: [],
    ...overrides,
  };
}

function window(overrides: Partial<UsageWindow> = {}): UsageWindow {
  return {
    label: "5-hour",
    used_percent: 26,
    resets_at_unix: NOW / 1000 + 2 * 3600 + 10 * 60,
    window_minutes: 300,
    ...overrides,
  };
}

function mount(overrides: Partial<ContextRingProps> = {}) {
  const props: ContextRingProps = {
    context: CONTEXT,
    usage: usage(),
    windows: [
      window(),
      window({
        label: "Weekly",
        used_percent: 70,
        resets_at_unix: NOW / 1000 + 6 * 86400,
        window_minutes: 10080,
      }),
    ],
    now: NOW,
    machineUp: true,
    onBreakdown: vi.fn(),
    session: null,
    ...overrides,
  };
  return { props, ...render(() => <ContextRing {...props} />) };
}

describe("ContextRing", () => {
  it("is the ring, and the ring is the door", () => {
    const { getByRole } = mount();

    const trigger = getByRole("button", { name: /Context/ });
    expect(trigger.textContent).toContain("16k / 200k");
    expect(trigger.getAttribute("aria-haspopup")).toBe("dialog");
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
  });

  it("opens on the ring and shows the window, the threshold, and the plan", () => {
    const { getByRole } = mount();

    getByRole("button", { name: /Context/ }).click();

    const panel = getByRole("dialog");
    expect(panel.getAttribute("aria-label")).toBe("Context and plan usage");
    expect(within(panel).getByText("Context window")).toBeInTheDocument();
    expect(within(panel).getByText(/16k \/ 200k/)).toBeInTheDocument();
    // One segment per category, and the remainder row never drew one.
    const track = within(panel).getByLabelText("Context window", {
      selector: '[role="progressbar"]',
    });
    expect(track.children).toHaveLength(3);
    expect(track.getAttribute("aria-valuenow")).toBe("8");
    expect(within(panel).getByText("Compacts automatically at 80%")).toBeInTheDocument();
    expect(within(panel).getByText("Plan usage")).toBeInTheDocument();
    expect(within(panel).getByText("5-hour")).toBeInTheDocument();
    expect(within(panel).getByText(/Resets in 2h 10m/)).toBeInTheDocument();
    expect(within(panel).getByText("Weekly")).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: /Refresh the breakdown/ })).toBeInTheDocument();
  });

  it("shows the answered breakdown inside the panel, not as a card", () => {
    const { getByRole } = mount();

    getByRole("button", { name: /Context/ }).click();

    const panel = getByRole("dialog");
    // One row per category — the segmented bar's legend — the remainder
    // included, because it was a category the harness reported.
    expect(within(panel).getByText("System prompt")).toBeInTheDocument();
    expect(within(panel).getByText("Free space")).toBeInTheDocument();
    expect(within(panel).getByText("deferred")).toBeInTheDocument();
    // Detail lists stay behind disclosures, carrying their totals.
    const detail = panel.querySelector("details");
    expect(detail).not.toBeNull();
    expect(detail?.textContent).toContain("MCP tools");
    expect(detail?.textContent).toContain("mcp__flyco__budget_status");
    // And who said so.
    expect(panel.textContent).toContain("claude-sonnet-4-6");
  });

  it("asks for the breakdown from inside the panel, and stays open to take the answer", () => {
    const { props, getByRole } = mount({ usage: null });

    getByRole("button", { name: /Context/ }).click();
    getByRole("button", { name: /See the detailed breakdown/ }).click();

    expect(props.onBreakdown).toHaveBeenCalledOnce();
    // The panel stays open and says it asked — the answer lands in it.
    const panel = getByRole("dialog");
    expect(within(panel).getByText("Asking the machine…")).toBeInTheDocument();
    expect(
      within(panel).getByRole("button", { name: "Asking the machine…" }),
    ).toBeDisabled();
  });

  it("draws the breakdown a stopped session already has, and offers no wake", () => {
    // The agent reports the breakdown at the end of every turn, so a
    // session whose machine was suspended still knows where its context
    // went. Offering to start a machine for an answer that is on the
    // screen is what this panel used to do.
    const { getByRole, queryByRole, getByText } = mount({ machineUp: false });

    getByRole("button", { name: /Context/ }).click();
    expect(getByText("Compacts automatically at 80%")).toBeInTheDocument();
    expect(queryByRole("button", { name: /breakdown/i })).toBeNull();
  });

  it("says the ask went unanswered rather than silently timing out", async () => {
    vi.useFakeTimers();
    try {
      const { props, getByRole, getByText } = mount({ usage: null });

      getByRole("button", { name: /Context/ }).click();
      getByRole("button", { name: /See the detailed breakdown/ }).click();
      expect(props.onBreakdown).toHaveBeenCalledOnce();

      await vi.advanceTimersByTimeAsync(16_000);
      expect(getByText(/No answer/)).toBeInTheDocument();
      expect(
        getByRole("button", { name: /See the detailed breakdown/ }),
      ).toBeEnabled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("names the empty state rather than hiding the plan section", () => {
    const { getByRole, getByText } = mount({ windows: [] });

    getByRole("button", { name: /Context/ }).click();
    expect(getByText("Plan usage")).toBeInTheDocument();
    expect(getByText("No plan limits")).toBeInTheDocument();
  });

  it("never stands a window that has already turned over in for the context", () => {
    // A page left open across a reset: the window that just emptied is
    // still the fullest one, and painting the ring red from it would say
    // the plan is spent when it is not (#381).
    const { getByRole, queryByRole } = mount({
      context: null,
      windows: [
        window({ used_percent: 100, resets_at_unix: NOW / 1000 - 60 }),
        window({
          label: "Weekly",
          used_percent: 12,
          resets_at_unix: NOW / 1000 + 6 * 86400,
          window_minutes: 10080,
        }),
      ],
    });

    expect(queryByRole("button", { name: /100%/ })).toBeNull();
    expect(getByRole("button", { name: /Plan/ }).textContent).toContain("12%");
  });

  it("draws the plain fill, not segments, before any breakdown has been asked for", () => {
    const { getByRole, queryByText } = mount({ usage: null });

    getByRole("button", { name: /Context/ }).click();

    const panel = getByRole("dialog");
    const track = within(panel).getByLabelText("Context window", {
      selector: '[role="progressbar"]',
    });
    expect(track.children).toHaveLength(1);
    expect(queryByText(/Compacts automatically/)).toBeNull();
    // The way to the list is still there — asking is how the answer arrives.
    expect(getByRole("button", { name: /See the detailed breakdown/ })).toBeInTheDocument();
  });

  it("reads the session's own accounting where a /usage answer used to spell it", () => {
    const { getByRole } = mount({
      session: { inputTokens: 1100, outputTokens: 4800, costMicros: 34700, workedSeconds: 290 },
    });

    getByRole("button", { name: /Context/ }).click();

    const panel = getByRole("dialog");
    expect(within(panel).getByText("This session")).toBeInTheDocument();
    expect(within(panel).getByText(/1k in · 5k out/)).toBeInTheDocument();
    expect(within(panel).getByText("$0.03")).toBeInTheDocument();
    expect(within(panel).getByText("4m 50s")).toBeInTheDocument();
  });

  it("stands the fullest plan window in for a context the harness never gave", () => {
    const { getByRole, getByText, queryByText } = mount({
      context: null,
      usage: null,
      windows: [window({ used_percent: 70 })],
    });

    const trigger = getByRole("button", { name: /Plan/ });
    expect(trigger.textContent).toContain("70%");

    trigger.click();
    expect(queryByText("Context window")).toBeNull();
    expect(getByText("Plan usage")).toBeInTheDocument();
    expect(getByRole("button", { name: /See the detailed breakdown/ })).toBeInTheDocument();
  });
});
