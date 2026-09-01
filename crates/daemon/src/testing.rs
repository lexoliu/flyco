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

// ── The harness, faked ──

/// Something the wire client asked the harness to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// A user message was pushed into the session.
    UserMessage(String),
    /// The current turn was interrupted.
    Interrupt,
    /// The session context was compacted.
    Compact,
    /// A pending approval was answered.
    Approval {
        /// The approval that was answered.
        id: ApprovalId,
        /// Whether it was allowed.
        allowed: bool,
    },
    /// The session was shut down.
    Shutdown,
}

/// The harness never fails in these tests.
#[derive(Debug, thiserror::Error)]
#[error("the fake harness session stopped")]
pub struct FakeSessionError;

/// A [`HarnessSession`] that records what it was told.
#[derive(Debug)]
pub struct FakeSession {
    calls: mpsc::UnboundedSender<Call>,
}

impl FakeSession {
    /// Creates a session and the stream of calls made against it.
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Call>) {
        let (calls, received) = mpsc::unbounded_channel();
        (Self { calls }, received)
    }

    fn record(&self, call: Call) -> Result<(), FakeSessionError> {
        self.calls.send(call).map_err(|_| FakeSessionError)
    }
}

impl HarnessSession for FakeSession {
    type Error = FakeSessionError;

    fn send_user_message(
        &self,
        text: String,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::UserMessage(text)))
    }

    fn interrupt(&self) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::Interrupt))
    }

    fn compact(&self) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        core::future::ready(self.record(Call::Compact))
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

// ── The session room, for real, on loopback ──

/// What the room saw a daemon do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// A daemon opened a socket, presenting this `Authorization` header.
    Connected(Option<String>),
    /// A daemon sent a frame.
    Frame(DaemonToControl),
    /// A daemon's socket ended.
    Disconnected,
}

/// What a test tells the room to do next.
#[derive(Debug)]
pub enum Directive {
    /// Send a command to the connected daemon.
    Send(ControlToDaemon),
    /// Close the current socket, so the daemon has to reconnect.
    ///
    /// A proper WebSocket close, and [`Seen::Disconnected`] is reported only
    /// once the daemon has acknowledged it — which is what makes "produced
    /// while disconnected" a state a test can be in rather than a race. It
    /// is also what a hibernating or redeployed room actually does.
    Close,
}

/// How the room answers a daemon's `Hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Greeting {
    /// Answer with `Welcome`, as a matching daemon deserves.
    Welcome,
    /// Close the socket, as a version or session mismatch does.
    Refuse,
}

/// A WebSocket server standing in for a session's Durable Object.
#[derive(Debug)]
pub struct Room {
    /// Base URL a daemon should be pointed at, e.g. `http://127.0.0.1:PORT/`.
    pub base: url::Url,
    /// What the room saw, in order.
    pub seen: mpsc::UnboundedReceiver<Seen>,
    /// What the room should do next.
    pub directives: mpsc::UnboundedSender<Directive>,
}

impl Room {
    /// Starts a room on a loopback port.
    ///
    /// # Panics
    ///
    /// Panics if the loopback socket cannot be bound, which would mean the
    /// test host has no usable networking.
    pub async fn start(greeting: Greeting) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address: SocketAddr = listener.local_addr().expect("read the bound port");

        let (seen_out, seen) = mpsc::unbounded_channel();
        let (directives, mut directive_in) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen_out = seen_out.clone();
                if !serve(stream, greeting, &seen_out, &mut directive_in).await {
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
    pub async fn next(&mut self) -> Option<Seen> {
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
    /// means the daemon stopped pumping.
    pub async fn next_frame(&mut self) -> DaemonToControl {
        loop {
            match self.next().await.expect("the daemon sent nothing") {
                Seen::Frame(frame) => return frame,
                Seen::Connected(_) | Seen::Disconnected => {}
            }
        }
    }
}

/// Serves one daemon connection; returns whether to keep accepting.
#[expect(
    clippy::result_large_err,
    reason = "tungstenite dictates the handshake callback's `ErrorResponse`; \
              this room never refuses one"
)]
async fn serve(
    stream: TcpStream,
    greeting: Greeting,
    seen: &mpsc::UnboundedSender<Seen>,
    directives: &mut mpsc::UnboundedReceiver<Directive>,
) -> bool {
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

    // The handshake, the way a room performs it: read `Hello`, then either
    // welcome or close.
    let Some(Ok(Message::Text(hello))) = socket.next().await else {
        return true;
    };
    let Ok(hello) = serde_json::from_str::<DaemonToControl>(&hello) else {
        return true;
    };
    if seen.send(Seen::Frame(hello)).is_err() {
        return false;
    }

    if greeting == Greeting::Refuse {
        let _ = socket.close(None).await;
        let _ = seen.send(Seen::Disconnected);
        return true;
    }
    let welcome = serde_json::to_string(&ControlToDaemon::Welcome).expect("serialize");
    if socket
        .send(Message::Text(Utf8Bytes::from(welcome)))
        .await
        .is_err()
    {
        return true;
    }

    loop {
        tokio::select! {
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<DaemonToControl>(&text) {
                        Ok(frame) => {
                            if seen.send(Seen::Frame(frame)).is_err() {
                                return false;
                            }
                        }
                        Err(error) => panic!("the daemon sent an unreadable frame: {error}: {text}"),
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
                    // Drain until the daemon's own close arrives: only then
                    // is it certainly reconnecting rather than still holding
                    // a socket it believes is live.
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
