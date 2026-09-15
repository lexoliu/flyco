//! The socket the agent knocks on.
//!
//! `flycod mcp` is a separate process — the harness's tool runner — and
//! the desktop lives in `flycod run`. The socket between them is a unix
//! listener the supervisor owns, speaking one JSON line per direction.
//! The MCP process connects per call, which is why a refused answer is a
//! value and not an error: "the user is driving" is a thing the model
//! should hear, not an exception it cannot parse.
//!
//! The same file holds both ends: [`Listener`] is the supervisor's,
//! [`ask`] is the MCP side. The protocol types are the contract between
//! them, so they live once here.

use std::io::{Read as _, Write as _};
use std::os::unix::net::{UnixListener as StdListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use flyco_core::SessionId;
use flyco_core::wire::DesktopInputEvent;
use rustix::event::{PollFd, PollFlags, Timespec};
use serde::{Deserialize, Serialize};

/// How long a served connection may hold the supervisor's attention.
///
/// A request is a line of JSON and the reply is computed in memory — a
/// client that cannot finish inside this is hung, not slow, and the
/// socket's next caller should not queue behind it.
const SERVICE_DEADLINE: Duration = Duration::from_millis(250);

/// The largest request the socket reads — one batched input burst.
const REQUEST_CAP: usize = 256 * 1024;

/// The path the MCP process finds the desktop socket at.
///
/// Derived from the session id rather than configured, because the MCP
/// process inherits the same config file and both processes must agree
/// on it without a second value to keep in sync.
#[must_use]
pub fn socket_path(session: SessionId) -> PathBuf {
    std::env::temp_dir().join(format!("flycod-{session}.desktop.sock"))
}

/// base64 — the protocol's binary arm. A screenshot's PNG crosses the
/// socket inside a JSON line.
#[must_use]
pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// One request from the agent's tool calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentRequest {
    /// Read the screen: a PNG of the current display.
    Screenshot,
    /// Drive the screen: pointer, wheel, and key events in display
    /// coordinates — the same shape the user's takeover input takes.
    Input {
        /// The events to inject, in order.
        events: Vec<DesktopInputEvent>,
    },
    /// Ask what the desktop is — state, and the `DISPLAY` value an app
    /// should be launched with.
    State,
}

/// What a request answered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentReply {
    /// The screen, PNG-encoded and base64'd.
    Screenshot {
        /// The PNG bytes, base64.
        png_base64: String,
    },
    /// The request was applied.
    Done,
    /// What the desktop is.
    State {
        /// Whether the user is currently driving.
        takeover: bool,
        /// The `DISPLAY` value apps should draw to, e.g. `:99`.
        display: String,
        /// The display's width in pixels — the coordinate space input
        /// lands in.
        width: u32,
        /// The display's height in pixels.
        height: u32,
    },
    /// The request was heard and declined — the user is driving, or the
    /// input could not land. `reason` is the sentence the model reads.
    Refused {
        /// Why.
        reason: String,
    },
}

/// Everything a call over the socket can fail with — the MCP side's
/// transport errors.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// The socket does not exist — this session has no desktop.
    #[error("this session has no desktop")]
    NoDesktop,
    /// The connection or the request itself failed.
    #[error("the desktop socket failed: {0}")]
    Io(#[from] std::io::Error),
    /// The answer did not parse.
    #[error("the desktop answered something unrecognizable: {0}")]
    Reply(String),
    /// A local encode inside the supervisor failed.
    #[error("{0}")]
    Encode(String),
    /// The socket answered after the deadline.
    #[error("the desktop did not answer in time")]
    Timeout,
}

/// The supervisor's end of the socket.
#[derive(Debug)]
pub struct Listener {
    /// The bound unix listener, nonblocking.
    socket: StdListener,
    /// The path it is bound on, so a drop unlinks it.
    path: PathBuf,
    /// A half-read request, carried between service passes.
    pending: Option<Pending>,
}

/// A connection being read a poll at a time.
#[derive(Debug)]
struct Pending {
    /// The connection.
    stream: UnixStream,
    /// What has been read so far.
    buffer: Vec<u8>,
    /// When this request stops being waited on.
    deadline: std::time::Instant,
}

impl Listener {
    /// Binds the agent socket, replacing a stale file.
    ///
    /// The socket's absence is meaningful — [`IpcError::NoDesktop`] is
    /// how the MCP side learns the session has no screen — so a previous
    /// run's leftover is unlinked rather than mistaken for a live one.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::Io`] if the path cannot be bound or put in
    /// nonblocking mode.
    pub fn bind(path: &Path) -> Result<Self, IpcError> {
        let _ = std::fs::remove_file(path);
        let socket = StdListener::bind(path).map_err(IpcError::Io)?;
        socket.set_nonblocking(true).map_err(IpcError::Io)?;
        Ok(Self {
            socket,
            path: path.to_owned(),
            pending: None,
        })
    }

    /// Serves one pending or arriving request.
    ///
    /// Returns whether the agent was heard *and answered yes* — the
    /// supervisor records that as desktop activity. A refused request is
    /// the agent being told rather than the desktop being driven.
    pub fn service(&mut self, serve: impl FnOnce(AgentRequest) -> AgentReply) -> bool {
        if self.pending.is_none() {
            match self.socket.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        return false;
                    }
                    self.pending = Some(Pending {
                        stream,
                        buffer: Vec::new(),
                        deadline: std::time::Instant::now() + SERVICE_DEADLINE,
                    });
                    // A fresh connection is rarely already readable —
                    // the request line arrives a syscall later. Fall
                    // through to the poll path rather than return, so a
                    // request that arrived with the accept is still read
                    // this pass.
                }
                Err(_) => return false,
            }
        }
        let Some(pending) = &mut self.pending else {
            return false;
        };
        if std::time::Instant::now() >= pending.deadline {
            self.pending = None;
            return false;
        }
        let mut fds = [PollFd::new(&pending.stream, PollFlags::IN)];
        let ready = rustix::event::poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        );
        if !matches!(ready, Ok(n) if n > 0) {
            return false;
        }
        let mut chunk = [0u8; 8192];
        match pending.stream.read(&mut chunk) {
            Ok(0) => {
                self.pending = None;
                return false;
            }
            Ok(n) => pending.buffer.extend_from_slice(&chunk[..n]),
            Err(_) => return false,
        }
        let Some(end) = pending.buffer.iter().position(|&b| b == b'\n') else {
            if pending.buffer.len() >= REQUEST_CAP {
                self.pending = None;
            }
            return false;
        };
        let line = pending.buffer[..end].to_vec();
        let Some(pending) = self.pending.take() else {
            return false;
        };
        let mut stream = pending.stream;
        let reply = match serde_json::from_slice::<AgentRequest>(&line) {
            Ok(request) => serve(request),
            Err(error) => AgentReply::Refused {
                reason: format!("the request did not parse: {error}"),
            },
        };
        let acted = !matches!(reply, AgentReply::Refused { .. });
        if let Ok(mut encoded) = serde_json::to_vec(&reply) {
            encoded.push(b'\n');
            let _ = stream.write_all(&encoded);
        }
        acted
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The MCP side: one request over a fresh connection.
///
/// Async because the MCP server runs on the daemon's runtime — a bare
/// `UnixStream::connect` there would block the executor the harness's
/// other tools live on.
///
/// # Errors
///
/// Returns [`IpcError::NoDesktop`] when the socket does not exist (the
/// session has no screen), [`IpcError::Io`] for transport failures,
/// [`IpcError::Reply`] for an answer that did not parse, and
/// [`IpcError::Timeout`] after the deadline.
pub async fn ask(path: &Path, request: &AgentRequest) -> Result<AgentReply, IpcError> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound
                || error.kind() == std::io::ErrorKind::ConnectionRefused
            {
                IpcError::NoDesktop
            } else {
                IpcError::Io(error)
            }
        })?;
    let (reader, mut writer) = stream.into_split();
    let mut encoded = serde_json::to_vec(request)
        .map_err(|error| IpcError::Reply(format!("the request did not encode: {error}")))?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await.map_err(IpcError::Io)?;
    let mut lines = BufReader::new(reader).lines();
    let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .map_err(|_| IpcError::Timeout)?
        .map_err(IpcError::Io)?
        .ok_or_else(|| IpcError::Reply("the desktop hung up".to_owned()))?;
    serde_json::from_str(&reply)
        .map_err(|error| IpcError::Reply(format!("the reply did not parse: {error}")))
}

/// The synchronous ask, for callers on a thread that may block — the
/// daemon's own tests and any future non-tokio path.
///
/// # Errors
///
/// The same set as [`ask`].
pub fn ask_blocking(path: &Path, request: &AgentRequest) -> Result<AgentReply, IpcError> {
    let mut stream = UnixStream::connect(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            IpcError::NoDesktop
        } else {
            IpcError::Io(error)
        }
    })?;
    stream
        .set_read_timeout(Some(SERVICE_DEADLINE * 8))
        .map_err(IpcError::Io)?;
    let mut encoded = serde_json::to_vec(request)
        .map_err(|error| IpcError::Reply(format!("the request did not encode: {error}")))?;
    encoded.push(b'\n');
    stream.write_all(&encoded).map_err(IpcError::Io)?;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                reply.extend_from_slice(&chunk[..n]);
                if reply.contains(&b'\n') {
                    break;
                }
                if reply.len() > REQUEST_CAP {
                    return Err(IpcError::Reply("the reply ran past its cap".to_owned()));
                }
            }
            Err(error) => return Err(IpcError::Io(error)),
        }
    }
    let end = reply
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(reply.len());
    serde_json::from_slice(&reply[..end])
        .map_err(|error| IpcError::Reply(format!("the reply did not parse: {error}")))
}
