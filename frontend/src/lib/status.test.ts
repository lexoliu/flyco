import { describe, expect, it } from "vitest";
import type { SessionActivity, SessionState } from "../api/client";
import type { TimedEvent } from "../api/relay";
import type { ClientEvent } from "../api/wire";
import {
  GROUP_LABEL,
  STATUS_ORDER,
  type SessionStatus,
  deriveStatus,
  elapsedSince,
  liveSignalsFrom,
} from "./status";

/** A fixed instant, so nothing here depends on when the suite runs. */
const NOW_UNIX = 1_800_000_000;
const NOW = NOW_UNIX * 1000;

function session(state: SessionState, createdAgo = 0) {
  return {
    state,
    created_at_unix: NOW_UNIX - createdAgo,
    last_active_unix: NOW_UNIX - createdAgo,
  };
}

/** The same session, having lost its machine to the provider. */
function reclaimed(state: SessionState, sinceAgo = 0) {
  return {
    state,
    created_at_unix: NOW_UNIX - 3600,
    last_active_unix: NOW_UNIX - sinceAgo,
    interrupted_reason: "spot_reclaimed" as const,
  };
}

describe("deriveStatus", () => {
  it("counts the wait while a machine is being built", () => {
    const view = deriveStatus(session("provisioning", 125), NOW);
    expect(view.status).toBe("provisioning");
    expect(view.detail).toBe("2m");
    expect(view.breathing).toBe(true);
  });

  it("reads an active session that says nothing about itself as idle", () => {
    // A summary from a control plane older than `activity`, and the honest
    // answer for one that has never run a turn.
    expect(deriveStatus(session("active"), NOW).status).toBe("idle");
    expect(deriveStatus(session("active"), NOW).label).toBe("Idle");
  });

  it("reads an active session with a turn in flight as working", () => {
    const view = deriveStatus(session("active"), NOW, { turnInFlight: true });
    expect(view.status).toBe("working");
    expect(view.tone).toBe("working");
    expect(view.breathing).toBe(true);
  });

  it("reads an active session blocked on the user as needing input", () => {
    const view = deriveStatus(session("active"), NOW, { awaitingUser: true });
    expect(view.status).toBe("needs_input");
    expect(view.tone).toBe("attention");
  });

  it("prefers a running turn over a stale awaiting-user signal", () => {
    const view = deriveStatus(session("active"), NOW, {
      turnInFlight: true,
      awaitingUser: true,
    });
    expect(view.status).toBe("working");
  });

  it("explains why a session stopped", () => {
    expect(deriveStatus(session("paused"), NOW).detail).toBe("budget exhausted");
    expect(deriveStatus(reclaimed("interrupted"), NOW).detail).toBe("spot reclaimed");
  });

  it("says nothing it cannot support about why a session was interrupted", () => {
    // The reason comes from the control plane; a build that has not heard
    // of one renders the status without a clause rather than a raw token.
    const view = deriveStatus(session("interrupted"), NOW);
    expect(view.label).toBe("Interrupted");
    expect(view.detail).toBeUndefined();
  });

  it("reads a session being put back on its own disk as migrating", () => {
    // Recovering runs through `provisioning`, exactly as a first machine
    // does. The reason is the only thing that tells them apart, and the
    // wait is counted from when the machine went rather than from when the
    // session was opened.
    const view = deriveStatus(reclaimed("provisioning", 125), NOW);
    expect(view.status).toBe("migrating");
    expect(view.label).toBe("Migrating");
    expect(view.detail).toBe("2m");
    expect(view.breathing).toBe(true);
  });

  it("reads a first machine as provisioning, not as a migration", () => {
    expect(deriveStatus(session("provisioning", 125), NOW).status).toBe("provisioning");
  });

  it("stops migrating once the reason is cleared", () => {
    // The control plane clears it when the session's daemon is back, which
    // is the moment the migration is genuinely over.
    expect(deriveStatus(session("active"), NOW).status).toBe("idle");
  });

  it("colours a failed session red and everything at rest quietly", () => {
    expect(deriveStatus(session("failed"), NOW).tone).toBe("failed");
    expect(deriveStatus(session("archived"), NOW).tone).toBe("quiet");
  });

  it("derives a status for every lifecycle state", () => {
    const states: SessionState[] = [
      "provisioning",
      "active",
      "paused",
      "interrupted",
      "archived",
      "failed",
    ];
    for (const state of states) {
      expect(STATUS_ORDER).toContain(deriveStatus(session(state), NOW).status);
    }
  });
});

describe("deriveStatus, from the summary's activity", () => {
  /** An active session the control plane has recorded an activity for. */
  function doing(activity: SessionActivity) {
    return { ...session("active"), activity };
  }

  it("says Working for a session with a turn in flight", () => {
    // The whole point of the field: the home list has no relay, and every
    // running session used to read as `Idle` there.
    const view = deriveStatus(doing("working"), NOW);
    expect(view.status).toBe("working");
    expect(view.label).toBe("Working");
    expect(view.tone).toBe("working");
    expect(view.breathing).toBe(true);
  });

  it("says Needs input for a session waiting on its user", () => {
    const view = deriveStatus(doing("needs_input"), NOW);
    expect(view.status).toBe("needs_input");
    expect(view.label).toBe("Needs input");
    expect(view.tone).toBe("attention");
    expect(view.breathing).toBe(false);
  });

  it("says Idle for a session that is running nothing", () => {
    expect(deriveStatus(doing("idle"), NOW).status).toBe("idle");
  });

  it("lets a live turn override a summary that has not caught up", () => {
    // The relay carries the newer fact: a turn that started a moment ago is
    // on the socket before the row it was written to is read again.
    expect(deriveStatus(doing("idle"), NOW, { turnInFlight: true }).status).toBe("working");
    expect(deriveStatus(doing("needs_input"), NOW, { turnInFlight: true }).status).toBe("working");
  });

  it("lets a live approval override a summary that says the agent is working", () => {
    expect(deriveStatus(doing("working"), NOW, { awaitingUser: true }).status).toBe("needs_input");
  });

  it("keeps the summary's answer while the relay has said nothing", () => {
    // An open socket that has replayed nothing yet reports both signals
    // false; that is silence, not a contradiction.
    const silent = { turnInFlight: false, awaitingUser: false };
    expect(deriveStatus(doing("working"), NOW, silent).status).toBe("working");
    expect(deriveStatus(doing("needs_input"), NOW, silent).status).toBe("needs_input");
  });

  it("ignores the activity of a session that is not running", () => {
    // A session keeps the activity it had when it was paused, interrupted
    // or archived; what the user reads there is why it stopped.
    for (const state of ["paused", "interrupted", "archived", "failed"] as const) {
      const view = deriveStatus({ ...session(state), activity: "working" }, NOW);
      expect(view.status).toBe(state);
    }
    expect(deriveStatus({ ...session("provisioning"), activity: "working" }, NOW).status).toBe(
      "provisioning",
    );
  });
});

describe("STATUS_ORDER", () => {
  it("puts what needs the user first, then what is running, then what is at rest", () => {
    // docs/ux.md §5: `Needs input` → `Working` → `Idle` → the rest.
    expect(STATUS_ORDER.slice(0, 3)).toEqual(["needs_input", "working", "idle"]);
    expect(STATUS_ORDER[STATUS_ORDER.length - 1]).toBe("archived");
  });

  it("names every status exactly once", () => {
    expect(new Set(STATUS_ORDER).size).toBe(STATUS_ORDER.length);
    for (const status of Object.keys(GROUP_LABEL) as SessionStatus[]) {
      expect(STATUS_ORDER).toContain(status);
    }
  });
});

/** Dates every event at the same instant: none of these assertions is about time. */
function stream(...events: ClientEvent[]): TimedEvent[] {
  return events.map((event) => ({ event, atUnix: NOW_UNIX }));
}

const TURN = "turn-1";
const started: ClientEvent = { type: "harness", event: { type: "turn_started", turn_id: TURN } };
const completed: ClientEvent = {
  type: "harness",
  event: {
    type: "turn_completed",
    turn_id: TURN,
    usage: { input_tokens: 1, output_tokens: 2, estimated_cost: null, context: null },
  },
};
const failed: ClientEvent = {
  type: "harness",
  event: { type: "turn_failed", turn_id: TURN, error: "the harness stopped" },
};
const asked: ClientEvent = {
  type: "approval_pending",
  id: "ap-1",
  payload: { kind: "tool_use", tool: "Bash", input: { command: "rm -rf build" } },
};
const answered: ClientEvent = { type: "approval_decided", id: "ap-1", decision: "approved" };
const said: ClientEvent = { type: "user_message", text: "audit the relay" };

describe("liveSignalsFrom", () => {
  it("reads an empty stream as a session that has done nothing yet", () => {
    expect(liveSignalsFrom([])).toEqual({ turnInFlight: false, awaitingUser: false });
  });

  it("sees a turn in flight between its start and its end", () => {
    expect(liveSignalsFrom(stream(said, started)).turnInFlight).toBe(true);
    expect(liveSignalsFrom(stream(said, started, completed)).turnInFlight).toBe(false);
    expect(liveSignalsFrom(stream(said, started, failed)).turnInFlight).toBe(false);
  });

  it("waits on the user once the agent has finished and nobody answered", () => {
    expect(liveSignalsFrom(stream(said, started, completed)).awaitingUser).toBe(true);
  });

  it("stops waiting as soon as the user says something back", () => {
    expect(liveSignalsFrom(stream(said, started, completed, said)).awaitingUser).toBe(false);
  });

  it("waits on the user while an approval is undecided, even mid-turn", () => {
    const signals = liveSignalsFrom(stream(said, started, asked));
    expect(signals.awaitingUser).toBe(true);
    expect(signals.turnInFlight).toBe(true);
  });

  it("stops waiting once every approval is decided", () => {
    expect(liveSignalsFrom(stream(said, started, asked, answered)).awaitingUser).toBe(false);
  });

  it("keeps waiting while any one of several approvals is undecided", () => {
    const second: ClientEvent = {
      type: "approval_pending",
      id: "ap-2",
      payload: { kind: "merge", repo: "lexoliu/flyco", from_branch: "feat", into_branch: "dev" },
    };
    expect(liveSignalsFrom(stream(asked, second, answered)).awaitingUser).toBe(true);
  });

  it("reads a failed turn as needing the user, who has to decide what next", () => {
    expect(liveSignalsFrom(stream(said, started, failed)).awaitingUser).toBe(true);
  });

  it("feeds deriveStatus the three statuses docs/ux.md §6 distinguishes", () => {
    const active = session("active");
    expect(deriveStatus(active, NOW, liveSignalsFrom(stream(said, started))).status).toBe("working");
    expect(deriveStatus(active, NOW, liveSignalsFrom(stream(said, started, completed))).status).toBe(
      "needs_input",
    );
    expect(deriveStatus(active, NOW, liveSignalsFrom(stream(said))).status).toBe("idle");
  });

  it("ignores events that say nothing about whose move it is", () => {
    const noise = stream(
      { type: "usage", usage: { input_tokens: 9, output_tokens: 9, estimated_cost: null, context: null } },
      { type: "terminal_output", data: "$ ls\n" },
      { type: "provisioning_stage", stage: "ready", at_unix: NOW_UNIX },
    );
    expect(liveSignalsFrom(noise)).toEqual({ turnInFlight: false, awaitingUser: false });
  });
});

describe("elapsedSince", () => {
  it("is coarse: seconds, then minutes, then hours", () => {
    expect(elapsedSince(NOW_UNIX, NOW)).toBe("0s");
    expect(elapsedSince(NOW_UNIX - 59, NOW)).toBe("59s");
    expect(elapsedSince(NOW_UNIX - 60, NOW)).toBe("1m");
    expect(elapsedSince(NOW_UNIX - 3599, NOW)).toBe("59m");
    expect(elapsedSince(NOW_UNIX - 7200, NOW)).toBe("2h");
  });

  it("never counts backwards from a clock that is behind the server", () => {
    expect(elapsedSince(NOW_UNIX + 30, NOW)).toBe("0s");
  });
});
