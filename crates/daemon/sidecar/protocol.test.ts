/**
 * The TypeScript half of the flycod⇄sidecar protocol contract.
 *
 * `crates/daemon/tests/protocol_fixtures.rs` runs the identical assertion
 * against the Rust types over the same files: decode each fixture into the
 * declared shape, re-encode it, and require the canonical bytes to be
 * unchanged. Neither declaration can drift without the other's test failing.
 */
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { describeError, sidecarCommandSchema, sidecarEventSchema } from "./protocol.ts";

const FIXTURES = join(import.meta.dir, "..", "fixtures", "protocol");

/** Every `type` tag a `SidecarCommand` can serialize under. */
const COMMAND_TAGS = [
  "start",
  "user_message",
  "interrupt",
  "approval_decision",
  "store_response",
  "shutdown",
] as const;

/** Every `type` tag a `SidecarEvent` can serialize under. */
const EVENT_TAGS = [
  "ready",
  "started",
  "capabilities",
  "sdk_message",
  "approval_request",
  "store_request",
  "fatal",
] as const;

/** Sorts every object's keys, recursively. */
function canonical(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(canonical);
  }
  if (value !== null && typeof value === "object") {
    const source = value as Record<string, unknown>;
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(source).sort()) {
      sorted[key] = canonical(source[key]);
    }
    return sorted;
  }
  return value;
}

/** The canonical text of a value: sorted keys, compact, one newline. */
function canonicalText(value: unknown): string {
  return `${JSON.stringify(canonical(value))}\n`;
}

/** Every fixture whose name starts with `prefix`, as [name, contents]. */
function fixtures(prefix: string): Array<[string, string]> {
  const found = readdirSync(FIXTURES)
    .filter((name) => name.startsWith(prefix) && name.endsWith(".json"))
    .sort()
    .map((name): [string, string] => [name, readFileSync(join(FIXTURES, name), "utf8")]);
  expect(found.length).toBeGreaterThan(0);
  return found;
}

describe("flycod commands", () => {
  const seen = new Set<string>();

  for (const [name, text] of fixtures("command_")) {
    test(`${name} round trips byte for byte`, () => {
      // The file itself must be canonical, so drift shows up in the diff.
      expect(canonicalText(JSON.parse(text))).toBe(text);
      const command = sidecarCommandSchema.parse(JSON.parse(text));
      expect(canonicalText(command)).toBe(text);
      seen.add(command.type);
    });
  }

  test("every command variant has a fixture", () => {
    expect([...seen].sort()).toEqual([...COMMAND_TAGS].sort());
  });
});

describe("sidecar events", () => {
  const seen = new Set<string>();

  for (const [name, text] of fixtures("event_")) {
    test(`${name} round trips byte for byte`, () => {
      expect(canonicalText(JSON.parse(text))).toBe(text);
      const event = sidecarEventSchema.parse(JSON.parse(text));
      expect(canonicalText(event)).toBe(text);
      seen.add(event.type);
    });
  }

  test("every event variant has a fixture", () => {
    expect([...seen].sort()).toEqual([...EVENT_TAGS].sort());
  });
});

describe("decoding is strict", () => {
  test("an unknown message type is refused", () => {
    expect(() => sidecarCommandSchema.parse({ type: "resume" })).toThrow();
  });

  test("a missing required field is refused", () => {
    expect(() => sidecarCommandSchema.parse({ type: "user_message" })).toThrow();
  });

  test("an opaque JSON field must still be present", () => {
    expect(() => sidecarEventSchema.parse({ type: "sdk_message" })).toThrow();
    expect(sidecarEventSchema.parse({ type: "sdk_message", message: null })).toEqual({
      type: "sdk_message",
      message: null,
    });
  });

  test("a field neither side declares is dropped, not carried", () => {
    const decoded = sidecarCommandSchema.parse({ type: "interrupt", reason: "because" });
    expect(decoded).toEqual({ type: "interrupt" });
  });

  test("a decoding failure reads as one line naming the field", () => {
    const failure = sidecarCommandSchema.safeParse({ type: "user_message" });
    expect(failure.success).toBe(false);
    const described = describeError(failure.error);
    expect(described).toContain("does not match the protocol");
    expect(described).toContain("text");
    expect(described).not.toContain("\n");
  });

  test("a non-zod failure keeps its own message", () => {
    expect(describeError(new Error("bun is not installed"))).toBe("bun is not installed");
  });

  test("started carries identity only, never capabilities", () => {
    // The two are separate events because the SDK reports them at
    // different times; a `started` that also claimed capabilities could
    // only ever be claiming an empty set.
    expect(() =>
      sidecarEventSchema.parse({ type: "started", session_id: "s-1", capabilities: [] }),
    ).not.toThrow();
    expect(
      sidecarEventSchema.parse({ type: "started", session_id: "s-1", capabilities: [] }),
    ).toEqual({ type: "started", session_id: "s-1" });
  });

  test("a capabilities event must carry a list of strings", () => {
    expect(sidecarEventSchema.parse({ type: "capabilities", capabilities: [] })).toEqual({
      type: "capabilities",
      capabilities: [],
    });
    expect(() => sidecarEventSchema.parse({ type: "capabilities" })).toThrow();
    expect(() => sidecarEventSchema.parse({ type: "capabilities", capabilities: [1] })).toThrow();
  });

  test("a permission mode outside the SDK's union is refused", () => {
    expect(() =>
      sidecarCommandSchema.parse({
        type: "start",
        cwd: "/tmp",
        auth: { mode: "inherit" },
        config_dir: null,
        project_dir_name: null,
        model: null,
        permission_mode: "yolo",
        resume_session_id: null,
      }),
    ).toThrow();
  });
});
