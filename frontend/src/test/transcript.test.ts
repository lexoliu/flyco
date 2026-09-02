import { describe, expect, it } from "vitest";
import {
  foldTranscript,
  machineChangePrice,
  machineChangeSummary,
  pendingApprovals,
} from "../lib/transcript";
import type { TimedEvent } from "../api/relay";
import type { ClientEvent } from "../api/wire";

const T0 = 1_800_000_000;

/** One event, dated `offset` seconds after the fixture's origin. */
function at(offset: number, event: ClientEvent): TimedEvent {
  return { event, atUnix: T0 + offset };
}

const TURN = "turn-1";

function usage() {
  return { input_tokens: 10, output_tokens: 20, estimated_cost: null, context: null };
}

describe("foldTranscript notices", () => {
  it("distinguishes a compaction that worked from one that did not", () => {
    const items = foldTranscript([
      at(0, { type: "harness", event: { type: "context_compacted" } }),
      at(1, {
        type: "harness",
        event: { type: "context_compaction_failed", error: "summary request failed" },
      }),
    ]);

    expect(items).toHaveLength(2);
    expect(items[0]).toMatchObject({ kind: "notice", tone: "info" });
    expect(items[1]).toMatchObject({
      kind: "notice",
      tone: "danger",
      text: "Context compaction failed: summary request failed",
    });
  });

  it("says when a usage limit resets, and stays honest when it does not know", () => {
    const [known] = foldTranscript([
      at(0, { type: "harness", event: { type: "usage_limited", resets_at_unix: T0 + 3600 } }),
    ]);
    expect(known).toMatchObject({ kind: "notice", tone: "warning" });
    expect((known as { text: string }).text).toContain("continues by itself at");

    const [unknown] = foldTranscript([
      at(0, { type: "harness", event: { type: "usage_limited", resets_at_unix: null } }),
    ]);
    expect((unknown as { text: string }).text).toContain("waits for the account's limit to reset");
  });

  it("counts down a spot reclamation", () => {
    const [notice] = foldTranscript([at(0, { type: "spot_notice", seconds_remaining: 30 })]);
    expect(notice).toMatchObject({ kind: "notice", tone: "warning" });
    expect((notice as { text: string }).text).toContain("30s");
  });

  it("says nothing about a clean working tree", () => {
    // An empty `git status --short` is the clean tree, and a clean tree is
    // not news; only a dirty one is worth a line in the transcript.
    expect(foldTranscript([at(0, { type: "repo_dirty", summary: "" })])).toEqual([]);
    expect(foldTranscript([at(0, { type: "repo_dirty", summary: " M src/lib.rs" })])).toHaveLength(
      1,
    );
  });
});

describe("foldTranscript machine changes", () => {
  it("reads as the line docs/ux.md §9.5 asks for", () => {
    const [change] = foldTranscript([
      at(0, {
        type: "machine_changed",
        machine_type: "Standard_D8s_v6",
        hourly: 380_000,
        spot: true,
        restarted: true,
      }),
    ]);

    expect(change).toMatchObject({ kind: "machine_change", machineType: "Standard_D8s_v6" });
    expect(machineChangeSummary(change as never)).toBe(
      "Switched to Standard_D8s_v6 · restarted the machine · disk kept",
    );
    expect(machineChangePrice(change as never)).toBe("$0.38/hr · spot");
  });

  it("does not claim a restart that did not happen", () => {
    const [change] = foldTranscript([
      at(0, {
        type: "machine_changed",
        machine_type: "Standard_D8s_v6",
        hourly: 380_000,
        spot: false,
        restarted: false,
      }),
    ]);

    expect(machineChangeSummary(change as never)).toBe("Switched to Standard_D8s_v6 · disk kept");
    expect(machineChangePrice(change as never)).toBe("$0.38/hr");
  });

  it("quotes no price on hardware flyco does not meter", () => {
    // `$0.00/hr` would read as "this is free", which is a different claim
    // from "flyco meters nothing here".
    const [change] = foldTranscript([
      at(0, {
        type: "machine_changed",
        machine_type: "build.lexo.cool",
        hourly: null,
        spot: false,
        restarted: true,
      }),
    ]);

    expect(machineChangePrice(change as never)).toBeNull();
  });
});

describe("foldTranscript turns", () => {
  it("dates a turn from its start and its end, which is what the footer reads", () => {
    const [turn] = foldTranscript([
      at(0, { type: "harness", event: { type: "turn_started", turn_id: TURN } }),
      at(503, { type: "harness", event: { type: "turn_completed", turn_id: TURN, usage: usage() } }),
    ]);

    expect(turn).toMatchObject({ kind: "turn", status: "completed" });
    expect(turn).toMatchObject({ startedAtUnix: T0, endedAtUnix: T0 + 503 });
  });

  it("leaves a running turn undated at the end, so no footer is drawn", () => {
    const [turn] = foldTranscript([
      at(0, { type: "harness", event: { type: "turn_started", turn_id: TURN } }),
      at(1, { type: "harness", event: { type: "assistant_delta", turn_id: TURN, text: "hi" } }),
    ]);
    expect(turn).toMatchObject({ status: "running", endedAtUnix: null });
  });

  it("times each tool call from its own start and finish", () => {
    const [turn] = foldTranscript([
      at(0, { type: "harness", event: { type: "turn_started", turn_id: TURN } }),
      at(
        2,
        {
          type: "harness",
          event: {
            type: "tool_started",
            turn_id: TURN,
            call_id: "c1",
            tool: "Bash",
            input: { command: "cargo test" },
          },
        },
      ),
      at(9, {
        type: "harness",
        event: { type: "tool_completed", turn_id: TURN, call_id: "c1", ok: true },
      }),
    ]);

    expect(turn).toMatchObject({ kind: "turn" });
    const tools = (turn as { tools: { startedAtUnix: number; endedAtUnix: number | null; ok: boolean | null }[] })
      .tools;
    expect(tools).toHaveLength(1);
    expect(tools[0]).toMatchObject({ startedAtUnix: T0 + 2, endedAtUnix: T0 + 9, ok: true });
  });

  it("carries a failure onto the turn it belongs to", () => {
    const [turn] = foldTranscript([
      at(0, { type: "harness", event: { type: "turn_started", turn_id: TURN } }),
      at(4, {
        type: "harness",
        event: { type: "turn_failed", turn_id: TURN, error: "the harness stopped" },
      }),
    ]);
    expect(turn).toMatchObject({ status: "failed", error: "the harness stopped" });
  });
});

describe("foldTranscript approvals", () => {
  const asked: ClientEvent = {
    type: "approval_pending",
    id: "ap-1",
    payload: { kind: "tool_use", tool: "Bash", input: { command: "rm -rf build" } },
  };

  it("puts an approval inline, where it happened", () => {
    const items = foldTranscript([
      at(0, { type: "user_message", text: "clean the build" }),
      at(1, asked),
    ]);
    expect(items.map((item) => item.kind)).toEqual(["user_message", "approval"]);
    expect(items[1]).toMatchObject({ kind: "approval", id: "ap-1", state: "pending" });
  });

  it("keeps a decided approval in place rather than dropping it from the record", () => {
    const items = foldTranscript([
      at(0, asked),
      at(5, { type: "approval_decided", id: "ap-1", decision: "approved" }),
    ]);
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ state: "approved" });
    expect(pendingApprovals(items)).toEqual([]);
  });

  it("reports every approval still waiting, in the order they were raised", () => {
    const second: ClientEvent = {
      type: "approval_pending",
      id: "ap-2",
      payload: { kind: "merge", repo: "lexoliu/flyco", from_branch: "feat", into_branch: "dev" },
    };
    const items = foldTranscript([at(0, asked), at(1, second)]);
    expect(pendingApprovals(items).map((approval) => approval.id)).toEqual(["ap-1", "ap-2"]);
  });
});

describe("foldTranscript provisioning timeline", () => {
  it("gathers every stage into one timeline, timed by the stage itself", () => {
    const items = foldTranscript([
      at(100, { type: "provisioning_stage", stage: "reserving", at_unix: T0 }),
      at(101, { type: "provisioning_stage", stage: "booting", at_unix: T0 + 40 }),
      at(102, { type: "provisioning_stage", stage: "installing", at_unix: T0 + 95 }),
      at(103, { type: "provisioning_stage", stage: "ready", at_unix: T0 + 210 }),
    ]);

    expect(items).toHaveLength(1);
    expect(items[0]).toEqual({
      kind: "provisioning",
      key: "provisioning",
      steps: [
        { stage: "reserving", atUnix: T0 },
        { stage: "booting", atUnix: T0 + 40 },
        { stage: "installing", atUnix: T0 + 95 },
        { stage: "ready", atUnix: T0 + 210 },
      ],
    });
  });

  it("ignores a stage delivered twice, which an at-least-once relay will do", () => {
    const items = foldTranscript([
      at(0, { type: "provisioning_stage", stage: "reserving", at_unix: T0 }),
      at(1, { type: "provisioning_stage", stage: "reserving", at_unix: T0 + 9 }),
    ]);
    expect(items[0]).toMatchObject({ steps: [{ stage: "reserving", atUnix: T0 }] });
  });

  it("keeps the timeline where provisioning happened, above the conversation", () => {
    const items = foldTranscript([
      at(0, { type: "provisioning_stage", stage: "reserving", at_unix: T0 }),
      at(1, { type: "user_message", text: "audit the relay" }),
      at(2, { type: "provisioning_stage", stage: "ready", at_unix: T0 + 200 }),
    ]);
    expect(items.map((item) => item.kind)).toEqual(["provisioning", "user_message"]);
  });
});
