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

use flyco_core::{
    ApprovalDecision, BudgetSignal, ControlToDaemon, DaemonToControl, HarnessEvent,
    HarnessObservation, RateLimitObservation, SessionId, WIRE_PROTOCOL_VERSION,
};
use futures_util::{SinkExt as _, StreamExt as _};
use rand::Rng as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::control::rest::{ControlApi, ControlApiError};
use crate::harness::{HarnessSession, SessionOutput, ToolApproval};

/// How many frames may wait for a socket that is not there.
///
/// Sized for a reconnect, not for an outage: at the ~50 frames a second a
/// busy turn produces, this is roughly twenty seconds of disconnection —
/// comfortably past the backoff's first few attempts and far short of a
/// heap that matters. Overflowing it is a fatal error.
pub const QUEUE_DEPTH: usize = 1024;

/// Shortest wait before a reconnect attempt.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Longest wait between reconnect attempts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// What the harness is told when a budget threshold is crossed.
///
/// The agent decides how to spend its budget, so a threshold is information
/// it acts on rather than a limit imposed on it — it arrives as a message in
/// the conversation, marked so the model can tell it from the user.
/// [`BudgetSignal::Pause`] is the exception: that one is enforced, not
/// announced.
const BUDGET_NOTICE_PREFIX: &str = "[flyco budget notice]";

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
}

/// The socket type a connected daemon holds.
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
        let mut url = base
            .join(&format!("v1/sessions/{session}/relay/daemon"))
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
        url.set_scheme(scheme).map_err(|()| {
            WireError::Unaddressable("the URL scheme cannot be changed".to_owned())
        })?;

        Ok(Self {
            url: url.to_string(),
            token,
            session,
        })
    }

    /// Opens and handshakes one connection.
    async fn connect(&self) -> Result<Socket, WireError> {
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|error| WireError::Unaddressable(error.to_string()))?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().map_err(|_| {
                WireError::Unaddressable("the daemon token is not a header value".to_owned())
            })?,
        );

        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|error| WireError::Unwelcome(error.to_string()))?;

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

/// Sends one frame.
async fn send(socket: &mut Socket, frame: &DaemonToControl) -> Result<(), WireError> {
    let json = serde_json::to_string(frame).expect("every wire frame serializes to JSON");
    socket
        .send(Message::Text(Utf8Bytes::from(json)))
        .await
        .map_err(|error| WireError::Unwelcome(error.to_string()))
}

/// Reads the next command, skipping anything that is not a text frame.
///
/// `None` means the socket ended; the caller reconnects.
async fn next_frame(socket: &mut Socket) -> Result<Option<ControlToDaemon>, WireError> {
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
/// restarted control plane from reconnecting in the same instant.
fn backoff(attempt: u32) -> Duration {
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
async fn collect<A: ControlApi>(
    mut outputs: mpsc::Receiver<SessionOutput>,
    queue: mpsc::Sender<DaemonToControl>,
    api: A,
) -> Result<(), WireError> {
    while let Some(output) = outputs.recv().await {
        let frame = match output {
            SessionOutput::Started { session_id } => DaemonToControl::Started {
                harness_session_id: session_id,
            },
            SessionOutput::Capabilities { capabilities } => {
                DaemonToControl::Capabilities { capabilities }
            }
            SessionOutput::Event { event } => {
                if let Some(observation) = observation_in(&event)
                    && let Err(error) = api.record_observation(observation).await
                {
                    tracing::debug!(%error, "a usage observation was not recorded");
                }
                DaemonToControl::Harness { event }
            }
            SessionOutput::ApprovalRequest { tool, input, .. } => {
                let payload = flyco_core::wire::ApprovalPayload::ToolUse { tool, input };
                let id = api.raise_approval(payload.clone()).await?;
                DaemonToControl::ApprovalRequest { id, payload }
            }
            SessionOutput::Fatal { error } => {
                // The harness is done; the room learns why through the
                // turn's own failure event, and the daemon stops.
                tracing::error!(error, "the harness session ended fatally");
                return Ok(());
            }
        };

        queue.try_send(frame).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => WireError::QueueOverflow,
            mpsc::error::TrySendError::Closed(_) => {
                WireError::Harness("the relay connection stopped".to_owned())
            }
        })?;
    }
    Ok(())
}

/// The socket half of the relay.
struct Connection<S> {
    session: S,
    endpoint: Endpoint,
    /// Whether a budget pause has stopped this session accepting work.
    ///
    /// Set once and never cleared: a paused session is resumed by the
    /// control plane provisioning a new one, not by the daemon deciding the
    /// pause is over.
    paused: bool,
}

/// Why one connection ended.
enum Ended {
    /// The socket dropped; reconnect.
    Disconnected,
    /// The control plane archived the session; stop.
    Archived,
    /// The queue ended because the harness stopped.
    HarnessStopped,
}

impl<S: HarnessSession> Connection<S> {
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
        queue: &mut mpsc::Receiver<DaemonToControl>,
        in_flight: &mut Option<DaemonToControl>,
    ) -> Result<Ended, WireError> {
        if let Some(frame) = in_flight.take()
            && let Err(error) = send(socket, &frame).await
        {
            tracing::warn!(%error, "a retried frame did not reach the room");
            *in_flight = Some(frame);
            return Ok(Ended::Disconnected);
        }

        loop {
            tokio::select! {
                outbound = queue.recv() => {
                    let Some(frame) = outbound else {
                        return Ok(Ended::HarnessStopped);
                    };
                    if let Err(error) = send(socket, &frame).await {
                        tracing::warn!(%error, "a frame did not reach the room; retrying it on the next connection");
                        *in_flight = Some(frame);
                        return Ok(Ended::Disconnected);
                    }
                }
                inbound = next_frame(socket) => {
                    let Some(command) = inbound? else {
                        return Ok(Ended::Disconnected);
                    };
                    if matches!(self.dispatch(command).await?, Ended::Archived) {
                        return Ok(Ended::Archived);
                    }
                }
            }
        }
    }

    /// Acts on one command from the control plane.
    async fn dispatch(&mut self, command: ControlToDaemon) -> Result<Ended, WireError> {
        match command {
            ControlToDaemon::Welcome => {
                tracing::debug!("the room welcomed an already-welcomed daemon");
            }
            ControlToDaemon::UserMessage { text } => {
                if self.refuse_while_paused("a user message") {
                    return Ok(Ended::Disconnected);
                }
                self.session
                    .send_user_message(text)
                    .await
                    .map_err(harness)?;
            }
            ControlToDaemon::Interrupt => self.session.interrupt().await.map_err(harness)?,
            ControlToDaemon::TerminalInput { data } => {
                if self.refuse_while_paused("terminal input") {
                    return Ok(Ended::Disconnected);
                }
                // The web terminal is a later milestone; refusing to pretend
                // beats swallowing the user's keystrokes.
                tracing::warn!(
                    bytes = data.len(),
                    "dropped terminal input: this build has no web terminal"
                );
            }
            ControlToDaemon::ApprovalDecision { id, decision } => {
                let answer = match decision {
                    ApprovalDecision::Approved => ToolApproval::Allow {
                        id,
                        updated_input: None,
                    },
                    ApprovalDecision::Denied => ToolApproval::Deny {
                        id,
                        message: "Denied by the flyco user.".to_owned(),
                    },
                };
                self.session
                    .decide_approval(answer)
                    .await
                    .map_err(harness)?;
            }
            ControlToDaemon::Budget { signal } => self.budget(signal).await?,
            ControlToDaemon::Archive => {
                tracing::info!(session = %self.endpoint.session, "the control plane archived this session");
                return Ok(Ended::Archived);
            }
        }
        Ok(Ended::Disconnected)
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
}

fn harness(error: impl core::fmt::Display) -> WireError {
    WireError::Harness(error.to_string())
}

/// Drives a session from the control plane until it is archived or the
/// harness stops.
///
/// # Errors
///
/// Returns [`WireError`] if the relay queue overflows, the control plane
/// speaks a protocol this daemon cannot read, or the harness stops
/// accepting commands. A dropped socket is not an error: it is reconnected.
pub async fn run<S, A>(
    endpoint: Endpoint,
    session: S,
    outputs: mpsc::Receiver<SessionOutput>,
    api: A,
) -> Result<(), WireError>
where
    S: HarnessSession + 'static,
    A: ControlApi,
{
    let (sender, mut queue) = mpsc::channel(QUEUE_DEPTH);
    let collector = tokio::spawn(collect(outputs, sender, api));

    let mut connection = Connection {
        session,
        endpoint,
        paused: false,
    };
    let mut attempt = 0_u32;
    let mut in_flight = None;

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
    match ending {
        Ended::Archived => tracing::info!("session archived; flycod is done"),
        Ended::HarnessStopped => tracing::info!("the harness stopped; flycod is done"),
        Ended::Disconnected => unreachable!("a disconnect reconnects rather than ending the run"),
    }
    Ok(())
}
