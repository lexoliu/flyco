//! In-process stand-ins for the two things the daemon talks to.
//!
//! A [`Room`] is a real WebSocket server and a [`ControlPlane`] is a real
//! HTTP server, both on loopback: the daemon's relay client and REST client
//! are exercised through the sockets they will use in production, headers
//! and status codes and all, rather than through a substitute for the
//! transport. What is faked is the *other* end of the harness — [`FakeSession`]
//! stands in for a running Claude Code process, because no test should need
//! Bun installed to prove that an interrupt reached the session.
//!
//! Everything records through channels rather than shared mutable state, so
//! a test reads what happened by draining a receiver.

use std::net::SocketAddr;

use flyco_core::wire::ApprovalPayload;
use flyco_core::{ApprovalId, ApprovalState, ApprovalView, ControlToDaemon, DaemonToControl};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

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
        control_plane_url: "https://dev.flyco.dev/".to_owned(),
        daemon_token: "fd_a-daemon-token".to_owned(),
        permission_mode: flyco_core::PermissionMode::Default,
        auth: flyco_provider::HarnessCredential::ClaudeCode(
            flyco_provider::ClaudeCredential::Inherit,
        ),
        repo: flyco_provider::testing::checkout(),
        machine_origin: flyco_core::MachineOrigin::Auto,
        machine: flyco_provider::testing::session_machine(),
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
    /// The session was put on another model.
    ModelSet(flyco_core::ModelChoice),
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
    /// The models the harness offers were filed with the control plane.
    ModelsReported(Vec<flyco_core::ModelOption>),
    /// How much of the plan is spent was filed with the control plane.
    UsageReported(Vec<flyco_core::UsageWindow>),
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

    fn set_model(
        &self,
        model: flyco_core::ModelChoice,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::ModelSet(model)))
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
/// sockets and they differ in nothing but their vocabulary: a session's
/// daemon holds one to its [`Room`], and an enrolled host holds one to its
/// [`HostRelay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen<Up = DaemonToControl> {
    /// A peer opened a socket, presenting this `Authorization` header.
    Connected(Option<String>),
    /// A peer sent a frame.
    Frame(Up),
    /// A peer's socket ended.
    Disconnected,
}

/// What a test tells a room to do next.
#[derive(Debug)]
pub enum Directive<Down = ControlToDaemon> {
    /// Send a command to the connected peer.
    Send(Down),
    /// Close the current socket, so the peer has to reconnect.
    ///
    /// A proper WebSocket close, and [`Seen::Disconnected`] is reported only
    /// once the peer has acknowledged it — which is what makes "produced
    /// while disconnected" a state a test can be in rather than a race. It
    /// is also what a hibernating or redeployed room actually does.
    Close,
}

/// What a room does with the first frame a peer sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake<Down> {
    /// Answer it, as a session room answers a daemon's `Hello` with
    /// `Welcome`.
    Answer(Down),
    /// Record it and say nothing, as a host room does: a machine that has
    /// greeted is simply one the room now writes to.
    Silent,
    /// Close the socket, as a version or session mismatch does.
    Refuse,
}

/// How the session room answers a daemon's `Hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Greeting {
    /// Answer with `Welcome`, as a matching daemon deserves.
    Welcome,
    /// Close the socket, as a version or session mismatch does.
    Refuse,
}

/// A WebSocket server standing in for one of flyco's Durable Objects.
#[derive(Debug)]
pub struct Relay<Up, Down> {
    /// Base URL the peer should be pointed at, e.g. `http://127.0.0.1:PORT/`.
    pub base: url::Url,
    /// What the room saw, in order.
    pub seen: mpsc::UnboundedReceiver<Seen<Up>>,
    /// What the room should do next.
    pub directives: mpsc::UnboundedSender<Directive<Down>>,
}

/// A WebSocket server standing in for a session's Durable Object.
pub type Room = Relay<DaemonToControl, ControlToDaemon>;

/// A WebSocket server standing in for an enrolled host's Durable Object.
pub type HostRelay =
    Relay<flyco_provider::host::HostToControl, flyco_provider::host::ControlToHost>;

impl<Up, Down> Relay<Up, Down>
where
    Up: serde::de::DeserializeOwned + Send + 'static,
    Down: serde::Serialize + Send + 'static,
{
    /// Starts a room on a loopback port.
    ///
    /// # Panics
    ///
    /// Panics if the loopback socket cannot be bound, which would mean the
    /// test host has no usable networking.
    pub async fn listen(handshake: Handshake<Down>) -> Self
    where
        Down: Clone,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address: SocketAddr = listener.local_addr().expect("read the bound port");

        let (seen_out, seen) = mpsc::unbounded_channel();
        let (directives, mut directive_in) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen_out = seen_out.clone();
                if !serve(stream, handshake.clone(), &seen_out, &mut directive_in).await {
                    break;
                }
            }
        });

        Self {
            base: format!("http://{address}/")
                .parse()
                .expect("a loopback URL"),
            seen,
            directives,
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
    /// # Panics
    ///
    /// Panics if no frame arrives before the timeout, which in these tests
    /// means the peer stopped pumping.
    pub async fn next_frame(&mut self) -> Up {
        loop {
            match self.next().await.expect("the peer sent nothing") {
                Seen::Frame(frame) => return frame,
                Seen::Connected(_) | Seen::Disconnected => {}
            }
        }
    }
}

impl Room {
    /// Starts a session room that greets a daemon the way `greeting` says.
    pub async fn start(greeting: Greeting) -> Self {
        Self::listen(match greeting {
            Greeting::Welcome => Handshake::Answer(ControlToDaemon::Welcome),
            Greeting::Refuse => Handshake::Refuse,
        })
        .await
    }
}

/// Serves one peer connection; returns whether to keep accepting.
#[expect(
    clippy::result_large_err,
    reason = "tungstenite dictates the handshake callback's `ErrorResponse`; \
              this room never refuses one"
)]
async fn serve<Up, Down>(
    stream: TcpStream,
    handshake: Handshake<Down>,
    seen: &mpsc::UnboundedSender<Seen<Up>>,
    directives: &mut mpsc::UnboundedReceiver<Directive<Down>>,
) -> bool
where
    Up: serde::de::DeserializeOwned,
    Down: serde::Serialize,
{
    let mut authorization = None;
    let accepted = tokio_tungstenite::accept_hdr_async(
        stream,
        |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            authorization = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            // This callback may refuse a handshake; nothing here ever does,
            // and the refusal type is the one tungstenite dictates.
            Ok::<_, tokio_tungstenite::tungstenite::handshake::server::ErrorResponse>(response)
        },
    )
    .await;

    let Ok(mut socket) = accepted else {
        return true;
    };
    if seen.send(Seen::Connected(authorization)).is_err() {
        return false;
    }

    // The handshake, the way a room performs it: read the first frame, then
    // answer it, say nothing, or close.
    let Some(Ok(Message::Text(hello))) = socket.next().await else {
        return true;
    };
    let Ok(hello) = serde_json::from_str::<Up>(&hello) else {
        return true;
    };
    if seen.send(Seen::Frame(hello)).is_err() {
        return false;
    }

    match &handshake {
        Handshake::Refuse => {
            let _ = socket.close(None).await;
            let _ = seen.send(Seen::Disconnected);
            return true;
        }
        Handshake::Answer(answer) => {
            let answer = serde_json::to_string(answer).expect("serialize");
            if socket
                .send(Message::Text(Utf8Bytes::from(answer)))
                .await
                .is_err()
            {
                return true;
            }
        }
        Handshake::Silent => {}
    }

    loop {
        tokio::select! {
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<Up>(&text) {
                        Ok(frame) => {
                            if seen.send(Seen::Frame(frame)).is_err() {
                                return false;
                            }
                        }
                        Err(error) => panic!("the peer sent an unreadable frame: {error}: {text}"),
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    let _ = seen.send(Seen::Disconnected);
                    return true;
                }
            },
            directive = directives.recv() => match directive {
                Some(Directive::Send(command)) => {
                    let json = serde_json::to_string(&command).expect("serialize");
                    if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
                        let _ = seen.send(Seen::Disconnected);
                        return true;
                    }
                }
                Some(Directive::Close) => {
                    let _ = socket.close(None).await;
                    // Drain until the peer's own close arrives: only then is
                    // it certainly reconnecting rather than still holding a
                    // socket it believes is live.
                    while let Some(Ok(_)) = socket.next().await {}
                    let _ = seen.send(Seen::Disconnected);
                    return true;
                }
                None => return false,
            },
        }
    }
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
