/**
 * The live session view's WebSocket manager.
 *
 * Protocol notes (see docs/ARCHITECTURE.md's "Session relay" section):
 *
 * - Browser auth is a single-use 60-second relay ticket
 *   (`POST /v1/sessions/{id}/relay-ticket`), presented as `?ticket=` on the
 *   socket URL — browsers cannot set headers on a WebSocket handshake, so a
 *   fresh ticket is minted on every connect and every reconnect.
 * - Catch-up (`GET /v1/sessions/{id}/events?after=<seq>`) replays recorded
 *   history; only `Harness`-kind events are ever persisted with a `seq`,
 *   everything else is live-only. Live frames carry no `seq` at all.
 * - Reconnection is capped exponential backoff with full jitter (1s→60s),
 *   and re-sends `Hello` on the daemon's side; from a browser's side that
 *   means re-running catch-up and minting a fresh ticket every time.
 *
 * Two dedup problems fall out of that shape, both handled by [`EventStream`]:
 *
 * 1. A reconnect's catch-up call can return a `Harness` event that was
 *    already shown live, moments before the socket dropped — the seq log
 *    and the live broadcast are two independent deliveries of the same
 *    fact. Guarded by a bounded ring buffer of already-shown
 *    events' canonicalized JSON — {@link canonicalKey}, which sorts object
 *    keys. Comparing the two deliveries byte for byte does not work: a live
 *    frame is `serde_json` output of the event struct, with its `type` tag
 *    first, while a catch-up row is read back into a `serde_json::Value`
 *    and re-serialized out of a `BTreeMap`, so its keys come back in
 *    alphabetical order. Nothing ever matched, and every event was rendered
 *    twice: a doubled answer, and a phantom tool row that never finished.
 *    The buffer is bounded, not unbounded: a
 *    burst of more than [`RECENT_WINDOW`] live events between two
 *    reconnects could in principle scroll a duplicate back into view. That
 *    trade-off (bounded memory vs. perfect dedup over an unbounded gap) is
 *    deliberate; widen the window if it is ever observed in practice.
 * 2. Anything that lands on the server between the last catch-up page and
 *    the socket actually being subscribed — including the time spent
 *    minting a ticket and completing the handshake — would otherwise be
 *    missed outright. [`createSessionRelay`] closes that window by running
 *    catch-up a second time the moment the socket reports `open`, from
 *    wherever the cursor is; the same dedup guard keeps that second pass
 *    from re-showing anything the first pass (or a live frame that beat it)
 *    already rendered.
 */
import { createSignal, type Accessor } from "solid-js";
import { createRelayTicket, getSessionEvents, apiWebSocketUrl, type StoredEvent } from "./client";
import { ApiProblem, NotImplementedError } from "./problem";
import { parseClientEvent, type ClientCommand, type ClientEvent } from "./wire";

const RECENT_WINDOW = 256;

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
 * Turns catch-up pages and live socket text into a deduplicated, dated
 * [`ClientEvent`] sequence. Pure and framework-free, so it is unit-testable
 * without a socket or a fetch mock.
 */
export class EventStream {
  private lastSeq: number | null = null;
  private readonly recentKeys: string[] = [];
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
      if (this.remember(canonicalKey(stored.event))) {
        out.push({ event: parseClientEvent(stored.event), atUnix: stored.at_unix });
      }
    }
    return out;
  }

  /**
   * Feeds one live frame's raw WebSocket text. Returns `null` when it
   * duplicates an event already shown (from catch-up or an earlier live
   * frame) rather than emitting it twice.
   */
  ingestLive(raw: string): TimedEvent | null {
    const parsed: unknown = JSON.parse(raw);
    if (!this.remember(canonicalKey(parsed))) {
      return null;
    }
    return { event: parseClientEvent(parsed), atUnix: this.clock() };
  }

  /** Records `key` as shown; returns whether it was new. */
  private remember(key: string): boolean {
    if (this.recentKeys.includes(key)) {
      return false;
    }
    this.recentKeys.push(key);
    if (this.recentKeys.length > RECENT_WINDOW) {
      this.recentKeys.shift();
    }
    return true;
  }
}

export interface BackoffOptions {
  /** Delay for the first retry, before jitter. Default 1000ms. */
  baseMs?: number;
  /** The cap the exponential grows toward. Default 60000ms. */
  maxMs?: number;
  /** Source of randomness in `[0, 1)`, for deterministic tests. Default `Math.random`. */
  random?: () => number;
}

/**
 * Capped exponential backoff with full jitter: `random(0, min(max, base *
 * 2^attempt))`. `attempt` is 0 for the first reconnect.
 */
export function nextBackoffDelay(attempt: number, options: BackoffOptions = {}): number {
  const base = options.baseMs ?? 1000;
  const max = options.maxMs ?? 60_000;
  const random = options.random ?? Math.random;
  const cap = Math.min(max, base * 2 ** attempt);
  return random() * cap;
}

export type ConnectionState = "connecting" | "live" | "reconnecting" | "failed" | "closed";

/**
 * Statuses that a retry can still get past.
 *
 * `401` is an expired session token, which the next request re-mints; `429`
 * is the control plane asking for a slower client. Every other 4xx is a
 * statement about the request itself — this session does not exist, or it
 * is not this account's — and repeating it cannot change the answer.
 */
const RETRYABLE_CLIENT_STATUSES: readonly number[] = [401, 429];

/**
 * Whether a failure means "not now" or "not ever".
 *
 * A relay that backs off and retries forever is right about a dropped
 * network, a restarting worker and a 5xx, and wrong about a session that
 * does not exist: the page would spin a `Reconnecting…` pill on a socket
 * that will never open (issue #137). So a 4xx problem document other than
 * the two above stops the relay for good, and so does a
 * [`NotImplementedError`] — a build without the relay does not grow one
 * while the page is open.
 *
 * Everything that is not an [`ApiProblem`] — a [`NetworkError`], an
 * [`UnexpectedResponseError`], a socket that closed — is retried, because
 * none of them is the server saying the request was wrong.
 */
export function isDefinitiveFailure(error: unknown): boolean {
  if (!(error instanceof ApiProblem)) {
    return false;
  }
  if (error instanceof NotImplementedError) {
    return true;
  }
  return (
    error.status >= 400 &&
    error.status < 500 &&
    !RETRYABLE_CLIENT_STATUSES.includes(error.status)
  );
}

export interface SessionRelay {
  /** Connection lifecycle: never a silently dead socket. */
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
   * Sends a client-initiated command over the live socket. Throws — fast
   * fail, no silent drop — when the socket is not currently live; the UI
   * must gate input on `state() === "live"` instead of racing this.
   */
  send(command: ClientCommand): void;
  /** Tears the relay down for good: no further reconnects. */
  dispose(): void;
}

export interface CreateSessionRelayOptions {
  /** Injectable WebSocket constructor, for tests. Defaults to `WebSocket`. */
  webSocketImpl?: typeof WebSocket;
  backoff?: BackoffOptions;
}

const OPEN = 1;

/**
 * Opens (and keeps open) the live relay for one session: catch-up, ticket,
 * socket, reconnect-with-backoff, all driven from Solid signals a component
 * can render directly.
 */
export function createSessionRelay(sessionId: string, options: CreateSessionRelayOptions = {}): SessionRelay {
  const WebSocketImpl = options.webSocketImpl ?? WebSocket;
  const [state, setState] = createSignal<ConnectionState>("connecting");
  const [failure, setFailure] = createSignal<unknown>(null);
  const [events, setEvents] = createSignal<TimedEvent[]>([]);
  const stream = new EventStream();

  let socket: WebSocket | null = null;
  let attempt = 0;
  let disposed = false;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;

  function pushEvent(event: TimedEvent): void {
    setEvents((prev) => [...prev, event]);
  }

  async function catchUp(): Promise<void> {
    let more = true;
    while (more && !disposed) {
      const page = await getSessionEvents(sessionId, stream.cursor ?? undefined);
      for (const event of stream.ingestCatchUp(page.events)) {
        pushEvent(event);
      }
      more = page.more;
    }
  }

  /**
   * What a failed connect attempt does next: back off, or stop.
   *
   * The one place the two outcomes are decided, so a failure raised by the
   * first catch-up, by the ticket, or by the catch-up the socket's `open`
   * handler fires is treated identically.
   */
  function onFailure(error: unknown): void {
    if (disposed) {
      return;
    }
    if (!isDefinitiveFailure(error)) {
      scheduleReconnect();
      return;
    }
    socket?.close();
    socket = null;
    setFailure(error);
    setState("failed");
  }

  function scheduleReconnect(): void {
    if (disposed) {
      return;
    }
    socket = null;
    setState("reconnecting");
    const delay = nextBackoffDelay(attempt, options.backoff);
    attempt += 1;
    reconnectTimer = setTimeout(() => {
      void connect();
    }, delay);
  }

  async function connect(): Promise<void> {
    if (disposed) {
      return;
    }
    setState(attempt === 0 ? "connecting" : "reconnecting");
    try {
      await catchUp();
      if (disposed) {
        return;
      }
      const ticket = await createRelayTicket(sessionId);
      if (disposed) {
        return;
      }
      const url = apiWebSocketUrl(`/v1/sessions/${sessionId}/relay/client`);
      url.searchParams.set("ticket", ticket.ticket);
      const ws = new WebSocketImpl(url.toString());
      socket = ws;

      ws.addEventListener("open", () => {
        attempt = 0;
        setState("live");
        // Closes the gap between the catch-up page just above and the
        // socket actually being subscribed: anything that landed on the
        // server during ticket-minting and the handshake is otherwise
        // never seen. ingestCatchUp's dedup keeps this from re-showing
        // anything a live frame already rendered in the meantime.
        catchUp().catch(onFailure);
      });
      ws.addEventListener("message", (message) => {
        if (typeof message.data !== "string") {
          throw new Error("relay socket sent a non-text frame");
        }
        const event = stream.ingestLive(message.data);
        if (event !== null) {
          pushEvent(event);
        }
      });
      ws.addEventListener("close", () => {
        // A socket that closes after a definitive failure has already been
        // accounted for; reconnecting here would undo the stop.
        if (state() !== "failed") {
          scheduleReconnect();
        }
      });
    } catch (error) {
      onFailure(error);
    }
  }

  function send(command: ClientCommand): void {
    if (socket === null || socket.readyState !== OPEN) {
      throw new Error("cannot send on the relay: the socket is not live");
    }
    socket.send(JSON.stringify(command));
  }

  function dispose(): void {
    disposed = true;
    if (reconnectTimer !== undefined) {
      clearTimeout(reconnectTimer);
    }
    socket?.close();
    socket = null;
    setState("closed");
  }

  void connect();

  return { state, failure, events, send, dispose };
}
