//! The session's desktop: a virtual screen, its stream, and the hands the
//! agent and the user take turns holding.
//!
//! One supervisor thread owns the whole subsystem, the way
//! [`crate::terminal`] owns the PTY: the `Xvfb` child, the window manager,
//! the X11 connection input is injected through and the screen is captured
//! off, the AV1 encoder, and the unix socket [`crate::mcp`] reaches in
//! through. Everything that can block — an X round-trip, an encode, a
//! socket read — happens on that thread, so the connection's pump never
//! waits on a display.
//!
//! The shape of it:
//!
//! - The control plane says what the desktop *is*: [`DesktopSession::set_enabled`]
//!   turns the stack on and off, [`set_watching`](DesktopSession::set_watching)
//!   says whether anyone is looking,
//!   [`set_takeover`](DesktopSession::set_takeover) says whose hands are on
//!   it. Each is a level, not an event — a fresh attach re-states all three.
//! - The desktop reports what it *did*: [`DesktopEvent::State`] when the
//!   stack moves, [`DesktopEvent::Chunk`] each time the encoder produces a
//!   packet worth streaming, [`DesktopEvent::AgentActive`] when the agent
//!   touches the screen after a quiet spell.
//! - The agent's requests arrive over the socket ([`ipc`]): the supervisor
//!   answers them itself, refusing while the user is driving. The user's
//!   input arrives through [`DesktopSession::inject`], gated the same way
//!   in reverse — honoured only while takeover is held.
//!
//! Nothing here is a global: the socket path is derived from the session
//! id, the display number is whatever `Xvfb` claimed, and every queue is
//! bounded.

mod encode;
pub mod ipc;
mod keys;
mod x11;

use std::path::Path;
use std::process::Child;
use std::sync::mpsc::{RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use flyco_core::SessionId;
use flyco_core::wire::{DesktopInputEvent, DesktopStatus};
use tokio::sync::mpsc;

use crate::config::ComputerConfig;

/// How many desktop reports the pump may owe the room before the
/// supervisor waits on it.
///
/// Chunks never wait — a full queue drops the packet and forces the next
/// encode to a keyframe, because a dropped inter-frame decodes as
/// garbage until one. State reports are rare and small enough to block
/// for: the pump is alive and draining by construction, so a bounded
/// wait is a stall, never a deadlock.
const EVENT_DEPTH: usize = 64;

/// How often the supervisor services the agent socket while idle.
///
/// While a browser is watching, capture cadence services it sooner; this
/// is the worst-case latency an MCP request waits when nothing is being
/// encoded.
const SOCKET_SERVICE: Duration = Duration::from_millis(120);

/// How long the agent's hands must have been off the screen before the
/// next touch re-announces the desktop.
///
/// A turn that works on the desktop emits a burst of tool calls; the
/// browser needs the seam between "quiet" and "the agent is on the
/// screen", not a line per call.
const AGENT_QUIET: Duration = Duration::from_secs(30);

/// What the desktop reports up to the connection.
#[derive(Debug)]
pub enum DesktopEvent {
    /// The desktop's lifecycle moved.
    State {
        /// Where it is now.
        status: DesktopStatus,
        /// The sentence beside it, when there is one.
        detail: Option<String>,
    },
    /// One encoded temporal unit of the stream.
    ///
    /// `keyframe` marks a chunk a decoder can start from cold: it carries
    /// the sequence header and a full frame. The room replays from the
    /// newest one when a watcher joins.
    Chunk {
        /// Whether a decoder can start from this chunk.
        keyframe: bool,
        /// The encoded bytes.
        bytes: Vec<u8>,
    },
    /// The agent touched the screen after a quiet spell — the `Screen`
    /// panel opens itself on this.
    AgentActive,
}

/// What the connection may ask of the desktop.
///
/// A trait rather than the handle directly because the relay's tests
/// drive a recording fake; every method is a level-set, and every one
/// fails the same way when the supervisor is gone.
pub trait DesktopSession: Send {
    /// Turns the desktop stack on or off.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError`] if the supervisor thread has exited.
    fn set_enabled(&mut self, enabled: bool) -> Result<(), DesktopError>;

    /// Says whether anyone is watching the stream — the gate on capture
    /// and upload, not on the display itself.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError`] if the supervisor thread has exited.
    fn set_watching(&mut self, watching: bool) -> Result<(), DesktopError>;

    /// Says whether the user owns the screen. While held, agent requests
    /// are refused and [`Self::inject`] is honoured.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError`] if the supervisor thread has exited.
    fn set_takeover(&mut self, active: bool) -> Result<(), DesktopError>;

    /// Injects one batch of user input, honoured while takeover is held
    /// and dropped otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError`] if the supervisor thread has exited.
    fn inject(&mut self, events: Vec<DesktopInputEvent>) -> Result<(), DesktopError>;

    /// Forces the next encoded frame to be a keyframe — the recovery a
    /// dropped chunk or a fresh attach asks for.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError`] if the supervisor thread has exited.
    fn keyframe_now(&mut self) -> Result<(), DesktopError>;
}

/// The one way a desktop ask fails: the supervisor thread is gone, so no
/// level is being held any more.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the desktop supervisor is gone")]
pub struct DesktopError;

/// The production handle: one channel into the supervisor thread.
#[derive(Debug)]
pub struct Desktop {
    control: SyncSender<Supervision>,
}

impl DesktopSession for Desktop {
    fn set_enabled(&mut self, enabled: bool) -> Result<(), DesktopError> {
        self.send(Supervision::Enabled(enabled))
    }

    fn set_watching(&mut self, watching: bool) -> Result<(), DesktopError> {
        self.send(Supervision::Watching(watching))
    }

    fn set_takeover(&mut self, active: bool) -> Result<(), DesktopError> {
        self.send(Supervision::Takeover(active))
    }

    fn inject(&mut self, events: Vec<DesktopInputEvent>) -> Result<(), DesktopError> {
        self.send(Supervision::Inject(events))
    }

    fn keyframe_now(&mut self) -> Result<(), DesktopError> {
        self.send(Supervision::Keyframe)
    }
}

impl Desktop {
    fn send(&self, message: Supervision) -> Result<(), DesktopError> {
        self.control.send(message).map_err(|_| DesktopError)
    }
}

/// The orders a supervisor takes, beside the channel's own hang-up.
#[derive(Debug)]
enum Supervision {
    /// Turn the display stack on or off.
    Enabled(bool),
    /// Someone is or isn't watching the stream.
    Watching(bool),
    /// The user owns or handed back the screen.
    Takeover(bool),
    /// One batch of the user's input.
    Inject(Vec<DesktopInputEvent>),
    /// Make the next packet a keyframe.
    Keyframe,
}

/// Spawns the desktop supervisor.
///
/// Cheap to call for a session without a screen: the thread parks on its
/// control channel until the first [`Supervision::Enabled`] wakes it, so
/// a desktop the session never gets costs one idle thread and nothing
/// else.
pub fn spawn(
    config: &ComputerConfig,
    session: SessionId,
) -> (Desktop, mpsc::Receiver<DesktopEvent>) {
    let (control, supervised) = std::sync::mpsc::sync_channel(64);
    let (events, out) = mpsc::channel(EVENT_DEPTH);
    let spawn = std::thread::Builder::new()
        .name("flycod-desktop".to_owned())
        .spawn({
            let config = config.clone();
            move || {
                let socket_path = ipc::socket_path(session);
                drive(&supervised, &events, &config, &socket_path);
            }
        });
    if let Err(error) = spawn {
        // The handle is already dead — every ask on it reports
        // `DesktopError`, the same story the relay would have been told
        // had the thread started and then fallen over.
        tracing::warn!(%error, "the desktop supervisor could not start");
    }
    (Desktop { control }, out)
}

/// The supervisor's whole world.
///
/// `running` is `Some` exactly while the display stack is up; the flags
/// are the levels the control plane last set, held here because the room
/// can re-state them across attaches in any order.
struct Supervisor {
    /// The display, the encoder, and the agent socket — absent while the
    /// session has no screen.
    running: Option<Running>,
    /// Whether anyone is watching the stream.
    watching: bool,
    /// Whether the user owns the screen.
    takeover: bool,
    /// When the agent last drove the desktop — the `AgentActive` seam.
    last_agent_activity: Instant,
    /// Whether the next encode must be a keyframe.
    force_keyframe: bool,
}

/// A live display stack: the X11 connection, the encoder, and the
/// processes and socket that must be reaped when it goes down.
struct Running {
    /// The X server, capture, and input path.
    display: x11::Display,
    /// The AV1 encoder over that display. `None` after an encode failure
    /// the supervisor reported — the display and the agent's socket stay
    /// up, because the agent's hands do not need the stream to work.
    encoder: Option<encode::Encoder>,
    /// The socket the MCP process talks to.
    agent: ipc::Listener,
    /// The window manager, when the image ships one.
    wm: Option<Child>,
    /// When the next capture is due.
    next_frame: Instant,
}

/// The supervisor loop.
///
/// Parks on the control channel between captures, and between socket
/// polls while idle; never touches the async world except to push events
/// into the channel.
fn drive(
    control: &std::sync::mpsc::Receiver<Supervision>,
    events: &mpsc::Sender<DesktopEvent>,
    config: &ComputerConfig,
    socket_path: &Path,
) {
    let mut supervisor = Supervisor {
        running: None,
        watching: false,
        takeover: false,
        last_agent_activity: Instant::now()
            .checked_sub(AGENT_QUIET)
            .unwrap_or_else(Instant::now),
        force_keyframe: false,
    };
    if config.enabled {
        supervisor.apply(Supervision::Enabled(true), events, config, socket_path);
    }
    loop {
        // The wait is the shorter of "until the next frame is due" and
        // "until the socket wants another look". With nothing running it
        // is just the socket poll — an MCP request never waits longer
        // than SOCKET_SERVICE to be noticed.
        let wait = supervisor
            .running
            .as_ref()
            .filter(|_| supervisor.watching)
            .map_or(SOCKET_SERVICE, |running| {
                running
                    .next_frame
                    .saturating_duration_since(Instant::now())
                    .min(SOCKET_SERVICE)
            });
        match control.recv_timeout(wait) {
            Ok(order) => supervisor.apply(order, events, config, socket_path),
            Err(RecvTimeoutError::Timeout) => {}
            // Every sender is gone: the connection ended. The stack comes
            // down with the session.
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // What the pass learned, applied after the running borrow ends:
        // the agent was heard and answered, or the X server died.
        let (agent_acted, dead) = supervisor
            .running
            .as_mut()
            .map_or((false, false), |running| {
                // Agent requests are served whether or not anyone watches —
                // a screenshot is how the agent sees without a viewer.
                let acted = running.serve_agent(supervisor.takeover);
                if supervisor.watching {
                    running.capture_and_send(&mut supervisor.force_keyframe, events);
                }
                running.display.drain_log();
                (acted, running.reap(events))
            });
        if agent_acted {
            supervisor.touched(events);
        }
        if dead {
            // The display is gone, and what remains is not a desktop.
            if let Some(mut running) = supervisor.running.take() {
                running.stop();
            }
        }
    }
}

impl Supervisor {
    /// One control-plane order.
    fn apply(
        &mut self,
        order: Supervision,
        events: &mpsc::Sender<DesktopEvent>,
        config: &ComputerConfig,
        socket_path: &Path,
    ) {
        match order {
            Supervision::Enabled(true) if self.running.is_none() => {
                Self::report(events, DesktopStatus::Starting, None);
                match Running::start(config, socket_path) {
                    Ok(running) => {
                        self.running = Some(running);
                        Self::report(events, DesktopStatus::Ready, None);
                    }
                    Err(error) => {
                        Self::report(events, DesktopStatus::Failed, Some(error.to_string()));
                    }
                }
            }
            Supervision::Enabled(false) => {
                if let Some(mut running) = self.running.take() {
                    running.stop();
                }
            }
            Supervision::Watching(watching) => {
                // Rising edge resynchronizes the stream: whatever the
                // room kept, the first packet a watcher sees is a
                // keyframe it can start from.
                if watching && !self.watching {
                    self.force_keyframe = true;
                }
                self.watching = watching;
            }
            Supervision::Takeover(active) => self.takeover = active,
            Supervision::Inject(batch) => {
                if !self.takeover {
                    // Input outside a takeover is a race a released
                    // client lost — dropped, not an error.
                    tracing::trace!(
                        events = batch.len(),
                        "dropped desktop input without takeover"
                    );
                    return;
                }
                if let Some(running) = &mut self.running {
                    for event in &batch {
                        if let Err(error) = running.display.inject(event) {
                            tracing::warn!(%error, "desktop input could not be injected");
                        }
                    }
                }
            }
            Supervision::Keyframe => self.force_keyframe = true,
            // Enable while running, disable while down: the level is
            // already what was asked for.
            Supervision::Enabled(_) => {}
        }
    }

    /// Reports a state change to the connection.
    fn report(events: &mpsc::Sender<DesktopEvent>, status: DesktopStatus, detail: Option<String>) {
        if events
            .blocking_send(DesktopEvent::State { status, detail })
            .is_err()
        {
            tracing::trace!("a desktop state report had nowhere to go");
        }
    }

    /// Records that the agent touched the screen, announcing the seam
    /// after a quiet spell.
    fn touched(&mut self, events: &mpsc::Sender<DesktopEvent>) {
        let now = Instant::now();
        if now.duration_since(self.last_agent_activity) >= AGENT_QUIET
            && events.blocking_send(DesktopEvent::AgentActive).is_err()
        {
            tracing::trace!("an agent-activity report had nowhere to go");
        }
        self.last_agent_activity = now;
    }
}

impl Running {
    /// Brings the display stack up: Xvfb on a free display, the XTEST
    /// probe, the keymap, the encoder, the window manager, and the socket
    /// the agent knocks on.
    fn start(config: &ComputerConfig, socket_path: &Path) -> Result<Self, x11::DisplayError> {
        let display = x11::Display::start(config)?;
        let encoder = encode::Encoder::new(config).map_err(|reason| x11::DisplayError::Encode {
            reason: reason.to_string(),
        })?;
        let wm = x11::start_window_manager(display.env_name());
        let agent = ipc::Listener::bind(socket_path)
            .map_err(|source| x11::DisplayError::AgentSocket { source })?;
        Ok(Self {
            display,
            encoder: Some(encoder),
            agent,
            wm,
            next_frame: Instant::now(),
        })
    }

    /// Serves the agent's pending socket request, if one is waiting.
    ///
    /// Returns whether the agent was heard *and answered yes* — the
    /// supervisor records that as desktop activity. While `takeover`
    /// holds, every request is refused with the sentence the model
    /// reads.
    fn serve_agent(&mut self, takeover: bool) -> bool {
        self.agent.service(|request| match request {
            ipc::AgentRequest::Screenshot => {
                if takeover {
                    ipc::AgentReply::Refused {
                        reason: "the user is driving the desktop".to_owned(),
                    }
                } else {
                    match self.display.capture() {
                        Ok(image) => match encode::png(&image) {
                            Ok(png) => ipc::AgentReply::Screenshot {
                                png_base64: ipc::b64(&png),
                            },
                            Err(error) => ipc::AgentReply::Refused {
                                reason: error.to_string(),
                            },
                        },
                        Err(error) => ipc::AgentReply::Refused {
                            reason: error.to_string(),
                        },
                    }
                }
            }
            ipc::AgentRequest::Input { events } => {
                if takeover {
                    ipc::AgentReply::Refused {
                        reason: "the user is driving the desktop".to_owned(),
                    }
                } else {
                    let mut refused = None;
                    for event in &events {
                        if let Err(error) = self.display.inject(event) {
                            refused = Some(error.to_string());
                            break;
                        }
                    }
                    refused.map_or(ipc::AgentReply::Done, |reason| ipc::AgentReply::Refused {
                        reason,
                    })
                }
            }
            ipc::AgentRequest::State => ipc::AgentReply::State {
                takeover,
                display: self.display.env_name().to_string_lossy().into_owned(),
                width: u32::from(self.display.width()),
                height: u32::from(self.display.height()),
            },
        })
    }

    /// Captures one frame when it is due and offers it to the
    /// connection.
    ///
    /// `force_keyframe` is the supervisor's resync flag: the encode takes
    /// it when set, and it is re-armed whenever a packet could not leave
    /// — a dropped chunk breaks the reference chain until the next
    /// keyframe, so the next encode is made to be one.
    fn capture_and_send(&mut self, force_keyframe: &mut bool, events: &mpsc::Sender<DesktopEvent>) {
        let now = Instant::now();
        if now < self.next_frame {
            return;
        }
        // Behind-schedule restarts the clock rather than bursts catch-up
        // frames nobody asked for.
        self.next_frame = (self.next_frame + encode::frame_interval(self.fps())).max(now);
        let force = core::mem::take(force_keyframe);
        let Ok(image) = self.display.capture() else {
            *force_keyframe = true;
            return;
        };
        let Some(encoder) = &mut self.encoder else {
            return;
        };
        match encoder.encode(&image, force) {
            Ok(Some(packet)) => {
                // A full queue drops the packet and resyncs the stream
                // rather than blocking the display on the network.
                if events
                    .try_send(DesktopEvent::Chunk {
                        keyframe: packet.keyframe,
                        bytes: packet.bytes,
                    })
                    .is_err()
                {
                    *force_keyframe = true;
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "the desktop encoder failed; the stream is done");
                // The display and the socket stay — the agent's hands
                // do not need the stream — but the encoder is finished.
                self.encoder = None;
                let _ = events.blocking_send(DesktopEvent::State {
                    status: DesktopStatus::Failed,
                    detail: Some(format!("the encoder failed: {error}")),
                });
            }
        }
    }

    /// Notices a child that exited underneath the stack.
    ///
    /// The window manager dying is a decoration loss, not a desktop
    /// loss — logged and carried on without. An `Xvfb` that died takes
    /// the display with it: reported and answered `true` so the
    /// supervisor drops the stack rather than carrying a dead display.
    fn reap(&mut self, events: &mpsc::Sender<DesktopEvent>) -> bool {
        if let Some(wm) = &mut self.wm
            && let Ok(Some(status)) = wm.try_wait()
        {
            tracing::warn!(%status, "the window manager exited; the desktop is bare");
            self.wm = None;
        }
        if let Ok(Some(status)) = self.display.server().try_wait() {
            tracing::warn!(%status, "the display server exited under the desktop");
            let _ = events.blocking_send(DesktopEvent::State {
                status: DesktopStatus::Failed,
                detail: Some(format!("the display server exited ({status})")),
            });
            return true;
        }
        false
    }

    /// Everything a display stack stops.
    fn stop(&mut self) {
        if let Some(mut wm) = self.wm.take() {
            let _ = wm.kill();
            let _ = wm.wait();
        }
        let _ = self.display.server().kill();
        let _ = self.display.server().wait();
    }

    /// The encoder's cadence, when it still exists.
    fn fps(&self) -> u32 {
        self.encoder.as_ref().map_or(5, encode::Encoder::fps)
    }
}

/// One thing a test's desktop was asked to do, in the order it was asked.
///
/// In the same vocabulary as [`crate::terminal::TerminalCall`]: the relay
/// tests assert the ordering across every double in the room, and the
/// desktop's asks join that list rather than speaking their own dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopCall {
    /// `computer_use` was turned on or off.
    Enabled(bool),
    /// The audience flag was set.
    Watching(bool),
    /// The takeover flag was set.
    Takeover(bool),
    /// A batch of user input arrived.
    Input(Vec<DesktopInputEvent>),
    /// The encoder was asked to resync.
    Keyframe,
}

/// A [`DesktopSession`] that records what it was told.
///
/// `pair` also hands back the report channel's sender: a test drives the
/// supervisor's half itself, so what the pump sees is whatever the test
/// says the desktop did.
pub struct FakeDesktop {
    calls: mpsc::UnboundedSender<DesktopCall>,
}

impl core::fmt::Debug for FakeDesktop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FakeDesktop").finish_non_exhaustive()
    }
}

impl FakeDesktop {
    /// The handle the relay owns, the stream of calls it made, and the
    /// sender a test reports the desktop's doings through.
    #[must_use]
    pub fn pair() -> (
        Self,
        mpsc::UnboundedReceiver<DesktopCall>,
        mpsc::Sender<DesktopEvent>,
        mpsc::Receiver<DesktopEvent>,
    ) {
        let (calls, received) = mpsc::unbounded_channel();
        let (reports, out) = mpsc::channel(EVENT_DEPTH);
        (Self { calls }, received, reports, out)
    }

    /// Records one ask.
    fn record(&self, call: DesktopCall) -> Result<(), DesktopError> {
        self.calls.send(call).map_err(|_| DesktopError)
    }
}

impl DesktopSession for FakeDesktop {
    fn set_enabled(&mut self, enabled: bool) -> Result<(), DesktopError> {
        self.record(DesktopCall::Enabled(enabled))
    }

    fn set_watching(&mut self, watching: bool) -> Result<(), DesktopError> {
        self.record(DesktopCall::Watching(watching))
    }

    fn set_takeover(&mut self, active: bool) -> Result<(), DesktopError> {
        self.record(DesktopCall::Takeover(active))
    }

    fn inject(&mut self, events: Vec<DesktopInputEvent>) -> Result<(), DesktopError> {
        self.record(DesktopCall::Input(events))
    }

    fn keyframe_now(&mut self) -> Result<(), DesktopError> {
        self.record(DesktopCall::Keyframe)
    }
}
