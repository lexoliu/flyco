/**
 * The one stream every session event rides.
 *
 * `GET /v1/events` answers with a `text/event-stream` that carries every
 * session the signed-in user owns, multiplexed by the `session` field of
 * each `SessionEvent` envelope. This module owns that connection for the
 * whole app: it opens on the first session view that subscribes, demuxes
 * envelopes to per-session listeners, and stays up through navigation —
 * one stream per user, not one per tab.
 *
 * Reconnects resume through the server's reconnect buffer: every SSE
 * `id:` is a position in it, and the next connection passes it back as
 * `?after=`, receiving strictly what it missed. The buffer is a window,
 * not the record — a connect longer than the window sees a gap the
 * session's own `events?after=` pages heal. That healing is the
 * listener's job: `onState("live")` fires on every (re)open, which is the
 * cue to re-run catch-up.
 *
 * Liveness is measured in bytes, not events: the server comments `ping`
 * every fifteen seconds, so a connection that has been silent for
 * [`SILENCE_LIMIT_MS`] is dead whether or not the network said so — a
 * hung fetch body reads forever otherwise.
 *
 * Client→server traffic is not this module's concern: commands are
 * ordinary REST calls (see `src/api/client.ts`); there is no socket.
 */
import { createParser, type EventSourceParser } from "eventsource-parser";
import { createSignal, type Accessor } from "solid-js";
import { openUserStream } from "./client";
import { ApiProblem, NotImplementedError } from "./problem";
import { parseSessionEvent, type SessionEvent } from "./wire";

export type ConnectionState = "connecting" | "live" | "reconnecting" | "failed" | "closed";

/**
 * Six missed heartbeats. The server pings every 15s; a stream this quiet
 * is a dead connection the network never reported.
 */
const SILENCE_LIMIT_MS = 90_000;

/** How often the watchdog checks the last-byte clock. */
const WATCHDOG_MS = 5_000;

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
 * A stream that backs off and retries forever is right about a dropped
 * network, a restarting worker and a 5xx, and wrong about an account that
 * cannot open the stream: the page would spin a `Reconnecting…` pill on a
 * connection that will never open. So a 4xx problem document other than
 * the two above stops the stream for good, and so does a
 * [`NotImplementedError`] — a build without the route does not grow one
 * while the page is open.
 *
 * Everything that is not an [`ApiProblem`] — a [`NetworkError`], an
 * [`UnexpectedResponseError`], a body that ended — is retried, because
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

/** What a session view registers to follow its envelopes. */
export interface SessionListener {
  /** One envelope for the session, as it arrived. */
  event(envelope: SessionEvent): void;
  /**
   * The stream's lifecycle moved. `live` fires on every open — the first
   * and every reconnect — which is the cue to re-run catch-up.
   */
  state(state: "connecting" | "live" | "reconnecting"): void;
  /** The stream stopped for good. */
  failed(error: unknown): void;
}

/** The shared stream's public face. */
export interface UserStream {
  /** Connection lifecycle: never a silently dead stream. */
  state: Accessor<ConnectionState>;
  /** Why the stream stopped, when `state` is `failed`; `null` otherwise. */
  failure: Accessor<unknown>;
  /**
   * Registers `listener` for one session's envelopes. The stream opens on
   * the first subscription and closes on the last unsubscribe; the
   * returned function is the unsubscribe.
   */
  subscribe(session: string, listener: SessionListener): () => void;
}

/** Opens the stream. Injectable for tests; defaults to the real route. */
export type OpenStream = (after: number | undefined, signal: AbortSignal) => Promise<Response>;

export interface UserStreamOptions {
  open?: OpenStream;
  backoff?: BackoffOptions;
  /** Override [`SILENCE_LIMIT_MS`], for tests. */
  silenceLimitMs?: number;
}

/**
 * Creates the stream manager. The app uses the shared instance from
 * {@link userStream}; tests build their own with an injected `open`.
 */
export function createUserStream(options: UserStreamOptions = {}): UserStream {
  const open = options.open ?? openUserStream;
  const silenceLimitMs = options.silenceLimitMs ?? SILENCE_LIMIT_MS;
  const [state, setState] = createSignal<ConnectionState>("closed");
  const [failure, setFailure] = createSignal<unknown>(null);

  const listeners = new Map<string, Set<SessionListener>>();
  /** The buffer position this client has seen; the reconnect's `?after=`. */
  let lastId: number | undefined;
  let attempt = 0;
  let running = false;
  let abort: AbortController | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;

  function dispatch(envelope: SessionEvent): void {
    const set = listeners.get(envelope.session);
    if (set === undefined) {
      return;
    }
    for (const listener of set) {
      listener.event(envelope);
    }
  }

  function announce(next: "connecting" | "live" | "reconnecting"): void {
    for (const set of listeners.values()) {
      for (const listener of set) {
        listener.state(next);
      }
    }
  }

  /**
   * Reads one open response to its end — the connection's drop, the
   * watchdog's abort, or a parse failure all land here as a throw or a
   * return, and all of them mean the same thing: connect again.
   */
  async function pump(response: Response): Promise<void> {
    const body = response.body;
    if (body === null) {
      throw new Error("the event stream answered without a body");
    }
    let silenced = false;
    let lastByteAt = Date.now();
    const watchdog = setInterval(() => {
      if (Date.now() - lastByteAt > silenceLimitMs) {
        silenced = true;
        abort?.abort();
      }
    }, WATCHDOG_MS);
    const parser: EventSourceParser = createParser({
      onEvent(event) {
        if (event.id !== undefined) {
          lastId = Number(event.id);
        }
        dispatch(parseSessionEvent(JSON.parse(event.data)));
      },
      onComment() {
        // A ping is data: it already moved lastByteAt by arriving.
      },
    });
    const reader = body.getReader();
    const decoder = new TextDecoder();
    try {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) {
          return;
        }
        lastByteAt = Date.now();
        parser.feed(decoder.decode(chunk.value, { stream: true }));
      }
    } catch (error) {
      if (silenced) {
        throw new Error("the event stream went silent", { cause: error });
      }
      throw error;
    } finally {
      clearInterval(watchdog);
      reader.releaseLock();
    }
  }

  async function connect(): Promise<void> {
    if (!running) {
      return;
    }
    const controller = new AbortController();
    abort = controller;
    setState(attempt === 0 ? "connecting" : "reconnecting");
    announce(attempt === 0 ? "connecting" : "reconnecting");
    try {
      const response = await open(lastId, controller.signal);
      if (!running) {
        return;
      }
      attempt = 0;
      setFailure(null);
      setState("live");
      announce("live");
      await pump(response);
      if (!running) {
        return;
      }
      // A body that ended is a dropped connection: the server never ends
      // this stream on purpose.
      scheduleReconnect();
    } catch (error) {
      // `running` alone separates the two aborts: dispose clears it, the
      // silence watchdog does not — a watchdog kill is a dead connection,
      // which is exactly the case to reconnect.
      if (!running) {
        return;
      }
      if (isDefinitiveFailure(error)) {
        setFailure(error);
        setState("failed");
        running = false;
        for (const set of listeners.values()) {
          for (const listener of set) {
            listener.failed(error);
          }
        }
        return;
      }
      scheduleReconnect();
    }
  }

  function scheduleReconnect(): void {
    if (!running) {
      return;
    }
    setState("reconnecting");
    announce("reconnecting");
    const delay = nextBackoffDelay(attempt, options.backoff);
    attempt += 1;
    reconnectTimer = setTimeout(() => {
      void connect();
    }, delay);
  }

  function ensureRunning(): void {
    if (running) {
      return;
    }
    running = true;
    void connect();
  }

  function stop(): void {
    running = false;
    if (reconnectTimer !== undefined) {
      clearTimeout(reconnectTimer);
      reconnectTimer = undefined;
    }
    abort?.abort();
    abort = null;
    setState("closed");
  }

  function subscribe(session: string, listener: SessionListener): () => void {
    let set = listeners.get(session);
    if (set === undefined) {
      set = new Set();
      listeners.set(session, set);
    }
    set.add(listener);
    ensureRunning();
    return () => {
      const set = listeners.get(session);
      set?.delete(listener);
      if (set !== undefined && set.size === 0) {
        listeners.delete(session);
      }
      if (listeners.size === 0) {
        stop();
      }
    };
  }

  return { state, failure, subscribe };
}

/** The stream the app shares. */
let shared: UserStream | null = null;

/**
 * The one stream for this page's user — created on first use, kept for
 * the app's lifetime. Session views subscribe through their relay rather
 * than touching this directly.
 */
export function userStream(): UserStream {
  shared ??= createUserStream();
  return shared;
}
