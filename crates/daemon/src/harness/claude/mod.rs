//! The Claude Code harness driver.
//!
//! Claude Code is driven through the [Claude Agent SDK], which is
//! TypeScript-only. The SDK is not an optional convenience here: it is the
//! only supported route to the three things flyco's product depends on —
//! `canUseTool` approval callbacks (flyco owns the approval UI, never the
//! model), `interrupt()` (SIGINT semantics for ending a turn), and
//! `SessionStore` (transcript persistence flyco controls, which is what
//! makes resume-onto-any-machine possible). So flycod ships a small Bun
//! sidecar, embedded in the binary, and speaks [`protocol`] to it over
//! stdio.
//!
//! # Shape
//!
//! One task owns everything mutable — the child's stdin, the transcript
//! store, and the [`normalize::Normalizer`]'s turn state. [`ClaudeSession`]
//! is a handle that sends it messages and awaits an acknowledgement; a
//! reader task turns the child's stdout into messages for the same task.
//! There is no shared state and therefore no lock.
//!
//! # Session identity arrives early, capabilities arrive late
//!
//! These are two events, [`SessionOutput::Started`] and
//! [`SessionOutput::Capabilities`], because the SDK reports them at two
//! different times and nothing can pull them together.
//!
//! Constructing the `query` spawns the CLI and sends its `initialize`
//! control request immediately, so the session is warm and identified
//! before the user types — the sidecar picks the session UUID itself
//! (`Options.sessionId`), or reuses the one being resumed. `Started` is
//! emitted right there.
//!
//! The capability list is only ever on the `system/init` **stream** frame,
//! which the CLI emits at the start of a *turn*: it does not exist at boot,
//! the `initialize` control response does not carry it, and `reinitialize()`
//! does not re-emit it (verified against 2.1.250 — the re-initialize pushes
//! `background_tasks_changed` and nothing else). So capabilities cannot be
//! known until the first turn runs, and **every consumer of them must
//! tolerate "unknown yet"**: gate a feature only once a
//! [`SessionOutput::Capabilities`] has named it, never treat the pre-first-turn
//! silence as "this build supports nothing", and never substitute a version
//! check. Later init frames may revise the set, so the newest one wins.
//!
//! [Claude Agent SDK]: https://docs.claude.com/en/api/agent-sdk/overview

pub mod normalize;
pub mod protocol;
pub mod sidecar;
pub mod store;

use std::collections::VecDeque;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot};

use self::normalize::Normalizer;
use self::protocol::{SidecarCommand, SidecarEvent, StoreOp, StoreRequestId};
use self::sidecar::{SidecarConfig, SidecarError};
use self::store::{StoreError, TranscriptStore};
use super::{Harness, HarnessSession, SessionOutput, StartRequest, Started, ToolApproval};
use crate::config::ClaudeConfig;
use crate::mount::{Mount, MountError};

/// How long a clean shutdown may take before the child is killed.
///
/// Killing is the failure path, never the routine one: a turn ends through
/// the SDK's `interrupt()`, and the session ends through
/// [`SidecarCommand::Shutdown`].
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Capacity of the driver's inbound and outbound channels.
const CHANNEL_DEPTH: usize = 256;

/// The Claude Code driver failed.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    /// The Bun sidecar could not be prepared or launched.
    #[error(transparent)]
    Sidecar(#[from] SidecarError),
    /// flyco's MCP server could not be declared to the harness, or the
    /// harness came up without it.
    #[error(transparent)]
    Mount(#[from] MountError),
    /// The transcript store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Reading from or writing to the sidecar failed.
    #[error("sidecar {stream} failed")]
    Io {
        /// Which stream broke.
        stream: &'static str,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// A piped stdio handle was not there after spawning.
    #[error("the sidecar process has no piped {stream}")]
    MissingStdio {
        /// Which handle was missing.
        stream: &'static str,
    },
    /// The sidecar exited before completing the handshake.
    #[error("the sidecar exited before reporting itself ready")]
    ExitedDuringHandshake,
    /// The sidecar's first line was not [`SidecarEvent::Ready`].
    #[error("the sidecar's first message was `{tag}`, not `ready`")]
    UnexpectedHandshake {
        /// The tag that arrived instead.
        tag: &'static str,
    },
    /// The sidecar reported it cannot run.
    #[error("the sidecar reported a fatal error: {error}")]
    SidecarFatal {
        /// What the sidecar said.
        error: String,
    },
    /// The sidecar broke the line protocol.
    #[error("the sidecar broke the line protocol: {detail}")]
    Protocol {
        /// What was wrong.
        detail: String,
    },
    /// The session's task is gone, so no command can be delivered.
    #[error("the Claude Code session has stopped")]
    Stopped,
    /// The sidecar did not exit within [`SHUTDOWN_GRACE`].
    #[error("the sidecar did not exit within {}s and was killed", SHUTDOWN_GRACE.as_secs())]
    ShutdownTimedOut,
}

/// A configured, not-yet-started Claude Code harness.
///
/// Generic over its [`TranscriptStore`] so the M3b control-plane store
/// swaps in without a trait object or a branch.
#[derive(Debug)]
pub struct ClaudeCodeHarness<S> {
    claude: ClaudeConfig,
    sidecar: SidecarConfig,
    mount: Mount,
    store: S,
}

impl<S: TranscriptStore> ClaudeCodeHarness<S> {
    /// Builds a harness from its configuration, the MCP servers the session
    /// may reach, and the store its transcripts go to.
    pub const fn new(claude: ClaudeConfig, sidecar: SidecarConfig, mount: Mount, store: S) -> Self {
        Self {
            claude,
            sidecar,
            mount,
            store,
        }
    }
}

impl<S: TranscriptStore> Harness for ClaudeCodeHarness<S> {
    type Session = ClaudeSession;
    type Error = ClaudeError;

    async fn start(self, request: StartRequest) -> Result<Started<Self::Session>, ClaudeError> {
        // Before the CLI exists, so there is no window in which a harness is
        // running against a policy file this daemon has not written yet.
        if let Some(dir) = &self.claude.managed_dir {
            self.mount.write_claude_managed(dir).await?;
        } else {
            tracing::warn!(
                "no `claude.managed_dir` in the config: this session's MCP servers are mounted \
                 through the Agent SDK, but nothing on this machine stops the agent adding more"
            );
        }
        sidecar::prepare(&self.sidecar).await?;
        let mut child = sidecar::spawn(&self.sidecar)?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or(ClaudeError::MissingStdio { stream: "stdin" })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ClaudeError::MissingStdio { stream: "stdout" })?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ClaudeError::MissingStdio { stream: "stderr" })?;

        let (commands, inbox) = mpsc::channel(CHANNEL_DEPTH);

        // Bun's own diagnostics and anything the SDK writes to stderr are
        // logged, never parsed — and kept, because when the sidecar dies
        // they are the only account of why.
        tokio::spawn(log_stderr(stderr, commands.clone()));

        let mut lines = BufReader::new(stdout).lines();
        handshake(&mut lines).await?;

        let isolation = self.claude.auth.isolation();
        write_command(
            &mut stdin,
            &SidecarCommand::Start {
                cwd: request.workdir,
                auth: self.claude.auth.sidecar_auth(),
                config_dir: isolation.map(|isolation| isolation.config_dir.clone()),
                project_dir_name: isolation.map(|isolation| isolation.project_dir_name.clone()),
                model: self.claude.model.clone(),
                permission_mode: self.claude.permission_mode,
                resume_session_id: request.resume_session_id,
                mcp_servers: self.mount.claude_sdk_servers(),
            },
        )
        .await?;

        let (outputs, output_rx) = mpsc::channel(CHANNEL_DEPTH);

        tokio::spawn(read_sidecar(lines, commands.clone()));
        tokio::spawn(
            Driver {
                stdin,
                child,
                store: self.store,
                normalizer: Normalizer::new(),
                outputs,
                capabilities: None,
                stopped: false,
                deliberate: false,
                announced: false,
                exit: None,
                stderr_tail: VecDeque::new(),
            }
            .run(inbox),
        );

        Ok(Started {
            session: ClaudeSession { commands },
            outputs: output_rx,
        })
    }
}

/// The control handle of a running Claude Code session.
#[derive(Debug, Clone)]
pub struct ClaudeSession {
    commands: mpsc::Sender<DriverCommand>,
}

impl ClaudeSession {
    /// Sends one command to the driver task and waits for its result.
    async fn ask(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), ClaudeError>>) -> DriverCommand,
    ) -> Result<(), ClaudeError> {
        let (ack, answer) = oneshot::channel();
        self.commands
            .send(make(ack))
            .await
            .map_err(|_| ClaudeError::Stopped)?;
        answer.await.map_err(|_| ClaudeError::Stopped)?
    }
}

impl HarnessSession for ClaudeSession {
    type Error = ClaudeError;

    async fn send_user_message(&self, text: String) -> Result<(), ClaudeError> {
        self.ask(|ack| DriverCommand::UserMessage { text, ack })
            .await
    }

    async fn interrupt(&self) -> Result<(), ClaudeError> {
        self.ask(|ack| DriverCommand::Interrupt { ack }).await
    }

    async fn flush(&self) -> Result<(), ClaudeError> {
        self.ask(|ack| DriverCommand::Flush { ack }).await
    }

    async fn compact(&self) -> Result<(), ClaudeError> {
        self.ask(|ack| DriverCommand::Compact { ack }).await
    }

    async fn decide_approval(&self, approval: ToolApproval) -> Result<(), ClaudeError> {
        self.ask(|ack| DriverCommand::Approval { approval, ack })
            .await
    }

    async fn shutdown(self) -> Result<(), ClaudeError> {
        self.ask(|ack| DriverCommand::Shutdown { ack }).await
    }
}

/// Everything the driver task can be asked to do, from either side.
#[derive(Debug)]
enum DriverCommand {
    /// From the handle: push a user message and open a turn.
    UserMessage {
        text: String,
        ack: oneshot::Sender<Result<(), ClaudeError>>,
    },
    /// From the handle: end the current turn.
    Interrupt {
        ack: oneshot::Sender<Result<(), ClaudeError>>,
    },
    /// From the handle: answer once every store request received so far has
    /// been written through.
    ///
    /// The driver does nothing for this beyond acknowledging it, and that
    /// is the point: it is a marker in the same queue the sidecar's store
    /// requests arrive on, so an acknowledgement means every batch queued
    /// ahead of it has already been `append`ed and awaited.
    Flush {
        ack: oneshot::Sender<Result<(), ClaudeError>>,
    },
    /// From the handle: compact the conversation context.
    Compact {
        ack: oneshot::Sender<Result<(), ClaudeError>>,
    },
    /// From the handle: answer a pending approval.
    Approval {
        approval: ToolApproval,
        ack: oneshot::Sender<Result<(), ClaudeError>>,
    },
    /// From the handle: stop the session.
    Shutdown {
        ack: oneshot::Sender<Result<(), ClaudeError>>,
    },
    /// From the reader: one event off the sidecar's stdout.
    Sidecar(Box<SidecarEvent>),
    /// From the stderr reader: one line the sidecar wrote to stderr.
    SidecarStderr { line: String },
    /// From the reader: the sidecar's stdout ended.
    SidecarClosed,
    /// From the reader: a line that is not a [`SidecarEvent`].
    SidecarProtocolError { detail: String },
}

/// Reads the sidecar's first line, which must be [`SidecarEvent::Ready`].
async fn handshake<R>(lines: &mut tokio::io::Lines<R>) -> Result<(), ClaudeError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let line = lines
        .next_line()
        .await
        .map_err(|source| ClaudeError::Io {
            stream: "stdout",
            source,
        })?
        .ok_or(ClaudeError::ExitedDuringHandshake)?;
    match parse_event(&line)? {
        SidecarEvent::Ready { sdk_version } => {
            tracing::info!(sdk_version, "the Claude Agent SDK sidecar is ready");
            Ok(())
        }
        SidecarEvent::Fatal { error } => Err(ClaudeError::SidecarFatal { error }),
        other => Err(ClaudeError::UnexpectedHandshake { tag: other.tag() }),
    }
}

fn parse_event(line: &str) -> Result<SidecarEvent, ClaudeError> {
    serde_json::from_str(line).map_err(|source| ClaudeError::Protocol {
        detail: format!("{source} in {line:?}"),
    })
}

async fn write_command(
    stdin: &mut ChildStdin,
    command: &SidecarCommand,
) -> Result<(), ClaudeError> {
    let mut line = serde_json::to_string(command).expect("every SidecarCommand serializes to JSON");
    tracing::trace!(command = command.tag(), "writing to the sidecar");
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|source| ClaudeError::Io {
            stream: "stdin",
            source,
        })?;
    stdin.flush().await.map_err(|source| ClaudeError::Io {
        stream: "stdin",
        source,
    })
}

/// How many of the sidecar's last stderr lines are kept to explain a death.
///
/// Enough for a stack trace or a credential refusal, short enough that the
/// sentence a session shows stays a sentence.
const STDERR_TAIL: usize = 20;

/// How long the driver waits for stderr already in flight when it stops.
///
/// Short: the child is reaped by then, so this is a handful of lines
/// crossing a channel, not a read that could block.
const STDERR_SETTLE: Duration = Duration::from_millis(250);

/// Logs the sidecar's stderr, and hands each line to the driver.
///
/// Both, not either: the journal is what an operator on the machine reads,
/// and the driver's copy is what a person looking at the session reads when
/// the process dies — which is the case where the journal is unreachable,
/// because `flycod` is stopping too.
async fn log_stderr(stderr: tokio::process::ChildStderr, commands: mpsc::Sender<DriverCommand>) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                tracing::debug!(target: "flycod::sidecar", "{line}");
                if commands
                    .send(DriverCommand::SidecarStderr { line })
                    .await
                    .is_err()
                {
                    // The driver is gone, so nobody is left to tell.
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "lost the sidecar's stderr");
                break;
            }
        }
    }
}

async fn read_sidecar<R>(mut lines: tokio::io::Lines<R>, commands: mpsc::Sender<DriverCommand>)
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let command = match lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<SidecarEvent>(&line) {
                Ok(event) => DriverCommand::Sidecar(Box::new(event)),
                Err(source) => DriverCommand::SidecarProtocolError {
                    detail: format!("{source} in {line:?}"),
                },
            },
            Ok(None) => DriverCommand::SidecarClosed,
            Err(error) => DriverCommand::SidecarProtocolError {
                detail: format!("could not read the sidecar's stdout: {error}"),
            },
        };
        let terminal = !matches!(command, DriverCommand::Sidecar(_));
        if commands.send(command).await.is_err() || terminal {
            break;
        }
    }
}

/// Sends one output; returns whether anyone is still listening.
///
/// A free function taking the sender rather than a `&self` method, so the
/// driver's future stays `Send` without demanding `Sync` of every
/// [`TranscriptStore`].
async fn emit(outputs: &mpsc::Sender<SessionOutput>, output: SessionOutput) -> bool {
    if outputs.send(output).await.is_err() {
        tracing::debug!("nothing is consuming the session's output stream");
        return false;
    }
    true
}

/// The task that owns the sidecar process and everything mutable about the
/// session.
struct Driver<S> {
    stdin: ChildStdin,
    child: Child,
    store: S,
    normalizer: Normalizer,
    outputs: mpsc::Sender<SessionOutput>,
    /// The most recent capability set the harness advertised, or `None`
    /// while it has not reported one yet.
    capabilities: Option<Vec<String>>,
    /// Whether [`Driver::stop`] has already reaped the child. The shutdown
    /// command reaps it to report the outcome, and the loop's exit path
    /// reaps whatever is left, so the two must not both wait.
    stopped: bool,
    /// What the sidecar exited with, once it has been reaped.
    ///
    /// Kept because the status is half of what a person needs to read: a
    /// process killed by the OOM killer and one that refused its
    /// credentials both close their stdout, and only the status and the
    /// stderr together tell them apart.
    exit: Option<std::process::ExitStatus>,
    /// Whether this driver was asked to stop, rather than stopping because
    /// the sidecar did. Only a deliberate stop is silent.
    deliberate: bool,
    /// Whether a [`SessionOutput::Fatal`] has already been emitted, so the
    /// loop's exit path does not write a second, vaguer one over it.
    announced: bool,
    /// The last [`STDERR_TAIL`] lines the sidecar wrote to stderr.
    ///
    /// A ring rather than the whole stream: a session that runs for hours
    /// writes a great deal of it, and what explains a death is always at
    /// the end.
    stderr_tail: VecDeque<String>,
}

impl<S: TranscriptStore> Driver<S> {
    async fn run(mut self, mut inbox: mpsc::Receiver<DriverCommand>) {
        while let Some(command) = inbox.recv().await {
            if !self.handle(command).await {
                break;
            }
        }
        // Whatever ended the loop, the child must be reaped: it may still
        // be running (handle returned false on a fatal) or already gone.
        if let Err(error) = self.stop().await {
            tracing::error!(%error, "the sidecar did not stop cleanly");
        }

        // Every way of stopping that nobody asked for ends here with a
        // sentence. Putting it after the loop rather than in the arms is
        // the point: the driver stops on a closed stdout, on a protocol
        // error, and on any command whose write fails — and a session whose
        // agent died in the third way is no less dead than the other two
        // (issue #193).
        if !self.deliberate && !self.announced {
            self.collect_late_stderr(&mut inbox).await;
            self.fatal(self.death_notice()).await;
        }
        tracing::debug!("the Claude Code driver task finished");
    }

    /// Takes the stderr lines that were still in flight when the driver
    /// stopped.
    ///
    /// stdout and stderr are read by two tasks, so a sidecar that writes its
    /// last words and exits can have its stdout EOF handled first, leaving
    /// exactly the lines that explain the death sitting in the channel. The
    /// child has been reaped by the time this runs, so its pipe is closed
    /// and whatever remains is already on its way rather than merely
    /// possible.
    async fn collect_late_stderr(&mut self, inbox: &mut mpsc::Receiver<DriverCommand>) {
        while let Ok(Some(command)) = tokio::time::timeout(STDERR_SETTLE, inbox.recv()).await {
            // Anything else queued behind them is skipped rather than
            // stopping the drain: the stdout EOF is usually the very thing
            // sitting in front of the lines that explain it, and a command
            // from the handle is one the dying driver could not have served
            // anyway.
            if let DriverCommand::SidecarStderr { line } = command {
                self.remember_stderr(line);
            }
        }
    }

    /// Says why the session is over, once.
    ///
    /// Every fatal goes through here so that `announced` cannot be
    /// forgotten at a call site: the loop's exit path writes a notice only
    /// when nothing else has, and a second, vaguer sentence written over a
    /// precise one would be worse than none.
    async fn fatal(&mut self, error: String) {
        emit(&self.outputs, SessionOutput::Fatal { error }).await;
        self.announced = true;
    }

    /// Keeps one stderr line, oldest dropped first.
    fn remember_stderr(&mut self, line: String) {
        if self.stderr_tail.len() == STDERR_TAIL {
            self.stderr_tail.pop_front();
        }
        self.stderr_tail.push_back(line);
    }

    /// Handles one command; returns whether the driver should keep running.
    async fn handle(&mut self, command: DriverCommand) -> bool {
        match command {
            DriverCommand::UserMessage { text, ack } => {
                let turn = uuid::Uuid::new_v4().to_string();
                let started = self.normalizer.begin_turn(turn);
                if !emit(&self.outputs, SessionOutput::Event { event: started }).await {
                    return false;
                }
                let result =
                    write_command(&mut self.stdin, &SidecarCommand::UserMessage { text }).await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Interrupt { ack } => {
                let result = write_command(&mut self.stdin, &SidecarCommand::Interrupt).await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Compact { ack } => {
                let result = write_command(&mut self.stdin, &SidecarCommand::Compact).await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Flush { ack } => {
                // Nothing to do: reaching this arm *is* the answer. Every
                // store request the sidecar sent before it was handled by
                // this loop, in order, and each one awaited its write to
                // the control plane before the next command was read.
                let _ = ack.send(Ok(()));
                true
            }
            DriverCommand::Approval { approval, ack } => {
                let command = match approval {
                    ToolApproval::Allow { id, updated_input } => SidecarCommand::ApprovalDecision {
                        id,
                        allow: true,
                        updated_input,
                        message: None,
                    },
                    ToolApproval::Deny { id, message } => SidecarCommand::ApprovalDecision {
                        id,
                        allow: false,
                        updated_input: None,
                        message: Some(message),
                    },
                };
                let result = write_command(&mut self.stdin, &command).await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Shutdown { ack } => {
                let written = write_command(&mut self.stdin, &SidecarCommand::Shutdown).await;
                let stopped = self.stop().await;
                let _ = ack.send(written.and(stopped));
                self.deliberate = true;
                false
            }
            DriverCommand::Sidecar(event) => self.on_sidecar(*event).await,
            DriverCommand::SidecarStderr { line } => {
                self.remember_stderr(line);
                true
            }
            DriverCommand::SidecarClosed => {
                tracing::info!("the sidecar closed its output stream");
                false
            }
            DriverCommand::SidecarProtocolError { detail } => {
                self.fatal(ClaudeError::Protocol { detail }.to_string())
                    .await;
                false
            }
        }
    }

    async fn on_sidecar(&mut self, event: SidecarEvent) -> bool {
        match event {
            SidecarEvent::Ready { sdk_version } => {
                self.fatal(format!(
                    "the sidecar announced itself ready twice (version {sdk_version})"
                ))
                .await;
                false
            }
            SidecarEvent::Started { session_id } => {
                tracing::info!(session_id, "the Claude Code session is identified");
                emit(&self.outputs, SessionOutput::Started { session_id }).await
            }
            SidecarEvent::Capabilities { capabilities } => {
                // "Newest frame wins": a later `system/init` may revise the
                // set, so the driver keeps the latest, not the first.
                if self.capabilities.as_ref() == Some(&capabilities) {
                    tracing::debug!("the harness re-advertised the same capabilities");
                } else {
                    tracing::info!(
                        ?capabilities,
                        previous = ?self.capabilities,
                        "the harness advertised its capabilities"
                    );
                    self.capabilities = Some(capabilities.clone());
                }
                emit(&self.outputs, SessionOutput::Capabilities { capabilities }).await
            }
            SidecarEvent::McpServers { servers } => {
                // The session's whole point is an agent that can see what
                // it is spending; one that cannot is stopped here rather
                // than left to find out by trying.
                if let Err(error) = crate::mount::verify(&servers) {
                    self.fatal(ClaudeError::Mount(error.into()).to_string())
                        .await;
                    return false;
                }
                true
            }
            SidecarEvent::SdkMessage { message } => {
                for event in self.normalizer.normalize(&message) {
                    if !emit(&self.outputs, SessionOutput::Event { event }).await {
                        return false;
                    }
                }
                true
            }
            SidecarEvent::ApprovalRequest {
                id,
                tool,
                input,
                suggestions,
            } => {
                tracing::info!(%id, tool, "the harness is waiting on a flyco approval");
                emit(
                    &self.outputs,
                    SessionOutput::ApprovalRequest {
                        id,
                        tool,
                        input,
                        suggestions,
                    },
                )
                .await
            }
            SidecarEvent::StoreRequest { id, op } => self.on_store_request(id, op).await,
            SidecarEvent::Fatal { error } => {
                self.fatal(error).await;
                false
            }
        }
    }

    async fn on_store_request(&mut self, id: StoreRequestId, op: StoreOp) -> bool {
        let result = match op {
            StoreOp::Append { key, entries } => self
                .store
                .append(&key, entries)
                .await
                .map(|()| serde_json::Value::Null),
            // The SDK's `load` contract distinguishes "never written"
            // (null) from "written and empty" (an empty array). flyco's
            // store is append-only, so those are the same state and both
            // answer null — which the SDK documents as the correct
            // response for adapters that cannot tell them apart.
            StoreOp::Load { key } => self.store.load(&key).await.map(|entries| {
                if entries.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Array(entries)
                }
            }),
        };
        // A transcript store that cannot write is not something to work
        // around: History is the feature it implements, and continuing
        // would silently produce an unresumable session.
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.fatal(ClaudeError::Store(error).to_string()).await;
                return false;
            }
        };
        write_command(
            &mut self.stdin,
            &SidecarCommand::StoreResponse { id, result },
        )
        .await
        .is_ok()
    }

    /// Waits for the child to exit, killing it if it overstays. Idempotent.
    /// What this session says when the agent process is gone.
    ///
    /// The exit status and the last thing the process said, because a
    /// person reading it has neither: `flycod` is stopping, so the journal
    /// on the machine is about to be as unreachable as the machine.
    fn death_notice(&self) -> String {
        let status = self.exit.map_or_else(
            || "the agent process ended without a status".to_owned(),
            |status| format!("the agent process exited ({status})"),
        );
        if self.stderr_tail.is_empty() {
            return format!("{status} and said nothing about why");
        }
        let said = self
            .stderr_tail
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        format!("{status}. Its last output was:\n{said}")
    }

    async fn stop(&mut self) -> Result<(), ClaudeError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        match tokio::time::timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => {
                tracing::info!(%status, "the sidecar exited");
                self.exit = Some(status);
                Ok(())
            }
            Ok(Err(source)) => Err(ClaudeError::Io {
                stream: "process",
                source,
            }),
            Err(_elapsed) => {
                tracing::error!("killing a sidecar that ignored its shutdown command");
                self.child.kill().await.map_err(|source| ClaudeError::Io {
                    stream: "process",
                    source,
                })?;
                Err(ClaudeError::ShutdownTimedOut)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeError, SidecarEvent, handshake, parse_event};
    use tokio::io::BufReader;

    fn lines(text: &'static str) -> tokio::io::Lines<BufReader<&'static [u8]>> {
        use tokio::io::AsyncBufReadExt as _;
        BufReader::new(text.as_bytes()).lines()
    }

    #[tokio::test]
    async fn a_ready_line_completes_the_handshake() {
        let mut lines = lines("{\"type\":\"ready\",\"sdk_version\":\"0.3.250\"}\n");
        handshake(&mut lines).await.expect("handshake");
    }

    #[tokio::test]
    async fn a_fatal_first_line_surfaces_the_sidecars_reason() {
        let mut lines = lines("{\"type\":\"fatal\",\"error\":\"cannot find module\"}\n");
        let error = handshake(&mut lines).await.expect_err("must fail");
        assert!(matches!(error, ClaudeError::SidecarFatal { .. }));
        assert!(error.to_string().contains("cannot find module"));
    }

    #[tokio::test]
    async fn any_other_first_line_is_a_protocol_break() {
        let mut lines = lines("{\"type\":\"sdk_message\",\"message\":{}}\n");
        assert!(matches!(
            handshake(&mut lines).await,
            Err(ClaudeError::UnexpectedHandshake { tag: "sdk_message" })
        ));
    }

    #[tokio::test]
    async fn a_sidecar_that_dies_before_ready_is_named_as_such() {
        let mut lines = lines("");
        assert!(matches!(
            handshake(&mut lines).await,
            Err(ClaudeError::ExitedDuringHandshake)
        ));
    }

    #[test]
    fn a_non_json_line_is_reported_with_the_line_that_broke() {
        let error = parse_event("not json").expect_err("must fail");
        assert!(error.to_string().contains("not json"));
    }

    #[test]
    fn events_round_trip_through_the_line_format() {
        let event = SidecarEvent::Capabilities {
            capabilities: vec!["interrupt_receipt_v1".to_owned()],
        };
        let line = serde_json::to_string(&event).expect("serialize");
        assert_eq!(parse_event(&line).expect("parse"), event);
    }
}
