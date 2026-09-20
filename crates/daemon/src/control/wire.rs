//! The daemon's end of the session relay: REST out, SSE in.
//!
//! There is no socket. A daemon holds three routes against its session's
//! room: it *attaches* over REST for an epoch, holds a command *stream*
//! open under that epoch, and *posts* its outbound frames in sequenced
//! batches that carry how far it has applied the command log. This
//! replaces the [REPL](crate::repl) as the way a shipped session is
//! driven; the REPL stays as the dev tool for reproducing a harness bug
//! without a control plane.
//!
//! # Two tasks, one bounded queue
//!
//! The [collector](collect) owns the harness's output stream, turns each
//! [`SessionOutput`] into a wire frame, and pushes it into a bounded queue.
//! The [connection](Connection) owns the command stream and the harness's
//! control handle: it drains the queue outward and dispatches commands
//! inward.
//!
//! They are separate because the room is not always reachable. A reconnect
//! takes seconds; the harness does not stop producing during them, and
//! nothing about a coding session tolerates its transcript being dropped.
//! The queue is what absorbs that gap — and it is *bounded*
//! ([`QUEUE_DEPTH`]), because a queue that grows without limit trades a
//! visible outage for an invisible one that ends in the OOM killer. An
//! overflow is a fatal error, not a dropped frame: losing part of a session
//! silently is worse than stopping.
//!
//! A frame that leaves the queue but whose POST was not confirmed stays in
//! `pending` and is re-sent — under the same epoch while the attach lives,
//! and under a fresh epoch after a reconnect, where the room deduplicates
//! by sequence number. Delivery is therefore **at least once**: a batch
//! whose response was lost after the room stored it produces one
//! duplicate. Exactly-once needs an acknowledgement the wire protocol
//! does not carry yet; until it does, a duplicated frame is the better
//! failure, because the room's stored tail is what a browser replays and
//! a gap in it can never be recovered.
//!
//! # Ordering that the product depends on
//!
//! An approval is recorded over REST *before* its frame is announced. The
//! control plane assigns the id, so the id a browser sees is one the API can
//! settle — and a decision that arrives while the daemon is detached still
//! finds a pending row waiting when it comes back.

use core::time::Duration;
use std::collections::{BTreeMap, VecDeque};

use askama::Template as _;
use flyco_core::wire::{ApprovalPayload, DaemonCommand, DaemonFrames};
use flyco_core::workdir::WorkdirRequest;
use flyco_core::{
    ApprovalDecision, ApprovalId, BudgetSignal, ControlToDaemon, DaemonToControl, HarnessEvent,
    HarnessObservation, ProvisioningStage, RateLimitObservation, SessionId, SessionMachine,
    ShellOutcome, ShellRunId, StopReason, Usd, WorkdirRequestId,
};
use futures_util::StreamExt as _;
use rand::Rng as _;
use tokio::sync::mpsc;

use crate::config::{GithubAccess, RepoConfig, valid_repo_dir};
use crate::control::rest::{CommandStream, ControlApi, ControlApiError, RelayTransport};
use crate::git::{GitError, WorkingSet};
use crate::harness::{HarnessSession, SessionOutput, ToolApproval};
use crate::notice::{
    ApprovalAfterSuspend, BudgetRaised, MachineChanged, MachineLine, OpeningMessage,
    RepoAddedNotice, SessionStart,
};
use crate::shell::{Shell, ShellEvent, ShellRun, ShellUpdate};
use crate::spot::{Disk, Notices, SpotNotice};
use crate::stop::Stops;
use crate::terminal::{TerminalError, TerminalEvent, TerminalSession};
use crate::workdir::Workspace;

/// How many frames may wait for a stream that is not there.
///
/// Sized for a reconnect, not for an outage: at the ~50 frames a second a
/// busy turn produces, this is roughly twenty seconds of disconnection —
/// comfortably past the backoff's first few attempts and far short of a
/// heap that matters. Overflowing it is a fatal error.
pub const QUEUE_DEPTH: usize = 1024;

/// How many answered workdir questions may wait for a stream.
///
/// Small on purpose: a browser asking what is in a directory is waiting on
/// an HTTP request the control plane is holding open, and an answer that
/// queued behind ten others is one nobody is still listening for.
pub const REPLY_DEPTH: usize = 8;

/// How many desktop stream chunks may sit in `pending` during a partition
/// before the next one is dropped.
///
/// The one place "a frame is never dropped" bends: video at even a few
/// frames a second makes an outage buffer a session's worth of footage no
/// watcher will ever replay — a late joiner starts from the newest
/// keyframe regardless. A minute of stream at the configured cadence is
/// the budget; past it the chunk is dropped and the encoder is re-armed
/// to a keyframe, because a hole in a reference chain is garbage until
/// one.
const PENDING_CHUNKS: usize = 300;

/// Shortest wait before a reconnect attempt.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Longest wait between reconnect attempts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How long a frame waits for company before its batch is posted.
///
/// Terminal output and desktop chunks arrive as a stream of small pieces,
/// and posting each as it lands is one request per piece — at a busy
/// harness's rate, the traffic that spends the account's daily request
/// quota (issue #342). Half a second bounds a session to two batches a
/// second however much it produces, and is under what a person notices
/// on a terminal.
pub const COALESCE: Duration = Duration::from_millis(500);

/// How many frames a batch holds before it is posted without waiting out
/// [`COALESCE`]: the bound on a batch's size, so a burst cannot grow one
/// past what the room accepts in a body.
pub const FLUSH_AT_FRAMES: usize = 128;

/// How long the command stream may deliver no bytes at all — not even a
/// ping — before the daemon calls the path dead.
///
/// The room comments `ping` down the stream every fifteen seconds, so this
/// is six missed heartbeats: a flow with no packets on it is what a cloud
/// NAT reclaims, and it drops the flow without a FIN, so neither end
/// learns the connection is gone. Abandoning is always safe — the frames
/// waiting are held, the loop re-attaches, and the room replays the
/// mailbox — but abandoning on a hiccup would re-attach a working session
/// all day, so the budget is misses rather than one late ping.
pub const STREAM_SILENCE_LIMIT: Duration = Duration::from_secs(90);

/// How long an attachment must hold before the reconnect ladder resets.
///
/// A stream ended moments after its attach is not a dropped connection —
/// it is one the room never really had, most often because a peer
/// daemon's attach superseded it on the way in. Counting those deaths
/// with the failed attaches is what makes the backoff ladder reach the
/// ping-pong at all: without it a stream killed the instant it opened
/// re-attaches at line rate for ever (issue #336). An attachment that
/// outlived this was established, and its loss resets the ladder rather
/// than climbing it — a real drop deserves a prompt reconnect.
pub(crate) const ATTACH_STABLE: Duration = Duration::from_secs(10);

/// How long one command may spend inside the harness before the session is
/// treated as wedged.
///
/// Every command the room sends resolves to a channel send and an
/// acknowledgement — milliseconds when the agent process is healthy. A
/// minute is therefore not a budget, it is a diagnosis: the harness has
/// stopped reading, and nothing about waiting longer will change that.
///
/// It exists because the pump awaits the harness *inside* its own loop, so
/// a command that never returns takes the stream read down with it: the
/// daemon stops answering, stops re-attaching, and stops being able to say
/// why — which is precisely the silence issue #201 describes.
pub const HARNESS_DEADLINE: Duration = Duration::from_secs(60);

/// When nothing-happening becomes something-is-wrong.
///
/// How long the command stream may be silent before it is re-attached, and
/// how long one command may spend inside the harness before the session is
/// called wedged. One struct rather than two constants because the two
/// answer the same question — how long may nothing happen before flyco
/// calls it broken — and because a test that wants a brisk relay wants
/// both brisk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    /// How long the command stream may be byte-silent before it is
    /// abandoned and re-attached.
    silence_limit: Duration,
    /// How long one command may spend inside the harness.
    harness_deadline: Duration,
}

impl Deadlines {
    /// Deadlines that abandon a stream silent for `silence_limit`.
    #[must_use]
    pub const fn silent(silence_limit: Duration) -> Self {
        Self {
            silence_limit,
            harness_deadline: HARNESS_DEADLINE,
        }
    }

    /// The same deadlines with a different harness deadline.
    #[must_use]
    pub const fn waiting_on_the_harness(self, harness_deadline: Duration) -> Self {
        Self {
            harness_deadline,
            ..self
        }
    }

    /// How long silence may last before the stream is presumed dead.
    #[must_use]
    pub const fn silence_limit(self) -> Duration {
        self.silence_limit
    }

    /// How long one command may spend inside the harness.
    ///
    /// Kept beside the silence limit because it answers the same question
    /// and because a test that wants a brisk relay wants all of them
    /// brisk.
    #[must_use]
    pub const fn harness_deadline(self) -> Duration {
        self.harness_deadline
    }
}

impl Default for Deadlines {
    fn default() -> Self {
        Self::silent(STREAM_SILENCE_LIMIT)
    }
}

/// What the harness is told when a budget threshold is crossed.
///
/// The agent decides how to spend its budget, so a threshold is information
/// it acts on rather than a limit imposed on it — it arrives as a message in
/// the conversation, marked so the model can tell it from the user.
/// [`BudgetSignal::Pause`] is the exception: that one is enforced, not
/// announced.
const BUDGET_NOTICE_PREFIX: &str = "[flyco budget notice]";

/// What the agent is told when a turn finishes on a dirty tree.
const DIRTY_NOTICE: &str = "[flyco repo notice] the working tree has uncommitted changes. Commit them before considering this task complete. Flyco keeps the session awake while it is dirty, unless the compute budget is exhausted.";

/// The daemon could not keep its end of the relay.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The configured control-plane URL is not an endpoint the daemon can
    /// reach.
    #[error("the control-plane URL cannot address the session relay: {0}")]
    Unaddressable(String),
    /// The relay carried a frame this protocol version does not define.
    #[error("the control plane sent a frame this daemon cannot read: {0}")]
    Undecodable(String),
    /// The control plane refused this daemon's attach, and retrying
    /// cannot help: the token is wrong or the build speaks another
    /// protocol version.
    #[error("the control plane would not attach this daemon: {0}")]
    Unwelcome(String),
    /// The harness stopped accepting commands.
    #[error("the harness session stopped: {0}")]
    Harness(String),
    /// The outbound queue overflowed while the stream was down.
    ///
    /// Fast fail: the alternative is an unbounded queue that turns a
    /// reconnect into an out-of-memory kill, or a silent drop that loses
    /// part of a session's transcript.
    #[error(
        "the relay queue overflowed after {QUEUE_DEPTH} frames with no connection; \
         the session's stream would have been truncated"
    )]
    QueueOverflow,
    /// A REST call the relay depends on failed.
    #[error(transparent)]
    ControlApi(#[from] ControlApiError),
    /// The web terminal failed.
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    /// The session checkout could not be read or snapshotted.
    #[error(transparent)]
    Git(#[from] GitError),
    /// A handoff's transcript could not be written where its prompt says
    /// it is.
    #[error("the handoff's transcript could not be materialized: {0}")]
    Handoff(String),
    /// A notice the agent has to read could not be rendered.
    ///
    /// This daemon's own bug rather than anything the control plane did:
    /// the templates are compiled in. Fatal, because the notices are how an
    /// agent learns its machine restarted — a session that silently stopped
    /// saying so would look fine and behave wrongly.
    #[error("a flyco notice could not be rendered: {0}")]
    Notice(String),
}

/// A template that would not render, which is a bug in this binary.
fn notice_failed(error: &askama::Error) -> WireError {
    WireError::Notice(error.to_string())
}

/// One attach to the session room: the epoch naming it, the command
/// stream opened under it, and the sequence the next outbound frame takes.
///
/// Sequence numbers are per-epoch: a daemon numbers from 1 on every
/// attach and the room deduplicates within the epoch, so an
/// acknowledgement lost on the wire resolves to a resend rather than a
/// hole.
struct Attachment {
    /// The attach this stream belongs to.
    epoch: u64,
    /// Commands from the room, in the order it sequenced them.
    commands: CommandStream<DaemonCommand>,
    /// The sequence `pending`'s head will carry.
    next_seq: u64,
}

/// The host clock, in seconds since the Unix epoch.
///
/// Panics on a clock set before 1970, which is not a state a session VM can
/// be in and not one this daemon could do anything sensible about.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the host clock is set before the Unix epoch")
        .as_secs()
}

/// Waits before the `attempt`-th reconnect.
///
/// Capped exponential with full jitter: the cap keeps a long outage from
/// turning into an hour-long silence, and the jitter keeps every daemon on a
/// restarted control plane from reconnecting in the same instant. Shared
/// with the host relay, which reconnects to a different room for the same
/// reasons.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let ceiling = BACKOFF_MIN
        .saturating_mul(1_u32 << attempt.min(6))
        .min(BACKOFF_MAX);
    let jittered = rand::rng().random_range(0..=ceiling.as_millis().max(1));
    Duration::from_millis(u64::try_from(jittered).unwrap_or(u64::MAX)).max(Duration::from_millis(1))
}

/// What a harness event tells the LLM usage panel, if anything.
///
/// Two of them carry a fact nothing else in flyco can obtain: a finished
/// turn knows what the harness said it cost, and a usage limit is only ever
/// announced, never queryable. Everything else observes nothing.
fn observation_in(event: &HarnessEvent) -> Option<HarnessObservation> {
    match event {
        HarnessEvent::TurnCompleted { usage, .. } => {
            usage.estimated_cost.map(|cost| HarnessObservation {
                observed_cost: Some(cost),
                rate_limit: None,
            })
        }
        HarnessEvent::UsageLimited { window } => Some(HarnessObservation {
            observed_cost: None,
            rate_limit: Some(RateLimitObservation {
                // The panel records *when* the account was last limited, and
                // an instant before the epoch is not one: a harness that
                // named no reset, or named one this build cannot represent,
                // records the limit with no reset rather than a wrong one.
                resets_at_unix: window
                    .resets_at_unix
                    .and_then(|resets| u64::try_from(resets).ok()),
            }),
        }),
        _ => None,
    }
}

/// Turns the harness's output stream into wire frames.
///
/// Runs until the harness stops. An approval is recorded over REST before
/// its frame is queued, so the id the room announces is one the control
/// plane can settle.
///
/// A usage observation is recorded the same way but is *not* allowed to
/// fail the session: it is telemetry for a panel, and a turn that ran must
/// not be lost because a number about it could not be filed. The refusal it
/// meets most often is the honest one — a session running on inherited
/// developer credentials has no linked account to attribute anything to —
/// which is why this is `debug` rather than a warning.
/// One outbound frame, plus the harness-native approval id when the frame
/// is an approval request.
///
/// The room announces the REST-assigned id. The harness still keys its
/// pending call on the id it minted, so the connection has to remember the
/// pairing until the user decides.
/// What the collector hands the pump.
enum Outbound {
    /// A frame for the room, paired with the harness's own id when the
    /// frame is an approval request the harness is waiting on.
    Frame {
        frame: DaemonToControl,
        harness_approval: Option<ApprovalId>,
    },
    /// A tool call the user already approved — before a suspension took
    /// the process that asked (issue #355) — answered on the machine and
    /// never raised again.
    Preapproved { harness_approval: ApprovalId },
}

/// Queues one item for the pump.
fn enqueue(queue: &mpsc::Sender<Outbound>, outbound: Outbound) -> Result<(), WireError> {
    queue.try_send(outbound).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => WireError::QueueOverflow,
        mpsc::error::TrySendError::Closed(_) => {
            WireError::Harness("the relay connection stopped".to_owned())
        }
    })
}

async fn collect<A: ControlApi>(
    mut outputs: mpsc::Receiver<SessionOutput>,
    queue: mpsc::Sender<Outbound>,
    api: A,
    mut standing: mpsc::UnboundedReceiver<ApprovalPayload>,
) -> Result<(), WireError> {
    // Tool calls the user approved after the machine that asked was
    // suspended. Each entry answers one identical request without raising
    // it, which is what keeps the user from being asked twice for the
    // same call; owned here because this is where a request becomes a
    // raise, and fed by the pump, which is where decisions arrive.
    let mut preapproved: Vec<ApprovalPayload> = Vec::new();
    while let Some(output) = outputs.recv().await {
        let (frame, harness_approval) = match output {
            SessionOutput::Started { session_id } => {
                api.record_harness_session(&session_id).await?;
                (
                    DaemonToControl::Started {
                        harness_session_id: session_id,
                    },
                    None,
                )
            }
            SessionOutput::Capabilities { capabilities } => {
                (DaemonToControl::Capabilities { capabilities }, None)
            }
            SessionOutput::Commands { commands } => {
                // A relay frame rather than a REST report, which is where
                // this parts company with the model list beneath it: the
                // command set includes the checkout's own skills, so it
                // belongs to this session and there is nothing to file
                // against the account.
                (DaemonToControl::Commands { commands }, None)
            }
            SessionOutput::Models { models } => {
                // Filed over REST and never queued as a relay frame: the
                // list is recorded against the account, which is D1, and a
                // session room is a Durable Object that cannot reach it.
                // The control plane announces it to the browsers itself.
                api.report_models(&models).await?;
                continue;
            }
            SessionOutput::PlanUsage { windows } => {
                // Filed over REST for the same reason the model list is:
                // the snapshot belongs to the account, which is D1. The
                // control plane broadcasts it to the browsers itself.
                api.report_usage(&windows).await?;
                continue;
            }
            SessionOutput::Event { event } => {
                match &event {
                    HarnessEvent::TurnStarted { .. } => api.notify_turn_started().await?,
                    HarnessEvent::TurnCompleted { .. } => api.notify_turn_completed().await?,
                    HarnessEvent::TurnFailed { .. } => api.notify_turn_failed().await?,
                    _ => {}
                }
                if let Some(observation) = observation_in(&event)
                    && let Err(error) = api.record_observation(observation).await
                {
                    tracing::debug!(%error, "a usage observation was not recorded");
                }
                // The one report that stops the session rather than
                // describing it, and unlike the observation above it is not
                // telemetry: without it the machine goes on costing money for
                // however long the window takes to turn over, so a control
                // plane that would not take it ends the connection and the
                // daemon reports the limit again on the next one.
                if let HarnessEvent::UsageLimited { window } = &event
                    && window.resets_at_unix.is_some()
                {
                    api.report_usage_limit(window).await?;
                }
                (DaemonToControl::Harness { event }, None)
            }
            SessionOutput::ApprovalRequest {
                id: harness_id,
                tool,
                input,
                ..
            } => {
                let payload = ApprovalPayload::ToolUse { tool, input };
                while let Ok(granted) = standing.try_recv() {
                    preapproved.push(granted);
                }
                if let Some(at) = preapproved.iter().position(|granted| *granted == payload) {
                    preapproved.swap_remove(at);
                    tracing::info!("answering a tool call the user approved before the suspension");
                    enqueue(
                        &queue,
                        Outbound::Preapproved {
                            harness_approval: harness_id,
                        },
                    )?;
                    continue;
                }
                let id = api.raise_approval(payload.clone()).await?;
                (
                    DaemonToControl::ApprovalRequest { id, payload },
                    Some(harness_id),
                )
            }
            SessionOutput::Fatal { error } => {
                // The harness is done, and this is the only account of why.
                // Reported over REST rather than queued as a frame: the
                // queue is drained by the pump, and the pump is about to
                // stop — a frame put there now is a frame nobody sends.
                //
                // Best effort. A session whose agent died and whose report
                // also failed is one the control plane must still stop
                // waiting on, and the stall sweep is what does that; an
                // error here would only replace one silence with another.
                tracing::error!(error, "the harness session ended fatally");
                if let Err(refused) = api.report_startup_failure(error).await {
                    tracing::error!(%refused, "the control plane did not take the failure report");
                }
                return Ok(());
            }
        };

        enqueue(
            &queue,
            Outbound::Frame {
                frame,
                harness_approval,
            },
        )?;
    }
    Ok(())
}

/// The room-facing half of the relay.
/// Whether the checkout currently has uncommitted work, and whether the
/// agent has been told this episode.
enum Tree {
    /// `git status --short` was empty, or nobody has looked yet.
    Clean,
    /// Uncommitted work is present.
    Dirty {
        /// Whether this episode has already been announced to the agent.
        noticed: bool,
    },
}

/// Whether one of the streams the pump selects on can still produce.
///
/// Named rather than a `bool` because four of them sit side by side in
/// [`Alive`], and a row of `true, true, false, true` at a construction site
/// says nothing about which stream is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Producing {
    /// Something may still arrive on it.
    Yes,
    /// Its producer is gone; the arm must be disabled.
    No,
}

impl Producing {
    /// Whether the `select!` arm guarded by this may run.
    const fn armed(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Which of the streams the pump selects on can still produce something.
///
/// A `select!` arm whose channel has closed is ready *immediately*, for
/// ever, so an arm that is not disabled once its producer is gone turns the
/// pump into a busy loop. Grouped rather than four fields on the connection
/// because they are one question asked four times, and the answers are read
/// together every time round the loop.
#[derive(Debug, Clone, Copy)]
struct Alive {
    /// Whether the harness is still producing output.
    harness: Producing,
    /// Whether the working-tree watcher is still producing summaries.
    repo_watch: Producing,
    /// Whether the desktop supervisor is still reporting.
    desktop: Producing,
    /// Whether an eviction watcher is still there to announce anything.
    ///
    /// [`Producing::No`] from the start on a machine nobody can reclaim,
    /// and again once a notice has arrived: a reclamation happens to a
    /// machine exactly once.
    spot_watch: Producing,
    /// Whether a stop signal can still arrive.
    ///
    /// [`Producing::No`] from the start on a machine whose disk survives a
    /// stop, and again once one has arrived, for the same reason as
    /// [`Self::spot_watch`]: a platform stops an execution exactly once.
    stop_watch: Producing,
}

struct Connection<S, T, A, W, D, H, X> {
    session: S,
    terminal: T,
    terminal_out: mpsc::Receiver<TerminalEvent>,
    /// The session's desktop, when the machine provides one.
    ///
    /// Present on every connection — the handle costs an idle thread for
    /// a session without a screen — and every desktop command lands on it
    /// whether or not the stack it supervises is up.
    desktop: X,
    /// What the desktop reports: lifecycle, stream chunks, the agent
    /// announcing itself on the screen.
    desktop_out: mpsc::Receiver<crate::desktop::DesktopEvent>,
    /// The session's harness as a terminal application — how a
    /// [`ControlToDaemon::TerminalHarness`] is launched.
    tui: crate::tui::HarnessTui,
    /// Runs the composer's `!` commands (docs/ux.md §9.3).
    shell: H,
    /// Where a running command's output and its exit arrive.
    ///
    /// Unbounded, and bounded all the same: one command runs at a time and
    /// [`crate::shell`] caps how much of it reaches the transcript, so the
    /// queue behind this cannot outgrow one run's output cap. That is what
    /// lets a refusal be written into it from this same task without the
    /// deadlock a full bounded channel would be.
    shell_updates: mpsc::UnboundedReceiver<ShellUpdate>,
    /// The other end of [`Self::shell_updates`], handed to each run.
    shell_reports: mpsc::UnboundedSender<ShellUpdate>,
    /// The one `!` command in flight, and the run it is.
    ///
    /// One at a time: a second command is refused rather than queued behind
    /// one that may never end, and Stop has exactly one thing to cancel.
    /// The id is kept beside the handle because a cancelled command is not
    /// over until it says so — its exit must not clear a slot that has
    /// since been taken by the next command.
    running_shell: Option<(ShellRunId, ShellRun)>,
    api: A,
    /// Every checkout in the workspace: snapshots, patch replay, and the
    /// `git status` summaries that keep the session awake while dirty.
    repos: W,
    /// The read-only view of the workspace the `Files` and `Diff` tabs are
    /// served from.
    workspace: Workspace,
    /// GitHub credentials for a mid-session `AddRepo` clone, or `None` on
    /// a developer machine.
    github: Option<GithubAccess>,
    /// Where an answered workdir question is handed back to the pump.
    ///
    /// Its own channel rather than the harness's outbound queue, because
    /// that queue closing is how the pump learns the harness stopped — a
    /// second sender kept for replies would hold it open for ever.
    reply_out: mpsc::Sender<DaemonToControl>,
    replies: mpsc::Receiver<DaemonToControl>,
    disk: D,
    /// Eviction notices from the provider's metadata endpoint.
    spot: Notices,
    /// The platform's stop signal, on a machine with no disk.
    stops: Stops,
    /// Which of the streams the pump selects on are still producing.
    alive: Alive,
    /// Whether this machine's capacity has been announced as going away.
    ///
    /// Set once and never cleared, unlike [`Self::paused`], which a raised
    /// budget lifts: what ends a reclamation is the machine stopping, not
    /// the daemon deciding it is over and not a decision anybody can make
    /// for it. While it is set no new turn may
    /// start — the transcript has already been flushed, and a turn opened
    /// after it would be work the next machine has no record of.
    reclaiming: bool,
    /// The harness-native session id, once the harness has announced one.
    ///
    /// Kept so a reclamation can re-file it and *know* it landed: the
    /// collector already records it when it arrives, but that write happened
    /// minutes ago and on an attachment that may since have dropped, and a
    /// replacement machine with no id to resume opens a new conversation
    /// instead of continuing this one.
    harness_session_id: Option<String>,
    /// The session this daemon serves — for logging; the transport itself
    /// is bound to it inside `api`.
    session_id: SessionId,
    /// Whether a budget pause has stopped this session accepting work.
    ///
    /// Cleared by exactly one thing, and never by the daemon's own
    /// judgement: [`ControlToDaemon::BudgetRaised`], which is the control
    /// plane reporting that the user gave the session more money than it
    /// has spent. What ends a budget pause is a decision, not a timeout.
    paused: bool,
    /// Per-checkout dirty state, keyed by checkout directory (`None` for
    /// the developer-machine root checkout).
    trees: BTreeMap<Option<String>, Tree>,
    /// REST-assigned approval id → harness-native id.
    approvals: BTreeMap<ApprovalId, ApprovalId>,
    /// Where an approval the user granted after a suspension goes: to the
    /// collector, which answers the agent's next identical request itself.
    standing: mpsc::UnboundedSender<ApprovalPayload>,
    /// The machine notice waiting to ride on the session's first message.
    ///
    /// Taken once. Everything after the first message is a conversation the
    /// agent is already in, and repeating what machine it is on would be
    /// noise it has to read every turn — `machine_status` is there for when
    /// it wants to know.
    opening: Option<String>,
    /// How this connection decides the stream is dead.
    deadlines: Deadlines,
    /// The wait the control plane named on the last refused batch.
    ///
    /// A `429` on the frames route carries `Retry-After`; the attach that
    /// follows a dropped attachment sleeps at least that long, whatever
    /// the backoff ladder says (issue #342). Taken by the reconnect loop,
    /// so it applies to exactly the reconnect after the refusal.
    retry_hint: Option<Duration>,
}

/// Why one connection ended.
enum Ended {
    /// The stream dropped; re-attach.
    Disconnected,
    /// A newer attach superseded this one; stop.
    ///
    /// Distinct from [`Ended::Disconnected`] because of what the stream's
    /// end means: another daemon holds this session's room, and
    /// re-attaching would end *its* stream in turn — which is exactly the
    /// ping-pong the room ended this stream over (issue #336). The unit
    /// exits `0` so `Restart=on-failure` leaves the spare stopped.
    Superseded,
    /// The control plane archived the session; stop.
    Archived,
    /// The queue ended because the harness stopped.
    HarnessStopped,
    /// The platform stopped this machine and the session has been saved.
    ///
    /// Distinct from every other ending because of what follows it: the
    /// process exits `0`. A container that died with a failure status would
    /// be an execution the platform records as failed, and this one did
    /// exactly what it was asked to.
    Stopped,
}

impl<
    S: HarnessSession,
    T: TerminalSession,
    A: ControlApi + RelayTransport,
    W: WorkingSet,
    D: Disk,
    H: Shell,
    X: crate::desktop::DesktopSession,
> Connection<S, T, A, W, D, H, X>
{
    /// Posts every unconfirmed frame as one sequenced batch.
    ///
    /// `pending` holds the frames this attach — or a superseded one —
    /// has not yet had confirmed stored. A batch that fails leaves it
    /// untouched and answers `false`, which is the caller's instruction
    /// to drop the attachment and dial again: an unconfirmed frame is
    /// never dropped on the way out, because the room's stored tail is
    /// what a browser replays and a hole in it can never be recovered.
    ///
    /// `applied` is the highest command sequence the daemon has acted on;
    /// it rides every batch, which is how the room learns which of its
    /// queued commands are done.
    async fn flush(
        &mut self,
        attach: &mut Attachment,
        pending: &mut VecDeque<DaemonToControl>,
        applied: u64,
    ) -> bool {
        if pending.is_empty() {
            return true;
        }
        let batch = DaemonFrames {
            epoch: attach.epoch,
            from_seq: attach.next_seq,
            ack_through: applied,
            frames: pending.iter().cloned().collect(),
        };
        if let Err(error) = self.api.frames(&batch).await {
            tracing::warn!(
                %error,
                frames = batch.frames.len(),
                "a frame batch did not reach the room; retrying it on the next attach"
            );
            self.retry_hint = error.retry_after();
            return false;
        }
        attach.next_seq = attach
            .next_seq
            .saturating_add(u64::try_from(pending.len()).unwrap_or(0));
        pending.clear();
        true
    }

    /// Acknowledges commands without waiting for a frame to carry it.
    ///
    /// An empty batch is still a contact — it renews presence — and its
    /// `ack_through` is how the room learns a command it delivered is
    /// done. Without it, a command applied on a quiet session would sit
    /// unacknowledged until the harness next produced output, and an
    /// attach in between would redeliver it.
    async fn ack(&mut self, epoch: u64, from_seq: u64, applied: u64) -> bool {
        let batch = DaemonFrames {
            epoch,
            from_seq,
            ack_through: applied,
            frames: Vec::new(),
        };
        if let Err(error) = self.api.frames(&batch).await {
            tracing::warn!(%error, "a command acknowledgement did not reach the room");
            self.retry_hint = error.retry_after();
            return false;
        }
        true
    }

    /// Posts what one loop turn owes the room: the pending batch once its
    /// window has closed or it is full, else a bare acknowledgement when
    /// the command cursor moved.
    ///
    /// Frames first, then the bare ack: a pending batch carries
    /// `ack_through` itself, so sending both would ack twice. `flush_at`
    /// is the coalescing window, armed by the first frame to land in an
    /// empty queue and cleared by the post. Answers whether the attachment
    /// is still usable.
    async fn settle(
        &mut self,
        attach: &mut Attachment,
        pending: &mut VecDeque<DaemonToControl>,
        applied: u64,
        acked: &mut u64,
        flush_at: &mut Option<tokio::time::Instant>,
    ) -> bool {
        if pending.is_empty() {
            *flush_at = None;
            if applied > *acked {
                if !self.ack(attach.epoch, attach.next_seq, applied).await {
                    return false;
                }
                *acked = applied;
            }
            return true;
        }
        let due = *flush_at.get_or_insert_with(|| tokio::time::Instant::now() + COALESCE);
        if pending.len() >= FLUSH_AT_FRAMES || tokio::time::Instant::now() >= due {
            if !self.flush(attach, pending, applied).await {
                return false;
            }
            *acked = applied;
            *flush_at = None;
        }
        true
    }

    /// Pumps one attachment until it ends.
    ///
    /// `pending` holds frames produced but not yet confirmed stored —
    /// across attachments: a batch refused or unanswered keeps them, and
    /// the next epoch re-sends them from sequence one.
    ///
    /// `applied` is the room's command-log cursor and likewise outlives
    /// the attach: command sequences are global rather than per-epoch, so
    /// a command whose acknowledgement was lost is redelivered on the next
    /// stream and must be recognized rather than applied a second time.
    async fn pump(
        &mut self,
        attach: &mut Attachment,
        queue: &mut mpsc::Receiver<Outbound>,
        pending: &mut VecDeque<DaemonToControl>,
        applied: &mut u64,
    ) -> Result<Ended, WireError> {
        // How far the room has been told the log is applied. It lags
        // `applied` by at most one loop turn: an ack goes out before the
        // next command is waited on. Starting at zero on each attach makes
        // the first turn re-acknowledge everything — which is exactly the
        // batch a room holding unacknowledged rows needs to see.
        let mut acked = 0_u64;
        // When the frames waiting in `pending` are posted: armed by the
        // first frame to land in an empty queue, so a burst — a terminal
        // printing, a desktop streaming — rides one batch per
        // [`COALESCE`] rather than one per piece.
        let mut flush_at: Option<tokio::time::Instant> = None;

        loop {
            if !self
                .settle(attach, pending, *applied, &mut acked, &mut flush_at)
                .await
            {
                return Ok(Ended::Disconnected);
            }
            // A disabled branch's expression is still evaluated, so the
            // deadline is a real instant either way.
            let flush_deadline = flush_at.unwrap_or_else(tokio::time::Instant::now);

            tokio::select! {
                () = tokio::time::sleep_until(flush_deadline), if flush_at.is_some() => {
                    // The window closed: the loop head posts the batch.
                }
                outbound = queue.recv(), if self.alive.harness.armed() => {
                    if let Some(ending) = self.on_outbound(outbound, pending).await? {
                        return Ok(ending);
                    }
                    // A busy harness fills the queue faster than one
                    // frame per turn; draining what is already there is
                    // what makes a burst one POST rather than one each.
                    while let Ok(outbound) = queue.try_recv() {
                        if let Some(ending) = self.on_outbound(Some(outbound), pending).await? {
                            return Ok(ending);
                        }
                    }
                }
                event = attach.commands.next() => {
                    let Some(command) = event else {
                        return Ok(Ended::Disconnected);
                    };
                    let command = match command {
                        Ok(command) => command,
                        Err(error) => {
                            tracing::warn!(%error, "the command stream failed");
                            return Ok(Ended::Disconnected);
                        }
                    };
                    if let Some(seq) = command.seq {
                        if seq <= *applied {
                            // An acknowledgement that never reached the
                            // room redelivers the command on the next
                            // stream. Applied is applied: running it again
                            // would put a second copy of a user message in
                            // the conversation.
                            continue;
                        }
                        *applied = seq;
                    }
                    match self.dispatch_before(command.command).await? {
                        Ended::Disconnected => {}
                        ended => return Ok(ended),
                    }
                }
                output = self.terminal_out.recv() => {
                    match output {
                        Some(TerminalEvent::Output(data)) => {
                            pending.push_back(DaemonToControl::TerminalOutput { data });
                        }
                        Some(TerminalEvent::Exited { code }) => {
                            pending.push_back(DaemonToControl::TerminalExited { code });
                        }
                        None => return Ok(Ended::Disconnected),
                    }
                }
                update = self.shell_updates.recv() => {
                    // This connection holds a sender of its own, so the
                    // channel outlives every run and never closes.
                    let update = update.expect("the relay holds the shell's own sender");
                    let frame = self.shell_frame(update);
                    pending.push_back(frame);
                }
                report = self.desktop_out.recv(), if self.alive.desktop.armed() => {
                    self.on_desktop_report(report, pending);
                }
                notice = self.spot.recv(), if self.alive.spot_watch.armed() => {
                    self.alive.spot_watch = Producing::No;
                    let Some(notice) = notice else {
                        // No watcher: this machine holds capacity nobody
                        // can reclaim.
                        continue;
                    };
                    if let Err(error) = self.reclaim(attach, queue, pending, *applied, notice).await {
                        // Whatever failed, the machine is still going. The
                        // relay keeps its attach rather than tearing down
                        // over an error it cannot act on.
                        tracing::error!(%error, "the reclamation sequence did not complete");
                    }
                }
                reason = self.stops.recv(), if self.alive.stop_watch.armed() => {
                    self.alive.stop_watch = Producing::No;
                    let Some(reason) = reason else {
                        // Nothing watching: this machine's filesystem
                        // outlives a stop, so a signal needs no sequence.
                        continue;
                    };
                    self.on_stop(attach, queue, pending, *applied, reason).await;
                    return Ok(Ended::Stopped);
                }
                reply = self.replies.recv() => {
                    let Some(frame) = reply else {
                        // Unreachable: the connection owns a sender for the
                        // life of the pump. Treated as a disconnect rather
                        // than ignored, so a channel that somehow closed
                        // cannot spin this loop.
                        return Ok(Ended::Disconnected);
                    };
                    pending.push_back(frame);
                }
                status = self.repos.next_status(), if self.alive.repo_watch.armed() => {
                    let Some((dir, summary)) = status else {
                        self.alive.repo_watch = Producing::No;
                        continue;
                    };
                    self.note_tree(dir.as_ref(), &summary);
                    pending.push_back(DaemonToControl::RepoDirty { dir, summary });
                }
            }
        }
    }

    /// Queues whatever the desktop reported as the frame it is.
    ///
    /// A closed channel means the supervisor thread is gone — the desktop
    /// it owned is gone with it, and the room hears `Failed` rather than
    /// silence about a screen the user may be watching.
    fn on_desktop_report(
        &mut self,
        report: Option<crate::desktop::DesktopEvent>,
        pending: &mut VecDeque<DaemonToControl>,
    ) {
        match report {
            Some(crate::desktop::DesktopEvent::State { status, detail }) => {
                pending.push_back(DaemonToControl::DesktopState { status, detail });
            }
            Some(crate::desktop::DesktopEvent::AgentActive) => {
                pending.push_back(DaemonToControl::DesktopActive);
            }
            Some(crate::desktop::DesktopEvent::Chunk { keyframe, bytes }) => {
                // Chunks are the one frame allowed to fall out of
                // `pending`: a partition that outlasts the cap would
                // otherwise buffer minutes of video nobody joined late
                // enough to see, and the tail a re-attach sends is
                // bounded by design, not by accident.
                let buffered = pending
                    .iter()
                    .filter(|frame| matches!(frame, DaemonToControl::DesktopChunk { .. }))
                    .count();
                if buffered < PENDING_CHUNKS {
                    pending.push_back(DaemonToControl::DesktopChunk {
                        keyframe,
                        data: bytes,
                    });
                } else {
                    // A dropped inter-frame breaks the reference chain,
                    // so the next encode is made one a decoder can start
                    // from.
                    let _ = self.desktop.keyframe_now();
                }
            }
            None => {
                self.alive.desktop = Producing::No;
                pending.push_back(DaemonToControl::DesktopState {
                    status: flyco_core::wire::DesktopStatus::Failed,
                    detail: Some("the desktop supervisor stopped".to_owned()),
                });
            }
        }
    }

    /// Records what one checkout's working tree looks like now.
    ///
    /// An episode of dirtiness is one episode: a tree that was already
    /// dirty and has been mentioned to the agent stays mentioned, so the
    /// nudge of [`Self::nudge_if_dirty`] is not repeated on every status
    /// poll while the agent works.
    fn note_tree(&mut self, dir: Option<&String>, summary: &str) {
        let dir = dir.cloned();
        let noticed = matches!(self.trees.get(&dir), Some(Tree::Dirty { noticed: true }));
        self.trees.insert(
            dir,
            if summary.trim().is_empty() {
                Tree::Clean
            } else {
                Tree::Dirty { noticed }
            },
        );
    }

    /// Runs the stop sequence under the platform's own clock.
    ///
    /// Bounded by flyco rather than by the `SIGKILL` that follows: a
    /// sequence that runs past the grace period is cut off here, with a log
    /// line saying so, instead of vanishing mid-write with no record of how
    /// far it got. Nothing is returned, because nothing the caller could do
    /// differs — whatever came of it, the machine is going.
    async fn on_stop(
        &mut self,
        attach: &mut Attachment,
        queue: &mut mpsc::Receiver<Outbound>,
        pending: &mut VecDeque<DaemonToControl>,
        applied: u64,
        reason: StopReason,
    ) {
        match tokio::time::timeout(
            crate::stop::GRACE,
            self.stopping(attach, queue, pending, applied, reason),
        )
        .await
        {
            Ok(Ok(())) => tracing::info!("the session was saved; stopping"),
            Ok(Err(error)) => tracing::error!(%error, "the stop sequence did not complete"),
            Err(_) => tracing::error!(
                grace = ?crate::stop::GRACE,
                "the stop sequence outlasted the platform's grace period"
            ),
        }
    }

    /// Takes one outbound frame's bookkeeping and queues it for the room.
    ///
    /// The frame is *queued*, not sent — the pump's next turn posts every
    /// pending frame as one batch, which is what makes a burst one POST
    /// rather than one per frame. `Ok(None)` is the ordinary case;
    /// `Ok(Some(_))` ends the attachment.
    async fn on_outbound(
        &mut self,
        outbound: Option<Outbound>,
        pending: &mut VecDeque<DaemonToControl>,
    ) -> Result<Option<Ended>, WireError> {
        let Some(outbound) = outbound else {
            self.alive.harness = Producing::No;
            if self
                .trees
                .values()
                .any(|tree| matches!(tree, Tree::Dirty { .. }))
                && !self.paused
            {
                tracing::info!("the harness stopped on a dirty tree; keeping the session awake");
                return Ok(None);
            }
            return Ok(Some(Ended::HarnessStopped));
        };

        let (frame, harness_approval) = match outbound {
            Outbound::Frame {
                frame,
                harness_approval,
            } => (frame, harness_approval),
            Outbound::Preapproved { harness_approval } => {
                self.session
                    .decide_approval(ToolApproval::Allow {
                        id: harness_approval,
                        updated_input: None,
                    })
                    .await
                    .map_err(harness)?;
                return Ok(None);
            }
        };
        if let Some(harness_id) = harness_approval {
            let DaemonToControl::ApprovalRequest { id, .. } = &frame else {
                return Err(WireError::Harness(
                    "an approval pairing was attached to a non-approval frame".to_owned(),
                ));
            };
            self.approvals.insert(*id, harness_id);
        }
        self.remember(&frame);
        let completed_dirty = matches!(
            &frame,
            DaemonToControl::Harness {
                event: HarnessEvent::TurnCompleted { .. },
            }
        );
        pending.push_back(frame);
        if completed_dirty {
            self.nudge_if_dirty().await?;
        }
        Ok(None)
    }

    /// Spends the seconds between an eviction notice and the machine going.
    ///
    /// Four steps, in this order, and the order *is* the feature — each one
    /// is only correct because the one before it has finished:
    ///
    /// 1. and 2. [Quiesce](Self::quiesce): interrupt the turn, then flush
    ///    the transcript batches and the harness-native session id.
    /// 3. **`sync`.** The disk outlives the machine on every provider flyco
    ///    provisions spot on, so what is still in the page cache is the only
    ///    part of the working tree that a reclamation could lose. This is
    ///    the step that has no counterpart in [`stopping`](Self::stopping),
    ///    where there is no disk to flush the cache onto.
    /// 4. **Report.** The durable half over REST — which marks the session
    ///    interrupted and queues its replacement — and then the relay frame
    ///    that puts the countdown in front of the user.
    ///
    /// The attach is then kept until the machine dies. There is
    /// nothing left to post on it and no reason to drop it: a daemon that
    /// detached cleanly would look like one that is coming back.
    async fn reclaim(
        &mut self,
        attach: &mut Attachment,
        queue: &mut mpsc::Receiver<Outbound>,
        pending: &mut VecDeque<DaemonToControl>,
        applied: u64,
        notice: SpotNotice,
    ) -> Result<(), WireError> {
        tracing::warn!(
            seconds_remaining = notice.seconds_remaining,
            "this machine's capacity is being reclaimed; saving the session"
        );
        self.quiesce(attach, queue, pending, applied).await?;

        if let Err(error) = self.disk.sync().await {
            // Not fatal, and not a reason to skip the notice: the seconds
            // left are better spent telling the control plane than dying
            // over a flush that may well have happened anyway.
            tracing::error!(%error, "the filesystem could not be flushed before reclamation");
        }

        self.api
            .report_spot_notice(notice.seconds_remaining)
            .await?;
        pending.push_back(DaemonToControl::SpotNotice {
            seconds_remaining: notice.seconds_remaining,
        });
        if self.flush(attach, pending, applied).await {
            Ok(())
        } else {
            Err(WireError::Unwelcome(
                "the reclamation notice did not reach the room".to_owned(),
            ))
        }
    }

    /// Brings the session to a stop that nothing is still writing to.
    ///
    /// The first two steps of every ending — a reclamation, and a platform
    /// stopping a container — because they are the same two steps for the
    /// same reason, and because what comes after them is only correct once
    /// they have finished:
    ///
    /// 1. **Interrupt the turn.** The harness stops writing, so what is
    ///    flushed next is a transcript that has stopped moving rather than
    ///    one truncated mid-sentence. Nothing asks the model about any of
    ///    this: it is thirty seconds, and an LLM is slow and unpredictable.
    /// 2. **Flush.** Everything the harness handed this daemon is written
    ///    through to the control plane — the transcript batches, and then
    ///    the harness-native session id, which is what lets the next
    ///    machine continue this conversation instead of opening a new one.
    ///    Both are awaited: the daemon has to *know* they landed, because
    ///    it is about to stop existing.
    async fn quiesce(
        &mut self,
        attach: &mut Attachment,
        queue: &mut mpsc::Receiver<Outbound>,
        pending: &mut VecDeque<DaemonToControl>,
        applied: u64,
    ) -> Result<(), WireError> {
        // Before anything else, so a user message that arrives during the
        // flush is refused rather than opening a turn nothing will record.
        self.reclaiming = true;

        self.session.interrupt().await.map_err(harness)?;
        self.session.flush().await.map_err(harness)?;
        self.drain(attach, queue, pending, applied).await;
        if let Some(id) = self.harness_session_id.clone() {
            self.api.record_harness_session(&id).await?;
        } else {
            tracing::warn!(
                "the harness never announced a session id; the next machine \
                 opens a new conversation"
            );
        }
        Ok(())
    }

    /// Spends the platform's grace period on a machine whose filesystem is
    /// about to go with it.
    ///
    /// The [`Runtime::Container`](flyco_core::Runtime::Container) ending, and
    /// it differs from a reclamation in exactly one place — the third step.
    /// A reclaimed virtual machine `sync`s its page cache and trusts the
    /// disk; a container has no disk to trust, so the working tree has to
    /// *leave the machine*:
    ///
    /// 1. [Quiesce](Self::quiesce): interrupt the turn, flush the transcript
    ///    batches and the harness session id.
    /// 2. **Write the workdir patch.** Everything the clone does not
    ///    already have — unpushed commits, staged and unstaged edits,
    ///    untracked files — diffed against the clone's landing commit, in
    ///    the same format an automatic archive stores and a fresh machine
    ///    replays onto its clone. This is the whole of the user's unpushed
    ///    work, and it is written *after* the flush so it is not competing
    ///    with it for the seconds available.
    /// 3. **Report.** `POST /v1/sessions/{id}/stopping`, awaited, which is
    ///    what makes the stop true for the control plane. Last on purpose:
    ///    it must not be true before the work is safe.
    ///
    /// A tree that still matches its clone writes no patch and says so —
    /// a session whose agent left nothing behind — and storing an empty
    /// patch would leave the next machine replaying nothing.
    async fn stopping(
        &mut self,
        attach: &mut Attachment,
        queue: &mut mpsc::Receiver<Outbound>,
        pending: &mut VecDeque<DaemonToControl>,
        applied: u64,
        reason: StopReason,
    ) -> Result<(), WireError> {
        self.quiesce(attach, queue, pending, applied).await?;
        self.snapshot_all().await?;
        self.api.report_stopping(reason).await?;
        Ok(())
    }

    /// Posts everything already queued as one batch.
    ///
    /// What makes the flush a *whole* one: the harness's output reaches the
    /// room through a queue the pump drains one frame per loop, so a
    /// reclamation that only flushed the transcript store would leave the
    /// room's stored tail — which is what a browser replays — short by
    /// whatever was still queued.
    ///
    /// A batch that will not send is kept for an attach that is not
    /// coming, which is the honest thing to do with it: the room's tail is
    /// the loss, and the transcript itself is already in object storage.
    async fn drain(
        &mut self,
        attach: &mut Attachment,
        queue: &mut mpsc::Receiver<Outbound>,
        pending: &mut VecDeque<DaemonToControl>,
        applied: u64,
    ) {
        while let Ok(outbound) = queue.try_recv() {
            // A reclamation is not the moment to run a tool: an approved
            // call left unanswered here is asked again on the next machine.
            let Outbound::Frame { frame, .. } = outbound else {
                continue;
            };
            self.remember(&frame);
            pending.push_back(frame);
        }
        if !self.flush(attach, pending, applied).await {
            tracing::warn!("the last frame batch did not reach the room before reclamation");
        }
    }

    /// Keeps what a reclamation will need out of a frame on its way past.
    fn remember(&mut self, frame: &DaemonToControl) {
        if let DaemonToControl::Started { harness_session_id } = frame {
            self.harness_session_id = Some(harness_session_id.clone());
        }
    }

    /// The frame one shell update leaves as, and the end of a run.
    ///
    /// The slot is cleared here rather than where the run was started,
    /// because the exit is the only thing that says a command is over:
    /// cancelling one asks it to stop, and it is still running until it
    /// says otherwise.
    fn shell_frame(&mut self, update: ShellUpdate) -> DaemonToControl {
        let ShellUpdate { run, event } = update;
        match event {
            ShellEvent::Output { stream, data } => {
                DaemonToControl::ShellOutput { run, stream, data }
            }
            ShellEvent::Exited { outcome, truncated } => {
                if self
                    .running_shell
                    .as_ref()
                    .is_some_and(|(open, _)| *open == run)
                {
                    self.running_shell = None;
                }
                DaemonToControl::ShellExited {
                    run,
                    outcome,
                    truncated,
                }
            }
        }
    }

    /// Runs one `!` command, or answers with why it will not.
    ///
    /// Every refusal is an [exit](DaemonToControl::ShellExited) rather than
    /// a log line: the room has already recorded the command and put it in
    /// front of the user, so a run that produces nothing at all would be a
    /// transcript row that waits for ever.
    fn run_shell(&mut self, run: ShellRunId, command: String) {
        let refusal = if self.paused || self.reclaiming {
            tracing::warn!(
                %run,
                "refused a shell command: this session is not accepting work"
            );
            Some(ShellOutcome::Refused)
        } else if self.running_shell.is_some() {
            tracing::warn!(%run, "refused a shell command: one is already running");
            Some(ShellOutcome::Busy)
        } else {
            None
        };

        if let Some(outcome) = refusal {
            self.report_shell(run, outcome);
            return;
        }

        tracing::info!(%run, command, "running a shell command for the composer");
        let started = self.shell.start(run, command, self.shell_reports.clone());
        self.running_shell = Some((run, started));
    }

    /// Files an exit this daemon decided on rather than a command produced.
    ///
    /// Through the same channel a real run's frames take, so the room sees
    /// one ordering of one run's life however it ended.
    fn report_shell(&self, run: ShellRunId, outcome: ShellOutcome) {
        let filed = self.shell_reports.send(ShellUpdate {
            run,
            event: ShellEvent::Exited {
                outcome,
                truncated: false,
            },
        });
        if filed.is_err() {
            tracing::error!(%run, "a shell refusal had nowhere to go");
        }
    }

    /// Acts on one command from the control plane.
    /// Dispatches one command, or gives up on the harness.
    ///
    /// A wedged harness must not be able to take the relay with it. Failing
    /// here ends the daemon with a sentence the session can show, which is
    /// the whole difference between a page that says what went wrong and
    /// one that spins for ever (issue #201).
    async fn dispatch_before(&mut self, command: ControlToDaemon) -> Result<Ended, WireError> {
        let named = command.name();
        match tokio::time::timeout(self.deadlines.harness_deadline(), self.dispatch(command)).await
        {
            Ok(dispatched) => dispatched,
            Err(_elapsed) => Err(WireError::Harness(format!(
                "the agent did not take `{named}` within {:?}; it has stopped accepting commands",
                self.deadlines.harness_deadline()
            ))),
        }
    }

    async fn dispatch(&mut self, command: ControlToDaemon) -> Result<Ended, WireError> {
        match command {
            ControlToDaemon::UserMessage { text, .. } => self.user_message(text).await?,
            ControlToDaemon::ShellCommand { .. } => {
                // The room reissues a browser's request as `RunShell` with
                // the identity it assigned; an unidentified one reaching a
                // daemon is a control plane that skipped that step, and
                // there is nothing to key the output to.
                tracing::error!(
                    "a shell command arrived without a run id; the session room did not assign one"
                );
            }
            ControlToDaemon::RunShell { run, command } => self.run_shell(run, command),
            ControlToDaemon::Interrupt => {
                // Stop ends whatever is running, and a `!` command is as
                // much "what is running" as a turn is: the user pressed one
                // button and means both.
                if let Some((_, running)) = self.running_shell.take() {
                    running.cancel();
                }
                self.session.interrupt().await.map_err(harness)?;
            }
            ControlToDaemon::Compact => {
                if self.refuse_while_paused("context compaction")
                    || self.refuse_while_reclaiming("context compaction")
                {
                    return Ok(Ended::Disconnected);
                }
                self.session.compact().await.map_err(harness)?;
            }
            ControlToDaemon::ContextUsage => {
                // A read, not work: asking what the window holds starts
                // nothing, spends nothing, and is answerable even while a
                // turn is running — so unlike a compaction it is refused
                // for nothing short of the harness being gone.
                self.session.context_usage().await.map_err(harness)?;
            }
            ControlToDaemon::SetModel { model } => {
                // Refused on the same terms as a compaction: both reach the
                // harness, and a session that has stopped accepting work or
                // is about to lose its machine has no harness to reach.
                // The control plane has already recorded the model, so the
                // change is redelivered on the next attach rather than
                // lost — `survives_a_disconnect` is what makes that true.
                if self.refuse_while_paused("a model change")
                    || self.refuse_while_reclaiming("a model change")
                {
                    return Ok(Ended::Disconnected);
                }
                self.session.set_model(model).await.map_err(harness)?;
            }
            ControlToDaemon::SetPermissionMode { mode } => {
                // Refused on the same terms as a model change: the harness
                // applies it, and a session that has stopped accepting work
                // has no harness to reach. The control plane has already
                // recorded the mode, so the change is redelivered on the
                // next attach rather than lost — `survives_a_disconnect`
                // is what makes that true.
                if self.refuse_while_paused("a permission mode change")
                    || self.refuse_while_reclaiming("a permission mode change")
                {
                    return Ok(Ended::Disconnected);
                }
                self.session
                    .set_permission_mode(mode)
                    .await
                    .map_err(harness)?;
            }
            ControlToDaemon::TerminalInput { data } => {
                if self.refuse_while_paused("terminal input") {
                    return Ok(Ended::Disconnected);
                }
                self.terminal.write(&data)?;
            }
            ControlToDaemon::TerminalHarness { resume } => {
                if self.refuse_while_paused("the harness TUI") {
                    return Ok(Ended::Disconnected);
                }
                self.launch_harness_tui(resume)?;
            }
            // A size is a fact about the pane, not work; a paused session's
            // shell may still be looked at.
            ControlToDaemon::TerminalResize { cols, rows } => {
                self.terminal.resize(cols, rows)?;
            }
            ControlToDaemon::ApprovalDecision {
                id,
                decision,
                payload,
            } => self.decide_approval(id, decision, payload).await?,
            ControlToDaemon::Budget { signal } => self.budget(signal).await?,
            ControlToDaemon::BudgetRaised { limit } => self.budget_raised(limit).await?,
            ControlToDaemon::MachineChanged {
                machine_type,
                hourly,
                spot,
                restarted,
            } => {
                self.machine_changed(machine_type, hourly, spot, restarted)
                    .await?;
            }
            ControlToDaemon::SetComputerUse { .. }
            | ControlToDaemon::DesktopAudience { .. }
            | ControlToDaemon::DesktopTakeover { .. }
            | ControlToDaemon::DesktopInput { .. } => self.desktop_command(command).await?,
            ControlToDaemon::InspectWorkdir { id, request } => self.answer_workdir(id, request),
            ControlToDaemon::AddRepo { slug, branch, dir } => {
                self.add_repo(slug, branch, dir).await?;
            }
            ControlToDaemon::Archive { preserve_workdir } => {
                if preserve_workdir {
                    self.snapshot_all().await?;
                }
                tracing::info!(session = %self.session_id, "the control plane archived this session");
                self.terminal.shutdown()?;
                return Ok(Ended::Archived);
            }
            ControlToDaemon::Superseded => return self.superseded(),
        }
        Ok(Ended::Disconnected)
    }

    /// The room's last word to a stream serving a superseded epoch.
    ///
    /// Composed by the room itself as it ends the stream — never a log
    /// row, never redelivered. What it means is decided here rather than
    /// inferred: another daemon holds the room, and this one standing
    /// down is the whole point of the signal.
    fn superseded(&mut self) -> Result<Ended, WireError> {
        tracing::warn!(
            session = %self.session_id,
            "a newer attach owns this session's room; this daemon is a spare"
        );
        self.terminal.shutdown()?;
        Ok(Ended::Superseded)
    }

    /// Snapshots every checkout's work to the control plane.
    ///
    /// Shared by [`stopping`](Self::stopping) and `Archive`: both are the
    /// same "the filesystem is about to be gone" moment, and both need
    /// every checkout's patch stored independently — one checkout that
    /// will not snapshot is not a reason the others lose their work, so
    /// each is tried before the first failure propagates. A tree that
    /// still matches its clone writes no patch, and the stored object
    /// carries the commit it was diffed against so the next machine can
    /// rewind a moved tip before applying it.
    async fn snapshot_all(&self) -> Result<(), WireError> {
        let mut failure = None;
        for dir in self.repos.dirs() {
            let label = dir.as_deref().unwrap_or(".");
            let result: Result<usize, WireError> = match self.repos.snapshot(&dir).await {
                Ok(Some(snapshot)) => {
                    let bytes = snapshot.patch.len();
                    self.api
                        .put_workdir_patch(dir.as_deref(), snapshot.encode())
                        .await
                        .map(|()| bytes)
                        .map_err(WireError::from)
                }
                Ok(None) => {
                    tracing::info!(
                        dir = label,
                        "the checkout matches its clone; the next machine needs only the clone"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::error!(dir = label, %error, "a checkout could not be snapshotted");
                    Err(WireError::from(error))
                }
            };
            match result {
                Ok(bytes) => tracing::warn!(
                    dir = label,
                    bytes,
                    "stored a checkout's work before it went"
                ),
                Err(error) => {
                    tracing::error!(
                        dir = label,
                        %error,
                        "a checkout's patch did not reach the control plane"
                    );
                    failure.get_or_insert(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Clones and registers a repository the control plane approved mid-session.
    ///
    /// The command is durable — it is redelivered until this daemon says it
    /// applied — so everything here is idempotent. A directory that already
    /// holds a checkout (a replay, or a replacement machine whose config
    /// already lists the repo) is adopted rather than cloned again, and
    /// only an actual clone announces [`DaemonToControl::RepoAdded`] and
    /// tells the agent: a replayed command announcing a second time would
    /// leave a transcript event saying a repository arrived that was there
    /// all along.
    ///
    /// A refusal — an unsafe directory name, no GitHub access, a failed
    /// clone — is logged and told to the agent rather than ending the
    /// relay: the session is healthy, it is this one command that cannot
    /// complete.
    async fn add_repo(
        &mut self,
        slug: flyco_core::RepoSlug,
        branch: flyco_core::BranchName,
        dir: String,
    ) -> Result<(), WireError> {
        if !valid_repo_dir(&dir) {
            // The room stores what it is given, so a directory that would
            // escape the workspace is refused here rather than passed to
            // `git clone`. `valid_repo_dir` is also how the config is
            // checked, so this can only fire on a room message this build
            // does not trust.
            tracing::error!(
                dir,
                "an AddRepo named a checkout directory that is not safe"
            );
            return Ok(());
        }
        let Some(github) = self.github.clone() else {
            tracing::error!("an AddRepo arrived on a machine with no GitHub access configured");
            return Ok(());
        };
        let repo = RepoConfig {
            slug: slug.clone(),
            branch: branch.clone(),
            dir: dir.clone(),
        };
        match self.repos.clone_repo(&repo, &github).await {
            Ok(false) => {
                // The set already knew the directory — the repo is in the
                // session's config, so this is a replayed command. Nothing
                // to clone, nothing to announce.
                tracing::info!(
                    dir,
                    "an AddRepo replayed for a checkout that already exists"
                );
            }
            Err(error) => {
                tracing::error!(dir, %slug, %error, "the approved repository could not be cloned");
                let notice = RepoAddedNotice::failed(&slug, &dir, &error)
                    .render()
                    .map_err(|error| notice_failed(&error))?;
                self.tell_the_agent(&notice).await?;
            }
            Ok(true) => {
                self.workspace.register(&dir, branch.as_str());
                self.reply_out
                    .send(DaemonToControl::RepoAdded {
                        slug: slug.clone(),
                        branch,
                        dir: dir.clone(),
                    })
                    .await
                    .map_err(|_| WireError::Harness("the reply channel closed".to_owned()))?;
                let notice = RepoAddedNotice::added(&slug, &dir)
                    .render()
                    .map_err(|error| notice_failed(&error))?;
                self.tell_the_agent(&notice).await?;
            }
        }
        Ok(())
    }

    /// Sends one user message into the harness.
    ///
    /// The opening notice rides on the first message rather than arriving
    /// as one of its own: the agent must know what machine it is on before
    /// it starts working, and a message carrying only that would open a
    /// turn about nothing.
    async fn user_message(&mut self, text: String) -> Result<(), WireError> {
        if self.refuse_while_paused("a user message")
            || self.refuse_while_reclaiming("a user message")
        {
            return Ok(());
        }
        let text = match self.opening.take() {
            Some(notice) => OpeningMessage { notice, text }
                .render()
                .map_err(|error| notice_failed(&error))?,
            None => text,
        };
        self.session.send_user_message(text).await.map_err(harness)
    }

    /// Tells the agent its machine changed.
    ///
    /// Told rather than discovered: a resize restarts the machine and
    /// kills this process, so the daemon reading this is a new one whose
    /// configuration still describes the machine the session booted on.
    /// The size and any licence minimum are deliberately not on the wire —
    /// the notice says what the machine is now and what the restart cost,
    /// and `machine_status` is where the full description is read from,
    /// live.
    async fn machine_changed(
        &self,
        machine_type: String,
        hourly: Option<Usd>,
        spot: bool,
        restarted: bool,
    ) -> Result<(), WireError> {
        let notice = MachineChanged {
            line: MachineLine::of(&SessionMachine {
                machine_type,
                hourly,
                spot,
                capacity: None,
                minimum: None,
            }),
            restarted,
        };
        self.tell_the_agent(&notice.render().map_err(|error| notice_failed(&error))?)
            .await
    }

    /// The desktop's four commands, which are levels on the supervisor
    /// rather than work for the harness.
    ///
    /// None of them is refused while a session is paused: none spends
    /// anything the pause exists to protect. A dead supervisor is a warn
    /// rather than an error, because the relay's own liveness is not the
    /// desktop's to end.
    async fn desktop_command(&mut self, command: ControlToDaemon) -> Result<(), WireError> {
        match command {
            ControlToDaemon::SetComputerUse { enabled } => {
                if let Err(error) = self.desktop.set_enabled(enabled) {
                    tracing::warn!(%error, "`computer_use` could not reach the desktop");
                }
            }
            ControlToDaemon::DesktopAudience { watching } => {
                if let Err(error) = self.desktop.set_watching(watching) {
                    tracing::warn!(%error, "the audience flag could not reach the desktop");
                }
            }
            ControlToDaemon::DesktopTakeover { active } => {
                // The user's hands and the model's cannot both be on the
                // screen: taking over ends whatever the turn is doing,
                // the same path Stop takes, because a click is the
                // strongest interrupt there is.
                if active {
                    if let Some((_, running)) = self.running_shell.take() {
                        running.cancel();
                    }
                    self.session.interrupt().await.map_err(harness)?;
                }
                if let Err(error) = self.desktop.set_takeover(active) {
                    tracing::warn!(%error, "the takeover flag could not reach the desktop");
                }
            }
            ControlToDaemon::DesktopInput { events } => {
                // The supervisor drops input that arrives without a held
                // takeover — a browser's late batch is a race, not an
                // error.
                if let Err(error) = self.desktop.inject(events) {
                    tracing::warn!(%error, "desktop input could not reach the desktop");
                }
            }
            _ => unreachable!("only the desktop's commands reach `desktop_command`"),
        }
        Ok(())
    }

    /// Reads the checkout for a browser, on a task of its own.
    ///
    /// Off the pump because a diff runs git over the whole tree, and the
    /// stream has harness output to carry while it does. Read-only, so
    /// nothing about it depends on what else the session is doing — a
    /// paused or reclaiming session still shows the user its files.
    fn answer_workdir(&self, id: WorkdirRequestId, request: WorkdirRequest) {
        let workspace = self.workspace.clone();
        let replies = self.reply_out.clone();
        tokio::spawn(async move {
            let reply = workspace.inspect(request).await;
            if replies
                .send(DaemonToControl::WorkdirReply { id, reply })
                .await
                .is_err()
            {
                tracing::warn!(%id, "a workdir reply outlived the connection that asked for it");
            }
        });
    }

    /// Puts a flyco notice into the conversation.
    ///
    /// A user message is the only channel a harness offers for "something
    /// happened that you need to know about", so every notice flyco injects
    /// goes in as one, marked with its `[flyco … notice]` prefix so the
    /// model can tell it from the person. A paused session is told nothing:
    /// it is not accepting work, and a notice would be an instruction it
    /// cannot act on.
    async fn tell_the_agent(&self, notice: &str) -> Result<(), WireError> {
        if self.paused || self.reclaiming {
            tracing::warn!("not telling a stopped session about a change it cannot act on");
            return Ok(());
        }
        self.session
            .send_user_message(notice.trim_end().to_owned())
            .await
            .map_err(harness)
    }

    /// Tells the agent it may not stop while a checkout is dirty.
    async fn nudge_if_dirty(&mut self) -> Result<(), WireError> {
        if self.paused || self.reclaiming {
            return Ok(());
        }
        let dirty: Vec<String> = self
            .trees
            .iter()
            .filter(|(_, tree)| matches!(tree, Tree::Dirty { noticed: false }))
            .map(|(dir, _)| dir.clone().unwrap_or_else(|| ".".to_owned()))
            .collect();
        if dirty.is_empty() {
            return Ok(());
        }
        for tree in self.trees.values_mut() {
            if matches!(tree, Tree::Dirty { noticed: false }) {
                *tree = Tree::Dirty { noticed: true };
            }
        }
        self.session
            .send_user_message(format!("{DIRTY_NOTICE} Dirty: {}.", dirty.join(", ")))
            .await
            .map_err(harness)
    }

    /// Puts the harness's own TUI on the terminal.
    ///
    /// A launch that cannot be built — the claude binary missing from the
    /// sidecar tree — is a line in the terminal, where a failed `claude`
    /// would have printed.
    fn launch_harness_tui(&mut self, resume: bool) -> Result<(), WireError> {
        match self.tui.command(resume) {
            Ok(command) => self.terminal.launch_harness(command)?,
            Err(error) => self.terminal.print(&format!("\r\nflycod: {error}\r\n"))?,
        }
        Ok(())
    }

    /// Hands the user's decision to the tool call waiting on it.
    ///
    /// A tool call this process never raised is one the machine's
    /// suspension took with it (issue #355): the agent asked, waited past
    /// the idle threshold, the machine was released, and the decision that
    /// arrives now is what started it again. The call is gone with the
    /// process that made it, so the agent is told what the user decided
    /// and, when that was yes, the collector answers the re-run without
    /// asking the user a second time.
    async fn decide_approval(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        payload: ApprovalPayload,
    ) -> Result<(), WireError> {
        let Some(harness_id) = self.approvals.remove(&id) else {
            let ApprovalPayload::ToolUse { tool, input } = payload else {
                // Not every approval is a harness tool call waiting on a
                // permission. The daemon's own MCP server raises one for a
                // license-bound resize, and the control plane performs
                // *that* itself; the decision still reaches every daemon
                // because the room echoes it. Nothing here is blocked on
                // it, so it is noted rather than treated as a protocol
                // violation.
                tracing::debug!(%id, ?decision, "a decided approval was not a harness tool call");
                return Ok(());
            };
            let approved = decision == ApprovalDecision::Approved;
            tracing::info!(%id, ?decision, %tool, "a decision arrived for a tool call a suspension took");
            let notice = ApprovalAfterSuspend {
                tool: tool.clone(),
                input: serde_json::to_string_pretty(&input)
                    .map_err(|error| WireError::Harness(error.to_string()))?,
                approved,
            };
            self.tell_the_agent(&notice.render().map_err(|error| notice_failed(&error))?)
                .await?;
            if approved
                && self
                    .standing
                    .send(ApprovalPayload::ToolUse { tool, input })
                    .is_err()
            {
                tracing::debug!("the harness has stopped; nothing will re-run the approved call");
            }
            return Ok(());
        };
        let answer = match decision {
            ApprovalDecision::Approved => ToolApproval::Allow {
                id: harness_id,
                updated_input: None,
            },
            ApprovalDecision::Denied => ToolApproval::Deny {
                id: harness_id,
                message: "Denied by the flyco user.".to_owned(),
            },
        };
        self.session.decide_approval(answer).await.map_err(harness)
    }

    /// Applies a budget threshold.
    async fn budget(&mut self, signal: BudgetSignal) -> Result<(), WireError> {
        if signal == BudgetSignal::Pause {
            // Enforced, not announced: the turn ends now and nothing new is
            // accepted. Interrupting first means the harness is not mid-tool
            // when the session stops.
            tracing::warn!("budget exhausted: interrupting the turn and pausing the session");
            self.paused = true;
            return self.session.interrupt().await.map_err(harness);
        }

        let notice = match signal {
            BudgetSignal::Notice50 => "half of this session's compute budget is spent",
            BudgetSignal::Warn80 => "80% of this session's compute budget is spent",
            BudgetSignal::FinalWarn90 => {
                "90% of this session's compute budget is spent — the session pauses at 100%"
            }
            BudgetSignal::Pause => unreachable!("handled above"),
        };
        tracing::info!(?signal, "relaying a budget threshold to the harness");
        self.session
            .send_user_message(format!("{BUDGET_NOTICE_PREFIX} {notice}."))
            .await
            .map_err(harness)
    }

    /// Lifts a budget pause the user paid to end.
    ///
    /// The only thing that clears [`Self::paused`]. Telling the harness is
    /// not decoration: the pause interrupted a turn mid-thought and said
    /// nothing, so an agent that is merely allowed to work again would sit
    /// there waiting for a user who thinks they already restarted it. The
    /// notice is a message in the conversation, which is what opens the
    /// turn that continues the work.
    async fn budget_raised(&mut self, limit: Usd) -> Result<(), WireError> {
        if !self.paused {
            // A budget raised on a session that was never paused is the
            // ordinary case of a user topping one up early. There is
            // nothing to lift and nothing the agent has to be told: it can
            // read the new limit from `budget_status` whenever it prices
            // its next move.
            tracing::info!(%limit, "the budget was raised on a session that is not paused");
            return Ok(());
        }
        tracing::info!(%limit, "the budget was raised: lifting the pause");
        self.paused = false;
        let notice = BudgetRaised {
            limit: limit.to_string(),
        }
        .render()
        .map_err(|error| notice_failed(&error))?;
        self.session
            .send_user_message(notice.trim_end().to_owned())
            .await
            .map_err(harness)
    }

    /// Whether a paused session must refuse this command.
    fn refuse_while_paused(&self, what: &str) -> bool {
        if self.paused {
            tracing::warn!(
                what,
                "refused: this session is paused on an exhausted budget"
            );
        }
        self.paused
    }

    /// Whether a machine being reclaimed must refuse this command.
    ///
    /// Everything that would open a turn, and nothing else. The transcript
    /// has already been flushed to the control plane and the disk has
    /// already been synced; a turn started after that is work the machine
    /// will be taken away in the middle of and the replacement will have no
    /// record of. The user's message is not lost — it stays in the room's
    /// mailbox and is delivered to the daemon on the new machine.
    fn refuse_while_reclaiming(&self, what: &str) -> bool {
        if self.reclaiming {
            tracing::warn!(
                what,
                "refused: this machine's capacity is being reclaimed; it is kept for the \
                 daemon on the replacement"
            );
        }
        self.reclaiming
    }
}

fn harness(error: impl core::fmt::Display) -> WireError {
    WireError::Harness(error.to_string())
}

/// Attaches to the session room and opens its command stream.
///
/// One step rather than two at every call site because neither half is
/// useful alone: an attach without its stream is a daemon that can speak
/// but not hear, and a stream without the attach's epoch is refused.
async fn attach<A: ControlApi + RelayTransport>(
    api: &A,
    deadlines: Deadlines,
) -> Result<Attachment, WireError> {
    let attached = api.attach().await?;
    let commands = api
        .commands(attached.epoch, deadlines.silence_limit())
        .await?;
    tracing::info!(epoch = attached.epoch, "attached to the session room");
    Ok(Attachment {
        epoch: attached.epoch,
        commands,
        next_seq: 1,
    })
}

/// Problem types an attach can never retry away.
///
/// A wrong token, a session that is gone, and a protocol the control
/// plane does not speak are all permanent: another attempt in a second
/// meets the same refusal. Everything else — a room under load, a
/// dropped flow — is worth the backoff.
const FATAL_REFUSALS: &[&str] = &[
    "invalid-daemon-credential",
    "missing-credential",
    "protocol-mismatch",
    "session-not-found",
];

impl WireError {
    /// The wait the control plane named, when what stopped the relay was
    /// a refusal carrying `Retry-After`.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::ControlApi(api) => api.retry_after(),
            _ => None,
        }
    }
}

/// Whether an attach failure ends the run rather than backing off.
fn fatal_attach(error: &WireError) -> bool {
    match error {
        WireError::Unaddressable(_) | WireError::Unwelcome(_) => true,
        WireError::ControlApi(api) => api
            .kind()
            .is_some_and(|kind| FATAL_REFUSALS.contains(&kind)),
        _ => false,
    }
}

/// Everything [`run`] needs to drive one session.
pub struct SessionRelay<S, A, T, W, D, H, X> {
    /// The session this daemon serves.
    pub session_id: SessionId,
    /// The live harness handle.
    pub session: S,
    /// Harness output, consumed exactly once.
    pub outputs: mpsc::Receiver<SessionOutput>,
    /// REST client for durable writes.
    pub api: A,
    /// The web terminal.
    pub terminal: T,
    /// What the terminal's foreground produces.
    pub terminal_out: mpsc::Receiver<TerminalEvent>,
    /// The session's desktop: present on every session, idle on one
    /// without a screen.
    pub desktop: X,
    /// What the desktop reports back.
    pub desktop_out: mpsc::Receiver<crate::desktop::DesktopEvent>,
    /// The session's harness as a terminal application, resolved from the
    /// daemon's configuration.
    pub tui: crate::tui::HarnessTui,
    /// Runs the composer's `!` commands on this machine.
    pub shell: H,
    /// Every checkout in the workspace: snapshot, restore, and the dirty
    /// summaries that keep a session awake.
    pub repos: W,
    /// The workspace root, read-only, for the `Files` and `Diff` tabs.
    pub workspace: Workspace,
    /// GitHub access for mid-session `AddRepo` clones — `None` on a
    /// developer machine, where the command is refused.
    pub github: Option<GithubAccess>,
    /// The filesystems a reclamation flushes before the compute goes.
    pub disk: D,
    /// Eviction notices from the provider's metadata endpoint, or a closed
    /// channel on a machine nobody can reclaim.
    pub spot: Notices,
    /// The platform's stop signal on a machine with no disk, or a closed
    /// channel on one whose filesystem outlives a stop.
    pub stops: Stops,
    /// The machine this session opened on, as the agent is told about it.
    pub machine: flyco_core::SessionMachine,
    /// Whether flyco or the user chose that machine.
    pub machine_origin: flyco_core::MachineOrigin,
    /// How this relay notices the stream is dead.
    pub deadlines: Deadlines,
}

impl<S, A, T, W, D, H, X> core::fmt::Debug for SessionRelay<S, A, T, W, D, H, X> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionRelay")
            .field("session", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// Drives a session from the control plane until it is archived or the
/// harness stops.
///
/// # Errors
///
/// Returns [`WireError`] if the relay queue overflows, the control plane
/// speaks a protocol this daemon cannot read, or the harness stops
/// accepting commands. A dropped stream is not an error: it is re-attached.
#[expect(
    clippy::too_many_lines,
    reason = "one loop owns the whole attachment lifecycle — attach, pump, \
              reconnect — and splitting it would scatter the state the loop \
              threads through itself"
)]
pub async fn run<S, A, T, W, D, H, X>(
    relay: SessionRelay<S, A, T, W, D, H, X>,
) -> Result<(), WireError>
where
    S: HarnessSession + 'static,
    A: ControlApi + RelayTransport + Clone,
    T: TerminalSession + 'static,
    W: WorkingSet + 'static,
    D: Disk,
    H: Shell,
    X: crate::desktop::DesktopSession + 'static,
{
    let (sender, mut queue) = mpsc::channel(QUEUE_DEPTH);
    let (standing, granted) = mpsc::unbounded_channel();
    let collector = tokio::spawn(collect(relay.outputs, sender, relay.api.clone(), granted));

    // Rendered before the first frame moves, so a session that cannot state
    // what machine it is on fails here rather than running an agent that was
    // never told (docs/ux.md §9.5).
    let opening = SessionStart::new(&relay.machine, relay.machine_origin)
        .render()
        .map_err(|error| notice_failed(&error))?;

    // Unbounded because the relay writes into it too — a refused `!`
    // command is reported through the same channel a real run's frames take
    // — and a bounded channel written to from the task that drains it is a
    // deadlock. What keeps it small is the shell itself: one command at a
    // time, with a cap on how much of it reaches the transcript.
    let (shell_reports, shell_updates) = mpsc::unbounded_channel();
    let (reply_out, replies) = mpsc::channel(REPLY_DEPTH);
    let mut connection = Connection {
        session: relay.session,
        terminal: relay.terminal,
        terminal_out: relay.terminal_out,
        desktop: relay.desktop,
        desktop_out: relay.desktop_out,
        tui: relay.tui,
        shell: relay.shell,
        shell_updates,
        shell_reports,
        running_shell: None,
        api: relay.api,
        repos: relay.repos,
        workspace: relay.workspace,
        github: relay.github,
        reply_out,
        replies,
        disk: relay.disk,
        spot: relay.spot,
        stops: relay.stops,
        reclaiming: false,
        harness_session_id: None,
        session_id: relay.session_id,
        paused: false,
        trees: BTreeMap::new(),
        alive: Alive {
            harness: Producing::Yes,
            repo_watch: Producing::Yes,
            desktop: Producing::Yes,
            spot_watch: Producing::Yes,
            stop_watch: Producing::Yes,
        },
        approvals: BTreeMap::new(),
        standing,
        opening: Some(opening),
        deadlines: relay.deadlines,
        retry_hint: None,
    };
    let mut attempt = 0_u32;
    // Frames produced but never confirmed stored. They outlive the attach
    // they were produced under: a dropped stream ends the epoch, and the
    // next attach re-sends the whole tail from sequence one.
    let mut pending = VecDeque::new();
    // How far down the room's command log this daemon has applied. Global
    // across attaches: the log's sequences are too, so a redelivery of a
    // command whose acknowledgement was lost is recognized and skipped.
    let mut applied = 0_u64;
    // The last stage of the provisioning timeline (docs/ux.md §9.2). The
    // control plane can watch a machine be reserved and boot but has no way
    // onto it, so "the agent is up" is a fact only this process holds: the
    // harness has already started by the time `run` is called, and the room
    // has just seen the attach. Announced once, not on every reconnect
    // — a reconnect is not a second provision.
    let mut ready_announced = false;

    let ending = loop {
        // A collector failure is fatal and must not wait for a reconnect
        // that will never help: an overflowed queue only grows.
        if collector.is_finished() {
            break match collector.await {
                Ok(result) => result.map(|()| Ended::HarnessStopped),
                Err(error) => Err(WireError::Harness(error.to_string())),
            };
        }

        let mut attachment = match attach(&connection.api, connection.deadlines).await {
            Ok(attachment) => attachment,
            Err(error) => {
                if fatal_attach(&error) {
                    break Err(error);
                }
                // A refusal that names its wait is honoured over the
                // ladder: a `429` is the control plane saying when, and
                // attaching sooner is one more refused request.
                let wait = backoff(attempt).max(error.retry_after().unwrap_or_default());
                tracing::warn!(%error, ?wait, attempt, "could not attach to the session room");
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(wait).await;
                continue;
            }
        };

        let attached_at = tokio::time::Instant::now();
        if !ready_announced {
            // A failed flush here is a dropped attachment, which the loop
            // is already built to survive: the announcement keeps the
            // instant the agent actually came up because it is re-sent
            // from `pending` rather than re-dated.
            pending.push_back(DaemonToControl::ProvisioningStage {
                stage: ProvisioningStage::Ready,
                at_unix: now_unix(),
            });
            ready_announced = true;
        }
        match connection
            .pump(&mut attachment, &mut queue, &mut pending, &mut applied)
            .await
        {
            Ok(Ended::Disconnected) => {
                // The ladder counts an attachment that never became
                // established: a stream dead within moments of opening is
                // the signature of a peer superseding it, and re-attaching
                // uncounted is the ping-pong that spends the account's
                // request budget. A stream that held resets instead — its
                // drop is the ordinary kind and earns a prompt retry.
                if attached_at.elapsed() < ATTACH_STABLE {
                    attempt = attempt.saturating_add(1);
                } else {
                    attempt = 0;
                }
                let wait = backoff(attempt).max(connection.retry_hint.take().unwrap_or_default());
                tracing::warn!(
                    ?wait,
                    attempt,
                    "the session room's stream ended; re-attaching"
                );
                tokio::time::sleep(wait).await;
            }
            Ok(ending) => break Ok(ending),
            Err(error) => break Err(error),
        }
    };

    // Whatever ended the loop, the harness stops cleanly: `shutdown` is what
    // flushes the transcript store, which the harness's own task owns.
    let ending = ending?;
    if let Err(error) = connection.session.shutdown().await {
        tracing::warn!(%error, "the harness did not shut down cleanly");
    }
    say_goodbye(&ending);
    Ok(())
}

/// The one sentence each ending leaves in the log.
///
/// # Panics
///
/// Panics on [`Ended::Disconnected`], which never reaches here: a dropped
/// stream is re-attached by the loop rather than ending the run.
fn say_goodbye(ending: &Ended) {
    match ending {
        Ended::Archived => tracing::info!("session archived; flycod is done"),
        Ended::Superseded => {
            tracing::warn!("another daemon owns this session's room; flycod is done");
        }
        Ended::HarnessStopped => tracing::info!("the harness stopped; flycod is done"),
        Ended::Stopped => tracing::info!("the platform stopped this machine; flycod is done"),
        Ended::Disconnected => unreachable!("a disconnect reconnects rather than ending the run"),
    }
}
