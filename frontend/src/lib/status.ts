/**
 * The status a person reads off a session row.
 *
 * `SessionState` is a lifecycle enum — where the machine is — and it is not
 * what a user wants to know. "Active" answers nothing: an active session may
 * be thinking, waiting for an approval, or sitting idle since yesterday.
 * docs/ux.md §6 defines the eight statuses the UI actually shows, and this
 * module is the single derivation of them, so a row, a group heading and a
 * session header can never disagree.
 *
 * Telling `Working` from `Needs input` needs the session's live relay
 * events — whether a turn is in flight, whether an approval is pending —
 * and those are not on `SessionSummary`. {@link liveSignalsFrom} reads them
 * off a relay stream, and {@link deriveStatus} takes the result as an extra
 * argument rather than deriving the same thing twice. The session page has
 * a relay open and passes them; the home list is built from summaries alone
 * and passes none, so an `active` row there reads as `Idle`, which is the
 * honest answer for what a summary knows.
 */
import type { InterruptedReason, SessionState, SessionSummary } from "../api/client";
import type { TimedEvent } from "../api/relay";

/** What a session looks like to the person who opened it. */
export type SessionStatus =
  | "provisioning"
  | "migrating"
  | "working"
  | "needs_input"
  | "idle"
  | "paused"
  | "interrupted"
  | "failed"
  | "archived";

/** How a status is coloured; see docs/ux.md §6. */
export type StatusTone = "neutral" | "working" | "attention" | "failed" | "quiet";

/** Everything a row needs to render one status. */
export interface StatusView {
  /** The derived status. */
  status: SessionStatus;
  /** The word shown beside the dot. */
  label: string;
  /** Which colour the dot takes. */
  tone: StatusTone;
  /** Whether the dot breathes, because something is genuinely happening. */
  breathing: boolean;
  /** The clause after the label, e.g. `2m` or `budget exhausted`. */
  detail?: string;
}

/** What the live relay knows that a summary cannot. */
export interface LiveSignals {
  /** Whether a turn is running right now. */
  turnInFlight?: boolean;
  /** Whether the agent is blocked on a decision from the user. */
  awaitingUser?: boolean;
}

const BASE: Record<Exclude<SessionState, "active">, StatusView> = {
  provisioning: {
    status: "provisioning",
    label: "Provisioning",
    tone: "neutral",
    breathing: true,
  },
  paused: {
    status: "paused",
    label: "Paused",
    tone: "quiet",
    breathing: false,
    detail: "budget exhausted",
  },
  interrupted: {
    status: "interrupted",
    label: "Interrupted",
    tone: "quiet",
    breathing: false,
  },
  failed: { status: "failed", label: "Failed", tone: "failed", breathing: false },
  archived: { status: "archived", label: "Archived", tone: "quiet", breathing: false },
};

/**
 * What flyco is doing while it puts a reclaimed session back.
 *
 * Not a `SessionState` of its own: the session is `provisioning`, exactly
 * as a brand-new one is, and what makes it different is *why*. Breathing
 * and neutral like `Provisioning`, because it is the same kind of wait —
 * something is happening and there is nothing for the user to do.
 */
const MIGRATING: StatusView = {
  status: "migrating",
  label: "Migrating",
  tone: "neutral",
  breathing: true,
};

/**
 * The clause after `Interrupted`, for a session that lost its machine.
 *
 * `undefined` for a session that never lost one, and for a reason this
 * build has not heard of: an unknown token is a newer control plane, and
 * rendering it raw would put a snake_case identifier in front of a person.
 */
function lostItsMachine(reason: InterruptedReason | null | undefined): string | undefined {
  return reason === "spot_reclaimed" ? "spot reclaimed" : undefined;
}

/**
 * The status of one session.
 *
 * `now` is passed in rather than read from the clock so that a list renders
 * every row against one instant, and so that this is testable.
 */
export function deriveStatus(
  session: Pick<
    SessionSummary,
    "state" | "created_at_unix" | "last_active_unix" | "interrupted_reason"
  >,
  now: number,
  live: LiveSignals = {},
): StatusView {
  const lost = lostItsMachine(session.interrupted_reason);
  if (session.state === "provisioning") {
    // Provisioning that follows a reclamation is flyco putting the session
    // back on the disk it never lost, which is a different thing to a user
    // than a machine being built for the first time (docs/ux.md §6). The
    // reason is the only thing that tells the two apart, and it is cleared
    // the moment the session's daemon is back.
    // Each counts from where its own wait began: a first machine from when
    // the session was opened, a replacement from when the old one went.
    return lost === undefined
      ? { ...BASE.provisioning, detail: elapsedSince(session.created_at_unix, now) }
      : { ...MIGRATING, detail: elapsedSince(session.last_active_unix, now) };
  }
  if (session.state === "interrupted") {
    // A session interrupted for a reason this build does not know reads as
    // `Interrupted` with nothing after it, rather than with a raw token.
    return lost === undefined ? BASE.interrupted : { ...BASE.interrupted, detail: lost };
  }
  if (session.state !== "active") {
    return BASE[session.state];
  }
  if (live.turnInFlight === true) {
    return { status: "working", label: "Working", tone: "working", breathing: true };
  }
  if (live.awaitingUser === true) {
    return { status: "needs_input", label: "Needs input", tone: "attention", breathing: false };
  }
  return { status: "idle", label: "Idle", tone: "quiet", breathing: false };
}

/**
 * Reads the two live facts a status needs off a session's relay stream.
 *
 * docs/ux.md §6 defines them in terms of what has happened, not of what the
 * lifecycle enum says, so both are folded from the events themselves:
 *
 * - **A turn is in flight** when a turn started and neither completed nor
 *   failed. Turns do not nest, so the last one seen is the answer.
 * - **The agent is waiting on the user** when an approval is pending, or
 *   when the last turn completed and no user message followed it. The
 *   second half is what makes a finished session read as `Needs input`
 *   rather than as `Idle`: the agent said its piece and it is the user's
 *   move.
 *
 * Pure, and taking the whole stream rather than an incremental update, so
 * it agrees with {@link foldTranscript} by construction — both are a single
 * pass over the same list.
 */
export function liveSignalsFrom(events: readonly TimedEvent[]): LiveSignals {
  const undecided = new Set<string>();
  let turnInFlight = false;
  /** Whether the most recent conversational move was the agent finishing. */
  let agentSpokeLast = false;

  for (const { event } of events) {
    switch (event.type) {
      case "user_message":
        agentSpokeLast = false;
        break;
      case "approval_pending":
        undecided.add(event.id);
        break;
      case "approval_decided":
        undecided.delete(event.id);
        break;
      case "harness":
        switch (event.event.type) {
          case "turn_started":
            turnInFlight = true;
            agentSpokeLast = false;
            break;
          case "turn_completed":
          case "turn_failed":
            turnInFlight = false;
            agentSpokeLast = true;
            break;
          default:
            break;
        }
        break;
      default:
        break;
    }
  }

  return { turnInFlight, awaitingUser: undecided.size > 0 || agentSpokeLast };
}

/**
 * Order the home page groups sessions in: what needs the user, then what is
 * running, then everything at rest. Archived is a tab of its own and is
 * therefore last.
 */
export const STATUS_ORDER: readonly SessionStatus[] = [
  "needs_input",
  "failed",
  "working",
  "provisioning",
  "migrating",
  "idle",
  "paused",
  "interrupted",
  "archived",
];

/** The heading a group of sessions sits under. */
export const GROUP_LABEL: Record<SessionStatus, string> = {
  needs_input: "Needs input",
  failed: "Failed",
  working: "Working",
  provisioning: "Provisioning",
  migrating: "Migrating",
  idle: "Idle",
  paused: "Paused",
  interrupted: "Interrupted",
  archived: "Archived",
};

/** Whether a status belongs on the archived tab rather than the main list. */
export function isArchived(status: SessionStatus): boolean {
  return status === "archived";
}

/**
 * How long something has been going on, as a status detail: `12s`, `4m`,
 * `2h`. Coarse on purpose — a provisioning machine takes minutes and a
 * second-by-second counter would be motion without information.
 */
export function elapsedSince(sinceUnix: number, now: number): string {
  const seconds = Math.max(0, Math.floor(now / 1000) - sinceUnix);
  if (seconds < 60) {
    return `${seconds}s`;
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m`;
  }
  return `${Math.floor(seconds / 3600)}h`;
}
