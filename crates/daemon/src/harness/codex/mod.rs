//! The Codex harness driver.
//!
//! Codex is driven through `codex app-server`, a long-lived JSON-RPC 2.0
//! process over stdio. Flycod is a JSON-RPC client: it handshakes, opens or
//! resumes a thread, translates `item/*` notifications into
//! [`flyco_core::HarnessEvent`], and answers server→client approval
//! requests with the user's decision from flyco's own UI.
//!
//! # Shape
//!
//! One task owns everything mutable — the child's stdin, the current turn,
//! and the map from flyco [`ApprovalId`]s to JSON-RPC request ids.
//! [`CodexSession`] is a handle that sends it messages and awaits an
//! acknowledgement; a reader task turns the child's stdout into messages
//! for the same task. There is no shared state and therefore no lock.

pub mod normalize;
pub mod protocol;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use flyco_core::ApprovalId;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use self::normalize::{ApprovalParams, Normalizer};
use self::protocol::{
    ApprovalDecision, ApprovalDecisionBody, ClientCapabilities, ClientInfo, Envelope,
    InitializeParams, RequestId, ThreadCompactStartParams, ThreadParams, TurnInterruptParams,
    TurnStartParams, UserInput, method,
};
use super::{Harness, HarnessSession, SessionOutput, StartRequest, Started, ToolApproval};
use crate::config::{CodexAuth, CodexConfig};

/// How long a clean shutdown may take before the child is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Capacity of the driver's inbound and outbound channels.
const CHANNEL_DEPTH: usize = 256;

/// JSON-RPC error code flycod returns for server requests it will not honour.
const METHOD_NOT_IMPLEMENTED: i64 = -32601;

/// The Codex driver failed.
#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    /// The `codex` executable is not where the config says it is.
    #[error(
        "codex was not found at {executable} — flycod drives Codex through `codex app-server` and \
         cannot run without it"
    )]
    BinaryMissing {
        /// The executable that could not be spawned.
        executable: PathBuf,
    },
    /// Spawning `codex` failed for some other reason.
    #[error("could not run {executable}")]
    Spawn {
        /// The executable being run.
        executable: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// Isolated `CODEX_HOME` could not be prepared.
    #[error("could not prepare CODEX_HOME at {path}")]
    Home {
        /// The directory that could not be written.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The session working directory could not be created.
    #[error("could not create the session workdir at {path}")]
    Workdir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// Reading from or writing to the app-server failed.
    #[error("codex app-server {stream} failed")]
    Io {
        /// Which stream broke.
        stream: &'static str,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// A piped stdio handle was not there after spawning.
    #[error("the app-server process has no piped {stream}")]
    MissingStdio {
        /// Which handle was missing.
        stream: &'static str,
    },
    /// The app-server exited before completing the handshake.
    #[error("the app-server exited before finishing its handshake")]
    ExitedDuringHandshake,
    /// The app-server broke the JSON-RPC framing.
    #[error("the app-server broke the JSON-RPC framing: {detail}")]
    Protocol {
        /// What was wrong.
        detail: String,
    },
    /// The app-server answered a handshake request with an error.
    #[error("the app-server rejected {method}: {message}")]
    HandshakeRejected {
        /// The method that failed.
        method: &'static str,
        /// The error message.
        message: String,
    },
    /// The session's task is gone, so no command can be delivered.
    #[error("the Codex session has stopped")]
    Stopped,
    /// A second compaction was requested before the first completed.
    #[error("a Codex context compaction is already in flight")]
    CompactionInFlight,
    /// The app-server rejected a runtime request.
    #[error("the app-server rejected {method}: {message}")]
    RequestRejected {
        /// Method that failed.
        method: &'static str,
        /// App-server explanation.
        message: String,
    },
    /// The app-server did not exit within [`SHUTDOWN_GRACE`].
    #[error("the app-server did not exit within {}s and was killed", SHUTDOWN_GRACE.as_secs())]
    ShutdownTimedOut,
}

/// A configured, not-yet-started Codex harness.
#[derive(Debug)]
pub struct CodexHarness {
    config: CodexConfig,
}

impl CodexHarness {
    /// Builds a harness from its configuration.
    #[must_use]
    pub const fn new(config: CodexConfig) -> Self {
        Self { config }
    }
}

impl Harness for CodexHarness {
    type Session = CodexSession;
    type Error = CodexError;

    async fn start(self, request: StartRequest) -> Result<Started<Self::Session>, CodexError> {
        prepare_home(&self.config.auth).await?;
        tokio::fs::create_dir_all(&request.workdir)
            .await
            .map_err(|source| CodexError::Workdir {
                path: request.workdir.clone(),
                source,
            })?;
        let mut child = spawn(&self.config, &request.workdir)?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or(CodexError::MissingStdio { stream: "stdin" })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(CodexError::MissingStdio { stream: "stdout" })?;
        let stderr = child
            .stderr
            .take()
            .ok_or(CodexError::MissingStdio { stream: "stderr" })?;

        tokio::spawn(log_stderr(stderr));

        let mut lines = BufReader::new(stdout).lines();
        let mut next_id = 1_u64;
        let thread_id =
            handshake(&mut stdin, &mut lines, &mut next_id, &self.config, &request).await?;
        let resumed = request.resume_session_id.is_some();

        let (commands, inbox) = mpsc::channel(CHANNEL_DEPTH);
        let (outputs, output_rx) = mpsc::channel(CHANNEL_DEPTH);

        tokio::spawn(read_stdout(lines, commands.clone()));
        tokio::spawn(
            Driver {
                stdin: Some(stdin),
                child,
                next_id,
                thread_id,
                normalizer: Normalizer::new(),
                outputs,
                pending_approvals: BTreeMap::new(),
                pending_compaction: None,
                stopped: false,
            }
            .run(inbox, resumed),
        );

        Ok(Started {
            session: CodexSession { commands },
            outputs: output_rx,
        })
    }
}

/// The control handle of a running Codex session.
#[derive(Debug, Clone)]
pub struct CodexSession {
    commands: mpsc::Sender<DriverCommand>,
}

impl CodexSession {
    async fn ask(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), CodexError>>) -> DriverCommand,
    ) -> Result<(), CodexError> {
        let (ack, answer) = oneshot::channel();
        self.commands
            .send(make(ack))
            .await
            .map_err(|_| CodexError::Stopped)?;
        answer.await.map_err(|_| CodexError::Stopped)?
    }
}

impl HarnessSession for CodexSession {
    type Error = CodexError;

    async fn send_user_message(&self, text: String) -> Result<(), CodexError> {
        self.ask(|ack| DriverCommand::UserMessage { text, ack })
            .await
    }

    async fn interrupt(&self) -> Result<(), CodexError> {
        self.ask(|ack| DriverCommand::Interrupt { ack }).await
    }

    async fn compact(&self) -> Result<(), CodexError> {
        self.ask(|ack| DriverCommand::Compact { ack }).await
    }

    async fn decide_approval(&self, approval: ToolApproval) -> Result<(), CodexError> {
        self.ask(|ack| DriverCommand::Approval { approval, ack })
            .await
    }

    async fn shutdown(self) -> Result<(), CodexError> {
        self.ask(|ack| DriverCommand::Shutdown { ack }).await
    }
}

/// Everything the driver task can be asked to do.
#[derive(Debug)]
enum DriverCommand {
    /// From the handle: push a user message and open a turn.
    UserMessage {
        text: String,
        ack: oneshot::Sender<Result<(), CodexError>>,
    },
    /// From the handle: end the current turn.
    Interrupt {
        ack: oneshot::Sender<Result<(), CodexError>>,
    },
    /// From the handle: compact the conversation context.
    Compact {
        ack: oneshot::Sender<Result<(), CodexError>>,
    },
    /// From the handle: answer a pending approval.
    Approval {
        approval: ToolApproval,
        ack: oneshot::Sender<Result<(), CodexError>>,
    },
    /// From the handle: stop the session.
    Shutdown {
        ack: oneshot::Sender<Result<(), CodexError>>,
    },
    /// From the reader: one framed message.
    Frame(Envelope),
    /// From the reader: the app-server's stdout ended.
    StdoutClosed,
    /// From the reader: a line that is not an [`Envelope`].
    ProtocolError { detail: String },
}

/// Writes isolated `CODEX_HOME` contents when this session injects credentials.
async fn prepare_home(auth: &CodexAuth) -> Result<(), CodexError> {
    let Some(home) = auth.home() else {
        return Ok(());
    };
    tokio::fs::create_dir_all(home)
        .await
        .map_err(|source| CodexError::Home {
            path: home.to_owned(),
            source,
        })?;

    let config = CodexHomeFile {
        cli_auth_credentials_store: "file",
    };
    let config_toml = toml::to_string_pretty(&config).expect("CodexHomeFile serializes");
    tokio::fs::write(home.join("config.toml"), config_toml)
        .await
        .map_err(|source| CodexError::Home {
            path: home.join("config.toml"),
            source,
        })?;

    let auth_file = match auth {
        CodexAuth::Inherit => return Ok(()),
        CodexAuth::ApiKey { key, .. } => AuthFile {
            auth_mode: None,
            openai_api_key: Some(key.clone()),
            tokens: None,
            last_refresh: None,
        },
        CodexAuth::ChatGpt {
            id_token,
            access_token,
            refresh_token,
            account_id,
            ..
        } => AuthFile {
            auth_mode: Some(AuthMode::Chatgpt),
            openai_api_key: None,
            tokens: Some(AuthTokens {
                id_token: id_token.clone(),
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                account_id: account_id.clone(),
            }),
            // Codex reads this to decide how stale the grant is. The grant
            // was minted or renewed by the control plane moments ago, on
            // this machine's own provision, so "now" is the truth.
            last_refresh: Some(now_rfc3339()),
        },
    };
    let auth_json = serde_json::to_vec_pretty(&auth_file).expect("AuthFile serializes");
    let path = home.join("auth.json");
    tokio::fs::write(&path, auth_json)
        .await
        .map_err(|source| CodexError::Home {
            path: path.clone(),
            source,
        })?;
    // The daemon is root on a provisioned machine and the agent runs as
    // somebody else, so this is the file that keeps a `ChatGPT` refresh
    // token out of the agent's reach — the same takeover rule the rest of
    // `CODEX_HOME` is written under.
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|source| CodexError::Home { path, source })?;
    Ok(())
}

/// The current instant as Codex writes `last_refresh`: RFC 3339, UTC.
///
/// # Panics
///
/// Panics if the host clock is set before the Unix epoch, or so far past it
/// that the timestamp is not a representable date — a broken machine the
/// daemon must not quietly write a wrong credential file on.
fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch")
        .as_secs();
    let seconds = i64::try_from(seconds).expect("the system clock is within the representable era");
    OffsetDateTime::from_unix_timestamp(seconds)
        .expect("a Unix timestamp from the system clock is a representable instant")
        .format(&Rfc3339)
        .expect("an OffsetDateTime always formats as RFC 3339")
}

#[derive(Debug, serde::Serialize)]
struct CodexHomeFile {
    cli_auth_credentials_store: &'static str,
}

/// How `auth.json` says the account is signed in.
///
/// Only the `ChatGPT` mode is written: an API key is recognised by the
/// `OPENAI_API_KEY` field alone, which is how Codex has always read one.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum AuthMode {
    Chatgpt,
}

/// `$CODEX_HOME/auth.json`, as Codex's own loader reads it.
#[derive(Debug, serde::Serialize)]
struct AuthFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_mode: Option<AuthMode>,
    #[serde(rename = "OPENAI_API_KEY", skip_serializing_if = "Option::is_none")]
    openai_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<AuthTokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_refresh: Option<String>,
}

/// The `tokens` object of a `ChatGPT` `auth.json`.
#[derive(Debug, serde::Serialize)]
struct AuthTokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
    account_id: String,
}

fn spawn(config: &CodexConfig, workdir: &std::path::Path) -> Result<Child, CodexError> {
    let mut command = Command::new(&config.bin);
    command
        .arg("app-server")
        .arg("--strict-config")
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(home) = config.auth.home() {
        command.env("CODEX_HOME", home);
    }
    command.env("LOG_FORMAT", "json");
    match command.spawn() {
        Ok(child) => Ok(child),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(CodexError::BinaryMissing {
                executable: config.bin.clone(),
            })
        }
        Err(source) => Err(CodexError::Spawn {
            executable: config.bin.clone(),
            source,
        }),
    }
}

async fn handshake<R>(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<R>,
    next_id: &mut u64,
    config: &CodexConfig,
    request: &StartRequest,
) -> Result<String, CodexError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let initialize_id = take_id(next_id);
    write_envelope(
        stdin,
        &Envelope::request(
            initialize_id.clone(),
            method::INITIALIZE,
            serde_json::to_value(InitializeParams {
                client_info: ClientInfo {
                    name: "flycod".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
                capabilities: ClientCapabilities {
                    experimental_api: false,
                },
            })
            .expect("InitializeParams serializes"),
        ),
    )
    .await?;
    expect_result(lines, &initialize_id, method::INITIALIZE).await?;

    write_envelope(
        stdin,
        &Envelope::notification(method::INITIALIZED, Value::Null),
    )
    .await?;

    let thread_id = take_id(next_id);
    let params = ThreadParams {
        cwd: path_string(&request.workdir),
        approval_policy: config.approval_policy.as_str().to_owned(),
        sandbox: config.sandbox.as_str().to_owned(),
        model: config.model.clone(),
        thread_id: request.resume_session_id.clone(),
    };
    let method_name = if request.resume_session_id.is_some() {
        method::THREAD_RESUME
    } else {
        method::THREAD_START
    };
    write_envelope(
        stdin,
        &Envelope::request(
            thread_id.clone(),
            method_name,
            serde_json::to_value(params).expect("ThreadParams serializes"),
        ),
    )
    .await?;
    let result = expect_result(lines, &thread_id, method_name).await?;
    thread_id_from(&result).ok_or_else(|| CodexError::Protocol {
        detail: "thread/start returned no thread id".to_owned(),
    })
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

const fn take_id(next_id: &mut u64) -> RequestId {
    let id = RequestId::number(*next_id);
    *next_id += 1;
    id
}

fn thread_id_from(result: &Value) -> Option<String> {
    result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .or_else(|| result.pointer("/thread/sessionId").and_then(Value::as_str))
        .map(str::to_owned)
}

async fn expect_result<R>(
    lines: &mut tokio::io::Lines<R>,
    id: &RequestId,
    method_name: &'static str,
) -> Result<Value, CodexError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|source| CodexError::Io {
                stream: "stdout",
                source,
            })?
            .ok_or(CodexError::ExitedDuringHandshake)?;
        let frame: Envelope =
            serde_json::from_str(&line).map_err(|source| CodexError::Protocol {
                detail: format!("{source} in {line:?}"),
            })?;
        match frame {
            Envelope::Response {
                id: response_id,
                result,
            } if &response_id == id => return Ok(result),
            Envelope::Error {
                id: response_id,
                error,
            } if &response_id == id => {
                return Err(CodexError::HandshakeRejected {
                    method: method_name,
                    message: error.message,
                });
            }
            Envelope::Notification { method, .. } => {
                tracing::debug!(method, "dropping a notification during handshake");
            }
            other => {
                return Err(CodexError::Protocol {
                    detail: format!("unexpected handshake frame: {other:?}"),
                });
            }
        }
    }
}

async fn write_envelope(stdin: &mut ChildStdin, envelope: &Envelope) -> Result<(), CodexError> {
    let mut line = serde_json::to_string(envelope).expect("every Envelope serializes to JSON");
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|source| CodexError::Io {
            stream: "stdin",
            source,
        })?;
    stdin.flush().await.map_err(|source| CodexError::Io {
        stream: "stdin",
        source,
    })
}

async fn log_stderr(stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => tracing::debug!(target: "flycod::codex", "{line}"),
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "lost the app-server's stderr");
                break;
            }
        }
    }
}

async fn read_stdout<R>(mut lines: tokio::io::Lines<R>, commands: mpsc::Sender<DriverCommand>)
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let command = match lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<Envelope>(&line) {
                Ok(frame) => DriverCommand::Frame(frame),
                Err(source) => DriverCommand::ProtocolError {
                    detail: format!("{source} in {line:?}"),
                },
            },
            Ok(None) => DriverCommand::StdoutClosed,
            Err(error) => DriverCommand::ProtocolError {
                detail: format!("could not read the app-server's stdout: {error}"),
            },
        };
        let terminal = !matches!(command, DriverCommand::Frame(_));
        if commands.send(command).await.is_err() || terminal {
            break;
        }
    }
}

async fn emit(outputs: &mpsc::Sender<SessionOutput>, output: SessionOutput) -> bool {
    if outputs.send(output).await.is_err() {
        tracing::debug!("nothing is consuming the session's output stream");
        return false;
    }
    true
}

/// The task that owns the app-server process and everything mutable.
struct Driver {
    stdin: Option<ChildStdin>,
    child: Child,
    next_id: u64,
    thread_id: String,
    normalizer: Normalizer,
    outputs: mpsc::Sender<SessionOutput>,
    pending_approvals: BTreeMap<ApprovalId, RequestId>,
    pending_compaction: Option<(RequestId, oneshot::Sender<Result<(), CodexError>>)>,
    stopped: bool,
}

impl Driver {
    async fn run(mut self, mut inbox: mpsc::Receiver<DriverCommand>, resumed: bool) {
        let started = SessionOutput::Started {
            session_id: self.thread_id.clone(),
        };
        if !emit(&self.outputs, started).await {
            return;
        }
        if resumed {
            tracing::info!(thread = %self.thread_id, "resumed a Codex thread");
        } else {
            tracing::info!(thread = %self.thread_id, "started a Codex thread");
        }

        while let Some(command) = inbox.recv().await {
            if !self.handle(command).await {
                break;
            }
        }
        if let Err(error) = self.stop().await {
            tracing::error!(%error, "the app-server did not stop cleanly");
        }
        tracing::debug!("the Codex driver task finished");
    }

    async fn handle(&mut self, command: DriverCommand) -> bool {
        match command {
            DriverCommand::UserMessage { text, ack } => {
                let result = self.start_turn(text).await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Interrupt { ack } => {
                let result = self.interrupt_turn().await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Compact { ack } => {
                if self.pending_compaction.is_some() {
                    let _ = ack.send(Err(CodexError::CompactionInFlight));
                    return true;
                }
                let id = take_id(&mut self.next_id);
                let result = self.start_compaction(id.clone()).await;
                match result {
                    Ok(()) => {
                        self.pending_compaction = Some((id, ack));
                        true
                    }
                    Err(error) => {
                        let _ = ack.send(Err(error));
                        false
                    }
                }
            }
            DriverCommand::Approval { approval, ack } => {
                let result = self.decide(approval).await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Shutdown { ack } => {
                let stopped = self.stop().await;
                let _ = ack.send(stopped);
                false
            }
            DriverCommand::Frame(frame) => self.on_frame(frame).await,
            DriverCommand::StdoutClosed => {
                tracing::info!("the app-server closed its output stream");
                false
            }
            DriverCommand::ProtocolError { detail } => {
                emit(
                    &self.outputs,
                    SessionOutput::Fatal {
                        error: CodexError::Protocol { detail }.to_string(),
                    },
                )
                .await;
                false
            }
        }
    }

    async fn start_turn(&mut self, text: String) -> Result<(), CodexError> {
        let id = take_id(&mut self.next_id);
        let params = TurnStartParams {
            thread_id: self.thread_id.clone(),
            input: vec![UserInput::Text {
                text,
                text_elements: [],
            }],
        };
        write_envelope(
            self.stdin.as_mut().ok_or(CodexError::Stopped)?,
            &Envelope::request(
                id,
                method::TURN_START,
                serde_json::to_value(params).expect("TurnStartParams serializes"),
            ),
        )
        .await
    }

    async fn interrupt_turn(&mut self) -> Result<(), CodexError> {
        let id = take_id(&mut self.next_id);
        let params = TurnInterruptParams {
            thread_id: self.thread_id.clone(),
        };
        write_envelope(
            self.stdin.as_mut().ok_or(CodexError::Stopped)?,
            &Envelope::request(
                id,
                method::TURN_INTERRUPT,
                serde_json::to_value(params).expect("TurnInterruptParams serializes"),
            ),
        )
        .await
    }

    async fn start_compaction(&mut self, id: RequestId) -> Result<(), CodexError> {
        let params = ThreadCompactStartParams {
            thread_id: self.thread_id.clone(),
        };
        write_envelope(
            self.stdin.as_mut().ok_or(CodexError::Stopped)?,
            &Envelope::request(
                id,
                method::THREAD_COMPACT_START,
                serde_json::to_value(params).expect("ThreadCompactStartParams serializes"),
            ),
        )
        .await
    }

    async fn decide(&mut self, approval: ToolApproval) -> Result<(), CodexError> {
        let id = approval.id();
        let Some(request_id) = self.pending_approvals.remove(&id) else {
            return Err(CodexError::Protocol {
                detail: format!("no app-server approval is waiting on {id}"),
            });
        };
        let decision = match approval {
            ToolApproval::Allow { .. } => ApprovalDecision::Accept,
            ToolApproval::Deny { .. } => ApprovalDecision::Decline,
        };
        write_envelope(
            self.stdin.as_mut().ok_or(CodexError::Stopped)?,
            &Envelope::response(
                request_id,
                serde_json::to_value(ApprovalDecisionBody { decision })
                    .expect("ApprovalDecisionBody serializes"),
            ),
        )
        .await
    }

    async fn on_frame(&mut self, frame: Envelope) -> bool {
        match frame {
            Envelope::Notification { method, params } => {
                for event in self.normalizer.on_notification(&method, &params) {
                    if !emit(&self.outputs, SessionOutput::Event { event }).await {
                        return false;
                    }
                }
                true
            }
            Envelope::Request { id, method, params } => {
                self.on_server_request(id, method, params).await
            }
            Envelope::Response { id, .. } => {
                if self
                    .pending_compaction
                    .as_ref()
                    .is_some_and(|(pending, _)| pending == &id)
                {
                    let (_, ack) = self.pending_compaction.take().expect("checked above");
                    let _ = ack.send(Ok(()));
                }
                // turn/start and turn/interrupt acknowledge with `{}`; the
                // terminal signal is the matching notification.
                true
            }
            Envelope::Error { id, error } => {
                if self
                    .pending_compaction
                    .as_ref()
                    .is_some_and(|(pending, _)| pending == &id)
                {
                    let (_, ack) = self.pending_compaction.take().expect("checked above");
                    let message = error.message;
                    let _ = ack.send(Err(CodexError::RequestRejected {
                        method: method::THREAD_COMPACT_START,
                        message: message.clone(),
                    }));
                    return emit(
                        &self.outputs,
                        SessionOutput::Event {
                            event: flyco_core::HarnessEvent::ContextCompactionFailed {
                                error: message,
                            },
                        },
                    )
                    .await;
                }
                emit(
                    &self.outputs,
                    SessionOutput::Fatal {
                        error: error.message,
                    },
                )
                .await
            }
        }
    }

    async fn on_server_request(
        &mut self,
        id: RequestId,
        method_name: String,
        params: Value,
    ) -> bool {
        match method_name.as_str() {
            method::COMMAND_APPROVAL
            | method::FILE_CHANGE_APPROVAL
            | method::PERMISSIONS_APPROVAL => {
                let parsed: ApprovalParams =
                    serde_json::from_value(params).unwrap_or(ApprovalParams {
                        thread_id: None,
                        turn_id: None,
                        item: None,
                        command: None,
                    });
                let flyco_id = ApprovalId::generate();
                self.pending_approvals.insert(flyco_id, id);
                emit(
                    &self.outputs,
                    SessionOutput::ApprovalRequest {
                        id: flyco_id,
                        tool: parsed.tool(&method_name),
                        input: parsed.input(),
                        suggestions: None,
                    },
                )
                .await
            }
            method::AUTH_REFRESH => {
                tracing::error!("the app-server asked to refresh ChatGPT tokens; flycod does not");
                if let Some(stdin) = self.stdin.as_mut() {
                    let _ = write_envelope(
                        stdin,
                        &Envelope::error_response(
                            id,
                            METHOD_NOT_IMPLEMENTED,
                            "flycod does not refresh ChatGPT tokens".to_owned(),
                        ),
                    )
                    .await;
                }
                emit(
                    &self.outputs,
                    SessionOutput::Fatal {
                        error: "the app-server asked to refresh ChatGPT tokens; flycod does not"
                            .to_owned(),
                    },
                )
                .await;
                false
            }
            other => {
                tracing::warn!(
                    method = other,
                    "denying an unimplemented app-server request"
                );
                match self.stdin.as_mut() {
                    Some(stdin) => write_envelope(
                        stdin,
                        &Envelope::error_response(
                            id,
                            METHOD_NOT_IMPLEMENTED,
                            format!("flycod does not implement {other}"),
                        ),
                    )
                    .await
                    .is_ok(),
                    None => false,
                }
            }
        }
    }

    async fn stop(&mut self) -> Result<(), CodexError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.shutdown().await;
            drop(stdin);
        }
        match tokio::time::timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(source)) => Err(CodexError::Io {
                stream: "wait",
                source,
            }),
            Err(_) => {
                self.child.start_kill().ok();
                let _ = self.child.wait().await;
                Err(CodexError::ShutdownTimedOut)
            }
        }
    }
}
