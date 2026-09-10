import { describe, expect, it } from "vitest";
import type { SessionActivity, SessionState, UsageLimitPause } from "../api/client";
import { formatTimeOfDay } from "./dates";
import { REFUSING, type SessionStatus, deriveStatus, sessionNotice } from "./status";

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

/** A session paused because a plan window is spent. */
function waiting() {
  return { ...session("paused"), paused_reason: "usage_limit" as const };
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
    const known: SessionStatus[] = [
      "provisioning",
      "migrating",
      "working",
      "disconnected",
      "needs_input",
      "idle",
      "paused",
      "usage_limit",
      "interrupted",
      "failed",
      "archived",
    ];
    for (const state of states) {
      expect(known).toContain(deriveStatus(session(state), NOW).status);
    }
  });
});

describe("a session waiting out a spent plan window", () => {
  /** The notice that session shows, for one shape of pause. */
  function notice(pause: UsageLimitPause) {
    return sessionNotice(deriveStatus(waiting(), NOW), {
      failure: null,
      budgetLimit: undefined,
      usageLimit: pause,
      now: NOW,
    });
  }

  it("is its own status, not the pause of a spent budget", () => {
    const view = deriveStatus(waiting(), NOW);
    expect(view.status).toBe("usage_limit");
    expect(view.label).toBe("Waiting on the plan");
    // The rail's dot is coloured for this one resting state, because it is
    // the one that ends by itself.
    expect(view.tone).toBe("waiting");
    expect(view.breathing).toBe(false);
    expect(deriveStatus({ ...session("paused"), paused_reason: "budget" }, NOW).status).toBe(
      "paused",
    );
  });

  it("reads a reason it has never heard of as a plain pause", () => {
    // A newer control plane, not a broken one: `Paused` is true of any
    // pause, and a snake_case token in front of a person is not.
    const view = deriveStatus(
      { ...session("paused"), paused_reason: "solar_flare" as "budget" },
      NOW,
    );
    expect(view.status).toBe("paused");
  });

  it("still takes messages, unlike every other stopped state", () => {
    // The control plane holds what is typed against the pause and sends it
    // when the window turns over, so the composer stays open.
    expect(REFUSING.has("usage_limit")).toBe(false);
    expect(REFUSING.has("paused")).toBe(true);
  });

  it("says which window, when it resets, and that the machine costs nothing", () => {
    const resets = NOW_UNIX + 4 * 3600;
    const view = notice({
      window: "5-hour",
      resets_at_unix: resets,
      resume_at_unix: resets - 600,
    });
    expect(view?.title).toBe("Waiting on the plan");
    expect(view?.tone).toBe("waiting");
    expect(view?.body).toBe(
      `The 5-hour usage limit on this session's plan is spent. It resets at ${formatTimeOfDay(resets)}, in 4h. ` +
        `The machine is stopped and costs nothing until then; flyco starts it again at ${formatTimeOfDay(resets - 600)}. ` +
        "Flyco then asks the agent to continue on your behalf.",
    );
  });

  it("says the machine is still running when the reset is close", () => {
    const view = notice({ window: "5-hour", resets_at_unix: NOW_UNIX + 12 * 60 });
    expect(view?.body).toContain("in 12m");
    expect(view?.body).toContain("The machine is still running");
    expect(view?.body).not.toContain("costs nothing");
  });

  it("quotes the message the user queued instead of promising flyco's own", () => {
    const view = notice({
      window: "Weekly",
      resets_at_unix: NOW_UNIX + 36 * 3600,
      queued_message: "then run the migration and open the pull request",
    });
    expect(view?.body).toContain("Your message is waiting and is sent then");
    expect(view?.body).toContain("then run the migration and open the pull request");
    expect(view?.body).not.toContain("on your behalf");
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
