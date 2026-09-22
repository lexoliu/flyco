import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import SessionHeader, { type SessionHeaderProps } from "./SessionHeader";
import type { SessionDetail } from "../api/client";

const SESSION_ID = "3f2b1c9d-6a4e-4d8b-9f21-7c5a0e3b8d14";

const SESSION: SessionDetail = {
  id: SESSION_ID,
  title: "Audit the relay for dropped frames",
  repos: [
    { slug: "octocat/hello-world", branch: "dev", dir: "hello-world", added_by: "user" },
  ],
  harness: "claude_code",
  model: { model: "default" },
  permission_mode: "auto",
  state: "active",
  activity: "working",
  machine_origin: "auto",
  computer_use: false,
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
    onKeepAwake: vi.fn(),
    drawerOpen: false,
    onToggleDrawer: vi.fn(),
    onOpenPanel: vi.fn(),
    ...overrides,
  };
  return render(() => <SessionHeader {...props} />);
}

describe("SessionHeader", () => {
  it("holds the machine awake for the time the menu was asked for", () => {
    const onKeepAwake = vi.fn();
    const { getByRole } = mount({ session: SESSION, onKeepAwake });

    getByRole("button", { name: "Session actions" }).click();
    getByRole("button", { name: "Keep machine awake" }).click();
    getByRole("button", { name: "4h" }).click();

    expect(onKeepAwake).toHaveBeenCalledWith(240);
  });

  it("counts down a hold that is running, and the same row ends it", () => {
    const onKeepAwake = vi.fn();
    const { getByRole, getByText } = mount({
      session: {
        ...SESSION,
        awake_until_unix: Math.floor(Date.now() / 1000) + 2 * 3600 + 15 * 60,
      },
      onKeepAwake,
    });

    getByRole("button", { name: "Session actions" }).click();
    expect(getByText("Awake for 2h 15m")).toBeInTheDocument();

    getByRole("button", { name: "Off" }).click();
    expect(onKeepAwake).toHaveBeenCalledWith(null);
  });

  it("reads a fresh hold as the whole duration the round trip cost a second of", () => {
    const { getByRole, getByText } = mount({
      session: {
        ...SESSION,
        // What the control plane answers a `8h` press with, once a
        // second of round trip has passed: the label still has to read
        // what the user asked for.
        awake_until_unix: Math.floor(Date.now() / 1000) + 8 * 3600 - 1,
      },
    });

    getByRole("button", { name: "Session actions" }).click();
    expect(getByText("Awake for 8h")).toBeInTheDocument();
  });

  it("holds the title's place while the session loads, without showing the id", () => {
    const { getByLabelText, queryByText } = mount({});

    expect(getByLabelText("Loading the session")).toBeInTheDocument();
    expect(queryByText(SESSION_ID)).not.toBeInTheDocument();
    // No status either: a session nothing is known about has no lifecycle
    // to report, and a pill is a claim (issue #137).
    expect(queryByText("Loading")).not.toBeInTheDocument();
  });

  it("reads as the title and the repository, and nothing that is not the conversation", () => {
    const { getByRole, queryByLabelText, queryByRole, queryByText } = mount({
      session: SESSION,
      machine: undefined,
    });

    expect(getByRole("button", { name: "Audit the relay for dropped frames" })).toBeInTheDocument();
    expect(queryByLabelText("Loading the session")).not.toBeInTheDocument();
    // The readout is the popover's trigger: primary repository and its
    // branch, one label.
    expect(
      getByRole("button", { name: "octocat/hello-world · dev" }),
    ).toBeInTheDocument();
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

  it("says a stream is coming back while it still is", () => {
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
