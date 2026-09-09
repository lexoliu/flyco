import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import SessionHeader, { type SessionHeaderProps } from "./SessionHeader";
import type { SessionDetail } from "../api/client";

const SESSION_ID = "3f2b1c9d-6a4e-4d8b-9f21-7c5a0e3b8d14";

const SESSION: SessionDetail = {
  id: SESSION_ID,
  title: "Audit the relay for dropped frames",
  repo: "octocat/hello-world",
  branch: "dev",
  harness: "claude_code",
  model: { model: "default" },
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
    connection: "live",
    machine: undefined,
    onRename: vi.fn(),
    onArchive: vi.fn(),
    archiving: false,
    onStartMachine: vi.fn(),
    onStopMachine: vi.fn(),
    drawerOpen: false,
    onToggleDrawer: vi.fn(),
    onOpenPanel: vi.fn(),
    ...overrides,
  };
  return render(() => <SessionHeader {...props} />);
}

describe("SessionHeader", () => {
  it("holds the title's place while the session loads, without showing the id", () => {
    const { getByLabelText, queryByText } = mount({});

    expect(getByLabelText("Loading the session")).toBeInTheDocument();
    expect(queryByText(SESSION_ID)).not.toBeInTheDocument();
    // No status either: a session nothing is known about has no lifecycle
    // to report, and a pill is a claim (issue #137).
    expect(queryByText("Loading")).not.toBeInTheDocument();
  });

  it("reads as the title and the repository, and nothing that is not the conversation", () => {
    const { getByRole, getByText, queryByLabelText, queryByRole, queryByText } = mount({
      session: SESSION,
      machine: undefined,
    });

    expect(getByRole("button", { name: "Audit the relay for dropped frames" })).toBeInTheDocument();
    expect(queryByLabelText("Loading the session")).not.toBeInTheDocument();
    expect(getByText("octocat/hello-world")).toBeInTheDocument();
    expect(getByText("· dev")).toBeInTheDocument();
    // No pill, no rings, no SKU (issue #229): the status is read off the
    // transcript, and the budget and context are the composer's row.
    expect(queryByText("Working")).not.toBeInTheDocument();
    expect(queryByRole("img", { name: /^Budget/ })).not.toBeInTheDocument();
    expect(queryByRole("img", { name: /^Context/ })).not.toBeInTheDocument();
    expect(queryByRole("button", { name: "Archive" })).not.toBeInTheDocument();
  });

  it("keeps every session action behind the one menu", async () => {
    const onArchive = vi.fn();
    const { getByRole, findByRole } = mount({ session: SESSION, onArchive });

    getByRole("button", { name: "Session actions" }).click();

    expect(await findByRole("button", { name: "Rename" })).toBeInTheDocument();
    expect(getByRole("button", { name: "Resize" })).toBeInTheDocument();
    expect(getByRole("button", { name: "Edit .env" })).toBeInTheDocument();
    expect(getByRole("button", { name: "Copy session id" })).toBeInTheDocument();
    getByRole("button", { name: "Archive" }).click();
    expect(onArchive).toHaveBeenCalledTimes(1);
  });

  it("offers no archive on a session that already is", async () => {
    const { getByRole, findByRole, queryByRole } = mount({
      session: { ...SESSION, state: "archived" },
    });

    getByRole("button", { name: "Session actions" }).click();

    expect(await findByRole("button", { name: "Rename" })).toBeInTheDocument();
    expect(queryByRole("button", { name: "Archive" })).not.toBeInTheDocument();
  });

  it("says a socket is coming back while it still is", () => {
    const { getByText } = mount({ session: SESSION, connection: "reconnecting" });
    expect(getByText("Reconnecting…")).toBeInTheDocument();
  });

  it("promises no reconnection once the relay has stopped for good", () => {
    // The page renders the problem itself, with a way out of it; a pill
    // here would only be a quieter version of the same sentence (#137).
    const { queryByText } = mount({ session: SESSION, connection: "failed" });
    expect(queryByText("Reconnecting…")).not.toBeInTheDocument();
    expect(queryByText("Disconnected")).not.toBeInTheDocument();
  });
});
