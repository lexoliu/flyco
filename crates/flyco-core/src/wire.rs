//! The daemon⇄control-plane wire protocol.
//!
//! One outbound WebSocket per daemon, relayed through the session's
//! Durable Object. Every frame is one JSON-encoded message from this
//! module. [`crate::WIRE_PROTOCOL_VERSION`] guards compatibility: a
//! mismatch closes the connection immediately.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetSignal;
use crate::harness::{HarnessEvent, UsageReport};
use crate::id::{ApprovalId, SessionId};

/// What the daemon asks the user to approve, mirrored in the approval UI.
///
/// Approvals are enforced by flyco's own UI and API — never by prompt
/// engineering inside the harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalPayload {
    /// Merge the agent's branch into a target branch.
    Merge {
        /// Repository in `owner/name` form.
        repo: String,
        /// Branch to merge from.
        from_branch: String,
        /// Branch to merge into.
        into_branch: String,
    },
    /// Rewrite history on a branch.
    HistoryRewrite {
        /// Repository in `owner/name` form.
        repo: String,
        /// Branch whose history changes.
        branch: String,
        /// What the rewrite does, as shown to the user.
        description: String,
    },
    /// Replace text in the shared global AGENTS.md / CLAUDE.md.
    AgentsMdChange {
        /// Text to replace.
        find: String,
        /// Replacement text.
        replace: String,
    },
    /// A harness tool call routed to the user for permission.
    ToolUse {
        /// Tool name as the harness reports it.
        tool: String,
        /// Tool input as the harness reports it.
        input: serde_json::Value,
    },
}

/// The user's decision on an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow the action.
    Approved,
    /// Refuse the action.
    Denied,
}

/// Messages from the daemon to the control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonToControl {
    /// First frame after connecting: identifies the daemon and its
    /// protocol version. The control plane closes on any mismatch.
    Hello {
        /// Wire protocol version the daemon speaks.
        protocol_version: u32,
        /// Session this daemon serves.
        session: SessionId,
    },
    /// A normalized harness event.
    Harness(HarnessEvent),
    /// Periodic usage snapshot for the UI meters.
    Usage(UsageReport),
    /// The daemon needs a user decision before the harness can proceed.
    ApprovalRequest {
        /// Identifier the decision must echo.
        id: ApprovalId,
        /// What is being approved.
        payload: ApprovalPayload,
    },
    /// Raw terminal output for the web terminal.
    TerminalOutput {
        /// UTF-8 lossy terminal bytes.
        data: String,
    },
    /// The repo has uncommitted changes; the agent is kept awake rather
    /// than allowed to complete.
    RepoDirty {
        /// `git status --porcelain` summary shown to the user.
        summary: String,
    },
    /// The provider announced imminent spot reclamation.
    SpotNotice {
        /// Seconds until reclamation, as announced.
        seconds_remaining: u32,
    },
}

/// Messages from the control plane to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlToDaemon {
    /// Acknowledges [`DaemonToControl::Hello`]; the session is live.
    Welcome,
    /// A user message to feed the harness.
    UserMessage {
        /// Message text.
        text: String,
    },
    /// Interrupt the current turn.
    Interrupt,
    /// The user decided a pending approval.
    ApprovalDecision {
        /// The approval being decided.
        id: ApprovalId,
        /// The decision.
        decision: ApprovalDecision,
    },
    /// A budget threshold was crossed; `Pause` requires the daemon to
    /// interrupt and stop the harness immediately.
    Budget(BudgetSignal),
    /// Raw input for the web terminal.
    TerminalInput {
        /// Bytes to write to the terminal, UTF-8.
        data: String,
    },
    /// Archive the session: flush state, snapshot the repo, shut down.
    Archive,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_messages_round_trip() {
        let msg = DaemonToControl::Hello {
            protocol_version: crate::WIRE_PROTOCOL_VERSION,
            session: SessionId::generate(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: DaemonToControl = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn tagged_encoding_is_stable() {
        let msg = ControlToDaemon::Budget(BudgetSignal::Pause);
        let json = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(json["type"], "budget");
    }
}
