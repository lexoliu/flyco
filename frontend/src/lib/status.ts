/**
 * The status a person reads off a session row.
 *
 * `SessionState` is a lifecycle enum — where the machine is — and it is not
 * what a user wants to know. "Active" answers nothing: an active session may
 * be thinking, waiting for an approval, or sitting idle since yesterday.
 * docs/ux.md §6 defines the statuses the UI actually shows, and this
 * module is the single derivation of them, so a row, a group heading and a
 * session header can never disagree.
 *
 * Telling `Working` from `Needs input` needs facts the lifecycle enum does
 * not carry — whether a turn is in flight, whether an approval is pending —
 * and they reach this module by two routes. The control plane maintains
 * them as `SessionSummary.activity`, which is what the home list is built
 * from; and the session page, which holds a relay open, folds the same two
 * facts out of the live event stream with {@link liveSignalsFrom}.
 *
 * The live stream wins where it says anything, because it is the newer of
 * the two — a turn that started a moment ago is on the socket before the
 * row it will be written to is read again. Where it says nothing, the
 * summary's `activity` answers, which is what makes a home list row say
 * `Working` at all.
 */
import type {
  InterruptedReason,
  PausedReason,
  SessionActivity,
  SessionState,
  SessionSummary,
  UsageLimitPause,
} from "../api/client";
import type { TimedEvent } from "../api/relay";
import { formatTimeOfDay } from "./dates";
import { formatDuration } from "./duration";
import { formatUsd } from "./money";

/** What a session looks like to the person who opened it. */
export type SessionStatus =
  | "provisioning"
  | "migrating"
  | "working"
  | "disconnected"
  | "needs_input"
  | "idle"
  | "paused"
  | "usage_limit"
  | "interrupted"
  | "failed"
  | "archived";

/** How a status is coloured; see docs/ux.md §6. */
export type StatusTone = "neutral" | "working" | "attention" | "failed" | "quiet" | "waiting";

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
  /**
   * Whether the session's machine has fallen off the room.
   *
   * Undefined until the room has said either way, which is most of the
   * time: only a page with a live socket ever hears it, and a list of
   * sessions never does.
   */
  machineOffline?: boolean;
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
 * A session waiting out a spent plan window (docs/ux.md §9.8).
 *
 * `Paused` is the same lifecycle state as a spent budget and reads as
 * nothing like it: a budget pause ends when the user raises a number, and
 * this one ends by itself at a time flyco already knows. So it is a status
 * of its own, with a tone of its own — the rail's dot is coloured, because
 * a session that will start working again on its own is worth seeing in the
 * corner of the eye, and it does not breathe, because right now nothing is
 * happening.
 */
const USAGE_LIMIT: StatusView = {
  status: "usage_limit",
  label: "Waiting on the plan",
  tone: "waiting",
  breathing: false,
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
 * Why a paused session is paused, as one of the reasons this build renders.
 *
 * `undefined` for a reason it has not heard of — a newer control plane — so
 * that an unknown token reads as the plain `Paused` of a spent budget rather
 * than as a state the page would then describe wrongly.
 */
function pausedFor(reason: PausedReason | null | undefined): PausedReason | undefined {
  return reason === "usage_limit" || reason === "budget" ? reason : undefined;
}

/**
 * What an active session reads as while nothing is holding its machine.
 *
 * Outranks every activity, because activity is inferred from frames that
 * have stopped arriving: a turn that started and never finished reads as
 * `Working` forever once the daemon is gone, which is the page telling the
 * user to keep waiting for something nobody is doing. Attention rather than
 * failure — the daemon reconnects on its own within a couple of heartbeats
 * — and not breathing, because nothing is happening (docs/ux.md §9.6).
 */
const DISCONNECTED: StatusView = {
  status: "disconnected",
  label: "Disconnected",
  tone: "attention",
  breathing: false,
  detail: "machine not reachable",
};

/** What an `active` session reads as, one view per activity. */
const ACTIVE: Record<SessionActivity, StatusView> = {
  working: { status: "working", label: "Working", tone: "working", breathing: true },
  needs_input: {
    status: "needs_input",
    label: "Needs input",
    tone: "attention",
    breathing: false,
  },
  idle: { status: "idle", label: "Idle", tone: "quiet", breathing: false },
};

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
  > &
    Partial<Pick<SessionSummary, "activity" | "paused_reason">>,
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
  if (session.state === "paused") {
    // Two unrelated waits share one lifecycle state, and only the reason
    // tells them apart. A reason this build has not heard of falls through
    // to the budget wording rather than putting a raw token on the page:
    // `Paused` with no clause is still true of any pause.
    return pausedFor(session.paused_reason) === "usage_limit" ? USAGE_LIMIT : BASE.paused;
  }
  if (session.state !== "active") {
    return BASE[session.state];
  }
  if (live.machineOffline === true) {
    return DISCONNECTED;
  }
  // A relay that has said something is the freshest answer there is, so it
  // is read first. One that has said nothing — a list with no socket, a
  // page whose catch-up has not landed — falls through to the fact the
  // control plane maintains, which is why a home row can say `Working`.
  if (live.turnInFlight === true) {
    return ACTIVE.working;
  }
  if (live.awaitingUser === true) {
    return ACTIVE.needs_input;
  }
  return ACTIVE[session.activity ?? "idle"];
}

/**
 * Reads the live facts a status needs off a session's relay stream.
 *
 * docs/ux.md §6 defines them in terms of what has happened, not of what the
 * lifecycle enum says, so both are folded from the events themselves:
 *
 * - **A turn is in flight** when a turn started and neither completed nor
 *   failed. Turns do not nest, so the last one seen is the answer.
 * - **The machine is off the room** when the room last said so. Only a
 *   page with a live socket ever hears this, so it is absent rather than
 *   false on a stream that has not been told.
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
  /**
   * What the room last said about the machine. `undefined` until it says
   * anything: a stream that predates the announcement must not be read as
   * a machine that is off.
   */
  let machineOffline: boolean | undefined;

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
      case "machine_connection":
        machineOffline = !event.connected;
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

  const signals: LiveSignals = {
    turnInFlight,
    awaitingUser: undecided.size > 0 || agentSpokeLast,
  };
  // Omitted rather than set to `undefined`: "the room has not said" and
  // "the room said the machine is on" are different answers, and a status
  // derived from the second when only the first is true would clear a
  // disconnection nobody reported the end of.
  return machineOffline === undefined ? signals : { ...signals, machineOffline };
}

/**
 * The one thing a stopped session offers to do about itself.
 *
 * A kind rather than a callback, because this module is pure: the page maps
 * each one onto the control it takes — `POST /v1/sessions/{id}/resume` for
 * a session that lost its machine, and the budget picker of docs/ux.md §9.1
 * for one that ran out of money.
 */
export type SessionNoticeAction =
  | { kind: "resume"; label: string }
  | { kind: "raise_budget"; label: string };

/**
 * What a session that is not running says for itself, above the transcript.
 *
 * A status pill in the header says `Failed` in one word, which is the right
 * size for a list and far too small for the page you opened to find out
 * what to do next: a failed session used to state its provider's error and
 * offer nothing, and an archived one said nothing at all while its composer
 * sat silently disabled (issue #133). So every state that is not running
 * says what happened, what it means, and — where there is one — carries the
 * way out on the notice itself.
 */
export interface SessionNotice {
  /** What happened, in the same words the header's pill uses. */
  title: string;
  /** What that means, and what to do next. */
  body: string;
  /** How it is coloured, from the status it describes. */
  tone: StatusTone;
  /** The one thing to do about it, where there is one. */
  action?: SessionNoticeAction;
}

/** What a resumable session's button says. */
const RESUME: SessionNoticeAction = { kind: "resume", label: "Resume" };

/** What a session paused on an exhausted budget offers instead. */
const RAISE_BUDGET: SessionNoticeAction = { kind: "raise_budget", label: "Raise budget" };

/** The facts a notice quotes beyond the status itself. */
export interface NoticeFacts {
  /** Why a failed session failed, in the provider's own words if it gave any. */
  failure: string | null | undefined;
  /** What the session may spend, in microdollars: the sum a pause is about. */
  budgetLimit: number | undefined;
  /**
   * What a session waiting on a spent plan window is waiting for.
   *
   * Only the session document carries it — a summary knows the reason but
   * not the window — so it is `null` in every other state and for a list
   * row.
   */
  usageLimit: UsageLimitPause | null | undefined;
  /** The clock, in milliseconds, for the countdown to the reset. */
  now: number;
}

/**
 * What a session waiting out a plan window says about the wait.
 *
 * Three facts, in the order they are asked for: which window, when it turns
 * over, and what the machine is doing meanwhile. The last is the one that
 * separates this state from every other pause — a session whose reset is
 * hours away has had its machine released and is costing nothing, and a user
 * who is not told that reads the whole wait as money burning.
 *
 * The countdown and the clock time are both given: the countdown answers
 * "should I wait for this", the clock time answers "when do I come back".
 */
function usageLimitBody(pause: UsageLimitPause, now: number): string {
  const resets = `The ${pause.window} usage limit on this session's plan is spent. It resets at ${formatTimeOfDay(pause.resets_at_unix)}, in ${formatDuration(pause.resets_at_unix - Math.floor(now / 1000))}.`;
  const machine =
    pause.resume_at_unix === null || pause.resume_at_unix === undefined
      ? "The machine is still running, so the session carries on the moment the window resets."
      : `The machine is stopped and costs nothing until then; flyco starts it again at ${formatTimeOfDay(pause.resume_at_unix)}.`;
  const next =
    pause.queued_message === null || pause.queued_message === undefined
      ? "Flyco then asks the agent to continue on your behalf."
      : `Your message is waiting and is sent then: ${pause.queued_message}`;
  return `${resets} ${machine} ${next}`;
}

/**
 * The notice for one session, or `null` while nothing is the matter.
 *
 * A running session says nothing here — the transcript is the page — and
 * neither does a first machine being built, because the provisioning
 * timeline in the transcript is already that story told better
 * (docs/ux.md §9.2).
 */
/**
 * `text` as a sentence of its own.
 *
 * A provider's reason ends where the provider ended it, which is often
 * without a full stop, and the sentence that follows it in the notice is
 * flyco's: the two must not run into one another.
 */
function sentence(text: string): string {
  const trimmed = text.trimEnd();
  return /[.!?]$/.test(trimmed) ? trimmed : `${trimmed}.`;
}

export function sessionNotice(view: StatusView, facts: NoticeFacts): SessionNotice | null {
  const title = view.detail === undefined ? view.label : `${view.label} · ${view.detail}`;
  const notice = (body: string, action?: SessionNoticeAction): SessionNotice => ({
    title,
    body,
    tone: view.tone,
    ...(action === undefined ? {} : { action }),
  });

  switch (view.status) {
    case "failed":
      return notice(
        `${sentence(facts.failure ?? "The session stopped and said nothing about why.")} Resuming builds the machine again and reopens the same conversation.`,
        RESUME,
      );
    case "interrupted":
      return notice(
        "The machine is gone and the session is waiting. Resuming puts it back on its own disk, with the conversation where it stopped.",
        RESUME,
      );
    case "migrating":
      // The one stopped state with nothing to offer and nothing to worry
      // about: flyco is already doing the thing a `Resume` would ask for.
      return notice(
        "Flyco is putting the session back on its own disk. Nothing is needed from you; the conversation continues where it stopped.",
      );
    case "archived":
      return notice(
        "This session is read-only and its machine has been released. Resuming builds a machine again and reopens the same conversation.",
        RESUME,
      );
    case "paused":
      // The sum, because it is the number the decision is about: a budget
      // above the spend is the only thing that ends this state (#134), and
      // the notice carries that control rather than sending the user to
      // look for it in the header.
      return notice(
        facts.budgetLimit === undefined
          ? "This session's budget is spent. Raise it to continue."
          : `The ${formatUsd(facts.budgetLimit)} budget is spent. Raise it to continue.`,
        RAISE_BUDGET,
      );
    case "usage_limit": {
      const pause = facts.usageLimit;
      // The pause and the state are two fields of one document, and the
      // control plane refuses to serve one without the other, so a state
      // this build read as `usage_limit` always has its pause here. Except
      // from a caller holding a list row: a summary carries the reason and
      // not the window, and such a notice says the part it knows.
      return pause === null || pause === undefined
        ? notice(
            "A usage limit on this session's plan is spent. Flyco continues the session by itself when the window resets.",
          )
        : notice(usageLimitBody(pause, facts.now));
    }
    case "disconnected":
      // Nothing to offer: the daemon dials back on its own within a couple
      // of heartbeats (docs/ux.md §9.6). The notice exists because the
      // page would otherwise look like an agent that stopped answering,
      // and a message typed into it waits rather than being lost.
      return notice(
        "The machine has dropped off the network. It reconnects on its own within a minute; anything you send waits for it.",
      );
    default:
      return null;
  }
}

/**
 * The states a session cannot be written to in.
 *
 * The page shows {@link sessionNotice} in the composer's place for these
 * rather than a box that would refuse: a disabled composer with a sentence
 * under it says the same thing twice and still leaves the reader looking at
 * somewhere to type (issue #133).
 *
 * Provisioning and migrating are deliberately absent. A message to a
 * session whose machine is still being built waits in the room's mailbox
 * and is worth sending.
 *
 * So is `usage_limit`, and for the same reason: a session waiting out a plan
 * window is coming back at a time flyco already knows, and what the user
 * wants to type is the next thing to do when it does. The control plane
 * holds that message against the pause and sends it instead of its own
 * continuation, so the composer stays open and says so.
 */
export const REFUSING: ReadonlySet<SessionStatus> = new Set([
  "failed",
  "interrupted",
  "paused",
  "archived",
]);

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
