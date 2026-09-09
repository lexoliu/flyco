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
use crate::id::{ApprovalId, SessionId, ShellRunId, WorkdirRequestId};
use crate::session::SessionState;
use crate::workdir::{WorkdirReply, WorkdirRequest};

/// One rolling window of the harness plan's rate limit.
///
/// Both harnesses answer the same question in the same shape — how much of
/// a window is spent, and when the window turns over — so flyco states it
/// once and both drivers fill it in. This is a *plan* limit, not the
/// session's token usage: [`UsageReport`] is what one conversation cost,
/// this is what is left of the account it was billed to.
///
/// A window nobody has reported is absent from the list rather than present
/// at zero; there is no "unknown" reading, because a ring drawn empty is
/// indistinguishable from a plan that has not been touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UsageWindow {
    /// What the window is called in the UI, e.g. `5-hour`, `Weekly`,
    /// `Weekly (Fable)`. Derived by [`UsageWindow::new`].
    pub label: String,
    /// How much of the window is spent, 0–100.
    pub used_percent: u8,
    /// When the window turns over, seconds since the Unix epoch.
    ///
    /// `None` for a window whose harness reports a utilization but no
    /// reset — Codex's `resetsAt` and Claude's `resets_at` are both
    /// nullable, and inventing a deadline would be worse than showing none.
    pub resets_at_unix: Option<i64>,
    /// How long the window is, in minutes.
    ///
    /// Kept alongside the label because it is what the UI orders by: the
    /// label is prose and sorts alphabetically into nonsense, while the
    /// length is the thing a reader scans in order. `None` for a window
    /// whose harness names no duration.
    pub window_minutes: Option<u32>,
}

/// Minutes in a day, the unit the day-and-longer labels are built from.
const MINUTES_PER_DAY: u32 = 24 * 60;

impl UsageWindow {
    /// One window, with its label derived from its length and its scope.
    ///
    /// The label is computed here rather than by each driver so that a
    /// five-hour window is called the same thing whichever harness reported
    /// it — and so that a window length flyco has never seen still gets a
    /// name instead of a blank. `scope` is the part of the plan the window
    /// covers when it covers only part of one: Claude's per-model weekly
    /// buckets name a model, and everything else is `None`.
    ///
    /// `used_percent` is clamped to 0–100 rather than trusted: both vendors
    /// type it as an unbounded number, and a plan that is 103% spent is
    /// still a full ring.
    #[must_use]
    pub fn new(
        window_minutes: Option<u32>,
        scope: Option<&str>,
        used_percent: u8,
        resets_at_unix: Option<i64>,
    ) -> Self {
        Self {
            label: window_label(window_minutes, scope),
            used_percent: used_percent.min(100),
            resets_at_unix,
            window_minutes,
        }
    }
}

/// What a window of `minutes` covering `scope` is called.
///
/// Human words where a human word exists — a day, a week, a month are read
/// as words and not as `1440 minutes` — and a counted unit otherwise, so a
/// vendor introducing a three-hour window tomorrow gets `3-hour` rather
/// than a gap.
fn window_label(minutes: Option<u32>, scope: Option<&str>) -> String {
    let base = match minutes {
        // A window whose length the harness did not state. Its scope, when
        // it has one, is the only true thing left to call it.
        None => "Plan".to_owned(),
        Some(minutes) if minutes % MINUTES_PER_DAY == 0 => match minutes / MINUTES_PER_DAY {
            1 => "Daily".to_owned(),
            7 => "Weekly".to_owned(),
            30 => "Monthly".to_owned(),
            days => format!("{days}-day"),
        },
        Some(minutes) if minutes % 60 == 0 => format!("{}-hour", minutes / 60),
        Some(minutes) => format!("{minutes}-minute"),
    };
    match scope {
        None => base,
        Some(scope) => format!("{base} ({scope})"),
    }
}

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
    /// Move the session onto a machine type that bills a minimum the moment
    /// it boots.
    ///
    /// Raised instead of performing the resize, because a license-bound type
    /// spends the user's money before anything runs on it: an EC2 Mac bills
    /// a full day under the Apple licence whether the agent uses it for
    /// twenty hours or for one minute. The minimum travels with the request
    /// so the approval card can quote the charge the user is agreeing to,
    /// rather than recomputing it from a catalog that may have moved
    /// (docs/ux.md §7.7, §9.5).
    MachineResizeLicenseBound {
        /// Provider-native machine type the agent wants to move to.
        machine_type: String,
        /// What booting it costs before it does any work.
        minimum: crate::machine::BillingMinimum,
        /// Why the agent says the session needs this machine.
        reason: String,
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
    /// The machine is powering on and its bootstrap is installing `flycod`.
    ///
    /// One stage and not two, because nothing can see where the boot ends
    /// and the install begins: the control plane hears nothing between
    /// handing the request to the provider and the daemon calling in, and
    /// the daemon only exists once the install has finished. A separate
    /// `Installing` was announced microseconds after this one and so timed
    /// every session's boot at `0s` — a row on the timeline with no
    /// interval behind it, which teaches a reader nothing and reads as
    /// broken.
    Booting,
    /// The session's repository is being checked out.
    ///
    /// Announced by the daemon over `POST
    /// /v1/sessions/{id}/provisioning-stage` rather than over the relay,
    /// because it happens *before* the harness exists: the checkout is what
    /// the harness is started in, and the relay socket is not opened until
    /// there is a session behind it.
    Cloning,
    /// The daemon is connected and the harness is accepting work.
    Ready,
}

/// Request body of `POST /v1/sessions/{id}/provisioning-stage`.
///
/// The daemon names the milestone; the control plane times it, exactly as it
/// times the stages its own provisioning queue announces. A daemon whose
/// clock is wrong would otherwise put its line of the timeline in 1970.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReportProvisioningStage {
    /// The milestone the machine has reached.
    pub stage: ProvisioningStage,
}

/// Request body of `POST /v1/sessions/{id}/startup-failure`.
///
/// What a daemon says on its way out. `flycod` is restarted on failure, so
/// a machine whose daemon cannot start is a machine that says nothing at
/// all: the queue's job finished, the relay was never opened, and the page
/// waits on a timeline that will not advance. This is the one thing the
/// dying process can still do, and it is recorded rather than acted on —
/// the restart may yet succeed, and the sentence is what the session says
/// if it does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReportStartupFailure {
    /// Why the daemon stopped, in its own words.
    pub message: String,
}

/// Request body of `POST /v1/sessions/{id}/spot-notice`.
///
/// The relay frame beside it ([`DaemonToControl::SpotNotice`]) is what puts
/// the countdown in front of the user; this is what makes the reclamation
/// *durable*. They are two routes for one fact because a session room is a
/// Durable Object, and a Durable Object can reach neither D1 nor the
/// provisioning queue — so the half that marks the session interrupted and
/// queues its replacement has to arrive at the Worker, over HTTP, from the
/// only process that knows: the daemon on the machine being taken away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReportSpotNotice {
    /// Seconds until reclamation, as the provider announced them.
    ///
    /// What the recovery is scheduled against: the machine is still up for
    /// this long, and a replacement started before it goes would find the
    /// disk still attached to a running instance.
    pub seconds_remaining: u32,
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

/// Which of a shell command's two output streams a chunk came from.
///
/// Kept apart rather than interleaved into one pipe: the two are separate
/// file descriptors and nothing orders them against each other, so merging
/// them would invent an order the machine never had. The transcript renders
/// both in one block and marks which is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShellStream {
    /// The command's standard output.
    Stdout,
    /// The command's standard error.
    Stderr,
}

/// How a `!` shell command ended.
///
/// Every way a run can finish, including the three where it never started:
/// a composer that swallowed a command because no machine was listening
/// would leave the user waiting for output that is never coming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShellOutcome {
    /// `bash` exited with this status code.
    Exited {
        /// The status code, `0` for success.
        code: i32,
    },
    /// It ended on a signal rather than with a status code.
    Signalled,
    /// The bounded timeout elapsed and the daemon killed it.
    TimedOut {
        /// The timeout that elapsed, in seconds.
        after_seconds: u64,
    },
    /// The user pressed Stop while it was running.
    Cancelled,
    /// No daemon was connected, so nothing ran it.
    Offline,
    /// Another shell command was still running: the machine runs one at a
    /// time, so the second is refused rather than queued behind a command
    /// that may never end.
    Busy,
    /// The session is paused on an exhausted budget, or its machine is
    /// being reclaimed.
    Refused,
    /// `bash` could not be started, or the daemon lost track of the child
    /// it started.
    Failed {
        /// What the operating system said.
        error: String,
    },
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
    /// Proof that this daemon and its socket are both still alive.
    ///
    /// A session relay is quiet for as long as the agent is thinking, and a
    /// quiet TCP flow is exactly what a cloud NAT reclaims — Azure's
    /// outbound idle timeout is four minutes by default. The frame is
    /// therefore sent on a timer rather than when there is news, and the
    /// room answers every one with [`ControlToDaemon::Heartbeat`]: an
    /// answer is what makes inbound silence mean something, so a
    /// half-open socket is abandoned and reconnected instead of read
    /// forever.
    Heartbeat,
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
    /// One chunk of a running `!` command's output.
    ///
    /// Streamed rather than held until the command ends, so a slow build
    /// shows what it is doing while it does it.
    ShellOutput {
        /// The run this belongs to, as [`ControlToDaemon::RunShell`] named
        /// it.
        run: ShellRunId,
        /// Which stream the chunk came from.
        stream: ShellStream,
        /// The bytes, decoded UTF-8 lossy.
        data: String,
    },
    /// A `!` command finished. Exactly one per run.
    ShellExited {
        /// The run that finished.
        run: ShellRunId,
        /// How it ended.
        outcome: ShellOutcome,
        /// Whether output was dropped after the run's byte cap.
        ///
        /// Said rather than silently elided: a transcript that showed the
        /// first megabyte of a command and no sign that there was more
        /// would be a lie about what the machine printed.
        truncated: bool,
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
    /// The answer to one
    /// [`InspectWorkdir`](ControlToDaemon::InspectWorkdir).
    ///
    /// The one frame in this direction that is addressed rather than
    /// broadcast: no browser sees it, because the browser that asked is
    /// waiting on the HTTP request the control plane is holding open for
    /// it. The room stores it under `id` and the Worker collects it there.
    WorkdirReply {
        /// The request this answers.
        id: WorkdirRequestId,
        /// The listing, the file, the diff, or the refusal.
        reply: WorkdirReply,
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
    /// Answers [`DaemonToControl::Heartbeat`].
    ///
    /// The room has nothing to say on its own schedule, so this is the only
    /// frame a daemon can count on receiving while a turn runs. That is the
    /// point: it is what lets the daemon tell a live socket from one a NAT
    /// dropped without a FIN.
    Heartbeat,
    /// A user message to feed the harness.
    UserMessage {
        /// Message text.
        text: String,
    },
    /// Run a shell command on the machine, as the composer's `!` prefix
    /// asks for (docs/ux.md §9.3).
    ///
    /// A *request*: it carries no identity, because the browser that typed
    /// it has no authority to name a run. The room assigns one and reissues
    /// it as [`Self::RunShell`], which is what the daemon acts on and what
    /// every frame about the run is keyed by.
    ///
    /// Never reaches the harness. A `!` command is the user talking to the
    /// machine, not to the agent, and putting it in the conversation would
    /// make the model think it had been asked to do something.
    ShellCommand {
        /// The command, without the `!`, to be run through `bash -c`.
        command: String,
    },
    /// Run this shell command, under the identity the room assigned it.
    ///
    /// Control-plane authority rather than a client command: the run id is
    /// what the recorded transcript row, every output chunk and the exit
    /// status are correlated by, so it is minted by the room — the single
    /// writer — rather than by whichever browser happened to send the
    /// request.
    RunShell {
        /// The run every frame about this command carries.
        run: ShellRunId,
        /// The command, without the `!`, to be run through `bash -c`.
        command: String,
    },
    /// Interrupt the current turn, and cancel a `!` command if one is
    /// running.
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
    /// The user raised the budget past what the session has spent, and the
    /// pause [`BudgetSignal::Pause`] imposed is lifted.
    ///
    /// The symmetric half of that signal, and the only thing that undoes
    /// it: the daemon accepts work again and the agent is told, in the
    /// conversation, that it may carry on from where it was interrupted.
    /// Not a [`BudgetSignal`], because a signal is a threshold the ledger
    /// crossed and this is a decision the user made.
    BudgetRaised {
        /// What the session may spend now, in total.
        limit: crate::money::Usd,
    },
    /// Raw input for the web terminal.
    TerminalInput {
        /// Bytes to write to the terminal, UTF-8.
        data: String,
    },
    /// The session's machine was replaced, and this is what it is now.
    ///
    /// Sent by the control plane rather than discovered by the daemon,
    /// because the daemon cannot see it happen: resizing restarts the
    /// machine, which kills `flycod` along with everything else the agent
    /// was running. The daemon that reads this is a *new* process whose
    /// configuration still describes the machine the session booted on, so
    /// this is both how it learns the current machine and how the agent is
    /// told, in the conversation, that its processes are gone and its disk
    /// is not.
    ///
    /// Held for a daemon that is not connected and delivered on its next
    /// `Hello` — the whole restart is a window with no daemon in it, so a
    /// command dropped for want of a listener would be the only case that
    /// ever mattered.
    MachineChanged {
        /// Provider-native type the session is on now.
        machine_type: String,
        /// What an hour of it costs, when flyco meters it at all.
        hourly: Option<crate::money::Usd>,
        /// Whether it holds interruptible capacity.
        spot: bool,
        /// Whether the change restarted the machine.
        ///
        /// A resize always does; the field exists because the agent's next
        /// move depends on it — a restart means every process it started is
        /// gone and the disk is exactly as it left it.
        restarted: bool,
    },
    /// Read something out of the session's checkout for the user.
    ///
    /// The `Files` and `Diff` tabs of docs/ux.md §9.4: the daemon is the
    /// only process that can see the disk, so a browser's question about it
    /// is relayed here and answered with a
    /// [`WorkdirReply`](DaemonToControl::WorkdirReply) carrying the same
    /// `id`. Read-only in both directions — nothing in this protocol writes
    /// to a session's working tree, which is the agent's alone.
    ///
    /// Dropped rather than held when no daemon is connected: a browser is
    /// waiting on the answer, and one delivered after the machine came back
    /// would arrive at a request that timed out long ago.
    InspectWorkdir {
        /// Identifier the reply must echo.
        id: WorkdirRequestId,
        /// What is being asked.
        request: WorkdirRequest,
    },
    /// Run the session on this model, and at this effort, from here on.
    ///
    /// Applied to the conversation already in progress rather than to the
    /// next one: Claude Code takes `setModel` and `applyFlagSettings` on a
    /// live query, and Codex's `turn/start` documents `model` and `effort`
    /// as overriding "this turn and subsequent turns". A session's model is
    /// therefore something the user changes while watching it work, which
    /// is the whole point of putting the picker in the composer.
    ///
    /// Held for a daemon that is not connected, for the same reason
    /// [`Self::MachineChanged`] is: it describes a state rather than an
    /// instant. The control plane has already recorded the model, so a
    /// daemon that comes back an hour later is still owed the change — and
    /// the alternative is a session whose row and whose harness disagree
    /// about what it is running.
    SetModel {
        /// What the session runs on now.
        model: crate::harness::ModelChoice,
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
    /// The wire tag this command is sent under.
    ///
    /// For diagnostics that have to name a command without quoting one: a
    /// `user_message` carries the user's own words, and a daemon reporting
    /// that it could not deliver one should not repeat them into a log.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Welcome => "welcome",
            Self::Heartbeat => "heartbeat",
            Self::UserMessage { .. } => "user_message",
            Self::ShellCommand { .. } => "shell_command",
            Self::RunShell { .. } => "run_shell",
            Self::TerminalInput { .. } => "terminal_input",
            Self::Interrupt => "interrupt",
            Self::Compact => "compact",
            Self::ApprovalDecision { .. } => "approval_decision",
            Self::Budget { .. } => "budget",
            Self::BudgetRaised { .. } => "budget_raised",
            Self::MachineChanged { .. } => "machine_changed",
            Self::SetModel { .. } => "set_model",
            Self::InspectWorkdir { .. } => "inspect_workdir",
            Self::Archive { .. } => "archive",
        }
    }

    /// Whether a browser may send this command.
    ///
    /// A session room accepts exactly five commands from a client socket;
    /// everything else is control-plane authority (budget signals, approval
    /// decisions, archival, and the identified [`Self::RunShell`] the room
    /// reissues a [`Self::ShellCommand`] as) and reaches the daemon only
    /// through the room itself or an authenticated REST handler. A client
    /// that sends anything else is closed rather than ignored.
    #[must_use]
    pub const fn is_client_command(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. }
                | Self::ShellCommand { .. }
                | Self::Interrupt
                | Self::Compact
                | Self::TerminalInput { .. }
        )
    }

    /// Whether the room must keep this command for a daemon that is away.
    ///
    /// Almost nothing survives a disconnect, and that is deliberate: an
    /// interrupt, a compaction or a keystroke held for a daemon that
    /// reconnects an hour later would arrive as an instruction about a turn
    /// that no longer exists.
    ///
    /// The two exceptions are the two commands that describe a *state*
    /// rather than an instant, so redelivering one late still says
    /// something true. [`MachineChanged`](Self::MachineChanged) is the
    /// exception by construction: the change it reports *is* a restart, so
    /// the daemon is guaranteed to be gone at the moment it is sent, and
    /// the machine is still the new one whenever it comes back.
    /// [`SetModel`](Self::SetModel) is the exception by consequence: the
    /// control plane has already recorded the model, and a daemon that
    /// missed the command would run the session on a model its own row
    /// disagrees with.
    ///
    /// A user message survives too, but through the room's mailbox, which
    /// is an index into the replayable stream rather than a queue, because
    /// a conversation must not be reordered.
    #[must_use]
    pub const fn survives_a_disconnect(&self) -> bool {
        matches!(self, Self::MachineChanged { .. } | Self::SetModel { .. })
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
    /// A shell command the user ran on the machine with `!`.
    ///
    /// Recorded like a user message and for the same reason — it is
    /// something a person did to this session, and a replay without it
    /// would show output nobody asked for — but it is *not* conversation:
    /// the harness never sees it, and it does not wait in the daemon's
    /// mailbox, because a command held for a machine that arrives an hour
    /// later would run against a working tree the user was not looking at.
    ShellCommand {
        /// The run this and every frame about it are keyed by.
        run: ShellRunId,
        /// The command, verbatim, without the `!`.
        command: String,
    },
    /// One chunk of a running `!` command's output.
    ShellOutput {
        /// The run it belongs to.
        run: ShellRunId,
        /// Which stream it came from.
        stream: ShellStream,
        /// The bytes, decoded UTF-8 lossy.
        data: String,
    },
    /// A `!` command finished. Exactly one per run.
    ShellExited {
        /// The run that finished.
        run: ShellRunId,
        /// How it ended.
        outcome: ShellOutcome,
        /// Whether output was dropped after the run's byte cap.
        truncated: bool,
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
    /// The session's machine attached to the room, or fell off it.
    ///
    /// The one fact a browser cannot deduce from anything else it is sent.
    /// Everything the page shows about a running turn — the spinner, the
    /// Stop button, the terminal — is only meaningful while a daemon is
    /// there to act on it, and a session whose daemon has gone otherwise
    /// looks exactly like one whose agent is thinking. Sent on the daemon's
    /// `Hello`, on its socket closing, and whenever a command finds nobody
    /// to take it (docs/ux.md §9.6).
    MachineConnection {
        /// Whether a greeted daemon holds the room right now.
        connected: bool,
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
    /// The session moved onto another machine.
    ///
    /// Rendered as one line in the transcript — `Switched to
    /// Standard_D8s_v6 · restarted the machine · disk kept` (docs/ux.md
    /// §9.5) — because a resize is not a silent operation: everything the
    /// agent had running died with the old compute, and the user is now
    /// being billed at a different rate.
    MachineChanged {
        /// Provider-native type the session is on now.
        machine_type: String,
        /// What an hour of it costs, when flyco meters it at all.
        hourly: Option<crate::money::Usd>,
        /// Whether it holds interruptible capacity.
        spot: bool,
        /// Whether the change restarted the machine. A resize always does.
        restarted: bool,
    },
    /// The session was put on another model.
    ///
    /// Rendered as one line in the transcript, like a machine change and
    /// for the same reason: what the agent answers with changes from here
    /// on, and a conversation whose second half was written by a different
    /// model with no note of where the seam is would be a transcript that
    /// misrepresents itself.
    ModelChanged {
        /// What the session runs on now.
        model: crate::harness::ModelChoice,
    },
    /// The models this session's harness offers.
    ///
    /// State rather than conversation, like
    /// [`Capabilities`](Self::Capabilities): the newest list wins, and it
    /// is what the composer's picker offers for the rest of the session.
    /// Reported by the daemon once its harness has answered — the earliest
    /// moment the answer exists — and recorded against the account, so the
    /// next session's picker opens on the list this one discovered.
    Models {
        /// Every model the harness listed, in its own order.
        models: Vec<crate::harness::ModelOption>,
    },
    /// How much of the plan behind this session's harness account is spent.
    ///
    /// State rather than conversation, like [`Models`](Self::Models): the
    /// newest snapshot wins and the composer reads the last one. Reported
    /// at session start and after every turn, because a turn is the only
    /// thing that moves the number and reading it any oftener would be
    /// polling the vendor on a timer. Named `plan_usage` and not `usage`
    /// because [`Usage`](Self::Usage) is already this session's token
    /// count — the two answer different questions and a reader has to be
    /// able to tell which one a frame is.
    PlanUsage {
        /// Every window the harness reported, in no particular order; the
        /// UI sorts them by [`UsageWindow::window_minutes`].
        windows: Vec<UsageWindow>,
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
    /// renders rather than the act of asking. A
    /// [`DaemonToControl::WorkdirReply`] is addressed to one waiting HTTP
    /// request and is collected by the Worker rather than broadcast, so it
    /// has no client form either.
    #[must_use]
    pub fn from_daemon(frame: DaemonToControl) -> Option<Self> {
        match frame {
            DaemonToControl::Hello { .. }
            | DaemonToControl::Heartbeat
            | DaemonToControl::WorkdirReply { .. } => None,
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
            DaemonToControl::ShellOutput { run, stream, data } => {
                Some(Self::ShellOutput { run, stream, data })
            }
            DaemonToControl::ShellExited {
                run,
                outcome,
                truncated,
            } => Some(Self::ShellExited {
                run,
                outcome,
                truncated,
            }),
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
        ProvisioningStage, ReportProvisioningStage, ReportSpotNotice, ShellOutcome, ShellStream,
        UsageWindow,
    };
    use crate::budget::BudgetSignal;
    use crate::harness::{ContextWindow, HarnessEvent, UsageReport};
    use crate::id::{ApprovalId, SessionId, ShellRunId, WorkdirRequestId};
    use crate::machine::BillingMinimum;
    use crate::money::Usd;
    use crate::session::SessionState;
    use crate::workdir::{
        DirectoryEntry, DirectoryListing, EntryKind, FileChange, FileContent, FileDiff,
        WorkdirDiff, WorkdirRefusal, WorkdirReply, WorkdirRequest,
    };

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
            DaemonToControl::ShellOutput {
                run: ShellRunId::generate(),
                stream: ShellStream::Stderr,
                data: "error: no such file\n".to_owned(),
            },
            DaemonToControl::ShellExited {
                run: ShellRunId::generate(),
                outcome: ShellOutcome::Exited { code: 2 },
                truncated: true,
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
            DaemonToControl::WorkdirReply {
                id: WorkdirRequestId::generate(),
                reply: WorkdirReply::Entries {
                    listing: DirectoryListing {
                        path: "src".to_owned(),
                        entries: vec![DirectoryEntry {
                            name: "lib.rs".to_owned(),
                            path: "src/lib.rs".to_owned(),
                            kind: EntryKind::File,
                            size_bytes: Some(2_048),
                            ignored: false,
                        }],
                        truncated: false,
                    },
                },
            },
            DaemonToControl::WorkdirReply {
                id: WorkdirRequestId::generate(),
                reply: WorkdirReply::File {
                    content: FileContent {
                        path: "README.md".to_owned(),
                        text: "# flyco\n".to_owned(),
                        bytes: 8,
                    },
                },
            },
            DaemonToControl::WorkdirReply {
                id: WorkdirRequestId::generate(),
                reply: WorkdirReply::Diff {
                    diff: WorkdirDiff {
                        base: "origin/main".to_owned(),
                        files: vec![FileDiff {
                            path: "src/lib.rs".to_owned(),
                            previous_path: None,
                            change: FileChange::Modified,
                            added_lines: 3,
                            removed_lines: 1,
                            binary: false,
                            patch: Some("@@ -1 +1,3 @@\n".to_owned()),
                        }],
                        added_lines: 3,
                        removed_lines: 1,
                        truncated: false,
                    },
                },
            },
            DaemonToControl::WorkdirReply {
                id: WorkdirRequestId::generate(),
                reply: WorkdirReply::refused(WorkdirRefusal::TooLarge {
                    path: "data/dump.json".to_owned(),
                    bytes: 9_000_000,
                }),
            },
        ]
    }

    fn every_control_frame() -> Vec<ControlToDaemon> {
        vec![
            ControlToDaemon::Welcome,
            ControlToDaemon::UserMessage {
                text: "what does this crate do?".to_owned(),
            },
            ControlToDaemon::ShellCommand {
                command: "cargo test -p flyco-core".to_owned(),
            },
            ControlToDaemon::RunShell {
                run: ShellRunId::generate(),
                command: "cargo test -p flyco-core".to_owned(),
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
            ControlToDaemon::BudgetRaised {
                limit: Usd::from_dollars(25),
            },
            ControlToDaemon::TerminalInput {
                data: "ls\n".to_owned(),
            },
            ControlToDaemon::MachineChanged {
                machine_type: "Standard_D8s_v6".to_owned(),
                hourly: Some(Usd::from_cents(38)),
                spot: true,
                restarted: true,
            },
            ControlToDaemon::MachineChanged {
                machine_type: "build.lexo.cool".to_owned(),
                hourly: None,
                spot: false,
                restarted: false,
            },
            ControlToDaemon::SetModel {
                model: crate::harness::ModelChoice {
                    model: "sonnet".to_owned(),
                    effort: Some("high".to_owned()),
                },
            },
            ControlToDaemon::SetModel {
                model: crate::harness::ModelChoice {
                    model: "haiku".to_owned(),
                    effort: None,
                },
            },
            ControlToDaemon::Archive {
                preserve_workdir: false,
            },
            ControlToDaemon::Archive {
                preserve_workdir: true,
            },
            ControlToDaemon::InspectWorkdir {
                id: WorkdirRequestId::generate(),
                request: WorkdirRequest::Entries {
                    path: "src".to_owned(),
                },
            },
            ControlToDaemon::InspectWorkdir {
                id: WorkdirRequestId::generate(),
                request: WorkdirRequest::File {
                    path: "src/lib.rs".to_owned(),
                },
            },
            ControlToDaemon::InspectWorkdir {
                id: WorkdirRequestId::generate(),
                request: WorkdirRequest::Diff,
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

    /// Every [`ClientEvent`] variant, at least once each.
    ///
    /// A builder rather than a `let` inside the test, so the list can keep
    /// growing with the protocol without the test that walks it growing too.
    fn every_client_event() -> Vec<ClientEvent> {
        vec![
            ClientEvent::Harness {
                event: harness_event(),
            },
            ClientEvent::UserMessage {
                text: "what does this crate do?".to_owned(),
            },
            ClientEvent::Started {
                harness_session_id: "9d0f4b1a".to_owned(),
            },
            ClientEvent::ModelChanged {
                model: crate::harness::ModelChoice {
                    model: "opus[1m]".to_owned(),
                    effort: Some("max".to_owned()),
                },
            },
            ClientEvent::Models {
                models: crate::harness::builtin_models(crate::harness::HarnessKind::Codex),
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
            ClientEvent::ShellCommand {
                run: ShellRunId::generate(),
                command: "git status --short".to_owned(),
            },
            ClientEvent::ShellOutput {
                run: ShellRunId::generate(),
                stream: ShellStream::Stdout,
                data: " M src/lib.rs\n".to_owned(),
            },
            ClientEvent::ShellExited {
                run: ShellRunId::generate(),
                outcome: ShellOutcome::Exited { code: 0 },
                truncated: false,
            },
            ClientEvent::ShellExited {
                run: ShellRunId::generate(),
                outcome: ShellOutcome::TimedOut { after_seconds: 120 },
                truncated: true,
            },
            ClientEvent::ShellExited {
                run: ShellRunId::generate(),
                outcome: ShellOutcome::Failed {
                    error: "No such file or directory (os error 2)".to_owned(),
                },
                truncated: false,
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
            ClientEvent::MachineChanged {
                machine_type: "Standard_D8s_v6".to_owned(),
                hourly: Some(Usd::from_cents(38)),
                spot: true,
                restarted: true,
            },
            ClientEvent::MachineChanged {
                machine_type: "build.lexo.cool".to_owned(),
                hourly: None,
                spot: false,
                restarted: false,
            },
            ClientEvent::PlanUsage {
                windows: vec![
                    UsageWindow::new(Some(300), None, 26, Some(1_789_002_000)),
                    UsageWindow::new(Some(10_080), Some("Fable"), 26, None),
                ],
            },
            ClientEvent::ApprovalPending {
                id: ApprovalId::generate(),
                payload: ApprovalPayload::MachineResizeLicenseBound {
                    machine_type: "mac2.metal".to_owned(),
                    minimum: BillingMinimum::new(24, Usd::from_cents(65)),
                    reason: "the build needs a signed macOS toolchain".to_owned(),
                },
            },
        ]
    }

    #[test]
    fn every_client_event_survives_the_wire() {
        for event in every_client_event() {
            round_trip(&event);
        }
    }

    #[test]
    fn a_usage_window_is_named_after_its_length() {
        // The three lengths the two vendors actually report, and the two
        // shapes a length flyco has not seen falls into.
        let named = [
            (300, "5-hour"),
            (10_080, "Weekly"),
            (43_200, "Monthly"),
            (1_440, "Daily"),
            (180, "3-hour"),
            (30, "30-minute"),
            (4_320, "3-day"),
        ];
        for (minutes, label) in named {
            assert_eq!(UsageWindow::new(Some(minutes), None, 0, None).label, label);
        }
        assert_eq!(
            UsageWindow::new(Some(10_080), Some("Fable"), 0, None).label,
            "Weekly (Fable)"
        );
        assert_eq!(UsageWindow::new(None, None, 0, None).label, "Plan");
    }

    #[test]
    fn a_usage_window_cannot_read_past_full() {
        assert_eq!(
            UsageWindow::new(Some(300), None, 103, None).used_percent,
            100
        );
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
    fn the_daemons_two_reports_survive_the_wire() {
        // The pair the machine files over REST rather than over the relay,
        // because both outlive the socket they would otherwise ride.
        round_trip(&ReportProvisioningStage {
            stage: ProvisioningStage::Cloning,
        });
        round_trip(&ReportSpotNotice {
            seconds_remaining: 30,
        });
        assert_eq!(
            serde_json::to_string(&ReportSpotNotice {
                seconds_remaining: 120,
            })
            .expect("serialize"),
            r#"{"seconds_remaining":120}"#
        );
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
    fn only_a_machine_change_and_a_model_change_outlive_the_daemon_they_were_sent_to() {
        // The two commands that describe a state rather than an instant.
        // Everything else replayed into a later turn would be an
        // instruction about something that is no longer happening.
        for frame in every_control_frame() {
            let held = matches!(
                frame,
                ControlToDaemon::MachineChanged { .. } | ControlToDaemon::SetModel { .. }
            );
            assert_eq!(frame.survives_a_disconnect(), held, "{frame:?}");
        }
    }

    #[test]
    fn a_client_may_only_drive_the_turn() {
        for frame in every_control_frame() {
            let allowed = matches!(
                frame,
                ControlToDaemon::UserMessage { .. }
                    | ControlToDaemon::ShellCommand { .. }
                    | ControlToDaemon::Interrupt
                    | ControlToDaemon::Compact
                    | ControlToDaemon::TerminalInput { .. }
            );
            assert_eq!(frame.is_client_command(), allowed, "{frame:?}");
        }
    }

    #[test]
    fn a_browser_may_ask_for_a_shell_command_but_not_name_the_run() {
        // The identity of a run is the room's to assign: it keys the
        // recorded row, the output and the exit status, and a browser that
        // could choose it could attach its output to another browser's run.
        assert!(
            ControlToDaemon::ShellCommand {
                command: "ls".to_owned(),
            }
            .is_client_command()
        );
        assert!(
            !ControlToDaemon::RunShell {
                run: ShellRunId::generate(),
                command: "ls".to_owned(),
            }
            .is_client_command()
        );
    }

    #[test]
    fn every_way_a_shell_command_can_end_survives_the_wire() {
        for outcome in [
            ShellOutcome::Exited { code: 0 },
            ShellOutcome::Exited { code: 127 },
            ShellOutcome::Signalled,
            ShellOutcome::TimedOut { after_seconds: 120 },
            ShellOutcome::Cancelled,
            ShellOutcome::Offline,
            ShellOutcome::Busy,
            ShellOutcome::Refused,
            ShellOutcome::Failed {
                error: "bash is not installed".to_owned(),
            },
        ] {
            round_trip(&ClientEvent::ShellExited {
                run: ShellRunId::generate(),
                outcome,
                truncated: false,
            });
        }
    }

    #[test]
    fn a_shell_exit_keeps_its_outcome_tag_apart_from_the_frame_tag() {
        // `ShellOutcome` is tagged on `kind` rather than on `type` for the
        // same reason every variant here is a struct variant: two `type`
        // keys in one object serialize and then refuse to read back.
        let json = serde_json::to_string(&ClientEvent::ShellExited {
            run: ShellRunId::generate(),
            outcome: ShellOutcome::Exited { code: 3 },
            truncated: false,
        })
        .expect("serialize");
        assert_eq!(json.matches("\"type\"").count(), 1);
        assert!(json.contains(r#""outcome":{"kind":"exited","code":3}"#));
    }

    #[test]
    fn only_the_handshake_and_an_addressed_reply_are_hidden_from_browsers() {
        for frame in every_daemon_frame() {
            // The two frames nobody watching the session is meant to see:
            // the handshake, and an answer addressed to the one HTTP
            // request that asked for it.
            let hidden = matches!(
                frame,
                DaemonToControl::Hello { .. } | DaemonToControl::WorkdirReply { .. }
            );
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
