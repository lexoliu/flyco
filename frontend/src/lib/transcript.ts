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
  MessageOrigin,
  ModelChoice,
  PermissionMode,
  ProvisioningStage,
  ShellOutcome,
  ShellStream,
  UsageReport,
  UsageWindow,
} from "../api/wire";
import type { ApprovalState } from "../api/client";
import { formatTimeOfDay } from "./dates";
import { formatDuration } from "./duration";
import { formatUsd } from "./money";

export interface ToolCall {
  /**
   * The identity `reconcile` diffs this row by, and `<For>` keys it with.
   * Same value as `callId`: the id is already unique inside its turn.
   */
  key: string;
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
  /**
   * The identity `reconcile` diffs this chunk by: its position, which is
   * stable because chunks only ever append or merge into the last one.
   */
  key: number;
  stream: ShellStream;
  data: string;
}

/**
 * One stretch of a turn, in the order the agent produced it.
 *
 * A turn used to be a string of prose and a flat list of tool calls,
 * rendered in that order whatever order they happened in. So a turn that
 * ran a command and then explained what it found showed the explanation
 * first and the command under it: the transcript told the reader the agent
 * answered and then went looking, which is the reverse of what it did.
 *
 * Contiguous calls are one part rather than one part each, because a run of
 * tool rows reads as a list and a list is what it should be in the markup.
 *
 * Every part carries a `key` — its position in the turn, stable because
 * parts only ever append or grow — because the page reconciles this list
 * rather than replacing it (see SessionDetail), and keyed reconciliation
 * is what keeps a streamed paragraph's DOM node alive between deltas.
 */
export type TurnPart =
  | { kind: "text"; key: number; text: string }
  | { kind: "tools"; key: number; calls: ToolCall[] };

export type TranscriptItem =
  | {
      kind: "user_message";
      key: string;
      text: string;
      atUnix: number;
      /**
       * Who wrote it. `flyco` is the continuation sent when a plan window
       * turned over, and the transcript marks it: a reader who comes back to
       * a session that carried on overnight has to be able to see which
       * sentences were theirs.
       */
      origin: MessageOrigin;
    }
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
      /** What the agent did, in the order it did it. */
      parts: TurnPart[];
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
       * machine it lost, rather than building its first one — or trying
       * again after the provider refused the last build, which is
       * `attempt` above one with this false.
       *
       * A session is reclaimed and recovered any number of times, so the
       * stages arrive in *episodes*: a second `reserving` after a `ready`
       * is a new machine being started, not a duplicate of the first. Each
       * episode is its own timeline, in the place in the transcript where
       * it happened, and every one after the first is a migration.
       */
      recovery: boolean;
      /**
       * Which attempt at a machine this is, counting from one and reset by
       * every build that came up. A retry headed "Migrating" told the user
       * their machine had been reclaimed when it had never existed.
       */
      attempt: number;
      /**
       * When a later episode opened with this one still short of `ready`,
       * which is when this one ended: the build it was waiting on was
       * given up, and the page has no other way to know. `null` while the
       * timeline is the latest or reached `ready`. A superseded timeline
       * left to tick against the clock (issue #254) showed a machine still
       * being reserved twenty minutes after its replacement came up.
       */
      endedAtUnix: number | null;
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
  | {
      /**
       * The user moved the session onto another model (docs/ux.md §9.3).
       *
       * Carries the choice's ids rather than a sentence, because the names
       * a sentence needs are the agent's own model list, which lives with
       * the page and not with the stream.
       */
      kind: "model_change";
      key: string;
      model: ModelChoice;
      atUnix: number;
    }
  | {
      /**
       * The user moved the session onto another permission mode
       * (docs/ux.md §9.3).
       *
       * Carries the mode's id rather than a sentence, like the model
       * change it sits beside: what a mode is called is the catalog's
       * business, which lives with the page and not with the stream.
       */
      kind: "mode_change";
      key: string;
      mode: PermissionMode;
      atUnix: number;
    }
  | { kind: "notice"; key: string; text: string; tone: NoticeTone; atUnix: number }
  | {
      /**
       * Something the harness printed on its own — a local slash command's
       * answer, like Claude Code's `/usage` table — rather than a reply to
       * a prompt. Rendered as output, not as assistant prose in a turn.
       */
      kind: "command_output";
      key: string;
      text: string;
      atUnix: number;
    };

/** One milestone on the provisioning timeline. */
export interface ProvisioningStep {
  /**
   * The identity `reconcile` diffs this step by: the milestone and the
   * instant it was reached, which together name one step exactly.
   */
  key: string;
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

/** The call this turn already has under that id, wherever it sits. */
function findCall(turn: Turn, callId: string): ToolCall | undefined {
  for (const part of turn.parts) {
    if (part.kind === "tools") {
      const call = part.calls.find((candidate) => candidate.callId === callId);
      if (call !== undefined) {
        return call;
      }
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
    parts: [],
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
  block.output.push({ key: block.output.length, stream, data });
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
 * already been through starts a new one. A timeline after one that reached
 * `ready` is a migration; one after a build the provider refused is a
 * retry, and says so.
 *
 * A stage is identified by when it happened, not by its name: the relay
 * delivers at least once and replays what it already sent after a
 * reconnect, and a second `reserving` bearing the *same* instant is that
 * replay. Only a `reserving` at a new instant is a new machine, so this
 * answers `null` for the replay rather than opening a migration the
 * session never had.
 */
function provisioningTimeline(
  items: TranscriptItem[],
  stage: ProvisioningStage,
  atUnix: number,
): Provisioning | null {
  let open: Provisioning | undefined;
  let episodes = 0;
  // How many episodes in a row ended without a machine.
  let refused = 0;
  for (const item of items) {
    if (item.kind === "provisioning") {
      if (item.steps.some((step) => step.stage === stage && step.atUnix === atUnix)) {
        return null;
      }
      open = item;
      episodes += 1;
      refused = item.steps.some((step) => step.stage === "ready") ? 0 : refused + 1;
    }
  }
  // Only `reserving` opens an episode, and only when the timeline in front
  // of it has already been through one. Every other repeat is a stage the
  // at-least-once relay delivered twice, which the caller drops.
  const starts = stage === "reserving" && open?.steps.some((step) => step.stage === stage) === true;
  if (open !== undefined && !starts) {
    return open;
  }
  if (open !== undefined && !open.steps.some((step) => step.stage === "ready")) {
    open.endedAtUnix = atUnix;
  }
  const timeline: Provisioning = {
    kind: "provisioning",
    key: `provisioning-${episodes}`,
    steps: [],
    recovery: episodes > 0 && refused === 0,
    attempt: refused + 1,
    endedAtUnix: null,
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

/**
 * When a usage limit resets, as a sentence rather than an epoch second.
 *
 * The window is named first — a five-hour limit and a weekly one are the
 * same event with wildly different consequences — then how long the wait is,
 * because that is the question `in 1h 30m` answers, then the clock time in
 * brackets for whoever wants to come back at it. The wait is measured
 * against `atUnix`, the moment the limit was announced, so a transcript read
 * back tomorrow still says what the user was told then rather than a
 * countdown that has long since run out.
 *
 * A reset already in the past when it was announced drops the countdown
 * rather than printing `in 0s`.
 */
export function usageLimitText(window: UsageWindow, atUnix: number): string {
  const limit = `The ${window.label} usage limit is spent.`;
  const resetsAtUnix = window.resets_at_unix;
  if (resetsAtUnix === null || resetsAtUnix === undefined) {
    return `${limit} The session waits for the account's limit to reset.`;
  }
  const clock = formatTimeOfDay(resetsAtUnix);
  if (resetsAtUnix <= atUnix) {
    return `${limit} Flyco continues the session at ${clock}.`;
  }
  const wait = formatDuration(resetsAtUnix - atUnix);
  return `${limit} Flyco continues the session in ${wait} (${clock}).`;
}

/**
 * What a turn that stopped mid-way says for itself.
 *
 * The harness's own error is the second half, not the whole notice: `stream
 * closed by the provider: overloaded_error` on a line of its own tells a
 * first-time reader neither that the turn is over nor that the session is
 * still theirs to continue (issue #136).
 */
export const TURN_FAILED_NOTE =
  "The turn stopped before it finished. Send a message to continue it.";

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
          origin: event.origin,
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
            const last = turn.parts.at(-1);
            if (last?.kind === "text") {
              last.text += harness.text;
            } else {
              turn.parts.push({ kind: "text", key: turn.parts.length, text: harness.text });
            }
            break;
          }
          case "tool_started": {
            const turn = startTurn(items, harness.turn_id, atUnix);
            // A call id names one call. Seeing it twice is a delivery
            // repeating itself, not the agent running the command again,
            // and appending would leave a second row that no completion
            // ever reaches — spinning under the finished one forever.
            if (findCall(turn, harness.call_id) !== undefined) {
              break;
            }
            const call: ToolCall = {
              key: harness.call_id,
              callId: harness.call_id,
              tool: harness.tool,
              input: harness.input,
              ok: null,
              startedAtUnix: atUnix,
              endedAtUnix: null,
            };
            const last = turn.parts.at(-1);
            if (last?.kind === "tools") {
              last.calls.push(call);
            } else {
              turn.parts.push({ kind: "tools", key: turn.parts.length, calls: [call] });
            }
            break;
          }
          case "tool_completed": {
            const turn = startTurn(items, harness.turn_id, atUnix);
            const call = findCall(turn, harness.call_id);
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
            notice(usageLimitText(harness.window, atUnix), "warning", atUnix);
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
          case "local_command_output":
            items.push({
              kind: "command_output",
              key: `output-${items.length}`,
              text: harness.content,
              atUnix,
            });
            break;
          case "context_usage":
            // The answer to the usage panel's "detailed breakdown" — the
            // panel reads the newest frame out of the stream itself; a
            // `context_usage` is a control answer, not something that
            // happened, so the transcript keeps no card of it
            // (docs/ux.md §9.3).
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
        // The stage carries its own time: it is when the milestone was
        // reached, which is not when the room recorded the frame — the
        // queue and the daemon are both minutes away from the room.
        const timeline = provisioningTimeline(items, event.stage, event.at_unix);
        if (timeline !== null && !timeline.steps.some((step) => step.stage === event.stage)) {
          timeline.steps.push({
            key: `${event.stage}-${event.at_unix}`,
            stage: event.stage,
            atUnix: event.at_unix,
          });
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
      case "model_changed":
        items.push({
          kind: "model_change",
          key: `model-${items.length}`,
          model: event.model,
          atUnix,
        });
        break;
      case "permission_mode_changed":
        items.push({
          kind: "mode_change",
          key: `mode-${items.length}`,
          mode: event.mode,
          atUnix,
        });
        break;
      case "spot_notice":
        // `120s` is how a provider states a deadline; `2m` is how a person
        // reads one (issue #136).
        notice(
          `Spot capacity is being reclaimed in ${formatDuration(
            event.seconds_remaining,
          )}. Flyco saves the work and moves the session.`,
          "warning",
          atUnix,
        );
        break;
      case "started":
      case "capabilities":
      case "models":
      case "plan_usage":
      case "commands":
      case "session_state_changed":
      case "machine_connection":
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
