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
import type { ApprovalPayload, ProvisioningStage, UsageReport } from "../api/wire";
import type { ApprovalState } from "../api/client";

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

export type TranscriptItem =
  | { kind: "user_message"; key: string; text: string; atUnix: number }
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
 * The timeline the provisioning stages accumulate into.
 *
 * One item, not one per stage: the five milestones are a single thing the
 * user reads top to bottom while a machine is built, and splitting them
 * across the transcript would interleave them with whatever else arrived.
 */
function provisioningTimeline(items: TranscriptItem[]): Provisioning {
  for (const item of items) {
    if (item.kind === "provisioning") {
      return item;
    }
  }
  const timeline: Provisioning = { kind: "provisioning", key: "provisioning", steps: [] };
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
        const timeline = provisioningTimeline(items);
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

/** The approvals still waiting on the user, in the order they were raised. */
export function pendingApprovals(items: readonly TranscriptItem[]): Approval[] {
  return items.filter(
    (item): item is Approval => item.kind === "approval" && item.state === "pending",
  );
}
