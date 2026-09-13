/**
 * The daemon <-> control-plane wire protocol, as seen by a browser.
 *
 * This is hand-typed rather than generated: `flyco_core::wire` is the
 * event vocabulary the SSE stream carries — `SessionEvent` envelopes on
 * `GET /v1/events`, `StoredEvent.event` on catch-up pages — with no
 * OpenAPI schema of its own (see `crates/core/src/wire.rs`). Only
 * `ClientEvent`, the `SessionEvent` envelope, and the client-sendable
 * `ControlToDaemon` variants matter here — everything else in that module
 * (`DaemonToControl`, the control-plane-only `ControlToDaemon` variants)
 * never reaches a browser.
 *
 * Every enum is internally tagged (`type` for events, `kind` for
 * `ApprovalPayload`) with `snake_case` variant names, matching serde's
 * `#[serde(tag = "...", rename_all = "snake_case")]` on the Rust side.
 * `ApprovalPayload`, `ApprovalDecision`, `SessionState`, `UsageReport` and
 * `ContextWindow` already have OpenAPI schemas (they also appear in REST
 * bodies), so they are re-exported from the generated client rather than
 * retyped here.
 */
import type { components } from "./schema";

export type ApprovalPayload = components["schemas"]["ApprovalPayload"];
export type ApprovalDecision = components["schemas"]["ApprovalDecision"];
export type SessionState = components["schemas"]["SessionState"];
export type UsageReport = components["schemas"]["UsageReport"];
export type ContextWindow = components["schemas"]["ContextWindow"];
export type ModelChoice = components["schemas"]["ModelChoice"];
export type ModelOption = components["schemas"]["ModelOption"];
export type PermissionMode = components["schemas"]["PermissionMode"];
export type UsageWindow = components["schemas"]["UsageWindow"];

/**
 * Who wrote a message that appears in the conversation as the user's.
 *
 * Mirrors `flyco_core::MessageOrigin`. Almost every user message is the
 * user's, and the one exception matters to whoever reads the transcript
 * back: flyco says `usage limit reset, please continue` on their behalf when
 * a plan window turns over, and a transcript that showed it as theirs would
 * be crediting them with a sentence they never typed.
 */
export type MessageOrigin = "user" | "flyco";

/**
 * One thing occupying the context window, as a `context_usage` answer lists it.
 *
 * Mirrors `flyco_core::harness::ContextCost` (WS-only, no OpenAPI schema):
 * one shape for every section the harness reports — a usage category, an
 * MCP tool's schema, a memory file, an agent, a skill's frontmatter.
 * `deferred` is counted toward the window but not materialized in it yet.
 */
export interface ContextCost {
  name: string;
  tokens: number;
  deferred: boolean;
}

/**
 * What the context window is spent on — the answer to a `context_usage`
 * control request, which the usage panel's "detailed breakdown" sends.
 *
 * Mirrors `flyco_core::harness::ContextUsage` exactly. `model`, `window`
 * and `auto_compact` are absent rather than `null`, matching serde's
 * `skip_serializing_if` on the `Option`s. A harness with no breakdown to
 * give (Codex) reports the fill alone and every list empty.
 */
export interface ContextUsage {
  model?: string | undefined;
  window?: ContextWindow | undefined;
  /** The fill at which the harness compacts on its own, in tokens. */
  auto_compact?: number | undefined;
  categories: ContextCost[];
  mcp_tools: ContextCost[];
  memory_files: ContextCost[];
  agents: ContextCost[];
  skills: ContextCost[];
}

/**
 * A normalized event extracted from either harness's native stream.
 *
 * Mirrors `flyco_core::harness::HarnessEvent` exactly (WS-only, no OpenAPI
 * schema).
 */
export type HarnessEvent =
  | { type: "turn_started"; turn_id: string }
  | { type: "assistant_delta"; turn_id: string; text: string }
  | { type: "tool_started"; turn_id: string; call_id: string; tool: string; input: unknown }
  | { type: "tool_completed"; turn_id: string; call_id: string; ok: boolean }
  | { type: "turn_completed"; turn_id: string; usage: UsageReport }
  | { type: "turn_failed"; turn_id: string; error: string }
  | { type: "usage_limited"; window: UsageWindow }
  | { type: "context_compacted" }
  | { type: "context_compaction_failed"; error: string }
  /**
   * Output the harness printed on its own — a local slash command's answer,
   * or a synthetic message that never streamed. Never a reply to a user
   * prompt.
   */
  | { type: "local_command_output"; content: string }
  /** The context-window breakdown answering a `context_usage` command. */
  | { type: "context_usage"; usage: ContextUsage };

/**
 * One slash command the running harness offers, mirroring
 * `flyco_core::wire::HarnessCommand`.
 *
 * Hand-typed like the rest of this file: the command list travels only over
 * the relay, so it has no OpenAPI schema to generate from. `name` carries
 * no leading slash and may contain a colon (`presence:status` is a plugin's
 * skill). `argument_hint` is `null` for a command that takes no argument,
 * which is what lets the palette send it in one keystroke.
 */
export interface HarnessCommand {
  name: string;
  description: string;
  argument_hint: string | null;
}

/**
 * How far a session's machine has got towards running an agent.
 *
 * Mirrors `flyco_core::wire::ProvisioningStage`: the milestones flyco can
 * observe, in the order they happen. The session page renders them as a
 * timeline inside the transcript (docs/ux.md §9.2). Only milestones that
 * something can actually see are here — boot and install are one, because
 * nothing watches the seam between them.
 */
export type ProvisioningStage = "reserving" | "booting" | "cloning" | "ready";

/**
 * Which of a shell command's two output streams a chunk came from, mirroring
 * `flyco_core::wire::ShellStream`.
 */
export type ShellStream = "stdout" | "stderr";

/**
 * How a `!` shell command ended, mirroring `flyco_core::wire::ShellOutcome`.
 *
 * Tagged on `kind` rather than `type`, because it is nested inside a frame
 * that is already tagged on `type`.
 */
export type ShellOutcome =
  | { kind: "exited"; code: number }
  | { kind: "signalled" }
  | { kind: "timed_out"; after_seconds: number }
  | { kind: "cancelled" }
  | { kind: "offline" }
  | { kind: "busy" }
  | { kind: "refused" }
  | { kind: "failed"; error: string };

/**
 * What a browser attached to a session room receives, mirroring
 * `flyco_core::wire::ClientEvent` exactly.
 */
export type ClientEvent =
  | { type: "harness"; event: HarnessEvent }
  | { type: "user_message"; text: string; origin: MessageOrigin }
  | { type: "shell_command"; run: string; command: string }
  | { type: "shell_output"; run: string; stream: ShellStream; data: string }
  | { type: "shell_exited"; run: string; outcome: ShellOutcome; truncated: boolean }
  | { type: "started"; harness_session_id: string }
  | { type: "capabilities"; capabilities: string[] }
  | { type: "approval_pending"; id: string; payload: ApprovalPayload }
  | { type: "approval_decided"; id: string; decision: ApprovalDecision }
  | { type: "session_state_changed"; state: SessionState }
  | { type: "machine_connection"; connected: boolean }
  | { type: "usage"; usage: UsageReport }
  | { type: "terminal_output"; data: string }
  | { type: "repo_dirty"; summary: string }
  | { type: "spot_notice"; seconds_remaining: number }
  | { type: "provisioning_stage"; stage: ProvisioningStage; at_unix: number }
  | {
      type: "machine_changed";
      machine_type: string;
      /** Integer microdollars, or `null` on hardware flyco does not meter. */
      hourly: number | null;
      spot: boolean;
      restarted: boolean;
    }
  /** The session was moved onto another model, by the user (docs/ux.md §9.3). */
  | { type: "model_changed"; model: ModelChoice }
  /**
   * The session's permission mode changed, by the user — the same shape a
   * model change takes: recorded by the control plane, echoed to every
   * browser, applied by the daemon when it can (docs/ux.md §9.3).
   */
  | { type: "permission_mode_changed"; mode: PermissionMode }
  /**
   * The models the agent offers, as it listed them at start. State rather
   * than history: the newest list wins and the page reads the last one.
   */
  | { type: "models"; models: ModelOption[] }
  /**
   * How much of the plan behind the session's harness account is spent.
   * State, like `models`: the newest snapshot wins and the composer's rings
   * read the last one. Named `plan_usage` and not `usage` because `usage`
   * is already this session's own token count.
   */
  | { type: "plan_usage"; windows: UsageWindow[] }
  /**
   * The slash commands the agent offers, as the running harness lists them.
   * State rather than history, like `models`: the newest list wins and the
   * composer's `/` palette reads the last one.
   */
  | { type: "commands"; commands: HarnessCommand[] };

/**
 * The seven `ControlToDaemon` variants a browser may send, mirroring
 * `ControlToDaemon::is_client_command()` — each reaches the daemon through
 * its own REST route (see `send()` in `src/api/relay.ts`). Every other
 * command (approval decisions, budget signals, archive, and the
 * identified `run_shell` the room reissues a `shell_command` as) is
 * control-plane authority and reaches the daemon only through the room
 * itself or an authenticated REST handler.
 */
export type ClientCommand =
  | { type: "user_message"; text: string }
  | { type: "shell_command"; command: string }
  | { type: "interrupt" }
  | { type: "compact" }
  /** flyco's `/context`: a read-only query, answered by a `context_usage` event. */
  | { type: "context_usage" }
  | { type: "terminal_input"; data: string }
  | { type: "terminal_resize"; cols: number; rows: number };

/**
 * One event on the per-user stream, as `GET /v1/events` frames it.
 *
 * Mirrors `flyco_core::wire::SessionEvent`: the envelope is what tells the
 * sessions multiplexed onto the one stream apart, and `seq` — the event's
 * position in its session's recorded history, when it has one — is what a
 * subscriber checks for a gap the reconnect buffer dropped (live-only
 * events like `machine_connection` carry `null` and have no position).
 */
export interface SessionEvent {
  /** Session the event belongs to. */
  session: string;
  /** Position in that session's recorded history, when it has one. */
  seq: number | null;
  /** The event. */
  event: ClientEvent;
}

/**
 * Parses one `data:` payload off the user stream into a {@link SessionEvent}.
 *
 * Fails fast like {@link parseClientEvent}: an envelope that is not an
 * object with a string `session` is a protocol violation, and the nested
 * `event` gets the same check.
 */
export function parseSessionEvent(value: unknown): SessionEvent {
  if (typeof value !== "object" || value === null || !("session" in value)) {
    throw new Error(`not a SessionEvent: ${JSON.stringify(value)}`);
  }
  const envelope = value as { session: unknown; seq: unknown; event: unknown };
  if (typeof envelope.session !== "string") {
    throw new Error(`not a SessionEvent: ${JSON.stringify(value)}`);
  }
  return {
    session: envelope.session,
    seq: typeof envelope.seq === "number" ? envelope.seq : null,
    event: parseClientEvent(envelope.event),
  };
}

/**
 * Parses a raw event — from a live `SessionEvent.event` or from a
 * catch-up `StoredEvent.event` (typed `unknown` in the OpenAPI schema,
 * because `ClientEvent` has no schema of its own).
 *
 * Fails fast: a value that is not an object with a recognized string `type`
 * tag is a protocol violation, not something to shrug off or coerce.
 */
export function parseClientEvent(value: unknown): ClientEvent {
  if (typeof value !== "object" || value === null || !("type" in value)) {
    throw new Error(`not a ClientEvent: ${JSON.stringify(value)}`);
  }
  const tag = (value as { type: unknown }).type;
  if (typeof tag !== "string" || !CLIENT_EVENT_TYPES.has(tag)) {
    throw new Error(`not a ClientEvent: unrecognized type ${JSON.stringify(tag)}`);
  }
  return value as ClientEvent;
}

const CLIENT_EVENT_TYPES: ReadonlySet<string> = new Set([
  "harness",
  "user_message",
  "shell_command",
  "shell_output",
  "shell_exited",
  "started",
  "capabilities",
  "approval_pending",
  "approval_decided",
  "session_state_changed",
  "machine_connection",
  "usage",
  "terminal_output",
  "repo_dirty",
  "spot_notice",
  "provisioning_stage",
  "machine_changed",
  "model_changed",
  "permission_mode_changed",
  "models",
  "plan_usage",
  "commands",
]);
