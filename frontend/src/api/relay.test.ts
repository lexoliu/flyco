import { describe, expect, it } from "vitest";
import { EventStream, nextBackoffDelay } from "./relay";
import type { StoredEvent } from "./client";

function stored(seq: number, event: unknown): StoredEvent {
  return { at_unix: 0, seq, event };
}

const started = { type: "started", harness_session_id: "h-1" };
const usage = { type: "usage", usage: { input_tokens: 1, output_tokens: 2, estimated_cost: null, context: null } };
const notice = { type: "spot_notice", seconds_remaining: 30 };

describe("EventStream.ingestCatchUp", () => {
  it("emits every event on an empty stream and advances the cursor", () => {
    const stream = new EventStream();
    const out = stream.ingestCatchUp([stored(1, started), stored(2, usage)]);
    expect(out).toEqual([started, usage]);
    expect(stream.cursor).toBe(2);
  });

  it("skips events at or below the cursor already reached", () => {
    const stream = new EventStream();
    stream.ingestCatchUp([stored(1, started), stored(2, usage)]);
    const out = stream.ingestCatchUp([stored(2, usage), stored(3, notice)]);
    expect(out).toEqual([notice]);
    expect(stream.cursor).toBe(3);
  });

  it("still advances the cursor for a duplicate event byte-identical to one already shown live", () => {
    const stream = new EventStream();
    // The exact same JSON text arrives live first...
    stream.ingestLive(JSON.stringify(notice));
    // ...then catch-up replays it with a seq attached. It must not be
    // shown twice, but the cursor still needs to move past it.
    const out = stream.ingestCatchUp([stored(5, notice)]);
    expect(out).toEqual([]);
    expect(stream.cursor).toBe(5);
  });

  it("returns nothing for an empty page and leaves the cursor untouched", () => {
    const stream = new EventStream();
    const out = stream.ingestCatchUp([]);
    expect(out).toEqual([]);
    expect(stream.cursor).toBeNull();
  });
});

describe("EventStream.ingestLive", () => {
  it("parses and emits a fresh live frame", () => {
    const stream = new EventStream();
    const event = stream.ingestLive(JSON.stringify(started));
    expect(event).toEqual(started);
  });

  it("dedupes a live frame byte-identical to one already shown via catch-up", () => {
    const stream = new EventStream();
    stream.ingestCatchUp([stored(1, started)]);
    const event = stream.ingestLive(JSON.stringify(started));
    expect(event).toBeNull();
  });

  it("dedupes a repeated live frame against itself", () => {
    const stream = new EventStream();
    const first = stream.ingestLive(JSON.stringify(notice));
    const second = stream.ingestLive(JSON.stringify(notice));
    expect(first).toEqual(notice);
    expect(second).toBeNull();
  });

  it("does not dedupe two distinct events", () => {
    const stream = new EventStream();
    const first = stream.ingestLive(JSON.stringify(started));
    const second = stream.ingestLive(JSON.stringify(usage));
    expect(first).toEqual(started);
    expect(second).toEqual(usage);
  });
});

describe("EventStream catch-up + live interleaving (the reconnect race)", () => {
  it("shows nothing twice across a catch-up pass, a live frame that beat it, and a second catch-up pass", () => {
    const stream = new EventStream();

    // First catch-up page on initial connect.
    const first = stream.ingestCatchUp([stored(1, started)]);
    expect(first).toEqual([started]);

    // A live frame arrives that duplicates what catch-up already showed
    // (the same fact reaching the browser twice during the handshake).
    const live = stream.ingestLive(JSON.stringify(usage));
    expect(live).toEqual(usage);

    // The "double catch-up" run fired from the socket's open handler
    // replays the same page plus one more row; only the new one shows.
    const second = stream.ingestCatchUp([stored(1, started), stored(2, usage), stored(3, notice)]);
    expect(second).toEqual([notice]);
    expect(stream.cursor).toBe(3);
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
