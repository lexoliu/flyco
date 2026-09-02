/**
 * Reading an approval out loud.
 *
 * An approval card asks the user to allow one specific operation, so the
 * card's whole job is to say *which* — and that is a different sentence for
 * every shape of `ApprovalPayload`. Turning the payload into those two
 * strings is pure, so it lives here rather than inside the component: it is
 * the part with rules worth testing, and the card is layout.
 *
 * The rules that are not obvious:
 *
 * - A machine resize onto a license-bound type quotes the **charge in
 *   dollars**, not the hours. `24 hours` is a fact about an Apple licence;
 *   `$15.60` is what pressing Approve costs, and it is the number the user
 *   is actually deciding about (docs/ux.md §7.7, §9.5).
 * - The agent's own reason is shown verbatim. The user is being asked to
 *   spend money on the agent's say-so, and "because it said so" is not a
 *   basis for a decision.
 */
import type { ApprovalPayload } from "../api/wire";
import { formatUsd } from "./money";

/** What one approval card says. */
export interface AskedOperation {
  /** The operation, named in a few words. */
  title: string;
  /** Exactly what would happen, in full. */
  detail: string;
}

/** Turns one approval payload into the exact operation being asked for. */
export function operation(payload: ApprovalPayload): AskedOperation {
  switch (payload.kind) {
    case "merge":
      return {
        title: "Merge a branch",
        detail: `${payload.repo}: ${payload.from_branch} → ${payload.into_branch}`,
      };
    case "history_rewrite":
      return {
        title: "Rewrite history",
        detail: `${payload.repo} on ${payload.branch}: ${payload.description}`,
      };
    case "agents_md_change":
      return {
        title: "Change AGENTS.md",
        detail: `Replace "${payload.find}" with "${payload.replace}"`,
      };
    case "machine_resize_license_bound":
      return {
        title: `Switch to ${payload.machine_type}`,
        detail: [
          `Starts a ${payload.minimum.hours}-hour minimum charge of ${formatUsd(
            payload.minimum.charge,
          )} the moment it boots.`,
          `The agent says: ${payload.reason}`,
          "Resizing restarts the machine; the disk is kept.",
        ].join("\n"),
      };
    case "tool_use":
      return { title: `Run ${payload.tool}`, detail: JSON.stringify(payload.input, null, 2) };
  }
}
