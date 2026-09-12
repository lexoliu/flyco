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
    mcp_tools: [],
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
    onBreakdown: vi.fn(),
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
    expect(within(panel).getByRole("button", { name: /detailed breakdown/ })).toBeInTheDocument();
  });

  it("asks for the breakdown from inside the panel, which then closes", () => {
    const { props, getByRole, queryByRole } = mount();

    getByRole("button", { name: /Context/ }).click();
    getByRole("button", { name: /detailed breakdown/ }).click();

    expect(props.onBreakdown).toHaveBeenCalledOnce();
    expect(queryByRole("dialog")).toBeNull();
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
    expect(getByRole("button", { name: /detailed breakdown/ })).toBeInTheDocument();
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
    expect(getByRole("button", { name: /detailed breakdown/ })).toBeInTheDocument();
  });
});
