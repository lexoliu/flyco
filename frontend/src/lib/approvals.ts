/**
 * Folds a session's [`ClientEvent`] stream into the current state of its
 * approvals: raised by `approval_pending`, resolved by `approval_decided`.
 * Pure, mirroring src/lib/transcript.ts.
 */
import type { ClientEvent } from "../api/wire";
import type { ApprovalItem } from "../components/ApprovalsPanel";

export function foldApprovals(events: readonly ClientEvent[]): ApprovalItem[] {
  const byId = new Map<string, ApprovalItem>();
  for (const event of events) {
    if (event.type === "approval_pending") {
      byId.set(event.id, { id: event.id, payload: event.payload, state: "pending" });
    } else if (event.type === "approval_decided") {
      const existing = byId.get(event.id);
      if (existing !== undefined) {
        byId.set(event.id, { ...existing, state: event.decision });
      }
    }
  }
  return [...byId.values()];
}
