/**
 * The desktop video pipe: one SSE stream per watching panel.
 *
 * `GET /v1/sessions/{id}/desktop/stream` answers with a `text/event-stream`
 * whose frames are `hello` (the watcher lease this connection minted, and
 * whether anyone already drives), `chunk` (one encoded AV1 temporal unit,
 * `data` base64, `keyframe` where a decoder can start from it) and
 * `resync` (the room's tail was cut ahead of this watcher — the decoder
 * must wait for the next keyframe, which the daemon is asked to send).
 *
 * The stream *is* the audience: the room keeps the watcher lease alive
 * while it is open and the daemon encodes only while a lease lives, so a
 * backgrounded tab that closes the stream is what turns the encoder off.
 * This module owns that connection for the one panel that created it —
 * reconnects mint a new watcher rather than resuming the old one, because
 * a decoder that missed a GOP has to start at a keyframe anyway.
 *
 * Commands the panel sends — takeover and input — are ordinary REST calls
 * in `src/api/client.ts`, named by the watcher id the `hello` announced.
 */
import { createParser, type EventSourceParser } from "eventsource-parser";
import { createSignal, type Accessor } from "solid-js";
import { openDesktopStream } from "./client";
import {
  isDefinitiveFailure,
  nextBackoffDelay,
  STABLE_MS,
  type BackoffOptions,
  type ConnectionState,
} from "./events";
import { ApiProblem } from "./problem";

export { STABLE_MS };

/**
 * What a `hello` frame announces: the watcher lease this connection holds
 * (which takeover and input calls name) and whether the screen is already
 * driven — so the panel opens on the truth rather than on a refusal.
 */
export interface DesktopHello {
  watcher: number;
  takeover: boolean;
}

/** What a desktop feed tells its panel. */
export interface DesktopListener {
  /** One encoded temporal unit; `data` is the raw AV1 bytes. */
  chunk(data: Uint8Array, keyframe: boolean): void;
  /** The stream's tail was cut — drop everything until the next keyframe. */
  resync(): void;
  /** A new watcher lease was minted (the first frame of every connect). */
  hello(hello: DesktopHello): void;
}

/** The panel's view of its desktop stream. */
export interface DesktopFeed {
  /** Connection lifecycle: never a silently dead stream. */
  state: Accessor<ConnectionState>;
  /**
   * The watcher lease the open connection holds, `null` between connects.
   * It changes on every reconnect — a takeover taken under the old id died
   * with it, which is why the id is a signal rather than a field.
   */
  watcher: Accessor<number | null>;
  /** Why the feed stopped, when it stopped for good; `null` otherwise. */
  failure: Accessor<unknown>;
  /** Tears the feed down for good: the watcher lease lapses with it. */
  dispose(): void;
}

export interface DesktopFeedOptions {
  /** The frame listener — the decoder the panel wires in. */
  listener: DesktopListener;
  backoff?: BackoffOptions;
  /**
   * Pause while the tab is hidden (default true): a covered screen is an
   * audience of nobody, and closing the stream is what stops the encoder.
   */
  pauseWhenHidden?: boolean;
}

/** How often the stream's silence means the connection died. */
const DESKTOP_SILENCE_LIMIT_MS = 90_000;
const WATCHDOG_MS = 5_000;

function decodeBase64(data: string): Uint8Array {
  const text = atob(data);
  const bytes = new Uint8Array(text.length);
  for (let i = 0; i < text.length; i += 1) {
    bytes[i] = text.charCodeAt(i);
  }
  return bytes;
}

/**
 * Follows one session's desktop stream, driven from Solid signals a panel
 * renders directly.
 */
export function createDesktopFeed(
  sessionId: string,
  options: DesktopFeedOptions,
): DesktopFeed {
  const listener = options.listener;
  const [state, setState] = createSignal<ConnectionState>("connecting");
  const [watcher, setWatcher] = createSignal<number | null>(null);
  const [failure, setFailure] = createSignal<unknown>(null);

  let attempt = 0;
  let running = false;
  let abort: AbortController | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;

  async function pump(response: Response): Promise<void> {
    const body = response.body;
    if (body === null) {
      throw new Error("the desktop stream answered without a body");
    }
    let silenced = false;
    let lastByteAt = Date.now();
    const watchdog = setInterval(() => {
      if (Date.now() - lastByteAt > DESKTOP_SILENCE_LIMIT_MS) {
        silenced = true;
        abort?.abort();
      }
    }, WATCHDOG_MS);
    const parser: EventSourceParser = createParser({
      onEvent(event) {
        const parsed: unknown = JSON.parse(event.data);
        if (event.event === "hello") {
          const hello = parsed as DesktopHello;
          setWatcher(hello.watcher);
          listener.hello(hello);
          return;
        }
        if (event.event === "chunk") {
          const chunk = parsed as { keyframe: boolean; data: string };
          listener.chunk(decodeBase64(chunk.data), chunk.keyframe);
          return;
        }
        if (event.event === "resync") {
          listener.resync();
        }
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
        throw new Error("the desktop stream went silent", { cause: error });
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
    // The old lease is gone the moment a new connect starts: takeover and
    // input calls must never name it again.
    setWatcher(null);
    setState(attempt === 0 ? "connecting" : "reconnecting");
    try {
      const response = await openDesktopStream(sessionId, controller.signal);
      if (!running) {
        return;
      }
      // The ladder resets on a stream that *held*, not on one that merely
      // opened — the open alone proves nothing about the connection.
      const openedAt = Date.now();
      setFailure(null);
      setState("live");
      try {
        await pump(response);
      } finally {
        if (Date.now() - openedAt >= STABLE_MS) {
          attempt = 0;
        }
      }
      if (!running) {
        return;
      }
      scheduleReconnect();
    } catch (error) {
      if (!running) {
        return;
      }
      if (isDefinitiveFailure(error)) {
        setFailure(error);
        setState("failed");
        running = false;
        return;
      }
      scheduleReconnect(error);
    }
  }

  function scheduleReconnect(error?: unknown): void {
    if (!running) {
      return;
    }
    setWatcher(null);
    setState("reconnecting");
    // A 429's Retry-After is a wait, not a transport error: it floors the
    // delay whatever rung the ladder is on.
    const retryAfterMs = error instanceof ApiProblem ? error.retryAfterMs : undefined;
    const delay = Math.max(nextBackoffDelay(attempt, options.backoff), retryAfterMs ?? 0);
    attempt += 1;
    reconnectTimer = setTimeout(() => {
      void connect();
    }, delay);
  }

  function stop(): void {
    running = false;
    if (reconnectTimer !== undefined) {
      clearTimeout(reconnectTimer);
      reconnectTimer = undefined;
    }
    abort?.abort();
    abort = null;
    setWatcher(null);
    setState("closed");
  }

  function onVisibility(): void {
    if (options.pauseWhenHidden === false) {
      return;
    }
    if (document.visibilityState === "hidden") {
      stop();
      return;
    }
    if (!running) {
      running = true;
      attempt = 0;
      void connect();
    }
  }

  running = true;
  void connect();
  if (options.pauseWhenHidden !== false && typeof document !== "undefined") {
    document.addEventListener("visibilitychange", onVisibility);
  }

  function dispose(): void {
    stop();
    if (options.pauseWhenHidden !== false && typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", onVisibility);
    }
  }

  return { state, watcher, failure, dispose };
}
