import { afterEach, describe, expect, it, vi } from "vitest";
import { createSessionRelay, EventStream } from "./relay";
import { isDefinitiveFailure, nextBackoffDelay, type UserStream } from "./events";
import { ApiProblem, NetworkError, NotImplementedError, UnexpectedResponseError } from "./problem";
import type { StoredEvent } from "./client";
import type { SessionEvent } from "./wire";

/** The instant the fixture's live frames are dated at. */
const LIVE_NOW = 1_800_000_000;

/** A stream whose live clock does not move, so an assertion can name it. */
function freshStream(): EventStream {
  return new EventStream(() => LIVE_NOW);
}

function stored(seq: number, event: unknown, atUnix = 0): StoredEvent {
  return { at_unix: atUnix, seq, event };
}

const SESSION = "session-1";

/** One live envelope, as the shared stream hands them to a relay. */
function live(event: unknown, seq: number | null = null): SessionEvent {
  return { session: SESSION, seq, at_unix: 0, event: event as SessionEvent["event"] };
}

const started = { type: "started", harness_session_id: "h-1" };
const usage = { type: "usage", usage: { input_tokens: 1, output_tokens: 2, estimated_cost: null, context: null } };
const notice = { type: "spot_notice", seconds_remaining: 30 };

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

/** Lets the relay's async catch-up chain run to a settle point. */
async function settle(rounds = 20): Promise<void> {
  for (let i = 0; i < rounds; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

/** A shared stream that never opens — catch-up is exercised on its own. */
function stubUserStream(): UserStream {
  return {
    state: () => "connecting",
    failure: () => null,
    subscribe: () => () => {},
  };
}

/** Drops the timestamps, for the assertions that are only about dedup and order. */
function eventsOf(timed: { event: unknown }[]): unknown[] {
  return timed.map((entry) => entry.event);
}

describe("EventStream.ingestCatchUp", () => {
  it("emits every event on an empty stream and advances the cursor", () => {
    const stream = freshStream();
    const out = stream.ingestCatchUp([stored(1, started), stored(2, usage)]);
    expect(eventsOf(out)).toEqual([started, usage]);
    expect(stream.cursor).toBe(2);
  });

  it("skips events at or below the cursor already reached", () => {
    const stream = freshStream();
    stream.ingestCatchUp([stored(1, started), stored(2, usage)]);
    const out = stream.ingestCatchUp([stored(2, usage), stored(3, notice)]);
    expect(eventsOf(out)).toEqual([notice]);
    expect(stream.cursor).toBe(3);
  });

  it("still advances the cursor for a duplicate event byte-identical to one already shown live", () => {
    const stream = freshStream();
    // The exact same event arrives live first...
    stream.ingestLive(live(notice));
    // ...then catch-up replays it with a seq attached. It must not be
    // shown twice, but the cursor still needs to move past it.
    const out = stream.ingestCatchUp([stored(5, notice)]);
    expect(out).toEqual([]);
    expect(stream.cursor).toBe(5);
  });

  it("keeps two stored rows with identical content — each took its own seq", () => {
    // The log records every occurrence; two identical `assistant_delta`
    // rows are two real chunks, and a reload has to show both.
    const stream = freshStream();
    const delta = { type: "harness", event: { type: "assistant_delta", turn_id: "t-1", text: "- " } };
    const out = stream.ingestCatchUp([stored(1, delta), stored(2, delta)]);
    expect(eventsOf(out)).toEqual([delta, delta]);
    expect(stream.cursor).toBe(2);
  });

  it("returns nothing for an empty page and leaves the cursor untouched", () => {
    const stream = freshStream();
    const out = stream.ingestCatchUp([]);
    expect(out).toEqual([]);
    expect(stream.cursor).toBeNull();
  });

  it("dates a replayed event by when the room recorded it, not by now", () => {
    // This is what makes a `Worked for 8m 23s` footer readable on a
    // session opened tomorrow: the times come off the stored stream.
    const stream = freshStream();
    const out = stream.ingestCatchUp([stored(1, started, 1_700_000_000)]);
    expect(out).toEqual([{ event: started, atUnix: 1_700_000_000 }]);
  });
});

describe("EventStream.ingestLive", () => {
  it("parses and emits a fresh live envelope, dated on arrival", () => {
    const stream = freshStream();
    const event = stream.ingestLive(live(started));
    expect(event).toEqual({ event: started, atUnix: LIVE_NOW });
  });

  it("keeps a live frame identical to one already shown via catch-up", () => {
    // A live frame is always a new occurrence: the room emits each fact to
    // the stream once, and replays come back through catch-up. Identical
    // content is a repeated fact, not a redelivery.
    const stream = freshStream();
    stream.ingestCatchUp([stored(1, started)]);
    const event = stream.ingestLive(live(started));
    expect(event).toEqual({ event: started, atUnix: LIVE_NOW });
  });

  it("drops a live envelope whose position the record already passed", () => {
    // The buffer replay after a reconnect can re-serve an event catch-up
    // already showed: the envelope's seq is the same position, and the
    // cursor makes the skip exact rather than a content-window guess.
    const stream = freshStream();
    stream.ingestCatchUp([stored(1, started), stored(2, usage)]);
    expect(stream.ingestLive(live(usage, 2))).toBeNull();
    expect(stream.ingestLive(live(started, 1))).toBeNull();
    // Above the cursor is still a new occurrence, even with content that
    // was shown — a second identical fact, not a replay.
    expect(stream.ingestLive(live(usage, 3))).not.toBeNull();
  });

  it("keeps every one of a run of identical live frames — repeated deltas are real output", () => {
    // The bug this guards: a content-keyed dedup ate the second `**`, `o `
    // or `"- "` of a markdown stream, which is what malformed assistant
    // rendering looked like (issue: merged lines and missing characters).
    const stream = freshStream();
    const delta = {
      type: "harness",
      event: { type: "assistant_delta", turn_id: "t-1", text: "**" },
    };
    const frames = [
      stream.ingestLive(live(delta)),
      stream.ingestLive(live(delta)),
      stream.ingestLive(live(delta)),
    ];
    expect(frames.map((frame) => frame?.event)).toEqual([delta, delta, delta]);
  });

  it("does not dedupe two distinct events", () => {
    const stream = freshStream();
    const first = stream.ingestLive(live(started));
    const second = stream.ingestLive(live(usage));
    expect(eventsOf([first!, second!])).toEqual([started, usage]);
  });

  it("dates each live frame against the clock at the moment it arrived", () => {
    let seconds = 1_000;
    const stream = new EventStream(() => seconds);
    const first = stream.ingestLive(live(started));
    seconds = 1_042;
    const second = stream.ingestLive(live(usage));
    expect(first?.atUnix).toBe(1_000);
    expect(second?.atUnix).toBe(1_042);
  });
});

describe("EventStream catch-up + live interleaving (the reconnect race)", () => {
  it("shows nothing twice across a catch-up pass, a live frame that beat it, and a second catch-up pass", () => {
    const stream = freshStream();

    // First catch-up page on initial connect.
    const first = stream.ingestCatchUp([stored(1, started)]);
    expect(eventsOf(first)).toEqual([started]);

    // A live envelope arrives that duplicates what catch-up already showed
    // (the same fact reaching the browser twice during the open).
    const live2 = stream.ingestLive(live(usage, 2));
    expect(live2?.event).toEqual(usage);

    // The catch-up run fired from the stream's `live` transition replays
    // the same page plus one more row; only the new one shows.
    const second = stream.ingestCatchUp([stored(1, started), stored(2, usage), stored(3, notice)]);
    expect(eventsOf(second)).toEqual([notice]);
    expect(stream.cursor).toBe(3);
  });
});

describe("EventStream dedup across two serializations of one event", () => {
  it("recognises a catch-up row whose keys came back in another order", () => {
    // A live envelope is `serde_json` output of the event struct, with the
    // `type` tag first. The same event read back from the seq log has been
    // through a `serde_json::Value` — a `BTreeMap` — so it re-serializes
    // with its keys in alphabetical order. Comparing the two byte for byte
    // matched nothing, and the page rendered every event twice: a doubled
    // answer and a phantom tool row that never finished.
    const stream = freshStream();
    const harness = {
      type: "harness",
      event: { type: "turn_started", turn_id: "t-1" },
    };
    expect(stream.ingestLive(live(harness))).not.toBeNull();

    const reordered = { event: { turn_id: "t-1", type: "turn_started" }, type: "harness" };
    expect(stream.ingestCatchUp([stored(1, reordered)])).toEqual([]);
  });

  it("still tells two different events apart", () => {
    const stream = freshStream();
    expect(
      stream.ingestLive(live({ type: "harness", event: { type: "turn_started", turn_id: "t-1" } })),
    ).not.toBeNull();
    expect(
      stream.ingestCatchUp([stored(1, { event: { turn_id: "t-2", type: "turn_started" }, type: "harness" })]),
    ).toHaveLength(1);
  });
});

describe("createSessionRelay catch-up", () => {
  it("fails the relay when the control plane keeps answering more: true past the page cap", async () => {
    // `more: true` forever is the server's bug, but a client that walks
    // pages at line rate would spend the day's request budget on it — the
    // cap turns the walk into a failure the page can show.
    let calls = 0;
    vi.stubGlobal("fetch", () => {
      calls += 1;
      const page = {
        events: [stored(calls, notice)],
        more: true,
      };
      return Promise.resolve(
        new Response(JSON.stringify(page), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      );
    });
    const relay = createSessionRelay(SESSION, { stream: stubUserStream() });
    await settle();
    expect(relay.state()).toBe("failed");
    const failure = relay.failure();
    expect(failure).toBeInstanceOf(Error);
    expect((failure as Error).message).toContain("without advancing");
    expect(calls).toBe(200);
    relay.dispose();
  });
});

describe("nextBackoffDelay", () => {
  it("scales the cap by 2^attempt up to maxMs, with full jitter via the injected random source", () => {
    const options = { baseMs: 1000, maxMs: 60_000, random: () => 1 };
    expect(nextBackoffDelay(0, options)).toBe(1000);
    expect(nextBackoffDelay(1, options)).toBe(2000);
    expect(nextBackoffDelay(2, options)).toBe(4000);
    expect(nextBackoffDelay(3, options)).toBe(8000);
  });

  it("caps the delay at maxMs even for a large attempt count", () => {
    const options = { baseMs: 1000, maxMs: 60_000, random: () => 1 };
    expect(nextBackoffDelay(10, options)).toBe(60_000);
    expect(nextBackoffDelay(20, options)).toBe(60_000);
  });

  it("scales the jittered result by the random source", () => {
    const options = { baseMs: 1000, maxMs: 60_000, random: () => 0.5 };
    expect(nextBackoffDelay(2, options)).toBe(2000);
  });

  it("returns 0 when random source returns 0", () => {
    expect(nextBackoffDelay(3, { random: () => 0 })).toBe(0);
  });

  it("uses default base/max when options are omitted", () => {
    expect(nextBackoffDelay(0, { random: () => 1 })).toBe(1000);
    expect(nextBackoffDelay(6, { random: () => 1 })).toBe(60_000);
  });
});

/** A problem document of one status, as the control plane would answer it. */
function problem(status: number, type = "https://flyco.dev/problems/not-found"): ApiProblem {
  return new ApiProblem({ type, title: "Not Found", status, detail: "no such session" });
}

describe("isDefinitiveFailure", () => {
  it("stops the stream on a request the server refuses outright", () => {
    // A 4xx is a statement about the request — this account cannot open
    // the stream — and repeating it cannot change the answer.
    expect(isDefinitiveFailure(problem(404))).toBe(true);
    expect(isDefinitiveFailure(problem(403))).toBe(true);
  });

  it("keeps retrying the two client statuses a retry can get past", () => {
    expect(isDefinitiveFailure(problem(401))).toBe(false);
    expect(isDefinitiveFailure(problem(429))).toBe(false);
  });

  it("keeps retrying a server that is having a bad minute", () => {
    expect(isDefinitiveFailure(problem(500))).toBe(false);
    expect(isDefinitiveFailure(problem(502))).toBe(false);
    expect(isDefinitiveFailure(problem(503))).toBe(false);
  });

  it("stops on a build that does not have the route, which no retry adds", () => {
    const unbuilt = new NotImplementedError({
      type: "https://flyco.dev/problems/not-implemented",
      title: "Not Implemented",
      status: 501,
      detail: "the event stream is not available on this build",
    });
    expect(isDefinitiveFailure(unbuilt)).toBe(true);
  });

  it("keeps retrying everything that is not the server refusing the request", () => {
    expect(isDefinitiveFailure(new NetworkError(new Error("offline")))).toBe(false);
    expect(isDefinitiveFailure(new UnexpectedResponseError(404, "<html>proxy</html>"))).toBe(false);
    expect(isDefinitiveFailure(new Error("the stream ended"))).toBe(false);
    expect(isDefinitiveFailure(null)).toBe(false);
  });
});
