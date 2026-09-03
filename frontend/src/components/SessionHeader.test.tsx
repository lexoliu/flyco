import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import SessionHeader, { type SessionHeaderProps } from "./SessionHeader";
import type { SessionDetail } from "../api/client";
import type { StatusView } from "../lib/status";

const SESSION_ID = "3f2b1c9d-6a4e-4d8b-9f21-7c5a0e3b8d14";

const LOADING: StatusView = { status: "idle", label: "Loading", tone: "quiet", breathing: false };
const WORKING: StatusView = { status: "working", label: "Working", tone: "working", breathing: true };

const SESSION: SessionDetail = {
  id: SESSION_ID,
  title: "Audit the relay for dropped frames",
  repo: "octocat/hello-world",
  branch: "dev",
  harness: "claude_code",
  state: "active",
  activity: "working",
  machine_origin: "auto",
  created_at_unix: 1_790_000_000,
  last_active_unix: 1_790_000_600,
  budget: { limit: 10_000_000, spent: 1_200_000, remaining: 8_800_000, stage: "ok" },
};

function mount(overrides: Partial<SessionHeaderProps>) {
  const props: SessionHeaderProps = {
    session: undefined,
    sessionId: SESSION_ID,
    status: LOADING,
    connection: "live",
    machine: undefined,
    budgetSpentUsd: undefined,
    budgetLimitUsd: undefined,
    contextUsed: undefined,
    contextSize: undefined,
    onRename: vi.fn(),
    onSetBudget: vi.fn(),
    settingBudget: false,
    onArchive: vi.fn(),
    archiving: false,
    onStartMachine: vi.fn(),
    onStopMachine: vi.fn(),
    onOpenPanel: vi.fn(),
    ...overrides,
  };
  return render(() => <SessionHeader {...props} />);
}

describe("SessionHeader", () => {
  it("holds the title's place while the session loads, without showing the id", () => {
    const { getByLabelText, getByRole, queryByText } = mount({});

    expect(getByLabelText("Loading the session")).toBeInTheDocument();
    expect(queryByText(SESSION_ID)).not.toBeInTheDocument();

    // The rings keep their names and read a dash: what is unknown is the
    // number, and the header has no business explaining its plumbing.
    expect(getByRole("img", { name: "Budget: —" })).toBeInTheDocument();
    expect(getByRole("img", { name: "Context: —" })).toBeInTheDocument();
    expect(queryByText(/not loaded/)).not.toBeInTheDocument();
    expect(queryByText(/not reported/)).not.toBeInTheDocument();
  });

  it("reads the title, the repository and both rings once everything is known", () => {
    const { getByRole, getByText, queryByLabelText } = mount({
      session: SESSION,
      status: WORKING,
      budgetSpentUsd: 1.2,
      budgetLimitUsd: 10,
      contextUsed: 41_000,
      contextSize: 200_000,
    });

    expect(getByRole("button", { name: "Audit the relay for dropped frames" })).toBeInTheDocument();
    expect(queryByLabelText("Loading the session")).not.toBeInTheDocument();
    expect(getByText("octocat/hello-world")).toBeInTheDocument();
    expect(getByRole("img", { name: "Budget: $1.20 / $10" })).toBeInTheDocument();
    expect(getByRole("img", { name: "Context: 41k / 200k" })).toBeInTheDocument();
  });

  it("raises the budget from the ring, never below what is already spent", async () => {
    const onSetBudget = vi.fn();
    const { getByRole, getByLabelText } = mount({
      session: {
        ...SESSION,
        state: "paused",
        budget: { limit: 10_000_000, spent: 10_000_000, remaining: 0, stage: "exhausted" },
      },
      budgetSpentUsd: 10,
      budgetLimitUsd: 10,
      onSetBudget,
    });

    // The ring is the button: what it is named by is the reading it shows,
    // which is what a person clicking it is acting on.
    getByRole("button", { name: "Budget: $10.00 / $10" }).click();

    // The floor is the first whole dollar above the spend: a limit at or
    // under it would leave the session paused on the same exhausted budget.
    const slider = getByLabelText("Session budget in dollars") as HTMLInputElement;
    expect(slider.min).toBe("11");

    slider.value = "25";
    slider.dispatchEvent(new Event("input", { bubbles: true }));
    getByRole("button", { name: "Set budget to $25" }).click();

    expect(onSetBudget).toHaveBeenCalledWith(25);
  });

  it("leaves the budget a readout on an archived session", () => {
    const { getByRole, queryByRole } = mount({
      session: { ...SESSION, state: "archived" },
      budgetSpentUsd: 1.2,
      budgetLimitUsd: 10,
    });

    expect(getByRole("img", { name: "Budget: $1.20 / $10" })).toBeInTheDocument();
    expect(queryByRole("button", { name: /^Budget:/ })).not.toBeInTheDocument();
  });
});
