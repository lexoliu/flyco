//! In-process stand-ins for the two things the daemon talks to.
//!
//! A [`Room`] is a real HTTP+SSE server and a [`ControlPlane`] is a real
//! HTTP server, both on loopback: the daemon's relay client and REST client
//! are exercised through the same requests they will use in production,
//! headers and status codes and all, rather than through a substitute for
//! the transport. What is faked is the *other* end of the harness —
//! [`FakeSession`] stands in for a running Claude Code process, because no
//! test should need Bun installed to prove that an interrupt reached the
//! session.
//!
//! Everything records through channels rather than shared mutable state, so
//! a test reads what happened by draining a receiver.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use flyco_core::wire::ApprovalPayload;
use flyco_core::{ApprovalId, ApprovalState, ApprovalView, ControlToDaemon, DaemonToControl};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};

use crate::harness::{HarnessSession, ToolApproval};

/// What a session's machine is provisioned with, as a container job carries
/// it.
///
/// The provider's own fixtures, so the machine a test bootstrap describes is
/// described in exactly one place — and `flycod host`'s tests plan jobs
/// against the same bootstrap the planner does.
#[must_use]
pub fn bootstrap() -> flyco_provider::DaemonBootstrap {
    flyco_provider::DaemonBootstrap {
        session: flyco_core::SessionId::generate(),
        provider: flyco_core::machine::CloudProviderKind::Host,
        runtime: flyco_core::Runtime::Container,
        control_plane_url: "https://dev.flyco.dev/".to_owned(),
        daemon_token: "fd_a-daemon-token".to_owned(),
        permission_mode: flyco_core::PermissionMode::Default,
        auth: flyco_provider::HarnessCredential::ClaudeCode(
            flyco_provider::ClaudeCredential::Inherit,
        ),
        repos: flyco_provider::testing::checkouts(),
        github: flyco_provider::testing::github(),
        machine_origin: flyco_core::MachineOrigin::Auto,
        machine: flyco_provider::testing::session_machine(),
        computer_use: false,
        resume_session_id: None,
        model: flyco_provider::testing::session_model(),
        mcp_servers: flyco_provider::testing::mcp_servers(),
    }
}

// ── The harness, faked ──

/// Something the wire client did, in the order it did it.
///
/// Mostly things it asked the harness to do, which is why it lives beside
/// [`FakeSession`]. The last three are not the harness at all — a durable
/// write, a disk flush — and they are in the same vocabulary on purpose:
/// the reclamation sequence is an *ordering* across all three of those
/// things (docs/ARCHITECTURE.md), and an ordering can only be asserted
/// against one channel. Every double in a relay test therefore records into
/// the sender [`FakeSession::recorder`] hands out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// A user message was pushed into the session.
    UserMessage(String),
    /// The current turn was interrupted.
    Interrupt,
    /// Everything the harness had produced was flushed through.
    Flush,
    /// The session context was compacted.
    Compact,
    /// The session was asked what its context window is spent on.
    ContextUsage,
    /// The session was put on another model.
    ModelSet(flyco_core::ModelChoice),
    /// The session was put under another permission mode.
    PermissionModeSet(flyco_core::PermissionMode),
    /// A pending approval was answered.
    Approval {
        /// The approval that was answered.
        id: ApprovalId,
        /// Whether it was allowed.
        allowed: bool,
    },
    /// The session was shut down.
    Shutdown,
    /// The harness-native session id was filed with the control plane.
    HarnessSessionRecorded(String),
    /// Every filesystem was flushed.
    Synced,
    /// The reclamation was reported to the control plane over REST.
    SpotNoticeReported(u32),
    /// A checkout's uncommitted work was stored as a patch, with the
    /// checkout's directory (`None` for the developer-machine root) and the
    /// patch's size in bytes.
    ///
    /// The container counterpart of [`Self::Synced`]: a machine with no
    /// disk gets its uncommitted work off the machine rather than onto a
    /// disk, and the ordering test asserts which of the two happened.
    WorkdirPatchStored(Option<String>, usize),
    /// The stop was reported to the control plane over REST.
    StoppingReported(flyco_core::StopReason),
    /// The models the harness offers were filed with the control plane.
    ModelsReported(Vec<flyco_core::ModelOption>),
    /// How much of the plan is spent was filed with the control plane.
    UsageReported(Vec<flyco_core::UsageWindow>),
    /// A spent plan window was filed with the control plane, which is what
    /// pauses the session until it turns over (issue #244).
    UsageLimitReported(flyco_core::UsageWindow),
}

/// The harness never fails in these tests.
#[derive(Debug, thiserror::Error)]
#[error("the fake harness session stopped")]
pub struct FakeSessionError;

/// A [`HarnessSession`] that records what it was told.
#[derive(Debug)]
pub struct FakeSession {
    calls: mpsc::UnboundedSender<Call>,
    /// Whether a user message is accepted or simply never answered.
    ///
    /// What a real harness looks like when its agent process has stopped
    /// reading: the command is written, and the acknowledgement that says
    /// it landed never comes.
    wedged: bool,
}

impl FakeSession {
    /// Creates a session and the stream of calls made against it.
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Call>) {
        let (calls, received) = mpsc::unbounded_channel();
        (
            Self {
                calls,
                wedged: false,
            },
            received,
        )
    }

    /// A session that never answers a user message.
    #[must_use]
    pub fn wedged() -> (Self, mpsc::UnboundedReceiver<Call>) {
        let (calls, received) = mpsc::unbounded_channel();
        (
            Self {
                calls,
                wedged: true,
            },
            received,
        )
    }

    /// The sender every other double in the same test records into.
    ///
    /// One channel, so what a test reads back is the order things actually
    /// happened in rather than the order it happened to drain them.
    #[must_use]
    pub fn recorder(&self) -> mpsc::UnboundedSender<Call> {
        self.calls.clone()
    }

    fn record(&self, call: Call) -> Result<(), FakeSessionError> {
        self.calls.send(call).map_err(|_| FakeSessionError)
    }
}

/// A [`Disk`](crate::spot::Disk) that records the flush instead of
/// performing one.
#[derive(Debug)]
pub struct FakeDisk {
    calls: mpsc::UnboundedSender<Call>,
}

impl FakeDisk {
    /// A disk recording into the same channel as everything else.
    #[must_use]
    pub const fn new(calls: mpsc::UnboundedSender<Call>) -> Self {
        Self { calls }
    }
}

impl crate::spot::Disk for FakeDisk {
    fn sync(
        &self,
    ) -> impl core::future::Future<Output = Result<(), crate::spot::DiskError>> + Send {
        let _ = self.calls.send(Call::Synced);
        core::future::ready(Ok(()))
    }
}

impl HarnessSession for FakeSession {
    type Error = FakeSessionError;

    fn send_user_message(
        &self,
        text: String,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        let answered = (!self.wedged).then(|| self.record(Call::UserMessage(text)));
        async move {
            match answered {
                Some(result) => result,
                None => core::future::pending().await,
            }
        }
    }

    fn interrupt(&self) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::Interrupt))
    }

    fn flush(&self) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::Flush))
    }

    fn compact(&self) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::Compact))
    }

    fn context_usage(&self) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::ContextUsage))
    }

    fn set_model(
        &self,
        model: flyco_core::ModelChoice,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::ModelSet(model)))
    }

    fn set_permission_mode(
        &self,
        mode: flyco_core::PermissionMode,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::PermissionModeSet(mode)))
    }

    fn decide_approval(
        &self,
        approval: ToolApproval,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::Approval {
            id: approval.id(),
            allowed: matches!(approval, ToolApproval::Allow { .. }),
        }))
    }

    fn shutdown(self) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::Shutdown))
    }
}

// ── The rooms, for real, on loopback ──

/// What a room saw the machine at the other end do.
///
/// Generic over the frames that end speaks, because flyco has two of these
/// relays and they differ in nothing but their vocabulary: a session's
/// daemon holds one to its [`Room`], and an enrolled host holds one to its
/// [`HostRelay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen<Up = DaemonToControl> {
    /// A peer attached, presenting this `Authorization` header and this
    /// attach body.
    Attached {
        /// The `Authorization` header the attach carried.
        authorization: Option<String>,
        /// The attach request's JSON body — a session's protocol version,
        /// a host's facts.
        body: serde_json::Value,
    },
    /// A peer opened its command stream under this epoch.
    StreamOpened(u64),
    /// A command stream ended — closed on a directive, superseded by a
    /// newer attach, or the peer went away.
    StreamClosed,
    /// A frames batch landed: one POST's worth, in order.
    Batch {
        /// The attach the batch belongs to.
        epoch: u64,
        /// The sequence `frames[0]` carries.
        from_seq: u64,
        /// How far the peer says it has applied the command log.
        ack_through: u64,
        /// The frames, in order.
        frames: Vec<Up>,
    },
}

/// What a test tells a room to do next.
#[derive(Debug)]
pub enum Directive<Down = ControlToDaemon> {
    /// Queue a command for the peer — emitted on the open stream now, or
    /// replayed on the next one, as the real room's mailbox does.
    Send(Down),
    /// End the open command stream bare, so the peer has to re-attach.
    ///
    /// The transient case: what a redeployed or restarted room looks like
    /// from the peer's end. A superseding attach is the other way a
    /// stream ends — the room emits `superseded` first — and needs no
    /// directive: a second attach does it.
    Close,
    /// Stop heartbeating the open stream, so the peer's byte-level idle
    /// watch fires — the dead path a dropped NAT flow looks like.
    Silence,
    /// Park every attach until [`Directive::ReleaseAttaches`] — the window
    /// a test needs to make the peer produce frames while it is provably
    /// detached, rather than racing the peer's notice of a dead stream.
    GateAttaches,
    /// Let a parked attach through again.
    ReleaseAttaches,
}

/// How the session room answers a daemon's attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachAnswer {
    /// Accept attaches, as a live room does.
    Accept,
    /// Refuse every attach with a retryable problem, as a room mid-deploy
    /// does.
    Refuse,
}

/// One frames POST's decoded body — the same envelope both relay clients
/// speak (`DaemonFrames` and `HostFrames` are the same four fields).
#[derive(Debug, serde::Deserialize)]
struct Batch<Up> {
    epoch: u64,
    from_seq: u64,
    ack_through: u64,
    frames: Vec<Up>,
}

/// What the fake room remembers across requests.
///
/// The log is the rendezvous, exactly as the real room's `daemon_commands`
/// table is: a command is a row before it is a stream event, and it stays
/// a row until a frames batch acknowledges it.
struct RoomState {
    /// The current attach; a stream opened under another is refused.
    epoch: u64,
    /// The next command's sequence.
    next_seq: u64,
    /// Commands not yet acknowledged, in order.
    log: VecDeque<(u64, serde_json::Value)>,
    /// Whether `run` rows survive `ack_through` — a host room's jobs are
    /// retired by the `job_result` frame that answers them, not by
    /// delivery, which is what makes a re-attach re-offer an unanswered
    /// job.
    jobs_held_until_answered: bool,
    /// Whether the open stream's heartbeat is suppressed — the dead path.
    silenced: bool,
    /// What attaches are answered with, when they are refused at all — a
    /// room mid-deploy, or a revoked credential.
    refusal: Option<(u16, &'static str, &'static str)>,
    /// Ends the open command stream, when one is open.
    close_stream: Option<oneshot::Sender<()>>,
    /// Whether attach POSTs park until released.
    attach_gated: bool,
    /// Wakes parked attaches when the gate lifts.
    attach_released: Arc<tokio::sync::Notify>,
}

/// A real HTTP+SSE server standing in for one of flyco's Durable Objects.
///
/// The peer's three routes are answered with the real room's semantics:
/// attach mints an epoch, the command stream cursors over a durable log
/// and heartbeats, and a frames batch is stored — and its `ack_through`
/// applied — before it is answered.
#[derive(Debug)]
pub struct Relay<Up, Down> {
    /// Base URL the peer should be pointed at, e.g. `http://127.0.0.1:PORT/`.
    pub base: url::Url,
    /// What the room saw, in order.
    pub seen: mpsc::UnboundedReceiver<Seen<Up>>,
    /// What the room should do next.
    pub directives: mpsc::UnboundedSender<Directive<Down>>,
    /// Batch frames past the one `next_frame` last handed out.
    pending: VecDeque<Up>,
    /// Borrow marker: the command vocabulary is the directive channel's.
    _down: core::marker::PhantomData<fn() -> Down>,
}

/// An HTTP+SSE server standing in for a session's Durable Object.
pub type Room = Relay<DaemonToControl, ControlToDaemon>;

/// An HTTP+SSE server standing in for an enrolled host's Durable Object.
pub type HostRelay =
    Relay<flyco_provider::host::HostToControl, flyco_provider::host::ControlToHost>;

/// How often the open stream heartbeats.
///
/// Well inside the tightest idle bound any test gives its peer: a
/// [`wire::Deadlines`](crate::control::wire::Deadlines) built for
/// watching gives the stream 150ms of silence, and this beats five times
/// in that.
const STREAM_PING: std::time::Duration = std::time::Duration::from_millis(30);

impl<Up, Down> Relay<Up, Down>
where
    Up: serde::de::DeserializeOwned + Send + 'static,
    Down: serde::Serialize + Send + 'static,
{
    /// Starts a room on a loopback port.
    ///
    /// `refusal` is what every attach is answered with, when attaches are
    /// refused at all. `jobs_held_until_answered` gives the room a host's
    /// job semantics: `run` rows survive `ack_through` and are retired by
    /// the `job_result` frame that answers them.
    ///
    /// # Panics
    ///
    /// Panics if the loopback socket cannot be bound, which would mean the
    /// test host has no usable networking.
    async fn serve(
        refusal: Option<(u16, &'static str, &'static str)>,
        jobs_held_until_answered: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address: SocketAddr = listener.local_addr().expect("read the bound port");

        let (seen_out, seen) = mpsc::unbounded_channel::<Seen<Up>>();
        let (directives, mut directive_in) = mpsc::unbounded_channel::<Directive<Down>>();
        let (log_changed, _) = watch::channel(0_u64);

        let state = Arc::new(Mutex::new(RoomState {
            epoch: 0,
            next_seq: 1,
            log: VecDeque::new(),
            jobs_held_until_answered,
            silenced: false,
            refusal,
            close_stream: None,
            attach_gated: false,
            attach_released: Arc::new(tokio::sync::Notify::new()),
        }));

        // The mailbox: directives land as log rows whether or not a
        // stream is open, exactly as the real room's command table does —
        // a command queued while the peer is detached is replayed on the
        // next stream.
        {
            let state = Arc::clone(&state);
            let log_changed = log_changed.clone();
            tokio::spawn(async move {
                while let Some(directive) = directive_in.recv().await {
                    match directive {
                        Directive::Send(command) => {
                            let json = serde_json::to_value(&command).expect("serialize");
                            let version = {
                                let mut state = state.lock().expect("the room state");
                                let seq = state.next_seq;
                                state.next_seq += 1;
                                state.log.push_back((seq, json));
                                seq
                            };
                            let _ = log_changed.send(version);
                        }
                        Directive::Close => {
                            let close = state.lock().expect("the room state").close_stream.take();
                            if let Some(close) = close {
                                let _ = close.send(());
                            }
                        }
                        Directive::Silence => {
                            state.lock().expect("the room state").silenced = true;
                        }
                        Directive::GateAttaches => {
                            state.lock().expect("the room state").attach_gated = true;
                        }
                        Directive::ReleaseAttaches => {
                            let released = {
                                let mut state = state.lock().expect("the room state");
                                state.attach_gated = false;
                                Arc::clone(&state.attach_released)
                            };
                            released.notify_one();
                        }
                    }
                }
            });
        }

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&state);
                let seen_out = seen_out.clone();
                let log_changed = log_changed.clone();
                tokio::spawn(async move {
                    handle::<Up>(stream, state, log_changed, seen_out).await;
                });
            }
        });

        Self {
            base: format!("http://{address}/")
                .parse()
                .expect("a loopback URL"),
            seen,
            directives,
            pending: VecDeque::new(),
            _down: core::marker::PhantomData,
        }
    }

    /// The next thing the room saw, or `None` if nothing arrived in time.
    pub async fn next(&mut self) -> Option<Seen<Up>> {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.seen.recv())
            .await
            .ok()
            .flatten()
    }

    /// Waits for the next frame, skipping connection bookkeeping.
    ///
    /// A batch's frames are handed out one at a time, so a test reads the
    /// room's inbound the same way it always has.
    ///
    /// # Panics
    ///
    /// Panics if no frame arrives before the timeout, which in these tests
    /// means the peer stopped pumping.
    pub async fn next_frame(&mut self) -> Up {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return frame;
            }
            if let Seen::Batch { frames, .. } = self.next().await.expect("the peer sent nothing") {
                self.pending.extend(frames);
            }
        }
    }
}

impl Room {
    /// Starts a session room that answers attaches the way `answer` says.
    pub async fn start(answer: AttachAnswer) -> Self {
        Self::serve(
            match answer {
                AttachAnswer::Accept => None,
                AttachAnswer::Refuse => Some((503, "relay-unavailable", "the room is mid-deploy")),
            },
            false,
        )
        .await
    }
}

impl HostRelay {
    /// Starts a host room that accepts attaches.
    pub async fn listen() -> Self {
        Self::serve(None, true).await
    }

    /// Starts a host room that refuses every attach the way a revoked
    /// credential is refused.
    pub async fn revoked() -> Self {
        Self::serve(
            Some((
                401,
                "invalid-host-credential",
                "the token this machine holds is revoked",
            )),
            true,
        )
        .await
    }
}

/// Serves one request on one connection: attach, command stream, or a
/// frames batch.
async fn handle<Up>(
    mut stream: TcpStream,
    state: Arc<Mutex<RoomState>>,
    log_changed: watch::Sender<u64>,
    seen: mpsc::UnboundedSender<Seen<Up>>,
) where
    Up: serde::de::DeserializeOwned,
{
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let target = request.target.clone();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));

    if request.method == "POST" && path.ends_with("/relay/attach") {
        let _ = seen.send(Seen::Attached {
            authorization: request.authorization,
            body: serde_json::from_slice(&request.body).unwrap_or_default(),
        });
        let refusal = state.lock().expect("the room state").refusal;
        if let Some((status, slug, detail)) = refusal {
            let _ = write_reply(&mut stream, &Reply::problem(status, slug, detail)).await;
            return;
        }
        // A gated attach parks here until released — the attempt is
        // already on `seen`, so the test knows the peer is detached while
        // it makes the peer produce frames.
        loop {
            let released = {
                let state = state.lock().expect("the room state");
                if !state.attach_gated {
                    break;
                }
                Arc::clone(&state.attach_released)
            };
            released.notified().await;
        }
        let epoch = {
            let mut state = state.lock().expect("the room state");
            // A newer attach supersedes the stream before it, as the real
            // room's presence epoch does — noticed on the open stream's
            // next check, which is what emits the `superseded` command
            // before ending it.
            state.epoch += 1;
            state.epoch
        };
        let _ = write_reply(
            &mut stream,
            &Reply::json(&serde_json::json!({ "epoch": epoch })),
        )
        .await;
    } else if request.method == "GET" && path.ends_with("/relay/commands") {
        let epoch = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("epoch="))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let current = state.lock().expect("the room state").epoch;
        if epoch != current {
            let _ = write_reply(
                &mut stream,
                &Reply::problem(
                    409,
                    "relay-epoch-stale",
                    "the epoch names a superseded attach",
                ),
            )
            .await;
            return;
        }
        let _ = seen.send(Seen::StreamOpened(epoch));
        serve_commands(stream, state, log_changed, seen, epoch).await;
    } else if request.method == "POST" && path.ends_with("/relay/frames") {
        handle_frames(&mut stream, &request, &state, &seen).await;
    } else {
        let _ = write_reply(
            &mut stream,
            &Reply::problem(
                404,
                "not-found",
                "the fake room knows only the relay routes",
            ),
        )
        .await;
    }
}

/// Answers `POST …/relay/frames`: epoch check, `ack_through`, then `Seen`.
async fn handle_frames<Up>(
    stream: &mut TcpStream,
    request: &Received,
    state: &Arc<Mutex<RoomState>>,
    seen: &mpsc::UnboundedSender<Seen<Up>>,
) where
    Up: serde::de::DeserializeOwned,
{
    let Ok(batch) = serde_json::from_slice::<Batch<Up>>(&request.body) else {
        return;
    };
    let current = state.lock().expect("the room state").epoch;
    if batch.epoch != current {
        let _ = write_reply(
            stream,
            &Reply::problem(
                409,
                "relay-epoch-stale",
                "the epoch names a superseded attach",
            ),
        )
        .await;
        return;
    }
    {
        let mut state = state.lock().expect("the room state");
        // `ack_through` retires everything at or below it — except a
        // host room's `run` rows, which only a `job_result` retires.
        let hold_jobs = state.jobs_held_until_answered;
        state.log.retain(|(seq, command)| {
            *seq > batch.ack_through || (hold_jobs && command["type"] == "run")
        });
        if hold_jobs {
            // The raw frames, for matching `job_result` to its row —
            // the decoded `Up` is for `Seen`, this pass is for the log.
            let raw: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            for frame in raw["frames"].as_array().into_iter().flatten() {
                if frame["type"] != "job_result" {
                    continue;
                }
                let Some(job_id) = frame["job_id"].as_str() else {
                    continue;
                };
                // The row the result answers is the `run` for the same
                // machine: a `create` names it outright, the others in
                // the container name it was derived into.
                if let Some(position) = state.log.iter().position(|(_, command)| {
                    command["type"] == "run"
                        && (command["job"]["machine"] == job_id
                            || command["job"]["container"]
                                .as_str()
                                .is_some_and(|name| name.ends_with(job_id)))
                }) {
                    state.log.remove(position);
                }
            }
        }
    }
    let _ = seen.send(Seen::Batch {
        epoch: batch.epoch,
        from_seq: batch.from_seq,
        ack_through: batch.ack_through,
        frames: batch.frames,
    });
    let _ = write_reply(stream, &Reply::json(&serde_json::json!({ "events": [] }))).await;
}

/// The epoch a command stream was superseded by is checked this often.
const SUPERSEDE_CHECK: std::time::Duration = std::time::Duration::from_millis(20);

/// Holds one command stream open: a cursor over the room's log, plus the
/// heartbeat comments that keep an idle stream's flow known-alive.
async fn serve_commands<Up>(
    mut stream: TcpStream,
    state: Arc<Mutex<RoomState>>,
    log_changed: watch::Sender<u64>,
    seen: mpsc::UnboundedSender<Seen<Up>>,
    epoch: u64,
) where
    Up: serde::de::DeserializeOwned,
{
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).await.is_err() {
        let _ = seen.send(Seen::StreamClosed);
        return;
    }
    let (mut reader, mut writer) = stream.into_split();
    let (close, mut closed) = oneshot::channel();
    state.lock().expect("the room state").close_stream = Some(close);

    let mut cursor = 0_u64;
    let mut incoming = log_changed.subscribe();
    let mut superseded = tokio::time::interval(SUPERSEDE_CHECK);
    let mut ping = tokio::time::interval(STREAM_PING);
    // The first ticks are immediate and a stream that just opened has
    // said nothing yet; neither fires now.
    superseded.tick().await;
    ping.tick().await;
    let mut read_buffer = [0_u8; 256];

    loop {
        // A silenced stream is a dead path: no bytes move in either
        // direction, commands included — a NAT that reclaimed the flow
        // drops the command events with the heartbeat.
        let mut superseded_by = false;
        let rows: Vec<(u64, serde_json::Value)> = {
            let state = state.lock().expect("the room state");
            if state.epoch != epoch {
                superseded_by = true;
                Vec::new()
            } else if state.silenced {
                Vec::new()
            } else {
                state
                    .log
                    .iter()
                    .filter(|(seq, _)| *seq > cursor)
                    .cloned()
                    .collect()
            }
        };
        if superseded_by {
            // The real room's answer to a superseded attach: the losing
            // stream is told why before it ends — a bare EOF reads as a
            // dropped connection, and an uninformed loser re-attaches
            // into the epoch that replaced it (issue #336).
            let _ = writer
                .write_all(
                    b"event: command\ndata: {\"seq\":null,\"command\":{\"type\":\"superseded\"}}\n\n",
                )
                .await;
            break;
        }
        let mut gone = false;
        for (seq, command) in rows {
            let event = format!(
                "event: command\nid: {seq}\ndata: {{\"seq\":{seq},\"command\":{command}}}\n\n"
            );
            if writer.write_all(event.as_bytes()).await.is_err() {
                gone = true;
                break;
            }
            cursor = seq;
        }
        if gone {
            break;
        }

        tokio::select! {
            _ = incoming.changed() => {}
            _ = superseded.tick() => {}
            _ = ping.tick() => {
                let silenced = state.lock().expect("the room state").silenced;
                if !silenced && writer.write_all(b": ping\n\n").await.is_err() {
                    break;
                }
            }
            _ = &mut closed => break,
            read = reader.read(&mut read_buffer) => {
                // The peer closed its side of the flow: a zero read, or an
                // error, both end the stream the same way.
                if matches!(read, Ok(0) | Err(_)) {
                    break;
                }
            }
        }
    }
    {
        let mut state = state.lock().expect("the room state");
        state.close_stream = None;
        // A dead flow ends with the stream that carried it: the next one
        // is a healthy path until a test silences it too.
        state.silenced = false;
    }
    let _ = seen.send(Seen::StreamClosed);
}

// ── The REST API, for real, on loopback ──

/// One request the control plane received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    /// Request method.
    pub method: String,
    /// Request target, path and query.
    pub target: String,
    /// The `Authorization` header, if one was sent.
    pub authorization: Option<String>,
    /// The request body.
    pub body: Vec<u8>,
}

/// One canned answer.
#[derive(Debug, Clone)]
pub struct Reply {
    /// Status line code.
    pub status: u16,
    /// Extra headers, beyond `Content-Length`.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
}

impl Reply {
    /// A `200 OK` carrying a JSON document.
    ///
    /// # Panics
    ///
    /// Panics if the value does not serialize, which would be a bug in the
    /// test rather than in the daemon.
    #[must_use]
    pub fn json<T: serde::Serialize>(value: &T) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: serde_json::to_vec(value).expect("serialize a test reply"),
        }
    }

    /// A `200 OK` carrying a transcript stream.
    #[must_use]
    pub fn transcript(body: &[u8], batches: u64) -> Self {
        Self {
            status: 200,
            headers: vec![
                ("content-type".to_owned(), "application/x-ndjson".to_owned()),
                ("x-flyco-transcript-batches".to_owned(), batches.to_string()),
            ],
            body: body.to_vec(),
        }
    }

    /// A `200 OK` carrying plain text.
    ///
    /// What an instance-metadata endpoint answers with: a JSON document
    /// Azure and EC2 do not label, and the bare word Compute Engine's
    /// `preempted` key is.
    #[must_use]
    pub fn text(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: body.as_bytes().to_vec(),
        }
    }

    /// A `204 No Content`.
    #[must_use]
    pub const fn no_content() -> Self {
        Self {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// An RFC 9457 problem document, the way the control plane refuses.
    #[must_use]
    pub fn problem(status: u16, slug: &str, detail: &str) -> Self {
        let problem = flyco_core::Problem::of_type(slug, status, "Refused", detail);
        Self {
            status,
            headers: vec![(
                "content-type".to_owned(),
                "application/problem+json".to_owned(),
            )],
            body: serde_json::to_vec(&problem).expect("serialize a problem"),
        }
    }

    /// A `201 Created` carrying a freshly raised approval.
    #[must_use]
    pub fn approval(id: ApprovalId, session: flyco_core::SessionId) -> Self {
        let mut reply = Self::json(&ApprovalView {
            id,
            session,
            payload: ApprovalPayload::ToolUse {
                tool: "Bash".to_owned(),
                input: serde_json::json!({ "command": "ls" }),
            },
            state: ApprovalState::Pending,
            created_at_unix: 0,
        });
        reply.status = 201;
        reply
    }
}

/// An HTTP server standing in for a provider's instance-metadata endpoint.
///
/// The same server the control-plane double is, under the name a watcher's
/// test reads it by: an instance-metadata endpoint *is* an HTTP server
/// answering a scripted sequence of documents, and a watcher polling one is
/// an HTTP client like any other. Scripting the sequence is what lets a
/// test assert the poll — quiet, quiet, then a notice — rather than only
/// the parse.
pub type MetadataEndpoint = ControlPlane;

/// An HTTP server standing in for the control plane's REST API.
#[derive(Debug)]
pub struct ControlPlane {
    /// Base URL a daemon should be pointed at.
    pub base: url::Url,
    /// What the control plane received, in order.
    pub received: mpsc::UnboundedReceiver<Received>,
}

impl ControlPlane {
    /// Starts a control plane that answers `replies` in order, then `404`s.
    ///
    /// # Panics
    ///
    /// Panics if the loopback socket cannot be bound.
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address: SocketAddr = listener.local_addr().expect("read the bound port");
        let (received_out, received) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut replies = replies.into_iter();
            while let Ok((mut stream, _)) = listener.accept().await {
                let reply = replies.next().unwrap_or_else(|| {
                    Reply::problem(404, "not-found", "the test scripted no more replies")
                });
                if let Some(request) = read_request(&mut stream).await
                    && received_out.send(request).is_err()
                {
                    return;
                }
                let _ = write_reply(&mut stream, &reply).await;
            }
        });

        Self {
            base: format!("http://{address}/")
                .parse()
                .expect("a loopback URL"),
            received,
        }
    }

    /// The next request, or `None` if nothing arrived in time.
    pub async fn next(&mut self) -> Option<Received> {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.received.recv())
            .await
            .ok()
            .flatten()
    }
}

/// Reads one HTTP/1.1 request: the start line, its headers, and its body.
async fn read_request(stream: &mut TcpStream) -> Option<Received> {
    let mut buffer = Vec::new();
    let head = loop {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
    };

    let text = String::from_utf8_lossy(&buffer[..head]).into_owned();
    let mut lines = text.lines();
    let mut start = lines.next()?.split_whitespace();
    let method = start.next()?.to_owned();
    let target = start.next()?.to_owned();

    let mut authorization = None;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "authorization" => authorization = Some(value.to_owned()),
            "content-length" => length = value.parse::<usize>().ok(),
            "transfer-encoding" => chunked |= value.eq_ignore_ascii_case("chunked"),
            _ => {}
        }
    }

    let mut raw = buffer[head + 4..].to_vec();
    // zenwave's backend streams a body, so it may arrive either
    // length-delimited or chunked; both shapes have to be read here or the
    // test would assert on an empty body and prove nothing.
    let body = if chunked {
        loop {
            if let Some(body) = dechunk(&raw) {
                break body;
            }
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                break dechunk(&raw).unwrap_or_default();
            }
            raw.extend_from_slice(&chunk[..read]);
        }
    } else {
        let length = length.unwrap_or(0);
        while raw.len() < length {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..read]);
        }
        raw.truncate(length);
        raw
    };

    Some(Received {
        method,
        target,
        authorization,
        body,
    })
}

/// Decodes a chunked body, or `None` if the terminator has not arrived.
fn dechunk(raw: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut rest = raw;
    loop {
        let at = rest.windows(2).position(|window| window == b"\r\n")?;
        let size =
            usize::from_str_radix(core::str::from_utf8(&rest[..at]).ok()?.trim(), 16).ok()?;
        rest = rest.get(at + 2..)?;
        if size == 0 {
            return Some(body);
        }
        body.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size + 2..)?;
    }
}

/// Writes one HTTP/1.1 response and closes the connection.
async fn write_reply(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} X\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("content-length: ");
    head.push_str(&reply.body.len().to_string());
    head.push_str("\r\n");
    head.push_str("connection: close\r\n\r\n");

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&reply.body).await?;
    stream.flush().await?;
    stream.shutdown().await
}

/// The machine a relay test's session is on.
///
/// Uninteresting on purpose: the relay tests are about ordering and
/// reconnection, and the only thing they need from a machine is that the
/// opening notice has something true to say. A test that cares about a
/// particular machine — a license-bound one — builds its own.
#[must_use]
pub fn session_machine() -> flyco_core::SessionMachine {
    flyco_core::SessionMachine {
        machine_type: "Standard_D4s_v6".to_owned(),
        hourly: Some(flyco_core::Usd::from_cents(19)),
        spot: true,
        capacity: Some(flyco_core::MachineCapacity {
            vcpus: 4,
            memory_mib: 16 * 1024,
        }),
        minimum: None,
    }
}
