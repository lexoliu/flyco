//! The daemon⇄control-plane wire protocol, and what browsers see of it.
//!
//! The transport is HTTP, not a socket: a daemon attaches over REST, holds
//! one SSE stream for the room's [`ControlToDaemon`] commands, and posts
//! its own [`DaemonToControl`] frames back in sequenced batches.
//! [`crate::WIRE_PROTOCOL_VERSION`] guards compatibility: a daemon speaking
//! another version is refused at `attach`, before any frame moves.
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
use crate::repo::{BranchName, RepoSlug};
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

    /// Whether this window has nothing left in it.
    ///
    /// A full window is what a refused turn means, so this is the predicate
    /// [`blocking_window`] is built from. It is `>=` rather than `==`
    /// because both vendors type the percentage as an unbounded number and
    /// [`new`](Self::new) has already clamped anything above a hundred.
    ///
    /// Both drivers round *down* below a hundred, so a hundred here means
    /// the vendor said a hundred: a window at 99.6% is spent enough to draw
    /// a full ring and not spent enough to stop a session for five hours.
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.used_percent >= 100
    }
}

/// The window an account blocked on its plan is waiting for.
///
/// Not simply "a window at a hundred percent": an account with two spent
/// windows is unblocked by neither until *both* have turned over, so the
/// one to wait for is the exhausted window that resets last. That is the
/// instant flyco schedules the session's return around, and getting it
/// wrong by taking the soonest reset would wake a session into a limit it
/// is still inside.
///
/// `None` when nothing is exhausted, and also when the exhausted windows
/// name no reset time: a pause flyco cannot see the end of is a session
/// stopped for ever, which is worse than one that goes on refusing turns
/// with its machine up and its user watching.
#[must_use]
pub fn blocking_window(windows: &[UsageWindow]) -> Option<&UsageWindow> {
    windows
        .iter()
        .filter(|window| window.is_exhausted())
        .filter(|window| window.resets_at_unix.is_some())
        .max_by_key(|window| window.resets_at_unix)
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

/// Who put a message into a session's conversation.
///
/// Every message the harness sees is shaped like the user's, because that is
/// the only door a coding agent has: flyco has no channel for saying
/// something *about* a session to the agent working in it. So the messages
/// flyco sends on the user's behalf — the notice that a machine was replaced,
/// a CI failure worth acting on, the continuation after a plan window turned
/// over — are user messages, and this is the one thing that distinguishes
/// them.
///
/// Read by the transcript and by nothing else. The harness is handed the text
/// and never this: a model told "the following was written by a program"
/// would reason about the framing instead of doing the work, and the notices
/// already say what they are in their own words.
///
/// Defaults to [`User`](Self::User), which is what every message that does
/// not say otherwise is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    /// The user typed it, in the composer or through
    /// `POST /v1/sessions/{id}/messages`.
    #[default]
    User,
    /// Flyco said it on the user's behalf, and the transcript says so.
    Flyco,
}

/// One slash command the running harness offers its user.
///
/// Flyco's own vocabulary rather than either harness's: Claude Code answers
/// `supportedCommands()` with `{name, description, argumentHint}` and Codex
/// answers `skills/list` with skill metadata, and the composer must not
/// have to know which one it is looking at. `name` never carries the
/// leading slash — that belongs to the syntax the palette renders, not to
/// the command's identity — and it may contain a colon, because a plugin's
/// skill is named `plugin:skill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HarnessCommand {
    /// The command's name, without the leading slash.
    pub name: String,
    /// One line saying what it does, as the harness words it.
    pub description: String,
    /// What the command's argument is, when it takes one.
    ///
    /// `None` is what makes a command runnable in one keystroke: the
    /// palette sends a command with no argument the moment it is chosen,
    /// and only inserts `/name ` into the field when there is something
    /// left for the user to type. Both harnesses state the absence as an
    /// empty string; it is normalized to `None` where it enters flyco, so
    /// nothing downstream has to treat `""` as a special case.
    pub argument_hint: Option<String>,
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
    /// Add a repository to the session's workspace.
    ///
    /// Raised rather than performed, because cloning a repository the user
    /// never picked onto the session's machine is a decision about which
    /// code the agent may act on — the same reason a license-bound resize
    /// is the user's call. Approving records the repository and sends the
    /// daemon [`ControlToDaemon::AddRepo`]; denying is answered to the
    /// agent as a refusal.
    RepoAdd {
        /// Repository in `owner/name` form.
        repo: String,
        /// Branch to check out. `None` asks for the repository's default.
        branch: Option<String>,
        /// Why the agent says the session needs it, as shown on the card.
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
    /// the harness is started in, and the command stream is not opened
    /// until there is a session behind it.
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

/// Why a machine's own daemon says it is going away.
///
/// One variant, and an enum all the same: the control plane's container
/// drivers have to tell "the platform stopped this execution" from "the
/// execution ran out of time" and from "flyco asked for it", and a boolean
/// or a bare string could not carry that distinction into the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum StopReason {
    /// The platform sent `SIGTERM`.
    ///
    /// What every managed container service does when it stops an
    /// execution, evicts a spot task, or reaches the run's timeout: Azure
    /// Container Apps, Cloud Run and Fargate all announce it this way, and
    /// all three follow it with `SIGKILL` about thirty seconds later.
    Sigterm,
}

/// Request body of `POST /v1/sessions/{id}/stopping`.
///
/// The last thing a container session's daemon files. Its counterpart on a
/// virtual machine is [`ReportSpotNotice`], and the two are deliberately
/// different routes rather than one with a flag: a reclaimed VM keeps its
/// disk and is *recovered* — the control plane queues a start against the
/// same machine after the provider's own countdown — while a stopping
/// container has already handed its working tree over as the
/// `workdir-patch` and is simply gone, with nothing to schedule against a
/// deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReportStopping {
    /// What made the machine stop.
    pub reason: StopReason,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonToControl {
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
    /// Every slash command the running harness offers, newest list wins.
    ///
    /// A relay frame rather than a REST report — which is what makes it
    /// different from the model list it otherwise resembles — because the
    /// set is a fact about *this* session's checkout: it carries the repo's
    /// own skills, and the same account's next session in another repo has
    /// a different one. Nothing about it belongs to the account, so nothing
    /// about it belongs in D1.
    Commands {
        /// The commands, in the order the harness listed them.
        commands: Vec<HarnessCommand>,
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
    /// The harness TUI exited on its own.
    ///
    /// The answer to a
    /// [`TerminalHarness`](ControlToDaemon::TerminalHarness) request: it is
    /// how a client learns the TUI ended. Only a harness's natural exit is
    /// reported — the shell is ambient, so its exits just put a fresh one
    /// back, and a child killed to make room for another says nothing.
    TerminalExited {
        /// The process's exit code. `None` when a signal ended it.
        code: Option<i32>,
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
    /// A checkout has uncommitted changes; the agent is kept awake rather
    /// than allowed to complete.
    RepoDirty {
        /// Which checkout — [`SessionRepo::dir`](crate::repo::SessionRepo::dir),
        /// or `None` for a session whose workdir is itself the checkout
        /// (the developer-machine shape).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<String>,
        /// `git status --porcelain` summary shown to the user.
        summary: String,
    },
    /// A repository was added to the session's workspace.
    ///
    /// Reported rather than implied by [`ControlToDaemon::AddRepo`], so a
    /// transcript records the checkout actually landing on the machine —
    /// and a clone that failed reports nothing instead of claiming a
    /// repository the disk does not hold.
    RepoAdded {
        /// The repository now checked out.
        slug: RepoSlug,
        /// The branch it is on.
        branch: BranchName,
        /// The directory under the workdir it was cloned into.
        dir: String,
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
    /// Where the session's desktop is: coming up, ready, in use, or failed.
    ///
    /// Reported on change only — a daemon that reports nothing yet has a
    /// screen nobody can watch, and the newest report is the whole story.
    /// `detail` carries the failure's sentence when `status` is
    /// [`DesktopStatus::Failed`], and the reason a desktop is taking its
    /// time when it is [`DesktopStatus::Starting`].
    DesktopState {
        /// The lifecycle state.
        status: DesktopStatus,
        /// The sentence a panel shows beside it, when there is one.
        detail: Option<String>,
    },
    /// The agent is on the screen — the `Screen` panel opens itself.
    ///
    /// Sent once per idle→use transition, never per action: the first
    /// `computer_*` call after a quiet spell announces the desktop, and a
    /// burst of them is still one announcement. The transition is what a
    /// browser opens the panel on, which is why it is a frame of its own
    /// rather than another reading of [`DesktopStatus`].
    DesktopActive,
    /// One encoded temporal unit of the desktop stream.
    ///
    /// Chunks ride the frame batch like every other report — one
    /// transport, one epoch, one ordering — and the room routes them to
    /// the stream's own table when they land rather than replaying them
    /// through the event log. `keyframe` marks a chunk a decoder can
    /// start from cold; it is what a joining watcher is replayed from,
    /// and what the room asks the daemon to re-arm when the tail is cut.
    DesktopChunk {
        /// Whether a decoder can start from this chunk.
        keyframe: bool,
        /// The encoded bytes — base64 on the wire, since a JSON array of
        /// numbers would cost three bytes per byte of stream.
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
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

/// Where a session's desktop is in its lifecycle, as the daemon reports it.
///
/// A state, not an instant: the newest report wins, and a browser that
/// opens the session after the fact reads the last one rather than a
/// history of the display server coming up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DesktopStatus {
    /// The display stack is being installed or started — nothing is on the
    /// screen yet, and a `Screen` panel opened now shows it warming up.
    Starting,
    /// The display is up and accepting input; nobody is driving it yet.
    Ready,
    /// The agent — or the user, under takeover — is on the screen.
    Active,
    /// The desktop could not be provided on this machine.
    ///
    /// The frame's `detail` carries the sentence the panel shows: a
    /// missing package, a permission the OS would not grant, a capture
    /// that never produced a frame.
    Failed,
}

/// Which mouse button a [`DesktopInputEvent::Button`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DesktopButton {
    /// The primary button.
    Left,
    /// The wheel-as-button.
    Middle,
    /// The secondary button.
    Right,
    /// The browser-back side button.
    Back,
    /// The browser-forward side button.
    Forward,
}

/// One user input on the session's desktop, in display coordinates.
///
/// Coordinates are the display's own, not the viewer's: a browser scales
/// its rendered frame down to fit the panel and has to scale the input
/// back up, so the protocol carries the pixel the screen itself would
/// see and nothing else.
///
/// Tagged on `kind` rather than `type`, because it nests inside
/// [`ControlToDaemon::DesktopInput`], which is already tagged on `type` —
/// the same convention [`ShellOutcome`] follows inside a `shell_exited`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DesktopInputEvent {
    /// The pointer moved to (x, y).
    Move {
        /// Display x.
        x: u16,
        /// Display y.
        y: u16,
    },
    /// A mouse button at (x, y) changed state.
    Button {
        /// Display x.
        x: u16,
        /// Display y.
        y: u16,
        /// Which button.
        button: DesktopButton,
        /// Whether it is now held.
        pressed: bool,
    },
    /// A wheel or trackpad scroll at (x, y), in pixels of intent.
    ///
    /// The daemon turns a scroll into the display's own unit — button-4/5
    /// clicks under X — so the browser sends the DOM delta it has rather
    /// than a guess at line counts.
    Scroll {
        /// Display x.
        x: u16,
        /// Display y.
        y: u16,
        /// Horizontal delta, DOM convention (positive scrolls right).
        delta_x: i32,
        /// Vertical delta, DOM convention (positive scrolls down).
        delta_y: i32,
    },
    /// A key changed state.
    ///
    /// `code` is the DOM `KeyboardEvent.code` — the physical position —
    /// and `key` the character or named key it produced. The daemon maps
    /// the position first, because a display has a keymap and a browser
    /// has a layout, and the two only agree through the physical key; the
    /// produced key is the fallback for anything the map does not cover.
    Key {
        /// `KeyboardEvent.code`, e.g. `KeyW` or `ShiftLeft`.
        code: String,
        /// `KeyboardEvent.key`, e.g. `w` or `Shift`.
        key: String,
        /// Whether it is now held.
        pressed: bool,
    },
}

/// Messages from the control plane to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlToDaemon {
    /// A user message to feed the harness.
    UserMessage {
        /// Message text.
        text: String,
        /// Who is speaking, for the transcript's benefit.
        ///
        /// Carried on the command because the room is what turns it into the
        /// [`ClientEvent::UserMessage`] browsers read, and the room is a
        /// Durable Object that knows nothing about why the Worker sent this.
        /// The daemon ignores it: what reaches the harness is the text.
        #[serde(default)]
        origin: MessageOrigin,
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
    /// Ask the harness what its context window is spent on.
    ///
    /// The usage panel's "detailed breakdown" sends this: a query, not a
    /// prompt. The harness answers
    /// it out of band — the Claude SDK's `get_context_usage` control
    /// request; Codex from the token readings its stream already carries —
    /// and the answer comes back as
    /// [`HarnessEvent::ContextUsage`](crate::harness::HarnessEvent::ContextUsage).
    /// It is a command of its own rather than a `user_message` carrying the
    /// text `/context`, because the CLI would treat that text as a *local*
    /// command whose answer never reaches the relay — and because a query
    /// about the context should never enter the context it describes.
    ContextUsage,
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
    /// Put the session's harness TUI in the terminal's foreground.
    ///
    /// The `flyco claude`/`flyco codex`/`flyco resume` path: the CLI
    /// bridges the user's local terminal to the machine's PTY and asks for
    /// the harness's own interface rather than the shell. The daemon
    /// supplies the credentials from its own configuration, so nothing a
    /// client sends ever carries them.
    ///
    /// `resume` makes the request an *ensure* and picks the re-entry
    /// command (`claude --continue`, `codex resume --last`): a TUI already
    /// in the foreground is left alone, because re-attaching to a session
    /// must not kill the turn on its screen. Dropped rather than held when
    /// no daemon is connected, on the same reasoning as
    /// [`Self::TerminalInput`] — a launch held for a daemon that returns
    /// an hour later would open a TUI nobody is watching.
    TerminalHarness {
        /// Re-enter the last conversation rather than start a new one.
        resume: bool,
    },
    /// The web terminal's size, as the browser has fitted it.
    ///
    /// Sent when the pane opens and whenever it is resized, so the PTY
    /// agrees with the view about where a line wraps: a PTY at a fixed
    /// size under a pane of another was a shell whose lines ran off the
    /// panel's edge (issue #253). Not held for a daemon that is away —
    /// the browser sends it again when the daemon is back.
    TerminalResize {
        /// Columns the pane shows.
        cols: u16,
        /// Rows the pane shows.
        rows: u16,
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
    /// attach — the whole restart is a window with no daemon in it, so a
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
    /// Put the session's agent on another permission mode.
    ///
    /// Applied to the conversation already in progress: Claude Code takes
    /// `setPermissionMode` on a live query, and Codex's `turn/start`
    /// documents `approvalPolicy` and `sandboxPolicy` as overriding "this
    /// turn and subsequent turns" — so on Codex the change is what the next
    /// turn runs under, and on Claude it is what the rest of this one does.
    ///
    /// Held for a daemon that is not connected on the same terms as
    /// [`Self::SetModel`]: it describes a state, the control plane has
    /// already recorded it, and a daemon that missed it would run the
    /// session under a mode its own row disagrees with.
    SetPermissionMode {
        /// The mode the session runs under now.
        mode: crate::harness::PermissionMode,
    },
    /// Give the session a screen, or take it away.
    ///
    /// Sent when `PATCH /v1/sessions/{id}` flips `computer_use` on a live
    /// session — a daemon that has been told starts its display stack in
    /// the background rather than on the boot path. Held for a daemon that
    /// is not connected on the same terms as [`Self::SetModel`]: the flag
    /// is a state the session row already records, and a daemon that
    /// missed it would run a session its own row disagrees with.
    SetComputerUse {
        /// Whether the session may have a desktop.
        enabled: bool,
    },
    /// Whether any browser is watching the session's screen.
    ///
    /// The room's aggregation of its open desktop streams — a fact the
    /// room composes itself, never one a client states — because every
    /// encoded frame out of the machine is billed egress on the user's
    /// cloud account: `false` tells the daemon to stop encoding entirely,
    /// and `true` starts the cadence again. Ephemeral rather than held:
    /// the room replays the current state to every fresh attach, so a
    /// queued copy would only say the same thing twice.
    DesktopAudience {
        /// Whether at least one browser is watching.
        watching: bool,
    },
    /// The user took the screen, or handed it back.
    ///
    /// Taking it interrupts the running turn exactly as Stop does, and
    /// while it is held the daemon refuses the agent's `computer_*` calls
    /// with "the user is driving" — a click into the desktop mid-gesture
    /// is never ambiguous, because the last one to take over owns it.
    /// Handed back by a release or by the user sending a message.
    ///
    /// Held for a daemon that is not connected, like
    /// [`Self::SetComputerUse`]: it describes a state — who owns the
    /// screen — and a daemon that missed it would inject the model's input
    /// over the user's own hands.
    DesktopTakeover {
        /// Whether the user now owns the screen.
        active: bool,
    },
    /// One batch of the user's input for the desktop.
    ///
    /// Batched rather than one command per motion, because a pointer drag
    /// is dozens of events a second and the command log is not the place
    /// for them. Ephemeral: input held for a daemon that reconnects later
    /// would be applied to a screen that no longer exists, so a session
    /// with no daemon attached drops it instead.
    DesktopInput {
        /// The events, in the order the user produced them.
        events: Vec<DesktopInputEvent>,
    },
    /// Add a repository to the session's workspace.
    ///
    /// Sent after the repository is recorded in the session's row set —
    /// the user picked it mid-session, or approved the agent's
    /// [`ApprovalPayload::RepoAdd`]. The daemon clones it under `dir`,
    /// announces [`DaemonToControl::RepoAdded`] when the checkout exists,
    /// and tells the harness about it in the conversation.
    ///
    /// Held for a daemon that is not connected, on the same reasoning as
    /// [`Self::SetModel`]: the repository is already a fact of the session,
    /// so a daemon that comes back an hour later is still owed it — and on
    /// a fresh machine the boot clone covers every recorded repository,
    /// which makes a held command reaching a new daemon a no-op to skip
    /// rather than a second clone.
    AddRepo {
        /// The repository to clone.
        slug: RepoSlug,
        /// The branch to check out.
        branch: BranchName,
        /// The directory under the workdir to clone into, as the session's
        /// row set recorded it.
        dir: String,
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
    /// A newer attach owns this session's room; this daemon is a spare.
    ///
    /// Composed by the room itself and only ever delivered down a stream
    /// serving a superseded epoch — the one thing that stream can still be
    /// told. A daemon that reads it stops rather than re-attaching into the
    /// epoch that replaced it: two daemons racing one room is how the relay
    /// burns the account's request budget (issue #336). Never a log row and
    /// never held — there is no daemon left to tell twice.
    Superseded,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires fn(&T) -> bool"
)]
pub(crate) const fn is_false(value: &bool) -> bool {
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
            Self::UserMessage { .. } => "user_message",
            Self::ShellCommand { .. } => "shell_command",
            Self::RunShell { .. } => "run_shell",
            Self::TerminalInput { .. } => "terminal_input",
            Self::TerminalHarness { .. } => "terminal_harness",
            Self::TerminalResize { .. } => "terminal_resize",
            Self::Interrupt => "interrupt",
            Self::Compact => "compact",
            Self::ContextUsage => "context_usage",
            Self::ApprovalDecision { .. } => "approval_decision",
            Self::Budget { .. } => "budget",
            Self::BudgetRaised { .. } => "budget_raised",
            Self::MachineChanged { .. } => "machine_changed",
            Self::SetModel { .. } => "set_model",
            Self::SetPermissionMode { .. } => "set_permission_mode",
            Self::SetComputerUse { .. } => "set_computer_use",
            Self::DesktopAudience { .. } => "desktop_audience",
            Self::DesktopTakeover { .. } => "desktop_takeover",
            Self::DesktopInput { .. } => "desktop_input",
            Self::InspectWorkdir { .. } => "inspect_workdir",
            Self::AddRepo { .. } => "add_repo",
            Self::Archive { .. } => "archive",
            Self::Superseded => "superseded",
        }
    }

    /// Whether a session's owner may send this command.
    ///
    /// A session room accepts exactly nine commands from a user client;
    /// everything else is control-plane authority (budget signals, approval
    /// decisions, archival, and the identified [`Self::RunShell`] the room
    /// reissues a [`Self::ShellCommand`] as) and reaches the daemon only
    /// through the room itself or an authenticated REST handler. A client
    /// that sends anything else is refused rather than ignored. The two
    /// desktop commands arrive through their own routes — the takeover
    /// route is also where the room decides the flip against the watcher
    /// leases — but they are still client commands by origin.
    #[must_use]
    pub const fn is_client_command(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. }
                | Self::ShellCommand { .. }
                | Self::Interrupt
                | Self::Compact
                | Self::ContextUsage
                | Self::TerminalInput { .. }
                | Self::TerminalResize { .. }
                | Self::TerminalHarness { .. }
                | Self::DesktopTakeover { .. }
                | Self::DesktopInput { .. }
        )
    }

    /// Whether the room must keep this command for a daemon that is away.
    ///
    /// Almost nothing survives a disconnect, and that is deliberate: an
    /// interrupt, a compaction or a keystroke held for a daemon that
    /// reconnects an hour later would arrive as an instruction about a turn
    /// that no longer exists.
    ///
    /// The exceptions are the commands that describe a *state* rather than
    /// an instant, so redelivering one late still says something true.
    /// [`MachineChanged`](Self::MachineChanged) is the exception by
    /// construction: the change it reports *is* a restart, so the daemon is
    /// guaranteed to be gone at the moment it is sent, and the machine is
    /// still the new one whenever it comes back. [`SetModel`](Self::SetModel)
    /// and [`SetPermissionMode`](Self::SetPermissionMode) are the
    /// exceptions by consequence: the control plane has already recorded
    /// what they carry, and a daemon that missed either would run the
    /// session on a model or under a mode its own row disagrees with.
    /// [`SetComputerUse`](Self::SetComputerUse) is the same shape: the
    /// flag on the session row is what the next machine comes up with.
    /// [`AddRepo`](Self::AddRepo) is the same kind of fact: the repository
    /// is on the session's row set the moment the command is queued, so the
    /// daemon owes the workspace a checkout for it whenever it next
    /// attaches.
    ///
    /// [`DesktopTakeover`](Self::DesktopTakeover) is a state too, but it
    /// is deliberately absent: who owns the screen is leased to a live
    /// watcher row in the room, and a reattaching daemon is reconciled
    /// against that table rather than against commands queued while it
    /// was away — a takeover whose browser died in the gap would
    /// otherwise arrive as a lock nobody can lift.
    ///
    /// A user message survives too, but because the room records it as
    /// conversation rather than because delivery is owed: a message is part
    /// of the transcript, so the room keeps it whether or not a daemon is
    /// there to hear it.
    #[must_use]
    pub const fn survives_a_disconnect(&self) -> bool {
        matches!(
            self,
            Self::MachineChanged { .. }
                | Self::SetModel { .. }
                | Self::SetPermissionMode { .. }
                | Self::SetComputerUse { .. }
                | Self::AddRepo { .. }
        )
    }
}

/// What a daemon offers the room when it attaches.
///
/// Attaching is a REST call, not a frame: the bearer token authenticates
/// before any of this is read, and the version lives here so a daemon
/// speaking another protocol is refused before either side moves a frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DaemonAttach {
    /// Wire protocol version the daemon speaks.
    pub protocol_version: u32,
}

/// What the room answers an attach with.
///
/// The epoch names the attachment. Every later [`DaemonFrames`] POST and
/// every command on the daemon's command stream carries it, so a daemon
/// that attached twice — a retry raced the first attempt's response — and
/// a room that watched the first stream die can agree about which
/// attachment the traffic belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DaemonAttached {
    /// Generation of this attachment; increments per attach.
    pub epoch: u64,
}

/// One POST of a daemon's outbound frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DaemonFrames {
    /// The attach this batch belongs to.
    pub epoch: u64,
    /// Sequence number of `frames[0]` within the epoch.
    ///
    /// The daemon numbers its frames from 1 on each attach and the room
    /// tracks how far it has stored. A batch whose `from_seq` says its
    /// head is already stored is a retransmission — answered without
    /// touching anything — and a batch that skips a number is a lost
    /// POST, refused so the daemon re-sends from the gap.
    pub from_seq: u64,
    /// The highest command sequence the daemon has applied.
    ///
    /// `daemon_commands` rows at or below it are delivered and done, and
    /// the room deletes them. Zero acknowledges nothing.
    pub ack_through: u64,
    /// The frames, in order.
    pub frames: Vec<DaemonToControl>,
}

/// One `command` event on the daemon's command stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DaemonCommand {
    /// Position in the room's command log, when the command came from it.
    ///
    /// `None` for the commands the stream composes itself — a replayed
    /// terminal size, the supersession notice — which carry no ordering
    /// obligation: a daemon applies them whenever they arrive and
    /// acknowledges nothing for them.
    pub seq: Option<u64>,
    /// The command.
    pub command: ControlToDaemon,
}

/// One event on a user's global stream.
///
/// `GET /v1/events` multiplexes every session the user owns onto one SSE
/// connection; the envelope is what tells them apart, and what lets a
/// client refill a single session's history with `events?after=` when the
/// stream's resume cursor finds a gap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionEvent {
    /// Session the event belongs to.
    pub session: SessionId,
    /// Position in that session's recorded history, when it has one.
    ///
    /// Events a session room emits are sequenced there; events the control
    /// plane composes about a session have no position to carry.
    pub seq: Option<u64>,
    /// When the control plane published it, seconds since the Unix epoch.
    ///
    /// Live events carry no recorded timestamp the way a
    /// [`StoredEvent`] does; this is the publish instant, which is the only
    /// time a live-only event ever has.
    pub at_unix: u64,
    /// The event.
    pub event: ClientEvent,
}

/// One stored event, as the catch-up API serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StoredEvent {
    /// Monotonic position in the room's stream. Pass the last one back as
    /// `after` to continue.
    pub seq: u64,
    /// The [`ClientEvent`] this position holds. Untyped: the room replays
    /// what the daemon sent, including a variant this build does not know.
    pub event: serde_json::Value,
    /// When the room recorded it, seconds since the Unix epoch.
    pub at_unix: u64,
}

/// A page of a session's event tail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EventPage {
    /// The events, oldest first.
    pub events: Vec<StoredEvent>,
    /// Whether more events exist past the last one returned.
    pub more: bool,
}

/// What a browser following a session receives.
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
    /// reports back: it arrives at the room through
    /// `POST /v1/sessions/{id}/messages`, and the room records and echoes it
    /// there. Without it a second browser would watch the agent answer
    /// questions it could not see, and a replayed session would be one side
    /// of a conversation — including the prompt every turn in the history
    /// list is named by.
    UserMessage {
        /// What was said to the agent, verbatim.
        text: String,
        /// Who said it, which is what lets the transcript attribute a
        /// message flyco sent on the user's behalf to flyco.
        #[serde(default)]
        origin: MessageOrigin,
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
    /// attach, when its command stream closes, and whenever a command finds
    /// nobody to take it (docs/ux.md §9.6).
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
    /// The session's harness TUI exited on its own.
    ///
    /// The `flyco claude`/`flyco codex`/`flyco resume` detach signal: after
    /// a client asks for the harness TUI, this is how it learns the TUI
    /// ended. The shell's own exits are not reported — the daemon just
    /// puts a fresh one back in the foreground.
    TerminalExited {
        /// The process's exit code. `None` when a signal ended it.
        code: Option<i32>,
    },
    /// A checkout has uncommitted changes and the agent is kept awake.
    RepoDirty {
        /// Which checkout, as [`SessionRepo::dir`](crate::repo::SessionRepo::dir)
        /// names it — `None` when the session's workdir is itself the
        /// checkout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<String>,
        /// `git status --porcelain` summary shown to the user.
        summary: String,
    },
    /// A repository landed in the session's workspace.
    ///
    /// Rendered as one line in the transcript, like a machine change and
    /// for the same reason: the code the agent can reach changed from here
    /// on, and a transcript without the seam would show edits to a
    /// repository nobody added.
    RepoAdded {
        /// The repository now checked out.
        slug: RepoSlug,
        /// The branch it is on.
        branch: BranchName,
        /// The directory under the workdir it was cloned into.
        dir: String,
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
    /// The session's agent was put under another permission mode.
    ///
    /// Rendered as one line in the transcript, like a model change and for
    /// the same reason: what the agent may do without asking changes from
    /// here on, and a conversation whose second half ran under a different
    /// mode with no note of where the seam is would be a transcript that
    /// misrepresents itself.
    PermissionModeChanged {
        /// The mode the session runs under now.
        mode: crate::harness::PermissionMode,
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
    /// The slash commands this session's harness offers.
    ///
    /// State rather than conversation, like [`Models`](Self::Models): the
    /// newest list wins and it is what the composer's `/` palette offers
    /// for the rest of the session. Appended to the room's stream rather
    /// than only broadcast, so a browser that opens the session an hour
    /// after the daemon reported it still gets the real list.
    Commands {
        /// Every command the harness listed, in its own order.
        commands: Vec<HarnessCommand>,
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
    /// Where the session's desktop is: coming up, ready, in use, failed.
    ///
    /// State, newest wins — the `Screen` panel's status line reads the
    /// last one. Recorded rather than live-only because a desktop that
    /// failed while nobody watched is still what the session did.
    DesktopState {
        /// The lifecycle state.
        status: DesktopStatus,
        /// The sentence beside it, when there is one.
        detail: Option<String>,
    },
    /// The agent is on the screen — the `Screen` panel opens itself.
    ///
    /// Recorded like a machine change: a replay that never showed the
    /// screen being touched would leave the transcript pretending the
    /// session only ever typed.
    DesktopActive,
    /// The user took the screen, or handed it back.
    ///
    /// Recorded for the same reason a takeover interrupts a turn: the
    /// seam between "the agent was driving" and "the user is driving" is
    /// where the transcript's actions stop being the model's.
    DesktopTakeover {
        /// Whether the user now owns the screen.
        active: bool,
    },
    /// The session's `computer_use` flag changed.
    ///
    /// Rendered as one line in the transcript, like a model change and for
    /// the same reason: what the agent may reach for changes from here on.
    ComputerUseChanged {
        /// Whether the session may have a desktop now.
        enabled: bool,
    },
}

impl ClientEvent {
    /// The client-facing form of a daemon frame, if browsers see it at all.
    ///
    /// An [`DaemonToControl::ApprovalRequest`] becomes
    /// [`Self::ApprovalPending`], because "pending" is the state the UI
    /// renders rather than the act of asking. A
    /// [`DaemonToControl::WorkdirReply`] is addressed to one waiting HTTP
    /// request and is collected by the Worker rather than broadcast, and a
    /// [`DaemonToControl::DesktopChunk`] is stream bytes rather than an
    /// event — the room moves it to the desktop's own table — so neither
    /// has a client form.
    #[must_use]
    pub fn from_daemon(frame: DaemonToControl) -> Option<Self> {
        match frame {
            DaemonToControl::WorkdirReply { .. } | DaemonToControl::DesktopChunk { .. } => None,
            DaemonToControl::Started { harness_session_id } => {
                Some(Self::Started { harness_session_id })
            }
            DaemonToControl::Capabilities { capabilities } => {
                Some(Self::Capabilities { capabilities })
            }
            DaemonToControl::Commands { commands } => Some(Self::Commands { commands }),
            DaemonToControl::Harness { event } => Some(Self::Harness { event }),
            DaemonToControl::Usage { usage } => Some(Self::Usage { usage }),
            DaemonToControl::ApprovalRequest { id, payload } => {
                Some(Self::ApprovalPending { id, payload })
            }
            DaemonToControl::TerminalOutput { data } => Some(Self::TerminalOutput { data }),
            DaemonToControl::TerminalExited { code } => Some(Self::TerminalExited { code }),
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
            DaemonToControl::RepoDirty { dir, summary } => Some(Self::RepoDirty { dir, summary }),
            DaemonToControl::RepoAdded { slug, branch, dir } => {
                Some(Self::RepoAdded { slug, branch, dir })
            }
            DaemonToControl::SpotNotice { seconds_remaining } => {
                Some(Self::SpotNotice { seconds_remaining })
            }
            DaemonToControl::ProvisioningStage { stage, at_unix } => {
                Some(Self::ProvisioningStage { stage, at_unix })
            }
            DaemonToControl::DesktopState { status, detail } => {
                Some(Self::DesktopState { status, detail })
            }
            DaemonToControl::DesktopActive => Some(Self::DesktopActive),
        }
    }
}

/// The base64 arm of the wire: a `Vec<u8>` that crosses JSON as text.
///
/// `serde_bytes` exists for formats that know what a byte string is; JSON
/// does not, so the honest `Vec<u8>` field gets an explicit string form
/// rather than the array-of-numbers it would otherwise emit.
mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    /// The serialized form.
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// The parsed form.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(&text)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApprovalDecision, ApprovalPayload, ClientEvent, ControlToDaemon, DaemonToControl,
        DesktopButton, DesktopInputEvent, DesktopStatus, HarnessCommand, MessageOrigin,
        ProvisioningStage, ReportProvisioningStage, ReportSpotNotice, ReportStopping, ShellOutcome,
        ShellStream, StopReason, UsageWindow, blocking_window,
    };
    use crate::budget::BudgetSignal;
    use crate::harness::{ContextWindow, HarnessEvent, UsageReport};
    use crate::id::{ApprovalId, ShellRunId, WorkdirRequestId};
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

    /// Two real rows of what Claude Code answers `supportedCommands()`
    /// with: one that takes an argument and one that takes none.
    fn sample_commands() -> Vec<HarnessCommand> {
        vec![
            HarnessCommand {
                name: "goal".to_owned(),
                description: "Keep working until a condition is met".to_owned(),
                argument_hint: Some("<condition>".to_owned()),
            },
            HarnessCommand {
                name: "context".to_owned(),
                description: "Visualize current context usage as a colored grid".to_owned(),
                argument_hint: None,
            },
        ]
    }

    fn payload() -> ApprovalPayload {
        ApprovalPayload::ToolUse {
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "ls" }),
        }
    }

    fn every_daemon_frame() -> Vec<DaemonToControl> {
        vec![
            DaemonToControl::Started {
                harness_session_id: "9d0f4b1a".to_owned(),
            },
            DaemonToControl::Capabilities {
                capabilities: vec!["can_use_tool".to_owned()],
            },
            DaemonToControl::Commands {
                commands: sample_commands(),
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
            DaemonToControl::TerminalExited { code: Some(0) },
            DaemonToControl::TerminalExited { code: None },
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
                dir: None,
                summary: " M src/lib.rs".to_owned(),
            },
            DaemonToControl::RepoDirty {
                dir: Some("flyco".to_owned()),
                summary: " M src/lib.rs".to_owned(),
            },
            DaemonToControl::RepoAdded {
                slug: "lexoliu/aither".parse().expect("valid"),
                branch: "main".parse().expect("valid"),
                dir: "aither".to_owned(),
            },
            DaemonToControl::SpotNotice {
                seconds_remaining: 30,
            },
            DaemonToControl::ProvisioningStage {
                stage: ProvisioningStage::Ready,
                at_unix: 1_800_000_000,
            },
            DaemonToControl::DesktopState {
                status: DesktopStatus::Ready,
                detail: None,
            },
            DaemonToControl::DesktopState {
                status: DesktopStatus::Failed,
                detail: Some("Xvfb is not installed on this image".to_owned()),
            },
            DaemonToControl::DesktopActive,
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
        ]
        .into_iter()
        .chain(workdir_replies())
        .collect()
    }

    fn workdir_replies() -> Vec<DaemonToControl> {
        vec![
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

    #[expect(
        clippy::too_many_lines,
        reason = "one entry per variant, which is the point: the vector is the \
                  protocol's whole vocabulary, so a variant added anywhere in \
                  it exercises every test that iterates it"
    )]
    fn every_control_frame() -> Vec<ControlToDaemon> {
        vec![
            ControlToDaemon::UserMessage {
                text: "what does this crate do?".to_owned(),
                origin: MessageOrigin::User,
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
            ControlToDaemon::ContextUsage,
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
            ControlToDaemon::TerminalHarness { resume: true },
            ControlToDaemon::TerminalResize {
                cols: 132,
                rows: 40,
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
            ControlToDaemon::SetPermissionMode {
                mode: crate::harness::PermissionMode::AcceptEdits,
            },
            ControlToDaemon::SetComputerUse { enabled: true },
            ControlToDaemon::SetComputerUse { enabled: false },
            ControlToDaemon::DesktopAudience { watching: true },
            ControlToDaemon::DesktopAudience { watching: false },
            ControlToDaemon::DesktopTakeover { active: true },
            ControlToDaemon::DesktopTakeover { active: false },
            ControlToDaemon::DesktopInput {
                events: vec![
                    DesktopInputEvent::Move { x: 640, y: 400 },
                    DesktopInputEvent::Button {
                        x: 640,
                        y: 400,
                        button: DesktopButton::Left,
                        pressed: true,
                    },
                    DesktopInputEvent::Scroll {
                        x: 640,
                        y: 400,
                        delta_x: 0,
                        delta_y: 240,
                    },
                    DesktopInputEvent::Key {
                        code: "KeyW".to_owned(),
                        key: "w".to_owned(),
                        pressed: true,
                    },
                ],
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
                request: WorkdirRequest::Diff { repo: None },
            },
            ControlToDaemon::InspectWorkdir {
                id: WorkdirRequestId::generate(),
                request: WorkdirRequest::Diff {
                    repo: Some("flyco".to_owned()),
                },
            },
            ControlToDaemon::AddRepo {
                slug: "lexoliu/aither".parse().expect("valid"),
                branch: "main".parse().expect("valid"),
                dir: "aither".to_owned(),
            },
            ControlToDaemon::Superseded,
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

    /// One shell run ending the way `outcome` says.
    fn shell_exited(outcome: ShellOutcome, truncated: bool) -> ClientEvent {
        ClientEvent::ShellExited {
            run: ShellRunId::generate(),
            outcome,
            truncated,
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
                origin: MessageOrigin::Flyco,
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
            ClientEvent::Commands {
                commands: sample_commands(),
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
            ClientEvent::TerminalExited { code: Some(1) },
            ClientEvent::ShellCommand {
                run: ShellRunId::generate(),
                command: "git status --short".to_owned(),
            },
            ClientEvent::ShellOutput {
                run: ShellRunId::generate(),
                stream: ShellStream::Stdout,
                data: " M src/lib.rs\n".to_owned(),
            },
            shell_exited(ShellOutcome::Exited { code: 0 }, false),
            shell_exited(ShellOutcome::TimedOut { after_seconds: 120 }, true),
            shell_exited(
                ShellOutcome::Failed {
                    error: "No such file or directory (os error 2)".to_owned(),
                },
                false,
            ),
            ClientEvent::RepoDirty {
                dir: None,
                summary: " M src/lib.rs".to_owned(),
            },
            ClientEvent::RepoDirty {
                dir: Some("flyco".to_owned()),
                summary: " M src/lib.rs".to_owned(),
            },
            ClientEvent::RepoAdded {
                slug: "lexoliu/aither".parse().expect("valid"),
                branch: "main".parse().expect("valid"),
                dir: "aither".to_owned(),
            },
            ClientEvent::SpotNotice {
                seconds_remaining: 30,
            },
            ClientEvent::ProvisioningStage {
                stage: ProvisioningStage::Booting,
                at_unix: 1_800_000_000,
            },
        ]
        .into_iter()
        .chain(machine_events())
        .collect()
    }

    fn machine_events() -> Vec<ClientEvent> {
        vec![
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

    /// A message with no origin stated is the user's, which is what every
    /// message written before this field existed is.
    #[test]
    fn a_message_that_says_nothing_about_its_author_is_the_users() {
        let event: ClientEvent =
            serde_json::from_str(r#"{"type":"user_message","text":"carry on"}"#)
                .expect("deserialize");
        assert_eq!(
            event,
            ClientEvent::UserMessage {
                text: "carry on".to_owned(),
                origin: MessageOrigin::User,
            }
        );
        let command: ControlToDaemon =
            serde_json::from_str(r#"{"type":"user_message","text":"carry on"}"#)
                .expect("deserialize");
        assert_eq!(
            command,
            ControlToDaemon::UserMessage {
                text: "carry on".to_owned(),
                origin: MessageOrigin::User,
            }
        );
    }

    #[test]
    fn a_usage_window_cannot_read_past_full() {
        assert_eq!(
            UsageWindow::new(Some(300), None, 103, None).used_percent,
            100
        );
        assert!(UsageWindow::new(Some(300), None, 103, None).is_exhausted());
        assert!(!UsageWindow::new(Some(300), None, 99, None).is_exhausted());
    }

    #[test]
    fn two_spent_windows_are_waited_out_by_the_one_that_resets_last() {
        // The account is blocked until *both* have turned over, so waking
        // the session at the sooner reset would wake it into the limit it
        // is still inside.
        let five_hour = UsageWindow::new(Some(300), None, 100, Some(1_789_002_000));
        let weekly = UsageWindow::new(Some(10_080), None, 100, Some(1_789_570_800));
        assert_eq!(
            blocking_window(&[five_hour, weekly.clone()]),
            Some(&weekly),
            "the later reset is the one the session comes back after"
        );
    }

    #[test]
    fn a_plan_with_something_left_in_every_window_is_not_blocked() {
        assert!(
            blocking_window(&[
                UsageWindow::new(Some(300), None, 99, Some(1_789_002_000)),
                UsageWindow::new(Some(10_080), None, 40, Some(1_789_570_800)),
            ])
            .is_none()
        );
        assert!(blocking_window(&[]).is_none());
    }

    #[test]
    fn a_spent_window_that_names_no_reset_cannot_be_waited_out() {
        // Nothing to schedule around: a pause with no stated end is a
        // session stopped for ever.
        assert!(blocking_window(&[UsageWindow::new(Some(300), None, 100, None)]).is_none());
        // …and it does not hide a window that *does* name one.
        let weekly = UsageWindow::new(Some(10_080), None, 100, Some(1_789_570_800));
        assert_eq!(
            blocking_window(&[UsageWindow::new(Some(300), None, 100, None), weekly.clone()]),
            Some(&weekly)
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
        // because both outlive the attachment they would otherwise ride.
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
    fn a_stopping_container_names_what_stopped_it() {
        round_trip(&ReportStopping {
            reason: StopReason::Sigterm,
        });
        assert_eq!(
            serde_json::to_string(&ReportStopping {
                reason: StopReason::Sigterm,
            })
            .expect("serialize"),
            r#"{"reason":"sigterm"}"#
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
    fn only_the_state_commands_outlive_the_daemon_they_were_sent_to() {
        // The commands that describe a state rather than an instant: a
        // machine, a model, a permission mode, whether the session may
        // have a screen, a repository owed to the workspace. Everything
        // else replayed into a later turn would be an instruction about
        // something that is no longer happening.
        for frame in every_control_frame() {
            let held = matches!(
                frame,
                ControlToDaemon::MachineChanged { .. }
                    | ControlToDaemon::SetModel { .. }
                    | ControlToDaemon::SetPermissionMode { .. }
                    | ControlToDaemon::SetComputerUse { .. }
                    | ControlToDaemon::AddRepo { .. }
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
                    | ControlToDaemon::ContextUsage
                    | ControlToDaemon::TerminalInput { .. }
                    | ControlToDaemon::TerminalResize { .. }
                    | ControlToDaemon::TerminalHarness { .. }
                    | ControlToDaemon::DesktopTakeover { .. }
                    | ControlToDaemon::DesktopInput { .. }
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
    fn only_an_addressed_reply_is_hidden_from_browsers() {
        for frame in every_daemon_frame() {
            // The one frame nobody watching the session is meant to see:
            // an answer addressed to the one HTTP request that asked for
            // it.
            let hidden = matches!(frame, DaemonToControl::WorkdirReply { .. });
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
