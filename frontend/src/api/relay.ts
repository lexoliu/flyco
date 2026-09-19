/**
 * The live session view's event pipe.
 *
 * The transport is one per-user SSE stream (`GET /v1/events`, owned by
 * `src/api/events.ts`) plus ordinary REST for client→server commands —
 * there is no socket. This module is the session-shaped view over that:
 * a relay follows one session's envelopes off the shared stream, runs
 * `GET /v1/sessions/{id}/events?after=` catch-up for the recorded past,
 * and sends the composer's commands through each one's REST route.
 *
 * Catch-up and live delivery are two independent paths to the same fact,
 * so [`EventStream`] dedupes between them:
 *
 * 1. A reconnect's catch-up call can return an event that was already
 *    shown live, moments before the stream dropped — the seq log and the
 *    live broadcast are two deliveries of the same fact. Sequenced
 *    envelopes carry their position (`SessionEvent.seq`), so a live frame
 *    at or below the catch-up cursor is skipped outright. The cursor
 *    cannot do the other direction: a live frame arrives ahead of the
 *    catch-up page that records it, so live frames are also remembered by
 *    canonicalized content key ({@link canonicalKey}, which sorts object
 *    keys) and catch-up rows check against that. Comparing the two
 *    deliveries byte for byte does not work: a live envelope is
 *    `serde_json` output of the event struct, with its `type` tag first,
 *    while a catch-up row is read back into a `serde_json::Value` and
 *    re-serialized out of a `BTreeMap`, so its keys come back in
 *    alphabetical order. The buffer is bounded, not unbounded: a burst of
 *    more than [`RECENT_WINDOW`] live events between two reconnects could
 *    in principle scroll a duplicate back into view. That trade-off
 *    (bounded memory vs. perfect dedup over an unbounded gap) is
 *    deliberate; widen the window if it is ever observed in practice.
 *
 *    The dedup is deliberately one-directional. Catch-up rows are checked
 *    against live frames only — two stored rows with identical content are
 *    two events (each took its own `seq`), so a transcript that twice
 *    printed `**` replays both. And a live frame is never compared against
 *    anything except the cursor: the room emits each fact to the stream
 *    exactly once, so an identical live payload is a *new* occurrence — a
 *    second identical `assistant_delta` is real output, and a repeated
 *    `machine_connection` is a real transition. Treating live content
 *    equality as duplication ate stream chunks whole.
 * 2. Anything that lands on the server between the last catch-up page and
 *    the stream actually delivering would otherwise be missed outright.
 *    [`createSessionRelay`] closes that window by running catch-up every
 *    time the stream reports `live` — the first open and every reconnect
 *    alike, which is also what heals a gap the reconnect buffer swept.
 */
import { createSignal, type Accessor } from "solid-js";
import {
  compactSession,
  contextSession,
  getSessionEvents,
  interruptSession,
  resizeSessionTerminal,
  runShellCommand,
  sendMessage,
  sendTerminalInput,
  type StoredEvent,
} from "./client";
import { userStream, type ConnectionState, type UserStream } from "./events";
import {
  parseClientEvent,
  type ClientCommand,
  type ClientEvent,
  type SessionEvent,
} from "./wire";

export type { ConnectionState } from "./events";

const RECENT_WINDOW = 256;

/**
 * The most pages one catch-up pass may read. A healthy server answers
 * `more: false` long before this; past it — or a page that fails to move
 * the cursor — the control plane is paging forever, which on a free-plan
 * request budget is a bug that spends the day.
 */
const MAX_CATCH_UP_PAGES = 200;

/**
 * A key for an event that does not depend on how its JSON was ordered.
 *
 * Two deliveries of the same fact reach this client through two
 * serializers, and only the values are guaranteed to agree. Sorting the
 * keys makes the comparison about the event rather than about whichever
 * code path emitted it.
 */
export function canonicalKey(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalKey).join(",")}]`;
  }
  if (typeof value === "object" && value !== null) {
    const entries = Object.entries(value as Record<string, unknown>).sort(
      ([left], [right]) => (left < right ? -1 : left > right ? 1 : 0),
    );
    return `{${entries
      .map(([key, entry]) => `${JSON.stringify(key)}:${canonicalKey(entry)}`)
      .join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

/**
 * One event and when it happened.
 *
 * The wire protocol times almost nothing: a `turn_started` frame says which
 * turn started, never when. The room does record an arrival time per stored
 * event (`StoredEvent.at_unix`), and a live frame arrives now, so the two
 * deliveries between them can date every event — which is what a
 * `Worked for 8m 23s` footer and a provisioning timeline are computed from
 * (docs/ux.md §9.2). Carrying the time beside the event rather than inside
 * it keeps `ClientEvent` an exact mirror of the Rust enum.
 */
export interface TimedEvent {
  /** The event itself. */
  event: ClientEvent;
  /** When it was recorded or received, seconds since the Unix epoch. */
  atUnix: number;
}

/** The clock a stream dates live frames against. Injectable for tests. */
export type Clock = () => number;

const systemClock: Clock = () => Math.floor(Date.now() / 1000);

/**
 * Turns catch-up pages and live envelopes into a deduplicated, dated
 * [`ClientEvent`] sequence. Pure and framework-free, so it is unit-testable
 * without a stream or a fetch mock.
 */
export class EventStream {
  private lastSeq: number | null = null;
  /**
   * Canonical keys of events already shown via a live frame.
   *
   * Catch-up checks against this set and nothing else does: it is the one
   * case where the same fact is genuinely delivered twice, because the seq
   * log and the broadcast are independent deliveries. See the module
   * comment for why neither path dedupes against itself.
   */
  private readonly liveKeys: string[] = [];
  private readonly clock: Clock;

  constructor(clock: Clock = systemClock) {
    this.clock = clock;
  }

  /** The highest `seq` consumed so far, for the next catch-up page's `after`. */
  get cursor(): number | null {
    return this.lastSeq;
  }

  /**
   * Feeds one catch-up page (events ascending by `seq`). Skips anything at
   * or below the cursor already reached, and anything already shown
   * (because it arrived live first).
   */
  ingestCatchUp(events: StoredEvent[]): TimedEvent[] {
    const out: TimedEvent[] = [];
    for (const stored of events) {
      if (this.lastSeq !== null && stored.seq <= this.lastSeq) {
        continue;
      }
      this.lastSeq = stored.seq;
      if (!this.liveKeys.includes(canonicalKey(stored.event))) {
        out.push({ event: parseClientEvent(stored.event), atUnix: stored.at_unix });
      }
    }
    return out;
  }

  /**
   * Feeds one live envelope off the stream. Returns `null` for a replay —
   * a sequenced event the record already served (`seq` at or below the
   * cursor). Everything else is a new occurrence — two identical payloads
   * are two events, not a redelivery — so it is never dropped on content.
   * It only records the event's key, which is what lets a later catch-up
   * recognise the same fact in the seq log.
   */
  ingestLive(envelope: SessionEvent): TimedEvent | null {
    if (
      envelope.seq !== null &&
      this.lastSeq !== null &&
      envelope.seq <= this.lastSeq
    ) {
      return null;
    }
    this.remember(canonicalKey(envelope.event));
    return { event: envelope.event, atUnix: this.clock() };
  }

  /** Records `key` as shown live, bounded to [`RECENT_WINDOW`]. */
  private remember(key: string): void {
    this.liveKeys.push(key);
    if (this.liveKeys.length > RECENT_WINDOW) {
      this.liveKeys.shift();
    }
  }
}

export interface SessionRelay {
  /** Connection lifecycle: never a silently dead stream. */
  state: Accessor<ConnectionState>;
  /**
   * Why the relay stopped, when it stopped for good; `null` otherwise.
   *
   * Set exactly when {@link state} is `failed`, so the page can render the
   * problem itself instead of showing a pill that promises a reconnection
   * that is never coming.
   */
  failure: Accessor<unknown>;
  /** Every event shown so far, oldest first, deduplicated and dated. */
  events: Accessor<TimedEvent[]>;
  /**
   * Sends a client-initiated command, each through its own REST route —
   * `/messages`, `/shell`, `/terminal/input`, `/terminal/resize`,
   * `/interrupt`, `/compact`, `/context` as the variant names it. The call
   * does not depend on stream state: the room decides what a daemon that
   * is away does with it, and the answer arrives back on the stream.
   */
  send(command: ClientCommand): Promise<void>;
  /** Tears the relay down for good: no further catch-up, no subscription. */
  dispose(): void;
}

export interface CreateSessionRelayOptions {
  /** The shared stream. Injectable for tests; defaults to the app's. */
  stream?: UserStream;
}

/**
 * Follows one session on the shared stream: catch-up, envelopes demuxed
 * by session, REST for commands — all driven from Solid signals a
 * component can render directly.
 */
export function createSessionRelay(
  sessionId: string,
  options: CreateSessionRelayOptions = {},
): SessionRelay {
  const [state, setState] = createSignal<ConnectionState>("connecting");
  const [failure, setFailure] = createSignal<unknown>(null);
  const [events, setEvents] = createSignal<TimedEvent[]>([]);
  const stream = new EventStream();
  const user = options.stream ?? userStream();

  let disposed = false;

  function pushEvent(event: TimedEvent): void {
    setEvents((prev) => [...prev, event]);
  }

  async function catchUp(): Promise<void> {
    let more = true;
    let pages = 0;
    while (more && !disposed) {
      const before = stream.cursor;
      const page = await getSessionEvents(sessionId, before ?? undefined);
      for (const event of stream.ingestCatchUp(page.events)) {
        pushEvent(event);
      }
      more = page.more;
      pages += 1;
      if (more && (pages >= MAX_CATCH_UP_PAGES || stream.cursor === before)) {
        throw new Error(
          "the control plane kept paging session events without advancing the cursor",
        );
      }
    }
  }

  function onFailure(error: unknown): void {
    if (disposed) {
      return;
    }
    setFailure(error);
    setState("failed");
  }

  const unsubscribe = user.subscribe(sessionId, {
    event(envelope) {
      const timed = stream.ingestLive(envelope);
      if (timed !== null) {
        pushEvent(timed);
      }
    },
    state(next) {
      if (next === "live") {
        // Every open — the first and every reconnect — closes the window
        // between the last catch-up page and the stream's delivery, and
        // heals whatever the reconnect buffer swept while we were away.
        catchUp()
          .then(() => {
            if (!disposed) {
              setState("live");
            }
          })
          .catch(onFailure);
        return;
      }
      setState(next);
    },
    failed: onFailure,
  });

  // The recorded past renders whether or not the stream has opened yet.
  catchUp().catch(onFailure);

  async function send(command: ClientCommand): Promise<void> {
    switch (command.type) {
      case "user_message":
        return sendMessage(sessionId, command.text);
      case "shell_command":
        return runShellCommand(sessionId, command.command);
      case "terminal_input":
        return sendTerminalInput(sessionId, command.data);
      case "terminal_resize":
        return resizeSessionTerminal(sessionId, command.cols, command.rows);
      case "interrupt":
        return interruptSession(sessionId);
      case "compact":
        return compactSession(sessionId);
      case "context_usage":
        return contextSession(sessionId);
    }
  }

  function dispose(): void {
    disposed = true;
    unsubscribe();
    setState("closed");
  }

  return { state, failure, events, send, dispose };
}
