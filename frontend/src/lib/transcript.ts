/**
 * Folds a session's raw [`ClientEvent`] stream into renderable transcript
 * items: one block per turn (assistant text plus its tool calls,
 * collapsible), one row per user message, and a row for anything else worth
 * a human's attention (spot eviction notices, a dirty repo).
 *
 * Pure and framework-free so it can be recomputed from the full event list
 * on every relay update without depending on Solid.
 */
import type { ClientEvent, UsageReport } from "../api/wire";

export interface ToolCall {
  callId: string;
  tool: string;
  input: unknown;
  /** `null` while the tool is still running. */
  ok: boolean | null;
}

export type TurnStatus = "running" | "completed" | "failed";

export type TranscriptItem =
  | { kind: "user_message"; key: string; text: string }
  | {
      kind: "turn";
      key: string;
      turnId: string;
      text: string;
      tools: ToolCall[];
      status: TurnStatus;
      error: string | null;
      usage: UsageReport | null;
    }
  | { kind: "notice"; key: string; text: string };

function findTurn(items: TranscriptItem[], turnId: string): Extract<TranscriptItem, { kind: "turn" }> | undefined {
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i];
    if (item !== undefined && item.kind === "turn" && item.turnId === turnId) {
      return item;
    }
  }
  return undefined;
}

function startTurn(items: TranscriptItem[], turnId: string): Extract<TranscriptItem, { kind: "turn" }> {
  const existing = findTurn(items, turnId);
  if (existing !== undefined) {
    return existing;
  }
  const turn: Extract<TranscriptItem, { kind: "turn" }> = {
    kind: "turn",
    key: `turn-${turnId}`,
    turnId,
    text: "",
    tools: [],
    status: "running",
    error: null,
    usage: null,
  };
  items.push(turn);
  return turn;
}

export function foldTranscript(events: readonly ClientEvent[]): TranscriptItem[] {
  const items: TranscriptItem[] = [];
  let noticeSeq = 0;

  for (const event of events) {
    switch (event.type) {
      case "user_message":
        items.push({ kind: "user_message", key: `user-${items.length}`, text: event.text });
        break;
      case "harness": {
        const harness = event.event;
        switch (harness.type) {
          case "turn_started":
            startTurn(items, harness.turn_id);
            break;
          case "assistant_delta": {
            const turn = startTurn(items, harness.turn_id);
            turn.text += harness.text;
            break;
          }
          case "tool_started": {
            const turn = startTurn(items, harness.turn_id);
            turn.tools.push({ callId: harness.call_id, tool: harness.tool, input: harness.input, ok: null });
            break;
          }
          case "tool_completed": {
            const turn = startTurn(items, harness.turn_id);
            const call = turn.tools.find((candidate) => candidate.callId === harness.call_id);
            if (call !== undefined) {
              call.ok = harness.ok;
            }
            break;
          }
          case "turn_completed": {
            const turn = startTurn(items, harness.turn_id);
            turn.status = "completed";
            turn.usage = harness.usage;
            break;
          }
          case "turn_failed": {
            const turn = startTurn(items, harness.turn_id);
            turn.status = "failed";
            turn.error = harness.error;
            break;
          }
          case "usage_limited":
            items.push({
              kind: "notice",
              key: `notice-${noticeSeq++}`,
              text:
                harness.resets_at_unix === null
                  ? "Usage limit reached."
                  : `Usage limit reached; resets ${new Date(harness.resets_at_unix * 1000).toLocaleString()}.`,
            });
            break;
          case "context_compacted":
            items.push({ kind: "notice", key: `notice-${noticeSeq++}`, text: "Context compacted." });
            break;
          case "context_compaction_failed":
            items.push({
              kind: "notice",
              key: `notice-${noticeSeq++}`,
              text: `Context compaction failed: ${harness.error}`,
            });
            break;
        }
        break;
      }
      case "repo_dirty":
        items.push({ kind: "notice", key: `notice-${noticeSeq++}`, text: `Working tree dirty: ${event.summary}` });
        break;
      case "spot_notice":
        items.push({
          kind: "notice",
          key: `notice-${noticeSeq++}`,
          text: `Spot capacity reclaiming in ${event.seconds_remaining}s.`,
        });
        break;
      case "started":
      case "capabilities":
      case "approval_pending":
      case "approval_decided":
      case "session_state_changed":
      case "usage":
      case "terminal_output":
        // Rendered elsewhere (approvals panel, usage meters, terminal pane).
        break;
    }
  }

  return items;
}
