//! The daemon⇄control-plane wire protocol, and what browsers see of it.
//!
//! One outbound WebSocket per daemon, relayed through the session's
//! Durable Object. Every frame is one JSON-encoded message from this
//! module. [`crate::WIRE_PROTOCOL_VERSION`] guards compatibility: a
//! mismatch closes the connection immediately.
//!
//! Three enums, one for each direction of the relay:
//!
//! | Enum | From | To |
//! |---|---|---|
//! | [`DaemonToControl`] | `flycod` | the session room |
//! | [`ControlToDaemon`] | the session room | `flycod` |
//! | [`ClientEvent`] | the session room | browsers |
//!
//! All three are internally tagged on `type`. **Every variant is a struct
//! variant, including the ones that carry a single value.** A newtype
//! variant holding another internally-tagged enum emits its tag twice —
//! `{"type":"harness","type":"turn_started",…}` — which `serde_json`
//! serializes happily and then refuses to read back. Naming the payload
//! field nests it instead, so the two tags never collide.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetSignal;
use crate::harness::{HarnessEvent, UsageReport};
use crate::id::{ApprovalId, SessionId};
use crate::session::SessionState;

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

/// How far a session's machine has got towards running an agent.
///
/// Provisioning takes minutes, and a spinner for those minutes tells the
/// user nothing about whether anything is wrong. The stages are the five
/// milestones flyco can actually observe, in the order they happen, and the
/// session page renders them as a timeline inside the transcript
/// (docs/ux.md §9.2).
///
/// Who announces which is decided by who can see it: the control plane's
/// provisioning queue owns everything up to the machine existing, and the
/// daemon on that machine owns everything after it boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProvisioningStage {
    /// The provider is being asked for capacity.
    Reserving,
    /// The provider handed back a machine and it is powering on.
    Booting,
    /// The machine's bootstrap is fetching and installing `flycod`.
    Installing,
    /// The session's repository is being checked out.
    ///
    /// Nothing emits this yet: flyco does not put a checkout on a machine —
    /// the bootstrap installs `flycod` and nothing clones a repository into
    /// [`WORKDIR`]. The stage is defined here because the timeline is one
    /// ordered protocol rather than five independent ones, and the daemon
    /// will announce it from the same place it announces
    /// [`Ready`](Self::Ready) once the checkout lands.
    ///
    /// [`WORKDIR`]: https://github.com/lexoliu/flyco/blob/main/crates/provider/src/flycod.rs
    Cloning,
    /// The daemon is connected and the harness is accepting work.
    Ready,
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
    /// The harness is identified and warming.
    ///
    /// Arrives before any user message, so the room can record the
    /// harness-native session id that a later resume needs.
    Started {
        /// Harness-native session id; resume uses this.
        harness_session_id: String,
    },
    /// The capability tokens this harness build advertises.
    ///
    /// Turn-derived and therefore late: Claude Code names them only on a
    /// turn's `system/init` frame. Newest set wins, and "unknown yet" is a
    /// state every consumer must tolerate.
    Capabilities {
        /// The capability tokens, as the harness names them.
        capabilities: Vec<String>,
    },
    /// A normalized harness event.
    Harness {
        /// The event.
        event: HarnessEvent,
    },
    /// Periodic usage snapshot for the UI meters.
    Usage {
        /// The snapshot.
        usage: UsageReport,
    },
    /// The daemon needs a user decision before the harness can proceed.
    ///
    /// The durable approval row is created over REST *first*; the `id` here
    /// is the one the control plane assigned, so a decision routed back
    /// through [`ControlToDaemon::ApprovalDecision`] names the same
    /// approval the user saw.
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
    /// The machine reached a provisioning milestone the daemon can see.
    ///
    /// The control plane cannot observe anything past the provider's
    /// answer — it has no way onto the machine — so the last stages are
    /// reported from the machine itself.
    ProvisioningStage {
        /// The milestone reached.
        stage: ProvisioningStage,
        /// When it was reached, seconds since the Unix epoch.
        at_unix: u64,
    },
}

/// Messages from the control plane to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
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
    /// Compact the session's context through the harness's native command.
    Compact,
    /// The user decided a pending approval.
    ApprovalDecision {
        /// The approval being decided.
        id: ApprovalId,
        /// The decision.
        decision: ApprovalDecision,
    },
    /// A budget threshold was crossed; [`BudgetSignal::Pause`] requires the
    /// daemon to interrupt and stop the harness immediately.
    Budget {
        /// The threshold that was crossed.
        signal: BudgetSignal,
    },
    /// Raw input for the web terminal.
    TerminalInput {
        /// Bytes to write to the terminal, UTF-8.
        data: String,
    },
    /// Archive the session: flush state, optionally snapshot the repo, shut
    /// down.
    Archive {
        /// Snapshot uncommitted work into object storage before the disk is
        /// released. Automatic archives set this; a confirmed manual archive
        /// of a dirty tree does not — the user chose to discard.
        #[serde(default, skip_serializing_if = "crate::wire::is_false")]
        preserve_workdir: bool,
    },
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires fn(&T) -> bool"
)]
const fn is_false(value: &bool) -> bool {
    !*value
}

impl ControlToDaemon {
    /// Whether a browser may send this command.
    ///
    /// A session room accepts exactly four commands from a client socket;
    /// everything else is control-plane authority (budget signals, approval
    /// decisions, archival) and reaches the daemon only through an
    /// authenticated REST handler. A client that sends anything else is
    /// closed rather than ignored.
    #[must_use]
    pub const fn is_client_command(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. } | Self::Interrupt | Self::Compact | Self::TerminalInput { .. }
        )
    }
}

/// What a browser attached to a session room receives.
///
/// A superset of the harness stream: everything a
/// [`DaemonToControl`] frame carries that a user may see, plus the
/// control-plane facts the daemon never knows about (an approval's
/// decision, a lifecycle change).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    /// A normalized harness event.
    Harness {
        /// The event.
        event: HarnessEvent,
    },
    /// A message the user sent to the agent.
    ///
    /// The user's own half of the conversation, which the daemon never
    /// reports back: it arrives at the room from a browser socket or from
    /// `POST /v1/sessions/{id}/messages`, and the room records and echoes it
    /// there. Without it a second browser would watch the agent answer
    /// questions it could not see, and a replayed session would be one side
    /// of a conversation — including the prompt every turn in the history
    /// list is named by.
    UserMessage {
        /// What was said to the agent, verbatim.
        text: String,
    },
    /// The harness announced its native session id.
    Started {
        /// Harness-native session id.
        harness_session_id: String,
    },
    /// The capability tokens the harness advertises. Newest set wins.
    Capabilities {
        /// The capability tokens, as the harness names them.
        capabilities: Vec<String>,
    },
    /// An approval is waiting for the user.
    ApprovalPending {
        /// The approval.
        id: ApprovalId,
        /// What is being approved.
        payload: ApprovalPayload,
    },
    /// An approval was decided — by this browser, another one, or the API.
    ApprovalDecided {
        /// The approval.
        id: ApprovalId,
        /// What was decided.
        decision: ApprovalDecision,
    },
    /// The session moved through its lifecycle.
    SessionStateChanged {
        /// The state it moved to.
        state: SessionState,
    },
    /// A usage snapshot for the UI meters.
    Usage {
        /// The snapshot.
        usage: UsageReport,
    },
    /// Raw terminal output for the web terminal.
    TerminalOutput {
        /// UTF-8 lossy terminal bytes.
        data: String,
    },
    /// The repo has uncommitted changes and the agent is kept awake.
    RepoDirty {
        /// `git status --porcelain` summary shown to the user.
        summary: String,
    },
    /// The provider announced imminent spot reclamation.
    SpotNotice {
        /// Seconds until reclamation, as announced.
        seconds_remaining: u32,
    },
    /// The machine reached a provisioning milestone.
    ///
    /// Announced by the provisioning queue up to the machine existing and
    /// by the daemon after it boots, and rendered as one timeline inside
    /// the transcript.
    ProvisioningStage {
        /// The milestone reached.
        stage: ProvisioningStage,
        /// When it was reached, seconds since the Unix epoch.
        at_unix: u64,
    },
}

impl ClientEvent {
    /// The client-facing form of a daemon frame, if browsers see it at all.
    ///
    /// [`DaemonToControl::Hello`] is handshake traffic and never reaches a
    /// browser; an [`DaemonToControl::ApprovalRequest`] becomes
    /// [`Self::ApprovalPending`], because "pending" is the state the UI
    /// renders rather than the act of asking.
    #[must_use]
    pub fn from_daemon(frame: DaemonToControl) -> Option<Self> {
        match frame {
            DaemonToControl::Hello { .. } => None,
            DaemonToControl::Started { harness_session_id } => {
                Some(Self::Started { harness_session_id })
            }
            DaemonToControl::Capabilities { capabilities } => {
                Some(Self::Capabilities { capabilities })
            }
            DaemonToControl::Harness { event } => Some(Self::Harness { event }),
            DaemonToControl::Usage { usage } => Some(Self::Usage { usage }),
            DaemonToControl::ApprovalRequest { id, payload } => {
                Some(Self::ApprovalPending { id, payload })
            }
            DaemonToControl::TerminalOutput { data } => Some(Self::TerminalOutput { data }),
            DaemonToControl::RepoDirty { summary } => Some(Self::RepoDirty { summary }),
            DaemonToControl::SpotNotice { seconds_remaining } => {
                Some(Self::SpotNotice { seconds_remaining })
            }
            DaemonToControl::ProvisioningStage { stage, at_unix } => {
                Some(Self::ProvisioningStage { stage, at_unix })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApprovalDecision, ApprovalPayload, ClientEvent, ControlToDaemon, DaemonToControl,
        ProvisioningStage,
    };
    use crate::budget::BudgetSignal;
    use crate::harness::{ContextWindow, HarnessEvent, UsageReport};
    use crate::id::{ApprovalId, SessionId};
    use crate::money::Usd;
    use crate::session::SessionState;

    /// Round-trips through the JSON *text*, not through `serde_json::Value`.
    ///
    /// A `Value` is a map, so it silently keeps the last of two identical
    /// keys — exactly the duplicate-`type` bug this protocol is shaped to
    /// avoid. Only the text form proves a frame survives the wire.
    fn round_trip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + core::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialize");
        let back: T = serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("{json} did not deserialize: {error}"));
        assert_eq!(&back, value);
    }

    fn usage() -> UsageReport {
        UsageReport {
            input_tokens: 12,
            output_tokens: 34,
            context: Some(ContextWindow {
                used_tokens: 1_000,
                size_tokens: 200_000,
            }),
            estimated_cost: Some(Usd::from_cents(7)),
        }
    }

    fn harness_event() -> HarnessEvent {
        HarnessEvent::AssistantDelta {
            turn_id: "turn-1".to_owned(),
            text: "hello".to_owned(),
        }
    }

    fn payload() -> ApprovalPayload {
        ApprovalPayload::ToolUse {
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "ls" }),
        }
    }

    fn every_daemon_frame() -> Vec<DaemonToControl> {
        vec![
            DaemonToControl::Hello {
                protocol_version: crate::WIRE_PROTOCOL_VERSION,
                session: SessionId::generate(),
            },
            DaemonToControl::Started {
                harness_session_id: "9d0f4b1a".to_owned(),
            },
            DaemonToControl::Capabilities {
                capabilities: vec!["can_use_tool".to_owned()],
            },
            DaemonToControl::Harness {
                event: harness_event(),
            },
            DaemonToControl::Usage { usage: usage() },
            DaemonToControl::ApprovalRequest {
                id: ApprovalId::generate(),
                payload: payload(),
            },
            DaemonToControl::TerminalOutput {
                data: "$ ls\n".to_owned(),
            },
            DaemonToControl::RepoDirty {
                summary: " M src/lib.rs".to_owned(),
            },
            DaemonToControl::SpotNotice {
                seconds_remaining: 30,
            },
            DaemonToControl::ProvisioningStage {
                stage: ProvisioningStage::Ready,
                at_unix: 1_800_000_000,
            },
        ]
    }

    fn every_control_frame() -> Vec<ControlToDaemon> {
        vec![
            ControlToDaemon::Welcome,
            ControlToDaemon::UserMessage {
                text: "what does this crate do?".to_owned(),
            },
            ControlToDaemon::Interrupt,
            ControlToDaemon::Compact,
            ControlToDaemon::ApprovalDecision {
                id: ApprovalId::generate(),
                decision: ApprovalDecision::Approved,
            },
            ControlToDaemon::Budget {
                signal: BudgetSignal::Pause,
            },
            ControlToDaemon::TerminalInput {
                data: "ls\n".to_owned(),
            },
            ControlToDaemon::Archive {
                preserve_workdir: false,
            },
            ControlToDaemon::Archive {
                preserve_workdir: true,
            },
        ]
    }

    #[test]
    fn every_daemon_frame_survives_the_wire() {
        for frame in every_daemon_frame() {
            round_trip(&frame);
        }
    }

    #[test]
    fn every_control_frame_survives_the_wire() {
        for frame in every_control_frame() {
            round_trip(&frame);
        }
    }

    #[test]
    fn every_client_event_survives_the_wire() {
        let events = [
            ClientEvent::Harness {
                event: harness_event(),
            },
            ClientEvent::UserMessage {
                text: "what does this crate do?".to_owned(),
            },
            ClientEvent::Started {
                harness_session_id: "9d0f4b1a".to_owned(),
            },
            ClientEvent::Capabilities {
                capabilities: vec!["can_use_tool".to_owned()],
            },
            ClientEvent::ApprovalPending {
                id: ApprovalId::generate(),
                payload: payload(),
            },
            ClientEvent::ApprovalDecided {
                id: ApprovalId::generate(),
                decision: ApprovalDecision::Denied,
            },
            ClientEvent::SessionStateChanged {
                state: SessionState::Archived,
            },
            ClientEvent::Usage { usage: usage() },
            ClientEvent::TerminalOutput {
                data: "$ ls\n".to_owned(),
            },
            ClientEvent::RepoDirty {
                summary: " M src/lib.rs".to_owned(),
            },
            ClientEvent::SpotNotice {
                seconds_remaining: 30,
            },
            ClientEvent::ProvisioningStage {
                stage: ProvisioningStage::Booting,
                at_unix: 1_800_000_000,
            },
        ];
        for event in events {
            round_trip(&event);
        }
    }

    #[test]
    fn a_nested_enum_keeps_both_tags_apart() {
        // The regression this protocol's shape exists to prevent: an
        // internally-tagged newtype variant wrapping another
        // internally-tagged enum writes `type` twice.
        let json = serde_json::to_string(&DaemonToControl::Harness {
            event: harness_event(),
        })
        .expect("serialize");
        assert_eq!(json.matches("\"type\"").count(), 2);
        assert!(json.contains(r#""type":"harness""#));
        assert!(json.contains(r#""event":{"type":"assistant_delta""#));
    }

    #[test]
    fn tagged_encoding_is_stable() {
        let json = serde_json::to_string(&ControlToDaemon::Budget {
            signal: BudgetSignal::Pause,
        })
        .expect("serialize");
        assert_eq!(json, r#"{"type":"budget","signal":"pause"}"#);
    }

    #[test]
    fn a_client_may_only_drive_the_turn() {
        for frame in every_control_frame() {
            let allowed = matches!(
                frame,
                ControlToDaemon::UserMessage { .. }
                    | ControlToDaemon::Interrupt
                    | ControlToDaemon::Compact
                    | ControlToDaemon::TerminalInput { .. }
            );
            assert_eq!(frame.is_client_command(), allowed, "{frame:?}");
        }
    }

    #[test]
    fn only_the_handshake_is_hidden_from_browsers() {
        for frame in every_daemon_frame() {
            let hidden = matches!(frame, DaemonToControl::Hello { .. });
            assert_eq!(ClientEvent::from_daemon(frame.clone()).is_none(), hidden);
        }
    }

    #[test]
    fn an_approval_request_reaches_browsers_as_a_pending_approval() {
        let id = ApprovalId::generate();
        assert_eq!(
            ClientEvent::from_daemon(DaemonToControl::ApprovalRequest {
                id,
                payload: payload(),
            }),
            Some(ClientEvent::ApprovalPending {
                id,
                payload: payload(),
            })
        );
    }
}
