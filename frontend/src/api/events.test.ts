import { afterEach, describe, expect, it, vi } from "vitest";
import { createUserStream, STABLE_MS, type SessionListener } from "./events";

const SESSION = "session-1";

/** An SSE body the test writes frames into and closes when it likes. */
function sseBody(): {
  body: ReadableStream<Uint8Array>;
  push(...frames: string[]): void;
  close(): void;
  error(cause: unknown): void;
} {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const encoder = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
    },
  });
  return {
    body,
    push: (...frames) => {
      for (const frame of frames) {
        controller.enqueue(encoder.encode(frame));
      }
    },
    close: () => controller.close(),
    error: (cause) => controller.error(cause),
  };
}

function sseResponse(stream: ReturnType<typeof sseBody>): Response {
  return new Response(stream.body, {
    status: 200,
    headers: { "Content-Type": "text/event-stream" },
  });
}

/** A 429 problem document carrying `Retry-After`, as the request budget answers. */
function rateLimited(seconds: number): Response {
  return new Response(
    JSON.stringify({
      type: "https://flyco.dev/problems/rate-limited",
      title: "Too Many Requests",
      status: 429,
      detail: "per-minute request budget reached",
    }),
    {
      status: 429,
      headers: {
        "Content-Type": "application/problem+json",
        "Retry-After": String(seconds),
      },
    },
  );
}

/** A listener that records the lifecycle states it is told. */
function recording(): SessionListener & { states: string[]; failures: unknown[] } {
  const seen = { states: [] as string[], failures: [] as unknown[] };
  return {
    ...seen,
    event() {},
    state(next) {
      seen.states.push(next);
    },
    failed(error) {
      seen.failures.push(error);
    },
  };
}

/** Stubs `fetch` to answer each call from `queue`, then hold `tail` open. */
function stubStreams(
  ...queue: (Response | (() => Response))[]
): { calls: string[]; tail: ReturnType<typeof sseBody> } {
  const tail = sseBody();
  const calls: string[] = [];
  const remaining = [...queue];
  vi.stubGlobal("fetch", (input: RequestInfo | URL) => {
    calls.push(String(input));
    const next = remaining.shift();
    if (next !== undefined) {
      return Promise.resolve(typeof next === "function" ? next() : next);
    }
    return Promise.resolve(sseResponse(tail));
  });
  return { calls, tail };
}

/**
 * Flushes the async connect/pump chain without moving the fake clock —
 * stream reads and fetch resolution are microtasks, not timers.
 */
async function flush(rounds = 20): Promise<void> {
  for (let i = 0; i < rounds; i += 1) {
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(0);
  }
}

const backoff = { baseMs: 1000, random: () => 0.5 };

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("createUserStream reconnect backoff", () => {
  it("climbs the ladder across streams that open and die at once", async () => {
    // A stream that closes a second after opening proves the connection
    // was never established: the retry must climb, not stay at the floor
    // re-requesting every fraction of a second forever.
    vi.useFakeTimers();
    const first = sseBody();
    const second = sseBody();
    const third = sseBody();
    const stub = stubStreams(
      () => sseResponse(first),
      () => sseResponse(second),
      () => sseResponse(third),
    );
    const stream = createUserStream({ backoff });
    const unsubscribe = stream.subscribe(SESSION, recording());
    await flush();
    expect(stub.calls).toHaveLength(1);

    // Rung 0: 0.5 * 1000 = 500ms.
    first.close();
    await flush();
    expect(stream.state()).toBe("reconnecting");
    await vi.advanceTimersByTimeAsync(499);
    expect(stub.calls).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    await flush();
    expect(stub.calls).toHaveLength(2);

    // Rung 1: 1000ms — the ladder climbed rather than resetting on open.
    second.close();
    await flush();
    await vi.advanceTimersByTimeAsync(999);
    expect(stub.calls).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    await flush();
    expect(stub.calls).toHaveLength(3);

    // Rung 2: 2000ms.
    third.close();
    await flush();
    await vi.advanceTimersByTimeAsync(1999);
    expect(stub.calls).toHaveLength(3);
    await vi.advanceTimersByTimeAsync(1);
    await flush();
    expect(stub.calls).toHaveLength(4);

    unsubscribe();
  });

  it("restarts at the first rung after a stream that held past STABLE_MS drops", async () => {
    vi.useFakeTimers();
    const first = sseBody();
    const second = sseBody();
    const stub = stubStreams(
      () => sseResponse(first),
      () => sseResponse(second),
    );
    const stream = createUserStream({ backoff });
    const unsubscribe = stream.subscribe(SESSION, recording());
    await flush();

    // A quick death climbs to rung 1...
    first.close();
    await flush();
    await vi.advanceTimersByTimeAsync(500);
    await flush();
    expect(stub.calls).toHaveLength(2);

    // ...but the second stream held past STABLE_MS, so its drop was an
    // established connection dying: back to rung 0's 500ms, not 1000ms.
    await vi.advanceTimersByTimeAsync(STABLE_MS + 1000);
    second.close();
    await flush();
    await vi.advanceTimersByTimeAsync(499);
    expect(stub.calls).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    await flush();
    expect(stub.calls).toHaveLength(3);

    unsubscribe();
  });

  it("waits out Retry-After on a 429 instead of re-requesting at once", async () => {
    vi.useFakeTimers();
    const stub = stubStreams(() => rateLimited(30));
    const stream = createUserStream({ backoff });
    const listener = recording();
    const unsubscribe = stream.subscribe(SESSION, listener);
    await flush();
    expect(stub.calls).toHaveLength(1);
    // A 429 is a wait, not a definitive failure: the stream stays on the
    // reconnect path, held by Retry-After rather than the 500ms rung.
    expect(stream.state()).toBe("reconnecting");

    await vi.advanceTimersByTimeAsync(29_999);
    expect(stub.calls).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    await flush();
    expect(stub.calls).toHaveLength(2);
    expect(stream.state()).toBe("live");

    unsubscribe();
  });
});
