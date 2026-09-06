import { describe, expect, it } from "vitest";
import {
  foldTranscript,
  machineChangePrice,
  machineChangeSummary,
  pendingApprovals,
} from "../lib/transcript";
import type { TimedEvent } from "../api/relay";
import type { ClientEvent } from "../api/wire";
import { formatTimeOfDay } from "../lib/dates";
import { shellCommandIn, shellOutcomeLabel, shellSucceeded } from "../lib/shell";

const T0 = 1_800_000_000;

/** One event, dated `offset` seconds after the fixture's origin. */
function at(offset: number, event: ClientEvent): TimedEvent {
  return { event, atUnix: T0 + offset };
}

const TURN = "turn-1";

function usage() {
  return { input_tokens: 10, output_tokens: 20, estimated_cost: null, context: null };
}

describe("foldTranscript, on a delivery that repeated itself", () => {
  it("keeps one row per call id rather than one per delivery", () => {
    // A call id names one call. Replaying `tool_started` used to append a
    // second row that no completion ever reached, so it spun under the
    // finished one forever.
    const call = {
      type: "tool_started" as const,
      turn_id: TURN,
      call_id: "toolu_1",
      tool: "Bash",
      input: { command: "ls" },
    };
    const items = foldTranscript([
      at(0, { type: "harness", event: { type: "turn_started", turn_id: TURN } }),
      at(1, { type: "harness", event: call }),
      at(1, { type: "harness", event: call }),
      at(2, {
        type: "harness",
        event: { type: "tool_completed", turn_id: TURN, call_id: "toolu_1", ok: true },
      }),
    ]);

    const turn = items.find((item) => item.kind === "turn");
    const parts = turn?.kind === "turn" ? turn.parts : [];
    expect(parts).toHaveLength(1);
    expect(parts[0]?.kind === "tools" ? parts[0].calls : []).toHaveLength(1);
    expect(parts[0]?.kind === "tools" ? parts[0].calls[0]?.ok : null).toBe(true);
  });
});

describe("foldTranscript turn order", () => {
  it("keeps prose and tool calls in the order the agent produced them", () => {
    // A turn used to be a string and a flat list, rendered prose-first
    // whatever order they happened in: a turn that ran `ls` and then
    // explained what it found showed the explanation above the command it
    // came from, which is the reverse of what the agent did.
    const items = foldTranscript([
      at(0, { type: "harness", event: { type: "turn_started", turn_id: TURN } }),
      at(1, {
        type: "harness",
        event: {
          type: "tool_started",
          turn_id: TURN,
          call_id: "c1",
          tool: "Bash",
          input: { command: "ls" },
        },
      }),
      at(2, {
        type: "harness",
        event: { type: "tool_completed", turn_id: TURN, call_id: "c1", ok: true },
      }),
      at(3, { type: "harness", event: { type: "assistant_delta", turn_id: TURN, text: "It is " } }),
      at(4, { type: "harness", event: { type: "assistant_delta", turn_id: TURN, text: "a kernel." } }),
      at(5, {
        type: "harness",
        event: {
          type: "tool_started",
          turn_id: TURN,
          call_id: "c2",
          tool: "Bash",
          input: { command: "wc -l Cargo.toml" },
        },
      }),
    ]);

    const turn = items.find((item) => item.kind === "turn");
    const parts = turn?.kind === "turn" ? turn.parts : [];
    expect(parts.map((part) => part.kind)).toEqual(["tools", "text", "tools"]);
    // Deltas collect into the one part they arrived in, not one part each.
    expect(parts[1]?.kind === "text" ? parts[1].text : "").toBe("It is a kernel.");
  });

  it("gathers a run of calls into one list rather than one list each", () => {
    const call = (id: string) =>
      at(1, {
        type: "harness" as const,
        event: {
          type: "tool_started" as const,
          turn_id: TURN,
          call_id: id,
          tool: "Read",
          input: { file_path: `src/${id}.rs` },
        },
      });
    const items = foldTranscript([
      at(0, { type: "harness", event: { type: "turn_started", turn_id: TURN } }),
      call("a"),
      call("b"),
    ]);

    const turn = items.find((item) => item.kind === "turn");
    const parts = turn?.kind === "turn" ? turn.parts : [];
    expect(parts).toHaveLength(1);
    expect(parts[0]?.kind === "tools" ? parts[0].calls.length : 0).toBe(2);
  });
});

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

  it("says how long a usage limit lasts before saying when it ends", () => {
    // How long the wait is decides whether to stay on the page; the clock
    // time is for whoever wants to come back at it (issue #136). Both are
    // measured from the moment the limit was announced, so the sentence
    // still says what the user was told when it is read back tomorrow.
    const [known] = foldTranscript([
      at(0, { type: "harness", event: { type: "usage_limited", resets_at_unix: T0 + 5_400 } }),
    ]);
    expect(known).toMatchObject({ kind: "notice", tone: "warning" });
    expect((known as { text: string }).text).toContain(
      `continues by itself in 1h 30m (${formatTimeOfDay(T0 + 5_400)}).`,
    );
    expect((known as { text: string }).text).not.toContain(":00:00");

    const [unknown] = foldTranscript([
      at(0, { type: "harness", event: { type: "usage_limited", resets_at_unix: null } }),
    ]);
    expect((unknown as { text: string }).text).toContain("waits for the account's limit to reset");
  });

  it("drops the countdown on a limit that had already reset when it was announced", () => {
    const [notice] = foldTranscript([
      at(0, { type: "harness", event: { type: "usage_limited", resets_at_unix: T0 - 60 } }),
    ]);
    expect((notice as { text: string }).text).toBe(
      `Usage limit reached. The agent continues by itself at ${formatTimeOfDay(T0 - 60)}.`,
    );
  });

  it("counts down a spot reclamation the way a person reads a countdown", () => {
    const [notice] = foldTranscript([at(0, { type: "spot_notice", seconds_remaining: 120 })]);
    expect(notice).toMatchObject({ kind: "notice", tone: "warning" });
    // `120s` is how a provider states a deadline (issue #136).
    expect((notice as { text: string }).text).toContain("reclaimed in 2m.");
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
    const part = turn?.kind === "turn" ? turn.parts[0] : undefined;
    const tools = part?.kind === "tools" ? part.calls : [];
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
      at(102, { type: "provisioning_stage", stage: "cloning", at_unix: T0 + 95 }),
      at(103, { type: "provisioning_stage", stage: "ready", at_unix: T0 + 210 }),
    ]);

    expect(items).toHaveLength(1);
    expect(items[0]).toEqual({
      kind: "provisioning",
      key: "provisioning-0",
      recovery: false,
      steps: [
        { stage: "reserving", atUnix: T0 },
        { stage: "booting", atUnix: T0 + 40 },
        { stage: "cloning", atUnix: T0 + 95 },
        { stage: "ready", atUnix: T0 + 210 },
      ],
    });
  });

  it("gives a session put back on its own disk a timeline of its own", () => {
    // A reclaimed session is recovered by starting the same machine again,
    // which is its own episode in the middle of the conversation — not
    // more lines on the timeline of the machine it was built on. Nothing
    // is installed and nothing is cloned: the disk already has both.
    const items = foldTranscript([
      at(100, { type: "provisioning_stage", stage: "reserving", at_unix: T0 }),
      at(101, { type: "provisioning_stage", stage: "ready", at_unix: T0 + 210 }),
      at(102, { type: "user_message", text: "audit the relay" }),
      at(103, { type: "spot_notice", seconds_remaining: 30 }),
      at(104, { type: "provisioning_stage", stage: "reserving", at_unix: T0 + 900 }),
      at(105, { type: "provisioning_stage", stage: "booting", at_unix: T0 + 930 }),
    ]);

    const timelines = items.filter((item) => item.kind === "provisioning");
    expect(timelines).toHaveLength(2);
    expect(timelines[0]).toMatchObject({ recovery: false });
    expect(timelines[1]).toMatchObject({
      recovery: true,
      steps: [
        { stage: "reserving", atUnix: T0 + 900 },
        { stage: "booting", atUnix: T0 + 930 },
      ],
    });
    // And it sits where it happened: after the countdown that explains it.
    expect(items.map((item) => item.kind)).toEqual([
      "provisioning",
      "user_message",
      "notice",
      "provisioning",
    ]);
  });

  it("treats a whole build replayed after a reconnect as the build it already showed", () => {
    // A reconnect's catch-up hands back frames the page has already seen.
    // Every stage carries the instant it happened, so the same `reserving`
    // at the same instant is that replay — not a machine being rebuilt,
    // which is what a second timeline headed `Migrating` would claim.
    const build = [
      { stage: "reserving", at: T0 },
      { stage: "booting", at: T0 + 40 },
      { stage: "cloning", at: T0 + 95 },
    ] as const;
    const frames = build.map((step, index) =>
      at(100 + index, { type: "provisioning_stage", stage: step.stage, at_unix: step.at }),
    );
    const items = foldTranscript([...frames, ...frames]);

    const timelines = items.filter((item) => item.kind === "provisioning");
    expect(timelines).toHaveLength(1);
    expect(timelines[0]).toMatchObject({
      recovery: false,
      steps: [
        { stage: "reserving", atUnix: T0 },
        { stage: "booting", atUnix: T0 + 40 },
        { stage: "cloning", atUnix: T0 + 95 },
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

describe("foldTranscript shell commands", () => {
  const RUN = "6f1c8e2a-1111-4b3a-9e1a-4c2f8b6d7a10";
  const OTHER = "6f1c8e2a-2222-4b3a-9e1a-4c2f8b6d7a10";

  it("collects a command, its output and its exit into one block", () => {
    const items = foldTranscript([
      at(0, { type: "shell_command", run: RUN, command: "cargo test" }),
      at(1, { type: "shell_output", run: RUN, stream: "stderr", data: "   Compiling\n" }),
      at(2, { type: "shell_output", run: RUN, stream: "stderr", data: "    Finished\n" }),
      at(3, { type: "shell_output", run: RUN, stream: "stdout", data: "ok\n" }),
      at(9, {
        type: "shell_exited",
        run: RUN,
        outcome: { kind: "exited", code: 0 },
        truncated: false,
      }),
    ]);

    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({
      kind: "shell",
      run: RUN,
      command: "cargo test",
      // Chunks off the same pipe are joined: where one 4 KiB read ended is
      // not something the reader should be able to see.
      output: [
        { stream: "stderr", data: "   Compiling\n    Finished\n" },
        { stream: "stdout", data: "ok\n" },
      ],
      outcome: { kind: "exited", code: 0 },
      truncated: false,
      endedAtUnix: T0 + 9,
    });
  });

  it("keeps two commands' output apart by the run the room named", () => {
    const items = foldTranscript([
      at(0, { type: "shell_command", run: RUN, command: "sleep 5; echo first" }),
      at(1, { type: "shell_command", run: OTHER, command: "echo second" }),
      at(2, { type: "shell_output", run: OTHER, stream: "stdout", data: "second\n" }),
      at(3, { type: "shell_output", run: RUN, stream: "stdout", data: "first\n" }),
    ]);

    expect(items).toHaveLength(2);
    expect(items[0]).toMatchObject({
      command: "sleep 5; echo first",
      output: [{ stream: "stdout", data: "first\n" }],
    });
    expect(items[1]).toMatchObject({
      command: "echo second",
      output: [{ stream: "stdout", data: "second\n" }],
    });
  });

  it("leaves a running command running", () => {
    const [block] = foldTranscript([
      at(0, { type: "shell_command", run: RUN, command: "cargo build" }),
    ]);
    expect(block).toMatchObject({ kind: "shell", outcome: null, endedAtUnix: null });
  });

  it("says a command never ran rather than showing nothing at all", () => {
    const [block] = foldTranscript([
      at(0, { type: "shell_command", run: RUN, command: "ls" }),
      at(0, { type: "shell_exited", run: RUN, outcome: { kind: "offline" }, truncated: false }),
    ]);
    expect(block).toMatchObject({ kind: "shell", outcome: { kind: "offline" } });
    expect(shellOutcomeLabel({ kind: "offline" })).toContain("Not run");
  });

  it("names every way a command can end", () => {
    expect(shellOutcomeLabel({ kind: "exited", code: 3 })).toBe("Exit 3");
    expect(shellOutcomeLabel({ kind: "timed_out", after_seconds: 120 })).toBe(
      "Timed out after 120s",
    );
    expect(shellOutcomeLabel({ kind: "cancelled" })).toBe("Stopped");
    expect(shellOutcomeLabel({ kind: "failed", error: "no bash" })).toContain("no bash");
    expect(shellSucceeded({ kind: "exited", code: 0 })).toBe(true);
    expect(shellSucceeded({ kind: "exited", code: 1 })).toBe(false);
    expect(shellSucceeded({ kind: "cancelled" })).toBe(false);
  });

  it("catches output whose command scrolled out of the replay window", () => {
    // The room's catch-up is paged, and a browser can join a session with
    // the command already behind it. Dropping the output would be worse
    // than an unnamed block.
    const [block] = foldTranscript([
      at(0, { type: "shell_output", run: RUN, stream: "stdout", data: "still here\n" }),
    ]);
    expect(block).toMatchObject({
      kind: "shell",
      command: "",
      output: [{ stream: "stdout", data: "still here\n" }],
    });
  });
});

describe("shellCommandIn", () => {
  it("takes the command out of a `!` message and leaves a prompt alone", () => {
    expect(shellCommandIn("!git status --short")).toBe("git status --short");
    expect(shellCommandIn("! cargo test ")).toBe("cargo test");
    expect(shellCommandIn("what does this crate do?")).toBeNull();
  });

  it("treats a bare `!` as a prompt, because there is nothing to run", () => {
    expect(shellCommandIn("!")).toBeNull();
    expect(shellCommandIn("!   ")).toBeNull();
  });
});
