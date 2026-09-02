/**
 * Folds a session's raw relay stream into renderable transcript items.
 *
 * One block per turn (assistant text plus its tool calls), one row per user
 * message, one card per approval, one timeline for the machine being built,
 * and a notice for anything else worth a human's attention — a budget
 * threshold, a spot eviction, a compaction, a dirty tree.
 *
 * Everything the session page shows in the scrolling column is decided
 * here, in one pass over the events, so the order things happened in is the
 * order they are read in. Nothing is rendered from a second source: an
 * approval card and the amber banner above it are the same item.
 *
 * Pure and framework-free so it can be recomputed from the full event list
 * on every relay update without depending on Solid.
 */
import type { TimedEvent } from "../api/relay";
import type {
  ApprovalPayload,
  ProvisioningStage,
  ShellOutcome,
  ShellStream,
  UsageReport,
} from "../api/wire";
import type { ApprovalState } from "../api/client";
import { formatUsd } from "./money";

export interface ToolCall {
  callId: string;
  tool: string;
  input: unknown;
  /** `null` while the tool is still running. */
  ok: boolean | null;
  /** When it started, seconds since the Unix epoch. */
  startedAtUnix: number;
  /** When it finished, or `null` while it is still running. */
  endedAtUnix: number | null;
}

export type TurnStatus = "running" | "completed" | "failed";

/**
 * What kind of attention a notice wants, which is what picks its icon and
 * its colour. Deliberately not a free-form string: the session page renders
 * a fixed set of icons, and a tone it has never heard of would render
 * nothing.
 */
export type NoticeTone = "info" | "warning" | "danger";

/** One run of output, as it came off one of the two pipes. */
export interface ShellChunk {
  stream: ShellStream;
  data: string;
}

export type TranscriptItem =
  | { kind: "user_message"; key: string; text: string; atUnix: number }
  | {
      /**
       * A `!` command the user ran on the machine (docs/ux.md §9.3).
       *
       * Its own item rather than a user message, because it is not part of
       * the conversation: the agent was never told about it, and what it
       * has to show is a command, its output and an exit status rather than
       * a sentence.
       */
      kind: "shell";
      key: string;
      /** The run every frame about this command carries. */
      run: string;
      /** What was typed, without the `!`. */
      command: string;
      /** Output so far, oldest first. */
      output: ShellChunk[];
      /** `null` while the command is still running. */
      outcome: ShellOutcome | null;
      /** Whether output was dropped after the run's byte cap. */
      truncated: boolean;
      atUnix: number;
      /** When it finished, or `null` while it is still running. */
      endedAtUnix: number | null;
    }
  | {
      kind: "turn";
      key: string;
      turnId: string;
      text: string;
      tools: ToolCall[];
      status: TurnStatus;
      error: string | null;
      usage: UsageReport | null;
      /** When the turn started, seconds since the Unix epoch. */
      startedAtUnix: number;
      /** When it ended, or `null` while it is still running. */
      endedAtUnix: number | null;
    }
  | {
      kind: "approval";
      key: string;
      id: string;
      payload: ApprovalPayload;
      state: ApprovalState;
      atUnix: number;
    }
  | {
      kind: "provisioning";
      key: string;
      steps: ProvisioningStep[];
      /**
       * Whether this timeline is flyco putting the session back on a
       * machine it lost, rather than building its first one.
       *
       * A session is reclaimed and recovered any number of times, so the
       * stages arrive in *episodes*: a second `reserving` after a `ready`
       * is a new machine being started, not a duplicate of the first. Each
       * episode is its own timeline, in the place in the transcript where
       * it happened, and every one after the first is a migration.
       */
      recovery: boolean;
    }
  | {
      /**
       * The session moved onto another machine (docs/ux.md §9.5).
       *
       * Its own item rather than a notice, because it says three things and
       * the middle one is the part people miss: what the machine is now,
       * that the old one was restarted out from under whatever was running
       * on it, and that the disk came across untouched.
       */
      kind: "machine_change";
      key: string;
      machineType: string;
      /** Integer microdollars, or `null` where flyco meters nothing. */
      hourlyMicros: number | null;
      spot: boolean;
      restarted: boolean;
      atUnix: number;
    }
  | { kind: "notice"; key: string; text: string; tone: NoticeTone; atUnix: number };

/** One milestone on the provisioning timeline. */
export interface ProvisioningStep {
  stage: ProvisioningStage;
  atUnix: number;
}

type Turn = Extract<TranscriptItem, { kind: "turn" }>;
type Approval = Extract<TranscriptItem, { kind: "approval" }>;
type Provisioning = Extract<TranscriptItem, { kind: "provisioning" }>;
type Shell = Extract<TranscriptItem, { kind: "shell" }>;

function findTurn(items: TranscriptItem[], turnId: string): Turn | undefined {
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i];
    if (item !== undefined && item.kind === "turn" && item.turnId === turnId) {
      return item;
    }
  }
  return undefined;
}

function startTurn(items: TranscriptItem[], turnId: string, atUnix: number): Turn {
  const existing = findTurn(items, turnId);
  if (existing !== undefined) {
    return existing;
  }
  const turn: Turn = {
    kind: "turn",
    key: `turn-${turnId}`,
    turnId,
    text: "",
    tools: [],
    status: "running",
    error: null,
    usage: null,
    startedAtUnix: atUnix,
    endedAtUnix: null,
  };
  items.push(turn);
  return turn;
}

/**
 * The block one run's frames belong to.
 *
 * Keyed by the run the session room assigned, so two commands started
 * moments apart cannot collect each other's output — and a run whose
 * command scrolled out of the replay window still gets a block rather than
 * having its output dropped on the floor. The command reads as empty there,
 * which is the truth: this browser never saw what was typed.
 */
function shellBlock(items: TranscriptItem[], run: string, atUnix: number): Shell {
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i];
    if (item !== undefined && item.kind === "shell" && item.run === run) {
      return item;
    }
  }
  const block: Shell = {
    kind: "shell",
    key: `shell-${run}`,
    run,
    command: "",
    output: [],
    outcome: null,
    truncated: false,
    atUnix,
    endedAtUnix: null,
  };
  items.push(block);
  return block;
}

/**
 * Appends a chunk, joining it to the one before it when both came off the
 * same pipe.
 *
 * A pipe is read in 4 KiB bites and nothing about where one ends is
 * meaningful, so rendering each as its own run of text would put a seam in
 * the middle of a line.
 */
function appendChunk(block: Shell, stream: ShellStream, data: string): void {
  const last = block.output[block.output.length - 1];
  if (last !== undefined && last.stream === stream) {
    last.data += data;
    return;
  }
  block.output.push({ stream, data });
}

/**
 * The timeline one stage belongs to.
 *
 * One item per *episode*, not one per stage and not one per session. The
 * milestones of a single build are one thing the user reads top to bottom,
 * so splitting them across the transcript would interleave them with
 * whatever else arrived. But a session on spot capacity is reclaimed and
 * recovered any number of times, and each recovery is its own build in its
 * own place in the conversation — so a stage that the open timeline has
 * already been through starts a new one, and every timeline after the first
 * is a migration.
 */
function provisioningTimeline(items: TranscriptItem[], stage: ProvisioningStage): Provisioning {
  let open: Provisioning | undefined;
  let episodes = 0;
  for (const item of items) {
    if (item.kind === "provisioning") {
      open = item;
      episodes += 1;
    }
  }
  // Only `reserving` opens an episode, and only when the timeline in front
  // of it has already been through one. Every other repeat is a stage the
  // at-least-once relay delivered twice, which the caller drops.
  const starts = stage === "reserving" && open?.steps.some((step) => step.stage === stage) === true;
  if (open !== undefined && !starts) {
    return open;
  }
  const timeline: Provisioning = {
    kind: "provisioning",
    key: `provisioning-${episodes}`,
    steps: [],
    recovery: episodes > 0,
  };
  items.push(timeline);
  return timeline;
}

function findApproval(items: TranscriptItem[], id: string): Approval | undefined {
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i];
    if (item !== undefined && item.kind === "approval" && item.id === id) {
      return item;
    }
  }
  return undefined;
}

/** When a usage limit resets, as a sentence rather than an epoch second. */
function usageLimitText(resetsAtUnix: number | null): string {
  if (resetsAtUnix === null) {
    return "Usage limit reached. The agent waits for the account's limit to reset.";
  }
  const resets = new Date(resetsAtUnix * 1000).toLocaleString();
  return `Usage limit reached. The agent continues by itself at ${resets}.`;
}

export function foldTranscript(events: readonly TimedEvent[]): TranscriptItem[] {
  const items: TranscriptItem[] = [];
  let noticeSeq = 0;

  const notice = (text: string, tone: NoticeTone, atUnix: number): void => {
    items.push({ kind: "notice", key: `notice-${noticeSeq}`, text, tone, atUnix });
    noticeSeq += 1;
  };

  for (const { event, atUnix } of events) {
    switch (event.type) {
      case "user_message":
        items.push({
          kind: "user_message",
          key: `user-${items.length}`,
          text: event.text,
          atUnix,
        });
        break;
      case "shell_command":
        shellBlock(items, event.run, atUnix).command = event.command;
        break;
      case "shell_output":
        appendChunk(shellBlock(items, event.run, atUnix), event.stream, event.data);
        break;
      case "shell_exited": {
        const block = shellBlock(items, event.run, atUnix);
        block.outcome = event.outcome;
        block.truncated = event.truncated;
        block.endedAtUnix = atUnix;
        break;
      }
      case "harness": {
        const harness = event.event;
        switch (harness.type) {
          case "turn_started":
            startTurn(items, harness.turn_id, atUnix);
            break;
          case "assistant_delta": {
            const turn = startTurn(items, harness.turn_id, atUnix);
            turn.text += harness.text;
            break;
          }
          case "tool_started": {
            const turn = startTurn(items, harness.turn_id, atUnix);
            turn.tools.push({
              callId: harness.call_id,
              tool: harness.tool,
              input: harness.input,
              ok: null,
              startedAtUnix: atUnix,
              endedAtUnix: null,
            });
            break;
          }
          case "tool_completed": {
            const turn = startTurn(items, harness.turn_id, atUnix);
            const call = turn.tools.find((candidate) => candidate.callId === harness.call_id);
            if (call !== undefined) {
              call.ok = harness.ok;
              call.endedAtUnix = atUnix;
            }
            break;
          }
          case "turn_completed": {
            const turn = startTurn(items, harness.turn_id, atUnix);
            turn.status = "completed";
            turn.usage = harness.usage;
            turn.endedAtUnix = atUnix;
            break;
          }
          case "turn_failed": {
            const turn = startTurn(items, harness.turn_id, atUnix);
            turn.status = "failed";
            turn.error = harness.error;
            turn.endedAtUnix = atUnix;
            break;
          }
          case "usage_limited":
            notice(usageLimitText(harness.resets_at_unix), "warning", atUnix);
            break;
          case "context_compacted":
            notice(
              "Context compacted. The conversation was summarised to make room.",
              "info",
              atUnix,
            );
            break;
          case "context_compaction_failed":
            notice(`Context compaction failed: ${harness.error}`, "danger", atUnix);
            break;
        }
        break;
      }
      case "approval_pending":
        items.push({
          kind: "approval",
          key: `approval-${event.id}`,
          id: event.id,
          payload: event.payload,
          state: "pending",
          atUnix,
        });
        break;
      case "approval_decided": {
        const approval = findApproval(items, event.id);
        if (approval !== undefined) {
          approval.state = event.decision;
        }
        break;
      }
      case "provisioning_stage": {
        const timeline = provisioningTimeline(items, event.stage);
        // The stage carries its own time: it is when the milestone was
        // reached, which is not when the room recorded the frame — the
        // queue and the daemon are both minutes away from the room.
        if (!timeline.steps.some((step) => step.stage === event.stage)) {
          timeline.steps.push({ stage: event.stage, atUnix: event.at_unix });
        }
        break;
      }
      case "repo_dirty":
        // An empty summary is a clean tree, and a clean tree is not news.
        if (event.summary.trim() !== "") {
          notice(`Uncommitted changes in the working tree: ${event.summary}`, "warning", atUnix);
        }
        break;
      case "machine_changed":
        items.push({
          kind: "machine_change",
          key: `machine-${items.length}`,
          machineType: event.machine_type,
          hourlyMicros: event.hourly,
          spot: event.spot,
          restarted: event.restarted,
          atUnix,
        });
        break;
      case "spot_notice":
        notice(
          `Spot capacity is being reclaimed in ${event.seconds_remaining}s. Flyco saves the work and moves the session.`,
          "warning",
          atUnix,
        );
        break;
      case "started":
      case "capabilities":
      case "session_state_changed":
      case "usage":
      case "terminal_output":
        // Rendered elsewhere (the header's status pill and rings, the
        // terminal tab of the drawer).
        break;
    }
  }

  return items;
}

type MachineChange = Extract<TranscriptItem, { kind: "machine_change" }>;

/**
 * The line docs/ux.md §9.5 asks for:
 * `Switched to Standard_D8s_v6 · restarted the machine · disk kept`.
 *
 * The restart is stated rather than implied because it is the part with
 * consequences — the dev server the user was watching is gone — and `disk
 * kept` is stated for the same reason in the other direction: nothing was
 * lost, and a person reading "restarted" needs to know that in the same
 * breath.
 */
export function machineChangeSummary(change: MachineChange): string {
  const parts = [`Switched to ${change.machineType}`];
  if (change.restarted) {
    parts.push("restarted the machine");
  }
  parts.push("disk kept");
  return parts.join(" · ");
}

/**
 * What the new machine costs: `$0.38/hr · spot`.
 *
 * `null` on hardware the user owns, where flyco meters nothing — `$0.00/hr`
 * there would read as "this is free", which is a different claim.
 */
export function machineChangePrice(change: MachineChange): string | null {
  if (change.hourlyMicros === null) {
    return null;
  }
  const hourly = `${formatUsd(change.hourlyMicros)}/hr`;
  return change.spot ? `${hourly} · spot` : hourly;
}

/** The approvals still waiting on the user, in the order they were raised. */
export function pendingApprovals(items: readonly TranscriptItem[]): Approval[] {
  return items.filter(
    (item): item is Approval => item.kind === "approval" && item.state === "pending",
  );
}
