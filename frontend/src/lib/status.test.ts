import { describe, expect, it } from "vitest";
import type { SessionActivity, SessionState } from "../api/client";
import type { TimedEvent } from "../api/relay";
import type { ClientEvent } from "../api/wire";
import {
  SESSION_GROUPS,
  STATUS_ORDER,
  type SessionStatus,
  REFUSING,
  deriveStatus,
  elapsedSince,
  groupOf,
  groupSessions,
  liveSignalsFrom,
  sessionNotice,
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
    // `Disconnected` sits between them: it is not the agent asking for
    // anything, but it is the one thing a user can act on that stops a
    // session dead.
    expect(STATUS_ORDER.slice(0, 4)).toEqual(["needs_input", "disconnected", "working", "idle"]);
    expect(STATUS_ORDER[STATUS_ORDER.length - 1]).toBe("archived");
  });

  it("names every status exactly once", () => {
    expect(new Set(STATUS_ORDER).size).toBe(STATUS_ORDER.length);
    expect(STATUS_ORDER).toHaveLength(EVERY_STATUS.length);
    for (const status of EVERY_STATUS) {
      expect(STATUS_ORDER).toContain(status);
    }
  });
});

/** Every status the UI can show, so a sweep cannot quietly miss one. */
const EVERY_STATUS: readonly SessionStatus[] = [
  "provisioning",
  "migrating",
  "working",
  "disconnected",
  "needs_input",
  "idle",
  "paused",
  "interrupted",
  "failed",
  "archived",
];

describe("groupSessions", () => {
  /** A row is just its name here: the grouping only reads the status. */
  function row(status: SessionStatus, session: string) {
    return { status, session };
  }

  it("divides the list into the four headings docs/ux.md §5 names", () => {
    const groups = groupSessions([
      row("idle", "idle"),
      row("failed", "failed"),
      row("working", "working"),
      row("needs_input", "blocked"),
    ]);

    expect(groups.map((group) => group.heading)).toEqual([
      "Needs input",
      "Working",
      "Idle",
      "Other",
    ]);
    expect(groups.map((group) => group.rows)).toEqual([["blocked"], ["working"], ["idle"], ["failed"]]);
  });

  it("puts everything that is the machine's own business under one heading", () => {
    // The bug this fixes: a heading reading FAILED over a single row that
    // already says Failed. Provisioning, migrating, paused and interrupted
    // were four more of the same.
    const groups = groupSessions([
      row("paused", "paused"),
      row("provisioning", "building"),
      row("migrating", "moving"),
      row("interrupted", "reclaimed"),
      row("failed", "failed"),
      row("idle", "idle"),
    ]);

    expect(groups.map((group) => group.heading)).toEqual(["Idle", "Other"]);
    // Inside that heading the rows keep STATUS_ORDER, so a failure is read
    // before a pause rather than in whatever order the API listed them.
    expect(groups[1]?.rows).toEqual(["failed", "building", "moving", "paused", "reclaimed"]);
  });

  it("drops the heading when the whole list is one pile", () => {
    // A heading divides a list. With one group there is nothing to divide,
    // and each row still says its own status.
    expect(groupSessions([row("failed", "one"), row("paused", "two")])).toEqual([
      { group: "other", heading: null, rows: ["one", "two"] },
    ]);
    expect(groupSessions([row("idle", "only")])).toEqual([
      { group: "idle", heading: null, rows: ["only"] },
    ]);
  });

  it("leaves out a heading nothing is under", () => {
    const groups = groupSessions([row("needs_input", "blocked"), row("idle", "idle")]);

    expect(groups.map((group) => group.group)).toEqual(["needs_input", "idle"]);
  });

  it("keeps the order the caller listed rows in within one status", () => {
    const groups = groupSessions([row("idle", "first"), row("idle", "second"), row("idle", "third")]);

    expect(groups[0]?.rows).toEqual(["first", "second", "third"]);
  });

  it("groups nothing into nothing", () => {
    expect(groupSessions([])).toEqual([]);
  });

  it("keeps the archived tab a list of its own", () => {
    expect(groupSessions([row("archived", "old")])).toEqual([
      { group: "archived", heading: null, rows: ["old"] },
    ]);
  });
});

describe("groupOf", () => {
  it("gives every status a heading, and only these five", () => {
    const groups = new Set(SESSION_GROUPS.map(({ group }) => group));
    for (const status of EVERY_STATUS) {
      expect(groups).toContain(groupOf(status));
    }
  });

  it("lets the three statuses a user acts on name themselves", () => {
    expect(groupOf("needs_input")).toBe("needs_input");
    expect(groupOf("working")).toBe("working");
    expect(groupOf("idle")).toBe("idle");
    expect(groupOf("archived")).toBe("archived");
  });

  it("reads everything else as the machine's own business", () => {
    for (const status of ["failed", "provisioning", "migrating", "paused", "interrupted"] as const) {
      expect(groupOf(status)).toBe("other");
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

describe("sessionNotice", () => {
  /** What a session that has spent nothing of a $10 budget is working with. */
  const FACTS = { failure: null, budgetLimit: 10_000_000 };

  /** The notice a session in one state carries, with no failure recorded. */
  function noticeFor(state: SessionState, failure: string | null = null) {
    return sessionNotice(deriveStatus(session(state), NOW), { ...FACTS, failure });
  }

  it("says nothing at all about a session that is running", () => {
    expect(noticeFor("active")).toBeNull();
  });

  it("leaves a first machine to the provisioning timeline", () => {
    // The transcript already tells that story, stage by stage.
    expect(noticeFor("provisioning")).toBeNull();
  });

  it("ends the provider's reason before its own sentence begins", () => {
    const notice = noticeFor(
      "failed",
      "lowPriorityCores in westeurope allows 3 and 0 are in use, which does not cover 4 more",
    );

    expect(notice?.body).toContain("does not cover 4 more. Resuming builds");
  });

  it("gives a failed session the provider's reason and a way on from it", () => {
    const notice = noticeFor("failed", "AWS refused the reservation: InsufficientInstanceCapacity.");

    expect(notice?.title).toBe("Failed");
    expect(notice?.tone).toBe("failed");
    expect(notice?.body).toContain("AWS refused the reservation");
    expect(notice?.body).toContain("reopens the same conversation");
    expect(notice?.action).toEqual({ kind: "resume", label: "Resume" });
  });

  it("stays honest about a failure nothing was recorded for", () => {
    expect(noticeFor("failed")?.body).toContain("said nothing about why");
  });

  it("offers to put an interrupted session back on its own disk", () => {
    const notice = sessionNotice(deriveStatus(reclaimed("interrupted"), NOW), FACTS);

    expect(notice?.title).toBe("Interrupted · spot reclaimed");
    expect(notice?.action).toEqual({ kind: "resume", label: "Resume" });
  });

  it("asks nothing of the user while flyco is already putting the session back", () => {
    const notice = sessionNotice(deriveStatus(reclaimed("provisioning", 40), NOW), FACTS);

    expect(notice?.title).toBe("Migrating · 40s");
    expect(notice?.body).toContain("Nothing is needed from you");
    expect(notice?.action).toBeUndefined();
  });

  it("says an archived session is read-only, and offers to build it again", () => {
    const notice = noticeFor("archived");

    expect(notice?.title).toBe("Archived");
    expect(notice?.body).toContain("read-only");
    expect(notice?.action).toEqual({ kind: "resume", label: "Resume" });
  });

  it("names the sum a budget pause is about, and offers to raise it", () => {
    const notice = noticeFor("paused");

    expect(notice?.title).toBe("Paused · budget exhausted");
    expect(notice?.body).toBe("The $10.00 budget is spent. Raise it to continue.");
    expect(notice?.action).toEqual({ kind: "raise_budget", label: "Raise budget" });
  });

  it("still asks for a raise when the limit has not been read yet", () => {
    const notice = sessionNotice(deriveStatus(session("paused"), NOW), {
      failure: null,
      budgetLimit: undefined,
    });

    expect(notice?.body).toBe("This session's budget is spent. Raise it to continue.");
    expect(notice?.action).toEqual({ kind: "raise_budget", label: "Raise budget" });
  });
});

describe("REFUSING", () => {
  it("lets a running session be spoken to, whatever it is doing", () => {
    const running: SessionStatus[] = ["working", "needs_input", "idle"];
    expect(running.some((status) => REFUSING.has(status))).toBe(false);
  });

  it("takes a message for a machine that is still being built", () => {
    // It waits in the room's mailbox, which is worth having said.
    expect(REFUSING.has("provisioning")).toBe(false);
    expect(REFUSING.has("migrating")).toBe(false);
  });

  it("names every state whose composer is replaced by the notice", () => {
    const refused: SessionStatus[] = ["failed", "interrupted", "paused", "archived"];
    expect([...REFUSING].sort()).toEqual([...refused].sort());
    // And every one of them has a notice to put there, or the slot would
    // be empty (issue #133).
    for (const status of refused) {
      expect(
        sessionNotice(
          { status, label: "x", tone: "neutral", breathing: false },
          { failure: undefined, budgetLimit: undefined },
        ),
      ).not.toBeNull();
    }
  });
});

describe("a machine that fell off the room", () => {
  it("reads as Disconnected rather than as a turn that never ends", () => {
    // The frames stopped arriving, so every other signal still says a turn
    // is running: that is exactly the state this exists to correct.
    const view = deriveStatus(session("active"), NOW, {
      turnInFlight: true,
      machineOffline: true,
    });

    expect(view.status).toBe("disconnected");
    expect(view.breathing).toBe(false);
  });

  it("goes back to what it was doing once the machine is back", () => {
    const view = deriveStatus(session("active"), NOW, {
      turnInFlight: true,
      machineOffline: false,
    });

    expect(view.status).toBe("working");
  });

  it("is read off the room's own announcement, and is absent until it says", () => {
    expect(liveSignalsFrom([]).machineOffline).toBeUndefined();
    expect(
      liveSignalsFrom(stream({ type: "machine_connection", connected: false })).machineOffline,
    ).toBe(true);
    expect(
      liveSignalsFrom(
        stream({ type: "machine_connection", connected: false }, { type: "machine_connection", connected: true }),
      ).machineOffline,
    ).toBe(false);
  });
});

describe("a session that cannot be running a turn", () => {
  it("never reads as Working, however the stream ended", () => {
    // What makes the composer's Stop button correct: `turnInFlight` is a
    // fold over frames that stopped arriving, so a session archived or
    // failed mid-turn folds to "a turn is running" forever. The lifecycle
    // is what settles it, and the page reads Stop off this.
    for (const state of ["archived", "failed", "interrupted", "paused"] as const) {
      expect(deriveStatus(session(state), NOW, { turnInFlight: true }).status).not.toBe("working");
    }
  });
});
