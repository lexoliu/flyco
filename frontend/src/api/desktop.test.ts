import { afterEach, describe, expect, it, vi } from "vitest";
import { createDesktopFeed, type DesktopListener } from "./desktop";

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

function sse(frame: { event: string; data: unknown }): string {
  return `event: ${frame.event}\ndata: ${JSON.stringify(frame.data)}\n\n`;
}

/** A listener that records everything it is told. */
function recording(): DesktopListener & {
  hellos: { watcher: number; takeover: boolean }[];
  chunks: { data: Uint8Array; keyframe: boolean }[];
  resyncs: number[];
} {
  const seen = {
    hellos: [] as { watcher: number; takeover: boolean }[],
    chunks: [] as { data: Uint8Array; keyframe: boolean }[],
    resyncs: [] as number[],
  };
  return {
    ...seen,
    hello(hello) {
      seen.hellos.push(hello);
    },
    chunk(data, keyframe) {
      seen.chunks.push({ data, keyframe });
    },
    resync() {
      seen.resyncs.push(seen.resyncs.length);
    },
  };
}

/** Lets the feed's async connect/pump chain run to a settle point. */
async function settle(rounds = 10): Promise<void> {
  for (let i = 0; i < rounds; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

/** Stubs `fetch` to answer each call from `queue`, then hold `tail`. */
function stubStreams(
  ...queue: (Response | (() => Response))[]
): { calls: string[]; tail: ReturnType<typeof sseBody> } {
  const tail = sseBody();
  const calls: string[] = [];
  let remaining = [...queue];
  vi.stubGlobal("fetch", (input: RequestInfo | URL) => {
    calls.push(String(input));
    const next = remaining.shift();
    if (next !== undefined) {
      return Promise.resolve(typeof next === "function" ? next() : next);
    }
    return Promise.resolve(
      new Response(tail.body, {
        status: 200,
        headers: { "Content-Type": "text/event-stream" },
      }),
    );
  });
  return { calls, tail };
}

function problem(status: number): Response {
  return new Response(
    JSON.stringify({ type: "https://flyco.dev/problems/gone", title: "Gone", status }),
    { status, headers: { "Content-Type": "application/problem+json" } },
  );
}

const feedOptions = {
  backoff: { baseMs: 1, random: () => 0 },
  pauseWhenHidden: false,
};

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("createDesktopFeed", () => {
  it("announces the watcher lease the hello minted", async () => {
    const stub = stubStreams();
    const listener = recording();
    const feed = createDesktopFeed(SESSION, { listener, ...feedOptions });
    stub.tail.push(sse({ event: "hello", data: { watcher: 7, takeover: false } }));
    await settle();
    expect(feed.watcher()).toBe(7);
    expect(feed.state()).toBe("live");
    expect(listener.hellos).toEqual([{ watcher: 7, takeover: false }]);
    feed.dispose();
  });

  it("decodes chunk frames into bytes and keyframe flags", async () => {
    const stub = stubStreams();
    const listener = recording();
    const feed = createDesktopFeed(SESSION, { listener, ...feedOptions });
    stub.tail.push(sse({ event: "hello", data: { watcher: 1, takeover: false } }));
    stub.tail.push(
      sse({ event: "chunk", data: { keyframe: true, data: btoa("key") } }),
      sse({ event: "chunk", data: { keyframe: false, data: btoa("delta") } }),
    );
    await settle();
    expect(listener.chunks.map((c) => [new TextDecoder().decode(c.data), c.keyframe])).toEqual([
      ["key", true],
      ["delta", false],
    ]);
    feed.dispose();
  });

  it("forwards a resync to the listener", async () => {
    const stub = stubStreams();
    const listener = recording();
    const feed = createDesktopFeed(SESSION, { listener, ...feedOptions });
    stub.tail.push(sse({ event: "resync", data: {} }));
    await settle();
    expect(listener.resyncs.length).toBe(1);
    feed.dispose();
  });

  it("mints a new watcher on reconnect when the stream ends", async () => {
    const first = sseBody();
    const stub = stubStreams(
      () =>
        new Response(first.body, {
          status: 200,
          headers: { "Content-Type": "text/event-stream" },
        }),
    );
    const listener = recording();
    const feed = createDesktopFeed(SESSION, { listener, ...feedOptions });
    first.push(sse({ event: "hello", data: { watcher: 3, takeover: false } }));
    await settle();
    expect(feed.watcher()).toBe(3);

    first.close();
    await settle();
    expect(feed.watcher()).toBeNull();

    stub.tail.push(sse({ event: "hello", data: { watcher: 4, takeover: true } }));
    await settle();
    expect(stub.calls.length).toBe(2);
    expect(feed.watcher()).toBe(4);
    expect(listener.hellos).toEqual([
      { watcher: 3, takeover: false },
      { watcher: 4, takeover: true },
    ]);
    feed.dispose();
  });

  it("stops for good on a definitive failure rather than retrying", async () => {
    const stub = stubStreams(() => problem(404));
    const feed = createDesktopFeed(SESSION, { listener: recording(), ...feedOptions });
    await settle();
    expect(feed.state()).toBe("failed");
    expect(feed.failure()).not.toBeNull();
    expect(feed.watcher()).toBeNull();
    expect(stub.calls.length).toBe(1);
    feed.dispose();
  });

  it("closes the lease with the feed — no reconnect after dispose", async () => {
    const stub = stubStreams();
    const feed = createDesktopFeed(SESSION, { listener: recording(), ...feedOptions });
    stub.tail.push(sse({ event: "hello", data: { watcher: 9, takeover: false } }));
    await settle();
    feed.dispose();
    expect(feed.state()).toBe("closed");
    expect(feed.watcher()).toBeNull();
    stub.tail.close();
    await settle();
    expect(stub.calls.length).toBe(1);
  });
});
