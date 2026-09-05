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
  SessionActivity,
  SessionState,
  SessionSummary,
} from "../api/client";
import type { TimedEvent } from "../api/relay";
import { formatUsd } from "./money";

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
    Partial<Pick<SessionSummary, "activity">>,
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
    default:
      return null;
  }
}

/**
 * Why the composer will not send, or `null` when it will.
 *
 * A disabled button with no explanation is the page refusing without
 * saying so (issue #133). A session with no machine is the one thing this
 * covers: a message to a session whose machine is still being built waits
 * in the room's mailbox and is worth sending, so provisioning and migrating
 * are not refused.
 */
export function composerRefusal(status: SessionStatus): string | null {
  switch (status) {
    case "failed":
      return "This session failed. Resume it to pick the conversation back up.";
    case "interrupted":
      return "This session has no machine right now. Resume it to send a message.";
    case "paused":
      return "This session is paused: its budget is spent. Raise it to continue.";
    case "archived":
      return "This session is archived and read-only.";
    default:
      return null;
  }
}

/**
 * Order the home page groups sessions in (docs/ux.md §5): `Needs input`,
 * then `Working`, then `Idle`, then everything that is neither running nor
 * waiting on anybody. Archived is a tab of its own and is therefore last.
 *
 * The three the user acts on lead, in the order they ask to be acted on: a
 * blocked agent is waiting on them right now, a working one is not, and an
 * idle one is a thread to pick back up. What follows is the machine's own
 * business — a failed provision, a build, a migration, a pause — which the
 * user reads when they go looking rather than first thing.
 *
 * It orders statuses, not headings: {@link SESSION_GROUPS} is what the list
 * is divided by, and this is what decides who leads inside a division that
 * holds more than one status.
 */
export const STATUS_ORDER: readonly SessionStatus[] = [
  "needs_input",
  "working",
  "idle",
  "failed",
  "provisioning",
  "migrating",
  "paused",
  "interrupted",
  "archived",
];

/**
 * The headings the home list is divided by (docs/ux.md §5).
 *
 * Four, not nine. A heading is a division of a list — it earns its line by
 * telling the user which pile to read next — and `FAILED` over a single row
 * that already says `Failed` is a heading that divides nothing. So the three
 * piles a user acts on get their own heading, and everything that is the
 * machine's own business shares one; the row still carries its own status,
 * which is where `Failed`, `Provisioning` and `Paused` are read.
 */
export type SessionGroup = "needs_input" | "working" | "idle" | "other" | "archived";

/** The order the groups appear in, and the heading each one carries. */
export const SESSION_GROUPS: readonly { group: SessionGroup; heading: string }[] = [
  { group: "needs_input", heading: "Needs input" },
  { group: "working", heading: "Working" },
  { group: "idle", heading: "Idle" },
  { group: "other", heading: "Other" },
  { group: "archived", heading: "Archived" },
];

/**
 * Which heading a status is read under.
 *
 * The three statuses a user acts on name themselves; archived is a tab of
 * its own. Everything else — a machine being built or put back, a run that
 * failed, a session paused or interrupted — is one pile.
 */
export function groupOf(status: SessionStatus): SessionGroup {
  switch (status) {
    case "needs_input":
    case "working":
    case "idle":
    case "archived":
      return status;
    default:
      return "other";
  }
}

/** One heading and the rows under it. */
export interface SessionGroupView<T> {
  group: SessionGroup;
  /**
   * The heading, or `null` when the whole list is one pile and a heading
   * would name it rather than divide it.
   */
  heading: string | null;
  rows: T[];
}

/**
 * Divides a list of sessions into the headings of docs/ux.md §5.
 *
 * Generic over the row, because what a caller carries alongside a status is
 * its business; all this needs is the status each row reads as. Groups that
 * hold nothing are left out, rows keep the order they came in except that a
 * shared heading sorts its statuses by {@link STATUS_ORDER}, and a list with
 * one group gets no heading at all.
 */
export function groupSessions<T>(
  rows: readonly { status: SessionStatus; session: T }[],
): SessionGroupView<T>[] {
  const byGroup = new Map<SessionGroup, { status: SessionStatus; session: T }[]>();
  for (const row of rows) {
    const group = groupOf(row.status);
    const held = byGroup.get(group);
    if (held === undefined) {
      byGroup.set(group, [row]);
    } else {
      held.push(row);
    }
  }

  const groups = SESSION_GROUPS.flatMap(({ group, heading }) => {
    const held = byGroup.get(group);
    if (held === undefined) {
      return [];
    }
    // Sorting is only ever felt in `Other`, where several statuses share a
    // heading and a failure should be read before a pause. `sort` is stable,
    // so rows of one status keep the order the caller listed them in.
    const rows = [...held]
      .sort((left, right) => STATUS_ORDER.indexOf(left.status) - STATUS_ORDER.indexOf(right.status))
      .map((row) => row.session);
    return [{ group, heading, rows }];
  });

  return groups.length === 1
    ? groups.map((only) => ({ ...only, heading: null }))
    : groups;
}

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
