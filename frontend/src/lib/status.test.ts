import { describe, expect, it } from "vitest";
import type { SessionState } from "../api/client";
import type { TimedEvent } from "../api/relay";
import type { ClientEvent } from "../api/wire";
import { STATUS_ORDER, deriveStatus, elapsedSince, liveSignalsFrom } from "./status";

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

describe("deriveStatus", () => {
  it("counts the wait while a machine is being built", () => {
    const view = deriveStatus(session("provisioning", 125), NOW);
    expect(view.status).toBe("provisioning");
    expect(view.detail).toBe("2m");
    expect(view.breathing).toBe(true);
  });

  it("reads an active session with no live signals as idle", () => {
    // Telling `Working` from `Needs input` needs relay events the session
    // list does not have; `Idle` is the honest answer until it does.
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
    expect(deriveStatus(session("interrupted"), NOW).detail).toBe("spot reclaimed");
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
