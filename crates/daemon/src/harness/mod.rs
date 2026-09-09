//! The harness abstraction flycod drives sessions through.
//!
//! Flyco supports exactly two harnesses and never builds its own, so this
//! is a two-implementation trait rather than a plugin surface. It is stated
//! as a trait anyway because the daemon's session loop, approvals routing,
//! and relay must be written once against a single shape — the Codex driver
//! ([`flyco_core::HarnessKind::Codex`], a JSON-RPC client to
//! `codex app-server`) slots in beside [`claude`] without touching them.
//!
//! Everything a harness produces is normalized to
//! [`flyco_core::HarnessEvent`], except the two things that are not events:
//! the harness-native session id (needed to resume) and approval requests
//! (which flyco routes to its own UI, never to the model). Those are
//! siblings in [`SessionOutput`], mirroring
//! [`flyco_core::wire::DaemonToControl`].

pub mod claude;
pub mod codex;

use std::future::Future;
use std::path::PathBuf;

use flyco_core::{ApprovalId, HarnessEvent, ModelChoice, ModelOption};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

/// What a session must be told before it can run.
///
/// Harness-specific settings (credentials, permission mode, model) belong
/// to the harness value itself; only what changes per session is here.
#[derive(Debug, Clone)]
pub struct StartRequest {
    /// Directory the agent works in.
    pub workdir: PathBuf,
    /// A harness-native session id to resume, for cross-host History.
    pub resume_session_id: Option<String>,
}

/// The user's answer to a [`SessionOutput::ApprovalRequest`].
///
/// Allow and deny carry different payloads, so they are different values:
/// a denial always has a reason for the model, and only an approval can
/// rewrite the tool input.
#[derive(Debug, Clone)]
pub enum ToolApproval {
    /// Let the call proceed, optionally with rewritten input.
    Allow {
        /// The approval being answered.
        id: ApprovalId,
        /// Replacement input, or `None` to run what the model proposed.
        updated_input: Option<Value>,
    },
    /// Refuse the call and tell the model why.
    Deny {
        /// The approval being answered.
        id: ApprovalId,
        /// Reason shown to the model.
        message: String,
    },
}

impl ToolApproval {
    /// The approval this decision answers.
    #[must_use]
    pub const fn id(&self) -> ApprovalId {
        match self {
            Self::Allow { id, .. } | Self::Deny { id, .. } => *id,
        }
    }
}

/// Everything a running session emits upward.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "output", rename_all = "snake_case")]
pub enum SessionOutput {
    /// The session is identified and its harness is warming.
    ///
    /// Arrives before any user message, so the control plane can record the
    /// session and the UI can go live without waiting for a turn.
    Started {
        /// Harness-native session id; resume uses this.
        session_id: String,
    },
    /// The capability tokens this harness build advertises.
    ///
    /// Feature detection reads these and never a version string. Separate
    /// from [`Self::Started`] because Claude Code reports them only on a
    /// turn's `system/init` frame, so they arrive late by construction —
    /// see [`claude`] for what that means for gating. The newest set wins.
    Capabilities {
        /// The capability tokens, as the harness names them.
        capabilities: Vec<String>,
    },
    /// The models this harness offers.
    ///
    /// Reported once the harness has answered its handshake and before any
    /// turn — the earliest moment the answer exists, and early enough that
    /// the composer's picker is right for the first message. The control
    /// loop files it over REST rather than as a relay frame, because it is
    /// recorded against the *account* and a Durable Object cannot reach D1.
    Models {
        /// Every model the harness listed, in its own order.
        models: Vec<ModelOption>,
    },
    /// A normalized harness event.
    Event {
        /// The event.
        event: HarnessEvent,
    },
    /// The harness is blocked until the user decides.
    ApprovalRequest {
        /// Echo this in a [`ToolApproval`].
        id: ApprovalId,
        /// Tool the model asked to run.
        tool: String,
        /// Tool input as the model produced it.
        input: Value,
        /// The harness's own suggested permission updates, if any.
        suggestions: Option<Value>,
    },
    /// The session cannot continue. Terminal: the output stream ends after
    /// this.
    Fatal {
        /// Human-readable cause.
        error: String,
    },
}

/// A harness that can be started once.
///
/// `start` consumes the harness because starting moves its owned
/// resources — the transcript store above all — into the session's actor
/// task. One harness value drives one session, which is exactly flyco's
/// model: one session per VM.
pub trait Harness {
    /// The live session handle this harness hands back.
    type Session: HarnessSession;
    /// Why a session could not be started.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Launches the harness and returns its control handle and output
    /// stream.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the harness process cannot be prepared,
    /// launched, or handshaked.
    fn start(
        self,
        request: StartRequest,
    ) -> impl Future<Output = Result<Started<Self::Session>, Self::Error>> + Send;
}

/// A started harness: the control handle plus its single output stream.
///
/// The receiver is moved out rather than fetched from the session, so
/// "consumed exactly once" is a compile-time fact.
#[derive(Debug)]
pub struct Started<S> {
    /// Control handle.
    pub session: S,
    /// Everything the session emits, in order.
    pub outputs: mpsc::Receiver<SessionOutput>,
}

/// The control handle of a running harness session.
///
/// Every method is a message to the task that owns the harness process;
/// nothing here holds a lock, and the handle is cheap to clone-free share
/// by reference.
pub trait HarnessSession: Send + Sync {
    /// Why a command could not be delivered.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Pushes a user message into the session, opening a new turn.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the session has already stopped.
    fn send_user_message(
        &self,
        text: String,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Ends the current turn with SIGINT semantics.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the session has already stopped.
    fn interrupt(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Waits until everything the harness has handed this daemon has been
    /// written through.
    ///
    /// Exists for the thirty seconds between a spot notice and the machine
    /// disappearing (see [`crate::spot`]). A transcript batch is only
    /// resumable once the *control plane* holds it, and the driver task
    /// that performs those writes is the same task that reads the harness's
    /// output — so a command that makes the round trip through it resolves
    /// only after every batch queued ahead of it has landed. That is the
    /// whole guarantee, stated plainly: not "the harness has stopped
    /// producing", which nothing can promise, but "everything it has
    /// produced so far is somewhere the next machine can read it".
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the session has already stopped, which
    /// means nothing is left to flush.
    fn flush(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Compacts the session context through the harness's native mechanism.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the session has already stopped or the
    /// harness rejects compaction.
    fn compact(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Puts the running session on another model, at another effort.
    ///
    /// Applied to the conversation in progress rather than the next one:
    /// the Claude Agent SDK takes `setModel` and `applyFlagSettings` on a
    /// live query, and Codex's `turn/start` documents `model` and `effort`
    /// as overriding "this turn and subsequent turns". Both mean a user can
    /// change the model while watching the agent work, which is what the
    /// composer's picker offers.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the session has already stopped or the
    /// harness refuses the model.
    fn set_model(&self, model: ModelChoice)
    -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Answers a pending approval.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the session has already stopped.
    fn decide_approval(
        &self,
        approval: ToolApproval,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Stops the harness cleanly and waits for its process to exit.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the harness did not stop cleanly.
    fn shutdown(self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// The sentence a failure is reported as: the error and every cause under
/// it, outermost first.
///
/// `Display` on a `thiserror` type prints only its own line, and a fatal
/// reason recorded from that alone said `transcript store I/O failed at
/// <control plane>` about a request the control plane had refused with a
/// status and a reason. The chain is what a person diagnoses from, so it
/// is what gets recorded (issue #231).
pub(crate) fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
