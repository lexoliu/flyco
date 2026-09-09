//! The daemon's end of the session relay.
//!
//! One outbound WebSocket to the session's room, carrying
//! [`DaemonToControl`] out and [`ControlToDaemon`] in. This replaces the
//! [REPL](crate::repl) as the way a shipped session is driven; the REPL
//! stays as the dev tool for reproducing a harness bug without a control
//! plane.
//!
//! # Two tasks, one bounded queue
//!
//! The [collector](collect) owns the harness's output stream, turns each
//! [`SessionOutput`] into a wire frame, and pushes it into a bounded queue.
//! The [connection](Connection) owns the socket and the harness's control
//! handle: it drains the queue outward and dispatches commands inward.
//!
//! They are separate because the socket is not always there. A reconnect
//! takes seconds; the harness does not stop producing during them, and
//! nothing about a coding session tolerates its transcript being dropped.
//! The queue is what absorbs that gap — and it is *bounded*
//! ([`QUEUE_DEPTH`]), because a queue that grows without limit trades a
//! visible outage for an invisible one that ends in the OOM killer. An
//! overflow is a fatal error, not a dropped frame: losing part of a session
//! silently is worse than stopping.
//!
//! A frame that leaves the queue but fails to write is handed back and
//! retried on the next connection, so delivery is **at least once**: a
//! socket that dies after accepting a write but before delivering it can
//! produce one duplicate. Exactly-once needs an acknowledgement the wire
//! protocol does not carry yet; until it does, a duplicated frame is the
//! better failure, because the room's stored tail is what a browser replays
//! and a gap in it can never be recovered.
//!
//! # Ordering that the product depends on
//!
//! An approval is recorded over REST *before* its frame is announced. The
//! control plane assigns the id, so the id a browser sees is one the API can
//! settle — and a decision that arrives while the socket is down still finds
//! a pending row waiting when the daemon comes back.

use core::time::Duration;
use std::collections::BTreeMap;

use askama::Template as _;
use flyco_core::workdir::WorkdirRequest;
use flyco_core::{
    ApprovalDecision, ApprovalId, BudgetSignal, ControlToDaemon, DaemonToControl, HarnessEvent,
    HarnessObservation, ProvisioningStage, RateLimitObservation, SessionId, SessionMachine,
    ShellOutcome, ShellRunId, StopReason, Usd, WIRE_PROTOCOL_VERSION, WorkdirRequestId,
};
use futures_util::{SinkExt as _, StreamExt as _};
use rand::Rng as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::control::rest::{ControlApi, ControlApiError};
use crate::git::{GitError, WorkingTree};
use crate::harness::{HarnessSession, SessionOutput, ToolApproval};
use crate::notice::{BudgetRaised, MachineChanged, MachineLine, OpeningMessage, SessionStart};
use crate::shell::{Shell, ShellEvent, ShellRun, ShellUpdate};
use crate::spot::{Disk, Notices, SpotNotice};
use crate::stop::Stops;
use crate::terminal::{TerminalError, TerminalSession};
use crate::workdir::Checkout;

/// How many frames may wait for a socket that is not there.
///
/// Sized for a reconnect, not for an outage: at the ~50 frames a second a
/// busy turn produces, this is roughly twenty seconds of disconnection —
/// comfortably past the backoff's first few attempts and far short of a
/// heap that matters. Overflowing it is a fatal error.
pub const QUEUE_DEPTH: usize = 1024;

/// How many answered workdir questions may wait for a socket.
///
/// Small on purpose: a browser asking what is in a directory is waiting on
/// an HTTP request the control plane is holding open, and an answer that
/// queued behind ten others is one nobody is still listening for.
pub const REPLY_DEPTH: usize = 8;

/// Shortest wait before a reconnect attempt.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Longest wait between reconnect attempts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How often a daemon with nothing to say says so anyway.
///
/// A session relay carries nothing at all while the agent is thinking, and
/// a flow with no packets on it is what a cloud NAT reclaims: Azure's
/// outbound idle timeout is four minutes by default, and it drops the flow
/// without a FIN, so neither end learns the socket is gone. Thirty seconds
/// is comfortably inside every such timeout flyco has met and costs one
/// small frame a minute in each direction.
pub const HEARTBEAT: Duration = Duration::from_secs(30);

/// How long one command may spend inside the harness before the session is
/// treated as wedged.
///
/// Every command the room sends resolves to a channel send and an
/// acknowledgement — milliseconds when the agent process is healthy. A
/// minute is therefore not a budget, it is a diagnosis: the harness has
/// stopped reading, and nothing about waiting longer will change that.
///
/// It exists because the pump awaits the harness *inside* its own loop, so
/// a command that never returns takes the socket read and the heartbeat
/// down with it: the daemon stops answering, stops reconnecting, and stops
/// being able to say why — which is precisely the silence issue #201
/// describes.
pub const HARNESS_DEADLINE: Duration = Duration::from_secs(60);

/// How many unanswered heartbeats mean the socket is gone.
///
/// The room answers every [`DaemonToControl::Heartbeat`], so three in a row
/// with nothing back is a path that no longer carries packets, whatever the
/// socket still claims. Three rather than one because a single answer can
/// be late; abandoning is always safe — the frames waiting are held, the
/// loop reconnects, and the room replays the mailbox — but abandoning on
/// every hiccup would reconnect a working session all day.
pub const MISSES_BEFORE_DEAD: u32 = 3;

/// How a relay proves its socket is still there, and when it gives up.
///
/// One value rather than two constants because the two are only meaningful
/// together: a deadline shorter than a couple of intervals abandons a
/// socket before the answer it is waiting for could possibly have arrived,
/// so the deadline is *derived* from the interval and cannot be set to
/// contradict it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keepalive {
    /// How often a daemon with nothing to say says so anyway.
    interval: Duration,
    /// How long the room may say nothing before its socket is abandoned.
    silence_limit: Duration,
    /// How long one command may spend inside the harness.
    harness_deadline: Duration,
}

impl Keepalive {
    /// A keepalive that beats every `interval` and gives up after `misses`
    /// unanswered beats.
    #[must_use]
    pub const fn every(interval: Duration, misses: u32) -> Self {
        Self {
            interval,
            silence_limit: interval.saturating_mul(misses),
            harness_deadline: HARNESS_DEADLINE,
        }
    }

    /// The same keepalive with a different harness deadline.
    #[must_use]
    pub const fn waiting_on_the_harness(self, harness_deadline: Duration) -> Self {
        Self {
            harness_deadline,
            ..self
        }
    }

    /// How often to beat.
    #[must_use]
    pub const fn interval(self) -> Duration {
        self.interval
    }

    /// How long silence may last before the socket is presumed dead.
    #[must_use]
    pub const fn silence_limit(self) -> Duration {
        self.silence_limit
    }

    /// How long one command may spend inside the harness.
    ///
    /// Kept beside the other two because it answers the same question they
    /// do — how long may nothing happen before flyco calls it broken — and
    /// because a test that wants a brisk relay wants all three brisk.
    #[must_use]
    pub const fn harness_deadline(self) -> Duration {
        self.harness_deadline
    }
}

impl Default for Keepalive {
    fn default() -> Self {
        Self::every(HEARTBEAT, MISSES_BEFORE_DEAD)
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

/// What the agent is told when a usage limit has reset.
const USAGE_RESET_NOTICE: &str =
    "[flyco usage notice] the account usage limit has reset. Continue.";

/// The daemon could not keep its end of the relay.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The configured control-plane URL is not a WebSocket endpoint.
    #[error("the control-plane URL cannot address the session relay: {0}")]
    Unaddressable(String),
    /// The relay carried a frame this protocol version does not define.
    #[error("the control plane sent a frame this daemon cannot read: {0}")]
    Undecodable(String),
    /// The control plane answered [`DaemonToControl::Hello`] with something
    /// other than [`ControlToDaemon::Welcome`].
    #[error("the control plane did not welcome this daemon: {0}")]
    Unwelcome(String),
    /// The harness stopped accepting commands.
    #[error("the harness session stopped: {0}")]
    Harness(String),
    /// The outbound queue overflowed while the socket was down.
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

/// The socket type a connected daemon holds.
pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The `ws`/`wss` URL of one route under a control plane's base URL.
///
/// `http`/`https` become `ws`/`wss`: a configuration names one control
/// plane, and nothing on a machine should have to be told its address twice
/// in two schemes. Shared with the host relay, which reaches a different
/// route on the same deployment.
///
/// # Errors
///
/// Returns [`WireError::Unaddressable`] if the base URL cannot address the
/// route, or is not an HTTP or WebSocket URL at all.
pub(crate) fn websocket_url(base: &url::Url, path: &str) -> Result<String, WireError> {
    let mut url = base
        .join(path)
        .map_err(|error| WireError::Unaddressable(error.to_string()))?;

    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => {
            return Err(WireError::Unaddressable(format!(
                "`{other}` is not an HTTP or WebSocket scheme"
            )));
        }
    };
    url.set_scheme(scheme)
        .map_err(|()| WireError::Unaddressable("the URL scheme cannot be changed".to_owned()))?;
    Ok(url.to_string())
}

/// Opens one authenticated WebSocket, presenting `token` as a bearer
/// credential.
///
/// # Errors
///
/// Returns [`WireError`] if the URL is not one tungstenite can request, the
/// token is not a legal header value, or the handshake failed.
pub(crate) async fn connect_bearer(url: &str, token: &str) -> Result<Socket, WireError> {
    let mut request = url
        .into_client_request()
        .map_err(|error| WireError::Unaddressable(error.to_string()))?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| WireError::Unaddressable("the token is not a header value".to_owned()))?,
    );

    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| WireError::Unwelcome(error.to_string()))?;
    Ok(socket)
}

/// Everything needed to reach one session's room.
#[derive(Clone)]
pub struct Endpoint {
    /// `wss://…/v1/sessions/{id}/relay/daemon`.
    url: String,
    /// The session's `fd_` daemon token.
    token: String,
    /// The session this daemon serves.
    session: SessionId,
}

impl core::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &self.url)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    /// Derives the relay endpoint from the control plane's base URL.
    ///
    /// `http`/`https` become `ws`/`wss`: the configuration names one control
    /// plane, and the daemon should not have to be told its address twice in
    /// two schemes.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::Unaddressable`] if the base URL cannot address
    /// the relay route.
    pub fn from_base(
        base: &url::Url,
        session: SessionId,
        token: String,
    ) -> Result<Self, WireError> {
        Ok(Self {
            url: websocket_url(base, &format!("v1/sessions/{session}/relay/daemon"))?,
            token,
            session,
        })
    }

    /// Opens and handshakes one connection.
    async fn connect(&self) -> Result<Socket, WireError> {
        let mut socket = connect_bearer(&self.url, &self.token).await?;

        send(
            &mut socket,
            &DaemonToControl::Hello {
                protocol_version: WIRE_PROTOCOL_VERSION,
                session: self.session,
            },
        )
        .await?;

        // Nothing is pumped until the room has welcomed this daemon: a
        // version or session mismatch closes the socket, and frames sent
        // into a socket that is about to close are frames the session lost.
        match next_frame(&mut socket).await? {
            Some(ControlToDaemon::Welcome) => {
                tracing::info!(session = %self.session, "the session room welcomed this daemon");
                Ok(socket)
            }
            Some(other) => Err(WireError::Unwelcome(format!(
                "the first frame was {other:?}"
            ))),
            None => Err(WireError::Unwelcome(
                "the room closed the socket during the handshake".to_owned(),
            )),
        }
    }
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

/// Sends one frame.
///
/// Generic over the frame type because flyco holds two of these sockets: a
/// session's daemon speaks [`DaemonToControl`] and an enrolled host speaks
/// `HostToControl`, and nothing else about writing one differs.
pub(crate) async fn send<T: serde::Serialize>(
    socket: &mut Socket,
    frame: &T,
) -> Result<(), WireError> {
    let json = serde_json::to_string(frame).expect("every wire frame serializes to JSON");
    socket
        .send(Message::Text(Utf8Bytes::from(json)))
        .await
        .map_err(|error| WireError::Unwelcome(error.to_string()))
}

/// Reads the next command, skipping anything that is not a text frame.
///
/// `None` means the socket ended; the caller reconnects.
pub(crate) async fn next_frame<T: serde::de::DeserializeOwned>(
    socket: &mut Socket,
) -> Result<Option<T>, WireError> {
    while let Some(message) = socket.next().await {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(%error, "the relay socket failed");
                return Ok(None);
            }
        };
        match message {
            Message::Text(text) => {
                return serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|error| WireError::Undecodable(error.to_string()));
            }
            Message::Close(_) => return Ok(None),
            // Pings are answered by tungstenite; binary frames are not part
            // of this protocol and are ignored rather than fatal, so a
            // future control plane can add one without stopping old daemons.
            other => tracing::debug!(kind = ?other, "ignoring a non-text relay frame"),
        }
    }
    Ok(None)
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
        HarnessEvent::UsageLimited { resets_at_unix } => Some(HarnessObservation {
            observed_cost: None,
            rate_limit: Some(RateLimitObservation {
                resets_at_unix: *resets_at_unix,
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
struct Outbound {
    frame: DaemonToControl,
    harness_approval: Option<ApprovalId>,
}

async fn collect<A: ControlApi>(
    mut outputs: mpsc::Receiver<SessionOutput>,
    queue: mpsc::Sender<Outbound>,
    api: A,
) -> Result<(), WireError> {
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
                (DaemonToControl::Harness { event }, None)
            }
            SessionOutput::ApprovalRequest {
                id: harness_id,
                tool,
                input,
                ..
            } => {
                let payload = flyco_core::wire::ApprovalPayload::ToolUse { tool, input };
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

        queue
            .try_send(Outbound {
                frame,
                harness_approval,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => WireError::QueueOverflow,
                mpsc::error::TrySendError::Closed(_) => {
                    WireError::Harness("the relay connection stopped".to_owned())
                }
            })?;
    }
    Ok(())
}

/// The socket half of the relay.
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

struct Connection<S, T, A, W, D, H> {
    session: S,
    terminal: T,
    terminal_out: mpsc::Receiver<String>,
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
    workdir: W,
    /// The read-only view of the checkout the `Files` and `Diff` tabs are
    /// served from.
    checkout: Checkout,
    /// Where an answered workdir question is handed back to the pump.
    ///
    /// Its own channel rather than the harness's outbound queue, because
    /// that queue closing is how the pump learns the harness stopped — a
    /// second sender kept for replies would hold it open for ever.
    reply_out: mpsc::Sender<DaemonToControl>,
    replies: mpsc::Receiver<DaemonToControl>,
    repo_status: mpsc::UnboundedReceiver<String>,
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
    /// minutes ago and on a socket that may since have dropped, and a
    /// replacement machine with no id to resume opens a new conversation
    /// instead of continuing this one.
    harness_session_id: Option<String>,
    endpoint: Endpoint,
    /// Whether a budget pause has stopped this session accepting work.
    ///
    /// Cleared by exactly one thing, and never by the daemon's own
    /// judgement: [`ControlToDaemon::BudgetRaised`], which is the control
    /// plane reporting that the user gave the session more money than it
    /// has spent. What ends a budget pause is a decision, not a timeout.
    paused: bool,
    tree: Tree,
    /// When to auto-continue after a usage limit, if one is in force.
    continue_at: Option<tokio::time::Instant>,
    /// REST-assigned approval id → harness-native id.
    approvals: BTreeMap<ApprovalId, ApprovalId>,
    /// The machine notice waiting to ride on the session's first message.
    ///
    /// Taken once. Everything after the first message is a conversation the
    /// agent is already in, and repeating what machine it is on would be
    /// noise it has to read every turn — `machine_status` is there for when
    /// it wants to know.
    opening: Option<String>,
    /// How this connection proves its socket is still there.
    keepalive: Keepalive,
}

/// Why one connection ended.
enum Ended {
    /// The socket dropped; reconnect.
    Disconnected,
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

impl<S: HarnessSession, T: TerminalSession, A: ControlApi, W: WorkingTree, D: Disk, H: Shell>
    Connection<S, T, A, W, D, H>
{
    /// Writes one frame, parking it for the next connection if the socket
    /// dies mid-write.
    ///
    /// `false` means the socket is gone. The frame is never dropped on the
    /// way out: a send that failed hands it to `in_flight`, and the next
    /// connection starts by writing it — without that, a socket dying
    /// mid-write silently truncates the tail a browser replays from.
    async fn relay(
        frame: DaemonToControl,
        socket: &mut Socket,
        in_flight: &mut Option<DaemonToControl>,
    ) -> bool {
        if let Err(error) = send(socket, &frame).await {
            tracing::warn!(
                %error,
                "a frame did not reach the room; retrying it on the next connection"
            );
            *in_flight = Some(frame);
            return false;
        }
        true
    }

    /// Writes one heartbeat, and says whether the socket is worth keeping.
    ///
    /// `false` means abandon it: either the room has been silent for longer
    /// than [`Keepalive::silence_limit`] — a path that no longer carries
    /// packets, whatever the socket still claims — or the write itself
    /// failed. Both are the same instruction to the caller, because both
    /// are answered the same way: drop it and dial again.
    async fn beat(&self, socket: &mut Socket, last_heard: tokio::time::Instant) -> bool {
        let silent_for = last_heard.elapsed();
        if silent_for > self.keepalive.silence_limit() {
            tracing::warn!(
                ?silent_for,
                "the session room stopped answering; abandoning the socket"
            );
            return false;
        }
        if let Err(error) = send(socket, &DaemonToControl::Heartbeat).await {
            tracing::warn!(%error, "a heartbeat did not reach the room");
            return false;
        }
        true
    }

    /// Pumps one connection until it ends.
    ///
    /// `in_flight` holds the one frame that has left the queue but has not
    /// been written yet. A frame is only dropped once the socket accepted
    /// it: a send that fails hands the frame back, and the next connection
    /// starts by writing it. Without that slot, a socket dying mid-write
    /// silently truncates the room's stored tail — which is exactly what a
    /// browser replays from.
    async fn pump(
        &mut self,
        socket: &mut Socket,
        queue: &mut mpsc::Receiver<Outbound>,
        in_flight: &mut Option<DaemonToControl>,
    ) -> Result<Ended, WireError> {
        if let Some(frame) = in_flight.take()
            && let Err(error) = send(socket, &frame).await
        {
            tracing::warn!(%error, "a retried frame did not reach the room");
            *in_flight = Some(frame);
            return Ok(Ended::Disconnected);
        }

        // The socket has just been greeted, so it counts as heard from now:
        // a connection is never abandoned for silence it predates.
        let mut last_heard = tokio::time::Instant::now();
        let mut heartbeat = tokio::time::interval(self.keepalive.interval());
        // The first tick is immediate and a `Hello` has just been written.
        heartbeat.tick().await;

        loop {
            tokio::select! {
                outbound = queue.recv(), if self.alive.harness.armed() => {
                    if let Some(ending) = self.on_outbound(socket, outbound, in_flight).await? {
                        return Ok(ending);
                    }
                }
                inbound = next_frame(socket) => {
                    let Some(command) = inbound? else {
                        return Ok(Ended::Disconnected);
                    };
                    // Any frame at all proves the path is live, so the
                    // deadline is reset here rather than only on a
                    // heartbeat answer: a busy turn is its own keepalive.
                    last_heard = tokio::time::Instant::now();
                    if matches!(self.dispatch_before(command).await?, Ended::Archived) {
                        return Ok(Ended::Archived);
                    }
                }
                _ = heartbeat.tick() => {
                    if !self.beat(socket, last_heard).await {
                        return Ok(Ended::Disconnected);
                    }
                }
                output = self.terminal_out.recv() => {
                    let Some(data) = output else {
                        return Ok(Ended::Disconnected);
                    };
                    let frame = DaemonToControl::TerminalOutput { data };
                    if !Self::relay(frame, socket, in_flight).await {
                        return Ok(Ended::Disconnected);
                    }
                }
                update = self.shell_updates.recv() => {
                    // This connection holds a sender of its own, so the
                    // channel outlives every run and never closes.
                    let update = update.expect("the relay holds the shell's own sender");
                    let frame = self.shell_frame(update);
                    if !Self::relay(frame, socket, in_flight).await {
                        return Ok(Ended::Disconnected);
                    }
                }
                () = tokio::time::sleep_until(self.continue_at.unwrap_or_else(tokio::time::Instant::now)), if self.continue_at.is_some() => {
                    self.continue_at = None;
                    if !self.paused && !self.reclaiming {
                        self.session
                            .send_user_message(USAGE_RESET_NOTICE.to_owned())
                            .await
                            .map_err(harness)?;
                    }
                }
                notice = self.spot.recv(), if self.alive.spot_watch.armed() => {
                    self.alive.spot_watch = Producing::No;
                    let Some(notice) = notice else {
                        // No watcher: this machine holds capacity nobody
                        // can reclaim.
                        continue;
                    };
                    if let Err(error) = self.reclaim(socket, queue, in_flight, notice).await {
                        // Whatever failed, the machine is still going. The
                        // relay keeps its socket rather than tearing down
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
                    self.on_stop(socket, queue, in_flight, reason).await;
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
                    if !Self::relay(frame, socket, in_flight).await {
                        return Ok(Ended::Disconnected);
                    }
                }
                summary = self.repo_status.recv(), if self.alive.repo_watch.armed() => {
                    let Some(summary) = summary else {
                        self.alive.repo_watch = Producing::No;
                        continue;
                    };
                    self.note_tree(&summary);
                    let frame = DaemonToControl::RepoDirty { summary };
                    if let Err(error) = send(socket, &frame).await {
                        tracing::warn!(%error, "a repo-status frame did not reach the room; retrying it on the next connection");
                        *in_flight = Some(frame);
                        return Ok(Ended::Disconnected);
                    }
                }
            }
        }
    }

    /// Records what the working tree looks like now.
    ///
    /// An episode of dirtiness is one episode: a tree that was already
    /// dirty and has been mentioned to the agent stays mentioned, so the
    /// nudge of [`Self::nudge_if_dirty`] is not repeated on every status
    /// poll while the agent works.
    fn note_tree(&mut self, summary: &str) {
        self.tree = if summary.trim().is_empty() {
            Tree::Clean
        } else {
            Tree::Dirty {
                noticed: matches!(self.tree, Tree::Dirty { noticed: true }),
            }
        };
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
        socket: &mut Socket,
        queue: &mut mpsc::Receiver<Outbound>,
        in_flight: &mut Option<DaemonToControl>,
        reason: StopReason,
    ) {
        match tokio::time::timeout(
            crate::stop::GRACE,
            self.stopping(socket, queue, in_flight, reason),
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

    /// Relays one frame the harness produced, or reacts to it stopping.
    ///
    /// `Ok(None)` is the ordinary case: the frame went out and the pump
    /// keeps going. `Ok(Some(_))` ends the connection.
    async fn on_outbound(
        &mut self,
        socket: &mut Socket,
        outbound: Option<Outbound>,
        in_flight: &mut Option<DaemonToControl>,
    ) -> Result<Option<Ended>, WireError> {
        let Some(outbound) = outbound else {
            self.alive.harness = Producing::No;
            if matches!(self.tree, Tree::Dirty { .. }) && !self.paused {
                tracing::info!("the harness stopped on a dirty tree; keeping the session awake");
                return Ok(None);
            }
            return Ok(Some(Ended::HarnessStopped));
        };

        if let Some(harness_id) = outbound.harness_approval {
            let DaemonToControl::ApprovalRequest { id, .. } = &outbound.frame else {
                return Err(WireError::Harness(
                    "an approval pairing was attached to a non-approval frame".to_owned(),
                ));
            };
            self.approvals.insert(*id, harness_id);
        }
        self.remember(&outbound.frame);
        let completed_dirty = matches!(
            &outbound.frame,
            DaemonToControl::Harness {
                event: HarnessEvent::TurnCompleted { .. },
            }
        );
        if let Err(error) = send(socket, &outbound.frame).await {
            tracing::warn!(
                %error,
                "a frame did not reach the room; retrying it on the next connection"
            );
            *in_flight = Some(outbound.frame);
            return Ok(Some(Ended::Disconnected));
        }
        if completed_dirty {
            self.nudge_if_dirty().await?;
        }
        self.schedule_usage_continue(&outbound.frame);
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
    /// The socket is then held open until the machine dies. There is
    /// nothing left to send on it and no reason to close it: a daemon that
    /// disconnected cleanly would look like one that is coming back.
    async fn reclaim(
        &mut self,
        socket: &mut Socket,
        queue: &mut mpsc::Receiver<Outbound>,
        in_flight: &mut Option<DaemonToControl>,
        notice: SpotNotice,
    ) -> Result<(), WireError> {
        tracing::warn!(
            seconds_remaining = notice.seconds_remaining,
            "this machine's capacity is being reclaimed; saving the session"
        );
        self.quiesce(socket, queue, in_flight).await?;

        if let Err(error) = self.disk.sync().await {
            // Not fatal, and not a reason to skip the notice: the seconds
            // left are better spent telling the control plane than dying
            // over a flush that may well have happened anyway.
            tracing::error!(%error, "the filesystem could not be flushed before reclamation");
        }

        self.api
            .report_spot_notice(notice.seconds_remaining)
            .await?;
        send(
            socket,
            &DaemonToControl::SpotNotice {
                seconds_remaining: notice.seconds_remaining,
            },
        )
        .await
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
        socket: &mut Socket,
        queue: &mut mpsc::Receiver<Outbound>,
        in_flight: &mut Option<DaemonToControl>,
    ) -> Result<(), WireError> {
        // Before anything else, so a user message that arrives during the
        // flush is refused rather than opening a turn nothing will record.
        self.reclaiming = true;

        self.session.interrupt().await.map_err(harness)?;
        self.session.flush().await.map_err(harness)?;
        self.drain(socket, queue, in_flight).await;
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
    /// 2. **Write the workdir patch.** Every uncommitted change including
    ///    untracked files, in the same format an automatic archive stores
    ///    and a fresh machine replays onto its clone. This is the whole of
    ///    the user's unpushed work, and it is written *after* the flush so
    ///    it is not competing with it for the seconds available.
    /// 3. **Report.** `POST /v1/sessions/{id}/stopping`, awaited, which is
    ///    what makes the stop true for the control plane. Last on purpose:
    ///    it must not be true before the work is safe.
    ///
    /// A clean tree writes no patch and says so. That is not a failure — it
    /// is a session whose agent committed everything — and storing an empty
    /// patch would leave the next machine replaying nothing.
    async fn stopping(
        &mut self,
        socket: &mut Socket,
        queue: &mut mpsc::Receiver<Outbound>,
        in_flight: &mut Option<DaemonToControl>,
        reason: StopReason,
    ) -> Result<(), WireError> {
        self.quiesce(socket, queue, in_flight).await?;

        if let Some(patch) = self.workdir.snapshot().await? {
            let bytes = patch.len();
            self.api.put_workdir_patch(patch).await?;
            tracing::warn!(
                bytes,
                "stored this session's uncommitted work before the container went"
            );
        } else {
            tracing::info!(
                "the checkout has nothing uncommitted; the next machine needs only the clone"
            );
        }

        self.api.report_stopping(reason).await?;
        Ok(())
    }

    /// Writes everything already queued into the socket.
    ///
    /// What makes the flush a *whole* one: the harness's output reaches the
    /// room through a queue the pump drains one frame per loop, so a
    /// reclamation that only flushed the transcript store would leave the
    /// room's stored tail — which is what a browser replays — short by
    /// whatever was still queued.
    ///
    /// A frame that will not send is kept for a connection that is not
    /// coming, which is the honest thing to do with it: the room's tail is
    /// the loss, and the transcript itself is already in object storage.
    async fn drain(
        &mut self,
        socket: &mut Socket,
        queue: &mut mpsc::Receiver<Outbound>,
        in_flight: &mut Option<DaemonToControl>,
    ) {
        if let Some(frame) = in_flight.take()
            && let Err(error) = send(socket, &frame).await
        {
            tracing::warn!(%error, "a retried frame did not reach the room before reclamation");
            *in_flight = Some(frame);
            return;
        }
        while let Ok(outbound) = queue.try_recv() {
            self.remember(&outbound.frame);
            if let Err(error) = send(socket, &outbound.frame).await {
                tracing::warn!(%error, "a frame did not reach the room before reclamation");
                *in_flight = Some(outbound.frame);
                return;
            }
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
        match tokio::time::timeout(self.keepalive.harness_deadline(), self.dispatch(command)).await
        {
            Ok(dispatched) => dispatched,
            Err(_elapsed) => Err(WireError::Harness(format!(
                "the agent did not take `{named}` within {:?}; it has stopped accepting commands",
                self.keepalive.harness_deadline()
            ))),
        }
    }

    async fn dispatch(&mut self, command: ControlToDaemon) -> Result<Ended, WireError> {
        match command {
            ControlToDaemon::Welcome => {
                tracing::debug!("the room welcomed an already-welcomed daemon");
            }
            ControlToDaemon::Heartbeat => {
                // Nothing to do: arriving at all is the whole of what this
                // frame carries, and the pump has already taken that.
            }
            ControlToDaemon::UserMessage { text } => {
                if self.refuse_while_paused("a user message")
                    || self.refuse_while_reclaiming("a user message")
                {
                    return Ok(Ended::Disconnected);
                }
                // The opening notice rides on the first message rather than
                // arriving as one of its own: the agent must know what
                // machine it is on before it starts working, and a message
                // carrying only that would open a turn about nothing.
                let text = match self.opening.take() {
                    Some(notice) => OpeningMessage { notice, text }
                        .render()
                        .map_err(|error| notice_failed(&error))?,
                    None => text,
                };
                self.session
                    .send_user_message(text)
                    .await
                    .map_err(harness)?;
            }
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
            ControlToDaemon::SetModel { model } => {
                // Refused on the same terms as a compaction: both reach the
                // harness, and a session that has stopped accepting work or
                // is about to lose its machine has no harness to reach.
                // The control plane has already recorded the model, so the
                // change is redelivered on the next `Hello` rather than
                // lost — `survives_a_disconnect` is what makes that true.
                if self.refuse_while_paused("a model change")
                    || self.refuse_while_reclaiming("a model change")
                {
                    return Ok(Ended::Disconnected);
                }
                self.session.set_model(model).await.map_err(harness)?;
            }
            ControlToDaemon::TerminalInput { data } => {
                if self.refuse_while_paused("terminal input") {
                    return Ok(Ended::Disconnected);
                }
                self.terminal.write(&data)?;
            }
            ControlToDaemon::ApprovalDecision { id, decision } => {
                self.decide_approval(id, decision).await?;
            }
            ControlToDaemon::Budget { signal } => self.budget(signal).await?,
            ControlToDaemon::BudgetRaised { limit } => self.budget_raised(limit).await?,
            ControlToDaemon::MachineChanged {
                machine_type,
                hourly,
                spot,
                restarted,
            } => {
                // Told rather than discovered: a resize restarts the machine
                // and kills this process, so the daemon reading this is a
                // new one whose configuration still describes the machine
                // the session booted on. The size and any licence minimum
                // are deliberately not on the wire — the notice says what
                // the machine is now and what the restart cost, and
                // `machine_status` is where the full description is read
                // from, live.
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
                    .await?;
            }
            ControlToDaemon::InspectWorkdir { id, request } => self.answer_workdir(id, request),
            ControlToDaemon::Archive { preserve_workdir } => {
                if preserve_workdir && let Some(patch) = self.workdir.snapshot().await? {
                    self.api.put_workdir_patch(patch).await?;
                }
                tracing::info!(session = %self.endpoint.session, "the control plane archived this session");
                self.terminal.shutdown()?;
                return Ok(Ended::Archived);
            }
        }
        Ok(Ended::Disconnected)
    }

    /// Reads the checkout for a browser, on a task of its own.
    ///
    /// Off the pump because a diff runs git over the whole tree, and the
    /// socket has a harness stream to carry while it does. Read-only, so
    /// nothing about it depends on what else the session is doing — a
    /// paused or reclaiming session still shows the user its files.
    fn answer_workdir(&self, id: WorkdirRequestId, request: WorkdirRequest) {
        let checkout = self.checkout.clone();
        let replies = self.reply_out.clone();
        tokio::spawn(async move {
            let reply = checkout.inspect(request).await;
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

    /// Tells the agent it may not stop while the tree is dirty.
    async fn nudge_if_dirty(&mut self) -> Result<(), WireError> {
        if !self.paused && !self.reclaiming && matches!(self.tree, Tree::Dirty { noticed: false }) {
            self.tree = Tree::Dirty { noticed: true };
            self.session
                .send_user_message(DIRTY_NOTICE.to_owned())
                .await
                .map_err(harness)?;
        }
        Ok(())
    }

    fn schedule_usage_continue(&mut self, frame: &DaemonToControl) {
        let DaemonToControl::Harness {
            event:
                HarnessEvent::UsageLimited {
                    resets_at_unix: Some(unix),
                },
        } = frame
        else {
            return;
        };
        let wait = unix.saturating_sub(now_unix());
        self.continue_at = Some(tokio::time::Instant::now() + Duration::from_secs(wait));
    }

    /// Hands the user's decision to the tool call waiting on it.
    async fn decide_approval(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), WireError> {
        let Some(harness_id) = self.approvals.remove(&id) else {
            // Not every approval is a harness tool call waiting on a
            // permission. The daemon's own MCP server raises one for a
            // license-bound resize, and the control plane performs *that*
            // itself; the decision still reaches every daemon because the
            // room echoes it. Nothing here is blocked on it, so it is noted
            // rather than treated as a protocol violation.
            tracing::debug!(%id, ?decision, "a decided approval was not a harness tool call");
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

/// Everything [`run`] needs to drive one session.
pub struct SessionRelay<S, A, T, W, D, H> {
    /// Where the daemon connects.
    pub endpoint: Endpoint,
    /// The live harness handle.
    pub session: S,
    /// Harness output, consumed exactly once.
    pub outputs: mpsc::Receiver<SessionOutput>,
    /// REST client for durable writes.
    pub api: A,
    /// The web terminal.
    pub terminal: T,
    /// Bytes the terminal produces.
    pub terminal_out: mpsc::Receiver<String>,
    /// Runs the composer's `!` commands on this machine.
    pub shell: H,
    /// Snapshot handle for the checkout.
    pub workdir: W,
    /// The same checkout, read-only, for the `Files` and `Diff` tabs.
    pub checkout: Checkout,
    /// `git status --short` summaries as they change.
    pub repo_status: mpsc::UnboundedReceiver<String>,
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
    /// How this relay keeps its socket alive and notices when it is not.
    pub keepalive: Keepalive,
}

impl<S, A, T, W, D, H> core::fmt::Debug for SessionRelay<S, A, T, W, D, H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionRelay")
            .field("endpoint", &self.endpoint)
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
/// accepting commands. A dropped socket is not an error: it is reconnected.
pub async fn run<S, A, T, W, D, H>(relay: SessionRelay<S, A, T, W, D, H>) -> Result<(), WireError>
where
    S: HarnessSession + 'static,
    A: ControlApi + Clone,
    T: TerminalSession + 'static,
    W: WorkingTree + 'static,
    D: Disk,
    H: Shell,
{
    let (sender, mut queue) = mpsc::channel(QUEUE_DEPTH);
    let collector = tokio::spawn(collect(relay.outputs, sender, relay.api.clone()));

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
        shell: relay.shell,
        shell_updates,
        shell_reports,
        running_shell: None,
        api: relay.api,
        workdir: relay.workdir,
        checkout: relay.checkout,
        reply_out,
        replies,
        repo_status: relay.repo_status,
        disk: relay.disk,
        spot: relay.spot,
        stops: relay.stops,
        reclaiming: false,
        harness_session_id: None,
        endpoint: relay.endpoint,
        paused: false,
        tree: Tree::Clean,
        alive: Alive {
            harness: Producing::Yes,
            repo_watch: Producing::Yes,
            spot_watch: Producing::Yes,
            stop_watch: Producing::Yes,
        },
        continue_at: None,
        approvals: BTreeMap::new(),
        opening: Some(opening),
        keepalive: relay.keepalive,
    };
    let mut attempt = 0_u32;
    let mut in_flight = None;
    // The last stage of the provisioning timeline (docs/ux.md §9.2). The
    // control plane can watch a machine be reserved and boot but has no way
    // onto it, so "the agent is up" is a fact only this process holds: the
    // harness has already started by the time `run` is called, and the room
    // has just welcomed the socket. Announced once, not on every reconnect
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

        match connection.endpoint.connect().await {
            Ok(mut socket) => {
                attempt = 0;
                if !ready_announced {
                    // A failure here is a dropped socket, which the loop is
                    // already built to survive: the announcement is a line
                    // in a timeline, and retrying it on the next connection
                    // would date it to the reconnect rather than to when
                    // the agent actually came up.
                    if let Err(error) = send(
                        &mut socket,
                        &DaemonToControl::ProvisioningStage {
                            stage: ProvisioningStage::Ready,
                            at_unix: now_unix(),
                        },
                    )
                    .await
                    {
                        tracing::warn!(%error, "the agent-ready stage did not reach the room");
                    }
                    ready_announced = true;
                }
                match connection
                    .pump(&mut socket, &mut queue, &mut in_flight)
                    .await
                {
                    Ok(Ended::Disconnected) => {
                        tracing::warn!("the session room disconnected; reconnecting");
                    }
                    Ok(ending) => break Ok(ending),
                    Err(error) => break Err(error),
                }
            }
            Err(error) => {
                let wait = backoff(attempt);
                tracing::warn!(%error, ?wait, attempt, "could not reach the session room");
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(wait).await;
            }
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
/// socket is reconnected by the loop rather than ending the run.
fn say_goodbye(ending: &Ended) {
    match ending {
        Ended::Archived => tracing::info!("session archived; flycod is done"),
        Ended::HarnessStopped => tracing::info!("the harness stopped; flycod is done"),
        Ended::Stopped => tracing::info!("the platform stopped this machine; flycod is done"),
        Ended::Disconnected => unreachable!("a disconnect reconnects rather than ending the run"),
    }
}
