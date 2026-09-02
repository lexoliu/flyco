/**
 * The daemon <-> control-plane wire protocol, as seen by a browser.
 *
 * This is hand-typed rather than generated: `flyco_core::wire` is a WebSocket
 * protocol with no OpenAPI schema (see `crates/core/src/wire.rs`). Only
 * `ClientEvent` and the three client-sendable `ControlToDaemon` variants
 * matter here — everything else in that module (`DaemonToControl`, the
 * control-plane-only `ControlToDaemon` variants) never reaches a browser.
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
  | { type: "usage_limited"; resets_at_unix: number | null }
  | { type: "context_compacted" }
  | { type: "context_compaction_failed"; error: string };

/**
 * How far a session's machine has got towards running an agent.
 *
 * Mirrors `flyco_core::wire::ProvisioningStage`: the five milestones flyco
 * can observe, in the order they happen. The session page renders them as a
 * timeline inside the transcript (docs/ux.md §9.2).
 */
export type ProvisioningStage = "reserving" | "booting" | "installing" | "cloning" | "ready";

/**
 * What a browser attached to a session room receives, mirroring
 * `flyco_core::wire::ClientEvent` exactly.
 */
export type ClientEvent =
  | { type: "harness"; event: HarnessEvent }
  | { type: "user_message"; text: string }
  | { type: "started"; harness_session_id: string }
  | { type: "capabilities"; capabilities: string[] }
  | { type: "approval_pending"; id: string; payload: ApprovalPayload }
  | { type: "approval_decided"; id: string; decision: ApprovalDecision }
  | { type: "session_state_changed"; state: SessionState }
  | { type: "usage"; usage: UsageReport }
  | { type: "terminal_output"; data: string }
  | { type: "repo_dirty"; summary: string }
  | { type: "spot_notice"; seconds_remaining: number }
  | { type: "provisioning_stage"; stage: ProvisioningStage; at_unix: number };

/**
 * The four `ControlToDaemon` variants a browser may send directly over the
 * relay socket, mirroring `ControlToDaemon::is_client_command()`. Every
 * other command (approval decisions, budget signals, archive) is
 * control-plane authority and reaches the daemon only through an
 * authenticated REST handler.
 */
export type ClientCommand =
  | { type: "user_message"; text: string }
  | { type: "interrupt" }
  | { type: "compact" }
  | { type: "terminal_input"; data: string };

/**
 * Parses a raw relay frame — from a live WebSocket message or from a
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
  "started",
  "capabilities",
  "approval_pending",
  "approval_decided",
  "session_state_changed",
  "usage",
  "terminal_output",
  "repo_dirty",
  "spot_notice",
  "provisioning_stage",
]);
