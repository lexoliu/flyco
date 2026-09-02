//! The daemon's end of the relay, driven against a real loopback room.

use core::time::Duration;

use flyco_core::wire::ApprovalPayload;
use flyco_core::{
    ApprovalDecision, ApprovalId, BudgetSignal, ControlToDaemon, DaemonToControl, HarnessEvent,
    HarnessObservation, MachineOrigin, ProvisioningStage, SessionId, ShellOutcome, ShellRunId,
    ShellStream, UsageReport, Usd, WIRE_PROTOCOL_VERSION,
};
use tokio::sync::mpsc;

use crate::control::rest::{
    ApprovalRaiser, ControlApi, ControlApiError, HttpControlApi, TranscriptRead,
};
use crate::control::store::{RemoteTranscriptStore, stream_key};
use crate::control::wire::{self, Endpoint, QUEUE_DEPTH, SessionRelay, WireError};
use crate::git::FakeWorkdir;
use crate::harness::SessionOutput;
use crate::harness::claude::protocol::SessionKey;
use crate::harness::claude::store::{StoreError, TranscriptStore};
use crate::shell::{FakeShell, ShellEvent, ShellUpdate, StartedRun};
use crate::spot::{FakeEviction, SpotNotice};
use crate::terminal::FakeTerminal;
use crate::testing::{
    Call, ControlPlane, Directive, FakeDisk, FakeSession, Greeting, Reply, Room, Seen,
};
use crate::workdir::Checkout;
use flyco_core::workdir::{WorkdirRefusal, WorkdirReply, WorkdirRequest};

/// A daemon token shaped the way the control plane mints them.
const TOKEN: &str = "fd_a-daemon-token";

/// One turn announcement the daemon filed over REST.
///
/// The three are told apart rather than reduced to "it ended well": the
/// control plane records a different session activity for each, and a test
/// that could not distinguish a start from an end would not be testing the
/// thing the home list reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnNotice {
    /// `POST /v1/sessions/{id}/turn-started`.
    Started,
    /// `POST /v1/sessions/{id}/turn-completed`.
    Completed,
    /// `POST /v1/sessions/{id}/turn-failed`.
    Failed,
}

/// A [`ControlApi`] that answers without a network.
///
/// The relay tests care about *ordering* — that an approval is durable
/// before it is announced — not about HTTP, which
/// [`the REST tests`](rest_client) cover against a real server.
#[derive(Debug, Clone)]
struct RecordingApi {
    approvals: mpsc::UnboundedSender<ApprovalPayload>,
    observations: mpsc::UnboundedSender<HarnessObservation>,
    notifications: mpsc::UnboundedSender<TurnNotice>,
    /// The one channel every double in a test records into, so an ordering
    /// across the harness, the control plane and the disk is assertable.
    calls: mpsc::UnboundedSender<Call>,
    id: ApprovalId,
}

impl ApprovalRaiser for RecordingApi {
    fn raise_approval(
        &self,
        payload: ApprovalPayload,
    ) -> impl core::future::Future<Output = Result<ApprovalId, ControlApiError>> + Send {
        let recorded = self
            .approvals
            .send(payload)
            .map_err(|error| ControlApiError::Transport(error.to_string()));
        core::future::ready(recorded.map(|()| self.id))
    }
}

impl ControlApi for RecordingApi {
    fn put_transcript_batch(
        &self,
        _stream: &str,
        _seq: u64,
        _body: Vec<u8>,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(Ok(()))
    }

    fn get_transcript(
        &self,
        _stream: &str,
    ) -> impl core::future::Future<Output = Result<TranscriptRead, ControlApiError>> + Send {
        core::future::ready(Ok(TranscriptRead {
            body: Vec::new(),
            batches: 0,
        }))
    }

    fn record_observation(
        &self,
        observation: HarnessObservation,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(
            self.observations
                .send(observation)
                .map_err(|error| ControlApiError::Transport(error.to_string())),
        )
    }

    fn notify_turn_started(
        &self,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(
            self.notifications
                .send(TurnNotice::Started)
                .map_err(|error| ControlApiError::Transport(error.to_string())),
        )
    }

    fn notify_turn_completed(
        &self,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(
            self.notifications
                .send(TurnNotice::Completed)
                .map_err(|error| ControlApiError::Transport(error.to_string())),
        )
    }

    fn notify_turn_failed(
        &self,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(
            self.notifications
                .send(TurnNotice::Failed)
                .map_err(|error| ControlApiError::Transport(error.to_string())),
        )
    }

    fn record_harness_session(
        &self,
        harness_session_id: &str,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(
            self.calls
                .send(Call::HarnessSessionRecorded(harness_session_id.to_owned()))
                .map_err(|error| ControlApiError::Transport(error.to_string())),
        )
    }

    fn report_spot_notice(
        &self,
        seconds_remaining: u32,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(
            self.calls
                .send(Call::SpotNoticeReported(seconds_remaining))
                .map_err(|error| ControlApiError::Transport(error.to_string())),
        )
    }

    fn harness_session_id(
        &self,
    ) -> impl core::future::Future<Output = Result<Option<String>, ControlApiError>> + Send {
        // The relay never asks: the id is resolved once, before the harness
        // is started, by `flycod run` itself.
        core::future::ready(Ok(None))
    }

    fn put_workdir_patch(
        &self,
        _patch: Vec<u8>,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(Ok(()))
    }

    fn get_workdir_patch(
        &self,
    ) -> impl core::future::Future<Output = Result<Option<Vec<u8>>, ControlApiError>> + Send {
        core::future::ready(Ok(None))
    }

    fn report_stage(
        &self,
        _stage: ProvisioningStage,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(Ok(()))
    }
}

/// Everything a relay test drives.
struct Harness {
    room: Room,
    session: SessionId,
    outputs: mpsc::Sender<SessionOutput>,
    calls: mpsc::UnboundedReceiver<Call>,
    approvals: mpsc::UnboundedReceiver<ApprovalPayload>,
    observations: mpsc::UnboundedReceiver<HarnessObservation>,
    notifications: mpsc::UnboundedReceiver<TurnNotice>,
    approval_id: ApprovalId,
    terminal_writes: mpsc::UnboundedReceiver<String>,
    terminal_inject: mpsc::Sender<String>,
    /// The `!` commands the daemon asked its shell to run.
    shell_runs: mpsc::UnboundedReceiver<StartedRun>,
    repo_inject: mpsc::UnboundedSender<String>,
    /// Makes the fake metadata endpoint announce a reclamation. Taken once:
    /// a provider announces one machine's reclamation exactly once.
    evict: Option<tokio::sync::oneshot::Sender<SpotNotice>>,
    run: tokio::task::JoinHandle<Result<(), WireError>>,
    /// Whether the agent-ready stage is still to come.
    ///
    /// It is announced once a room has *welcomed* the daemon, and never
    /// again after that, so [`Harness::handshake`] expects it on the first
    /// greeting a welcoming room answers and never on a re-greeting or on
    /// a room that refuses the handshake outright.
    expect_ready: bool,
    /// The scratch directory the relay's checkout reads, removed with the
    /// harness.
    #[expect(
        dead_code,
        reason = "held for its Drop: the directory outlives every frame the test sends"
    )]
    checkout_dir: ScratchDir,
}

/// A directory that removes itself.
///
/// A field rather than a `Drop` on the harness: a test hands
/// [`Harness::run`]'s join handle out by value, which a type that
/// implements `Drop` cannot have moved out of it.
struct ScratchDir(std::path::PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A directory with one file in it, for the relay's read-only checkout.
///
/// Not a git repository: what the relay is responsible for is carrying a
/// question to the checkout and its answer back, and a listing is the
/// cheapest question that proves it. What the answers themselves are made
/// of is [`crate::workdir`]'s own business, and is tested there against a
/// real clone.
fn scratch_checkout() -> ScratchDir {
    let path = std::env::temp_dir().join(format!("flyco-relay-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).expect("a scratch checkout");
    std::fs::write(path.join("NOTES.md"), "the agent was here\n").expect("write");
    ScratchDir(path)
}

impl Harness {
    async fn start(greeting: Greeting) -> Self {
        Self::with_capacity(greeting, 32).await
    }

    async fn with_capacity(greeting: Greeting, outputs: usize) -> Self {
        let room = Room::start(greeting).await;
        let session = SessionId::generate();
        let endpoint = Endpoint::from_base(&room.base, session, TOKEN.to_owned())
            .expect("a loopback relay endpoint");

        let (fake, calls) = FakeSession::new();
        let recorder = fake.recorder();
        let (sender, receiver) = mpsc::channel(outputs);
        let (approval_sender, approvals) = mpsc::unbounded_channel();
        let (observation_sender, observations) = mpsc::unbounded_channel();
        let (notification_sender, notifications) = mpsc::unbounded_channel();
        let approval_id = ApprovalId::generate();
        let api = RecordingApi {
            approvals: approval_sender,
            observations: observation_sender,
            notifications: notification_sender,
            calls: recorder.clone(),
            id: approval_id,
        };
        let (watcher, evict) = FakeEviction::pair();
        let (notices, spot) = mpsc::channel(1);
        crate::spot::spawn(watcher, notices);

        let (terminal, terminal_writes, terminal_inject, terminal_out) = FakeTerminal::pair();
        let (shell, shell_runs) = FakeShell::pair();
        let (workdir, repo_inject, repo_status) = FakeWorkdir::pair();
        let checkout_dir = scratch_checkout();
        let run = tokio::spawn(wire::run(SessionRelay {
            endpoint,
            session: fake,
            outputs: receiver,
            api,
            terminal,
            terminal_out,
            shell,
            workdir,
            checkout: Checkout::new(checkout_dir.0.clone(), None),
            repo_status,
            disk: FakeDisk::new(recorder),
            spot,
            machine: crate::testing::session_machine(),
            machine_origin: MachineOrigin::Auto,
        }));

        Self {
            room,
            session,
            outputs: sender,
            calls,
            approvals,
            observations,
            notifications,
            approval_id,
            terminal_writes,
            terminal_inject,
            shell_runs,
            repo_inject,
            evict: Some(evict),
            run,
            expect_ready: greeting == Greeting::Welcome,
            checkout_dir,
        }
    }

    /// Makes the provider announce this machine's reclamation.
    ///
    /// The notice travels the whole way a real one does — through the
    /// watcher's own task and the channel the relay selects on — so what
    /// the test drives is the daemon's reaction rather than a function call
    /// into the middle of it.
    fn evict(&mut self, seconds_remaining: u32) {
        self.evict
            .take()
            .expect("a machine is reclaimed once")
            .send(SpotNotice { seconds_remaining })
            .expect("the watcher is live");
    }

    /// Waits for the room to see this daemon's `Hello`.
    ///
    /// On the first connection the greeting is followed by the last stage
    /// of the provisioning timeline (docs/ux.md §9.2): the harness is up
    /// and the room has welcomed the socket, which is the whole meaning of
    /// "the agent is ready". A reconnect does not repeat it.
    async fn handshake(&mut self) -> Option<String> {
        let Some(Seen::Connected(authorization)) = self.room.next().await else {
            panic!("the daemon did not connect");
        };
        assert_eq!(
            self.room.next_frame().await,
            DaemonToControl::Hello {
                protocol_version: WIRE_PROTOCOL_VERSION,
                session: self.session,
            }
        );
        if self.expect_ready {
            self.expect_ready = false;
            let DaemonToControl::ProvisioningStage {
                stage: ProvisioningStage::Ready,
                ..
            } = self.room.next_frame().await
            else {
                panic!("the first connection did not announce that the agent is ready");
            };
        }
        authorization
    }

    async fn emit(&self, output: SessionOutput) {
        self.outputs.send(output).await.expect("the daemon is live");
    }

    fn command(&self, command: ControlToDaemon) {
        self.room
            .directives
            .send(Directive::Send(command))
            .expect("the room is live");
    }

    /// The next observation the daemon filed.
    async fn next_observation(&mut self) -> HarnessObservation {
        tokio::time::timeout(Duration::from_secs(5), self.observations.recv())
            .await
            .expect("the daemon filed an observation")
            .expect("the collector is live")
    }

    async fn next_notification(&mut self) -> TurnNotice {
        tokio::time::timeout(Duration::from_secs(5), self.notifications.recv())
            .await
            .expect("the daemon filed a notification")
            .expect("the collector is live")
    }

    /// Makes every further observation fail, the way a session with no
    /// linked harness account meets a `404` on each one.
    fn refuse_observations(&mut self) {
        self.observations.close();
    }

    /// The next `!` command the daemon asked its shell to run.
    async fn next_shell_run(&mut self) -> StartedRun {
        tokio::time::timeout(Duration::from_secs(5), self.shell_runs.recv())
            .await
            .expect("the daemon started the command")
            .expect("the shell is live")
    }

    /// The next thing the harness was told to do.
    async fn next_call(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(5), self.calls.recv())
            .await
            .expect("the daemon acted on the command")
            .expect("the harness is live")
    }

    /// Stops the run by archiving, and returns its result.
    async fn archive(mut self) -> Result<(), WireError> {
        self.command(ControlToDaemon::Archive {
            preserve_workdir: false,
        });
        // Every double records into one channel, so anything the test left
        // undrained is still in front of the shutdown.
        while self.next_call().await != Call::Shutdown {}
        tokio::time::timeout(Duration::from_secs(5), self.run)
            .await
            .expect("the run ended")
            .expect("the run did not panic")
    }
}

fn delta(text: &str) -> HarnessEvent {
    HarnessEvent::AssistantDelta {
        turn_id: "turn-1".to_owned(),
        text: text.to_owned(),
    }
}

// ── The handshake ──

#[tokio::test]
async fn a_daemon_greets_with_its_token_and_waits_to_be_welcomed() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    let authorization = harness.handshake().await;

    assert_eq!(
        authorization.as_deref(),
        Some("Bearer fd_a-daemon-token"),
        "the relay upgrade carries the session's daemon token"
    );
    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn nothing_is_pumped_before_the_welcome() {
    // A room that refuses the handshake gets the `Hello` and nothing else,
    // however much the harness produces: a frame sent into a socket that is
    // about to close is a frame the session lost.
    let mut harness = Harness::start(Greeting::Refuse).await;
    harness.handshake().await;
    harness
        .emit(SessionOutput::Event { event: delta("hi") })
        .await;

    assert_eq!(
        harness.room.next().await,
        Some(Seen::Disconnected),
        "a refused daemon is disconnected, not pumped"
    );

    // And it comes back rather than giving up — the refusal may have been a
    // control plane mid-deploy.
    assert!(matches!(
        harness.room.next().await,
        Some(Seen::Connected(_))
    ));
    harness.run.abort();
}

// ── The event pump ──

#[tokio::test]
async fn session_output_reaches_the_room_as_wire_frames() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .emit(SessionOutput::Started {
            session_id: "9d0f4b1a".to_owned(),
        })
        .await;
    harness
        .emit(SessionOutput::Capabilities {
            capabilities: vec!["can_use_tool".to_owned()],
        })
        .await;
    harness
        .emit(SessionOutput::Event { event: delta("hi") })
        .await;

    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Started {
            harness_session_id: "9d0f4b1a".to_owned(),
        }
    );
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Capabilities {
            capabilities: vec!["can_use_tool".to_owned()],
        }
    );
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness { event: delta("hi") }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn an_approval_is_recorded_over_rest_before_it_is_announced() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .emit(SessionOutput::ApprovalRequest {
            id: ApprovalId::generate(),
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "ls" }),
            suggestions: None,
        })
        .await;

    let payload = ApprovalPayload::ToolUse {
        tool: "Bash".to_owned(),
        input: serde_json::json!({ "command": "ls" }),
    };
    assert_eq!(
        harness.approvals.recv().await,
        Some(payload.clone()),
        "the durable row is written first"
    );
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::ApprovalRequest {
            id: harness.approval_id,
            payload,
        },
        "and the frame announces the id the control plane assigned, not the harness's"
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn an_approval_decision_reaches_the_harness() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    let native = ApprovalId::generate();
    harness
        .emit(SessionOutput::ApprovalRequest {
            id: native,
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "ls" }),
            suggestions: None,
        })
        .await;
    let _ = harness.approvals.recv().await;
    let _ = harness.room.next_frame().await;

    harness.command(ControlToDaemon::ApprovalDecision {
        id: harness.approval_id,
        decision: ApprovalDecision::Approved,
    });
    assert_eq!(
        harness.next_call().await,
        Call::Approval {
            id: native,
            allowed: true
        },
        "the harness is told its own id, not the REST-assigned one"
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn terminal_input_reaches_the_shell_and_output_reaches_the_room() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::TerminalInput {
        data: "ls\n".to_owned(),
    });
    assert_eq!(
        harness.terminal_writes.recv().await.as_deref(),
        Some("ls\n")
    );

    harness
        .terminal_inject
        .send("file.txt\n".to_owned())
        .await
        .expect("inject shell output");
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::TerminalOutput {
            data: "file.txt\n".to_owned()
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

// ── The composer's `!` commands ──

#[tokio::test]
async fn a_shell_command_runs_on_the_machine_and_its_output_reaches_the_room() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    let run = ShellRunId::generate();
    harness.command(ControlToDaemon::RunShell {
        run,
        command: "cargo test".to_owned(),
    });
    let started = harness.next_shell_run().await;
    assert_eq!(
        started.run, run,
        "the run keeps the identity the room gave it"
    );
    assert_eq!(started.command, "cargo test");

    started
        .updates
        .send(ShellUpdate {
            run,
            event: ShellEvent::Output {
                stream: ShellStream::Stderr,
                data: "   Compiling flyco-core\n".to_owned(),
            },
        })
        .expect("the relay is listening");
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::ShellOutput {
            run,
            stream: ShellStream::Stderr,
            data: "   Compiling flyco-core\n".to_owned(),
        }
    );

    started
        .updates
        .send(ShellUpdate {
            run,
            event: ShellEvent::Exited {
                outcome: ShellOutcome::Exited { code: 0 },
                truncated: false,
            },
        })
        .expect("the relay is listening");
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::ShellExited {
            run,
            outcome: ShellOutcome::Exited { code: 0 },
            truncated: false,
        }
    );

    // Nothing about a `!` command is conversation. The harness was told
    // none of it: the machine ran a command, and the model has no idea it
    // happened.
    assert!(
        harness.calls.try_recv().is_err(),
        "a shell command must never reach the harness"
    );
    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn stop_cancels_a_running_shell_command() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::RunShell {
        run: ShellRunId::generate(),
        command: "sleep 600".to_owned(),
    });
    let started = harness.next_shell_run().await;

    harness.command(ControlToDaemon::Interrupt);
    // Stop means both halves of "what is running": the turn is interrupted
    // and the command is cancelled.
    assert_eq!(harness.next_call().await, Call::Interrupt);
    tokio::time::timeout(Duration::from_secs(5), started.cancelled)
        .await
        .expect("the command was cancelled")
        .expect("the run was still there to cancel");

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_second_shell_command_is_refused_while_one_is_running() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::RunShell {
        run: ShellRunId::generate(),
        command: "sleep 600".to_owned(),
    });
    let _first = harness.next_shell_run().await;

    let second = ShellRunId::generate();
    harness.command(ControlToDaemon::RunShell {
        run: second,
        command: "echo hello".to_owned(),
    });
    // Refused rather than queued behind a command that may never end — and
    // answered, because the room has already shown the user the row.
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::ShellExited {
            run: second,
            outcome: ShellOutcome::Busy,
            truncated: false,
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_paused_session_refuses_a_shell_command_rather_than_ignoring_it() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::Budget {
        signal: BudgetSignal::Pause,
    });
    assert_eq!(harness.next_call().await, Call::Interrupt);

    let run = ShellRunId::generate();
    harness.command(ControlToDaemon::RunShell {
        run,
        command: "echo hello".to_owned(),
    });
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::ShellExited {
            run,
            outcome: ShellOutcome::Refused,
            truncated: false,
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_question_about_the_checkout_is_answered_on_the_socket_that_asked() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    let id = flyco_core::WorkdirRequestId::generate();
    harness.command(ControlToDaemon::InspectWorkdir {
        id,
        request: WorkdirRequest::Entries {
            path: String::new(),
        },
    });

    let DaemonToControl::WorkdirReply {
        id: answered,
        reply: WorkdirReply::Entries { listing },
    } = harness.room.next_frame().await
    else {
        panic!("the daemon answered something other than the listing it was asked for");
    };
    assert_eq!(
        answered, id,
        "the reply echoes the id, so the control plane knows whose answer it is"
    );
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["NOTES.md"]
    );

    // The harness stream is untouched by the detour: a reply rides its own
    // channel, and the transcript keeps flowing while a diff is computed.
    harness
        .emit(SessionOutput::Event {
            event: delta("still here"),
        })
        .await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: delta("still here")
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_refused_question_about_the_checkout_comes_back_as_a_refusal() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    let id = flyco_core::WorkdirRequestId::generate();
    harness.command(ControlToDaemon::InspectWorkdir {
        id,
        request: WorkdirRequest::File {
            path: "../elsewhere".to_owned(),
        },
    });

    let DaemonToControl::WorkdirReply {
        reply: WorkdirReply::Refused { refusal },
        ..
    } = harness.room.next_frame().await
    else {
        panic!("a path outside the checkout must not be answered with a file");
    };
    assert!(matches!(refusal, WorkdirRefusal::OutsideCheckout { .. }));

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn user_messages_interrupts_and_compaction_reach_the_harness() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::UserMessage {
        text: "what does this crate do?".to_owned(),
    });
    // The first message carries the machine notice in front of it; every
    // message after it is the user's words alone.
    let Call::UserMessage(opening) = harness.next_call().await else {
        panic!("a user message must reach the harness as one");
    };
    assert!(opening.ends_with("what does this crate do?"));

    harness.command(ControlToDaemon::UserMessage {
        text: "and what does it depend on?".to_owned(),
    });
    assert_eq!(
        harness.next_call().await,
        Call::UserMessage("and what does it depend on?".to_owned())
    );

    harness.command(ControlToDaemon::Interrupt);
    assert_eq!(harness.next_call().await, Call::Interrupt);

    harness.command(ControlToDaemon::Compact);
    assert_eq!(harness.next_call().await, Call::Compact);

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn the_agent_is_told_what_machine_it_is_on_before_it_is_given_any_work() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::UserMessage {
        text: "port the build to arm64".to_owned(),
    });

    let Call::UserMessage(opening) = harness.next_call().await else {
        panic!("a user message must reach the harness as one");
    };
    // Ahead of the work, because a machine the agent learns about afterwards
    // is one it may already have resized away from.
    assert!(opening.starts_with("[flyco machine notice] This session runs on Standard_D4s_v6"));
    assert!(opening.contains("$0.19/hr · spot"));
    assert!(opening.ends_with("port the build to arm64"));

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_resize_tells_the_agent_the_machine_restarted_and_the_disk_did_not() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::MachineChanged {
        machine_type: "Standard_D8s_v6".to_owned(),
        hourly: Some(Usd::from_cents(38)),
        spot: true,
        restarted: true,
    });

    let Call::UserMessage(notice) = harness.next_call().await else {
        panic!("a machine change reaches the agent as a message");
    };
    assert!(notice.starts_with("[flyco machine notice] The machine restarted"));
    assert!(notice.contains("Standard_D8s_v6 · $0.38/hr · spot"));
    assert!(notice.contains("Everything you had running is gone"));
    assert!(notice.contains("The disk was kept"));

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_decision_on_an_approval_the_harness_never_raised_is_not_fatal() {
    // The daemon's own MCP server raises one for a license-bound resize, and
    // the control plane performs that itself; the decision still reaches
    // every daemon because the room echoes it to whoever is connected.
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::ApprovalDecision {
        id: ApprovalId::generate(),
        decision: ApprovalDecision::Approved,
    });

    // The session keeps working, which is what "not fatal" means here.
    harness.command(ControlToDaemon::Interrupt);
    assert_eq!(harness.next_call().await, Call::Interrupt);

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn the_agent_ready_stage_is_announced_once_and_not_on_every_reconnect() {
    let mut harness = Harness::start(Greeting::Welcome).await;

    // The first connection carries it; `handshake` asserts the frame and
    // its stage.
    harness.handshake().await;

    harness
        .room
        .directives
        .send(Directive::Close)
        .expect("the room is live");
    assert_eq!(harness.room.next().await, Some(Seen::Disconnected));
    harness.handshake().await;

    // A reconnect is not a second provision, so the next frame the room
    // sees is the session's own traffic rather than another timeline entry.
    harness
        .emit(SessionOutput::Event {
            event: delta("back"),
        })
        .await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: delta("back")
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

// ── Reconnection ──

#[tokio::test]
async fn a_dropped_socket_is_reconnected_and_re_greeted() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .room
        .directives
        .send(Directive::Close)
        .expect("the room is live");
    assert_eq!(harness.room.next().await, Some(Seen::Disconnected));

    // The daemon comes back and greets again: a room that hibernated and
    // woke has forgotten the handshake, so re-greeting is the protocol.
    let authorization = harness.handshake().await;
    assert_eq!(authorization.as_deref(), Some("Bearer fd_a-daemon-token"));

    // And it is pumping again.
    harness
        .emit(SessionOutput::Event {
            event: delta("back"),
        })
        .await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: delta("back")
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn frames_produced_while_disconnected_are_buffered_and_then_sent() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .room
        .directives
        .send(Directive::Close)
        .expect("the room is live");
    assert_eq!(harness.room.next().await, Some(Seen::Disconnected));

    // Produced with no socket to carry them.
    harness
        .emit(SessionOutput::Event { event: delta("a") })
        .await;
    harness
        .emit(SessionOutput::Event { event: delta("b") })
        .await;

    harness.handshake().await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness { event: delta("a") }
    );
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness { event: delta("b") }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn an_overflowing_queue_stops_the_daemon_rather_than_truncating_a_session() {
    // No room at all: nothing is ever drained, so the queue fills.
    let session = SessionId::generate();
    let base: url::Url = "http://127.0.0.1:1/".parse().expect("a dead loopback URL");
    let endpoint = Endpoint::from_base(&base, session, TOKEN.to_owned()).expect("a relay endpoint");

    let (fake, _calls) = FakeSession::new();
    let recorder = fake.recorder();
    let (outputs, receiver) = mpsc::channel(1);
    let (approvals, _) = mpsc::unbounded_channel();
    let (observations, _) = mpsc::unbounded_channel();
    let (notifications, _) = mpsc::unbounded_channel();
    let api = RecordingApi {
        approvals,
        observations,
        notifications,
        calls: recorder.clone(),
        id: ApprovalId::generate(),
    };
    let (terminal, _, _, terminal_out) = FakeTerminal::pair();
    let (shell, _shell_runs) = FakeShell::pair();
    let (workdir, _, repo_status) = FakeWorkdir::pair();
    let run = tokio::spawn(wire::run(SessionRelay {
        endpoint,
        session: fake,
        outputs: receiver,
        api,
        terminal,
        terminal_out,
        shell,
        workdir,
        checkout: Checkout::new(std::env::temp_dir(), None),
        repo_status,
        disk: FakeDisk::new(recorder),
        spot: crate::spot::nothing_to_watch(),
        machine: crate::testing::session_machine(),
        machine_origin: MachineOrigin::Auto,
    }));

    // One more than the queue holds, plus the one the collector is carrying.
    for index in 0..=QUEUE_DEPTH + 1 {
        if outputs
            .send(SessionOutput::Event {
                event: delta(&index.to_string()),
            })
            .await
            .is_err()
        {
            break;
        }
    }

    let ended = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("the daemon stopped")
        .expect("the run did not panic");
    assert!(
        matches!(ended, Err(WireError::QueueOverflow)),
        "an overflow must stop the daemon: {ended:?}"
    );
}

// ── Budgets ──

#[tokio::test]
async fn a_budget_pause_interrupts_the_turn_and_stops_accepting_work() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::Budget {
        signal: BudgetSignal::Pause,
    });
    assert_eq!(
        harness.next_call().await,
        Call::Interrupt,
        "an exhausted budget ends the turn immediately"
    );

    // Nothing new is accepted afterwards…
    harness.command(ControlToDaemon::UserMessage {
        text: "keep going".to_owned(),
    });
    harness.command(ControlToDaemon::TerminalInput {
        data: "ls\n".to_owned(),
    });
    // …but the session is still reachable, so an archive still lands.
    assert_eq!(harness.archive().await.map_err(|e| e.to_string()), Ok(()));
}

#[tokio::test]
async fn a_budget_threshold_below_the_pause_is_told_to_the_agent() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    for signal in [
        BudgetSignal::Notice50,
        BudgetSignal::Warn80,
        BudgetSignal::FinalWarn90,
    ] {
        harness.command(ControlToDaemon::Budget { signal });
        let Call::UserMessage(text) = harness.next_call().await else {
            panic!("a budget threshold must reach the agent as a message");
        };
        assert!(
            text.starts_with("[flyco budget notice]"),
            "the agent must be able to tell this from the user: {text}"
        );
    }

    harness.archive().await.expect("the run ended cleanly");
}

// ── Archival ──

#[tokio::test]
async fn archiving_shuts_the_harness_down_and_ends_the_run() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;
    // A turn's usage travels inside its completion event: the Claude driver
    // has no periodic meter to sample, so `DaemonToControl::Usage` waits for
    // a harness that reports one.
    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::TurnCompleted {
                turn_id: "turn-1".to_owned(),
                usage: UsageReport {
                    input_tokens: 1,
                    output_tokens: 2,
                    context: None,
                    estimated_cost: None,
                },
            },
        })
        .await;
    assert!(matches!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: HarnessEvent::TurnCompleted { .. }
        }
    ));
    harness.archive().await.expect("the run ended cleanly");
}

// ── Spot reclamation ──

#[tokio::test]
async fn a_reclaimed_machine_stops_the_turn_flushes_syncs_and_then_reports() {
    // The order is the feature: the harness stops writing before the
    // transcript is flushed, the transcript is in the control plane before
    // the disk is synced, and both are done before anything says so — a
    // notice sent first would start a countdown against a session whose
    // last minute of work was still in a page cache.
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;
    harness
        .emit(SessionOutput::Started {
            session_id: "harness-native-thread".to_owned(),
        })
        .await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Started {
            harness_session_id: "harness-native-thread".to_owned(),
        }
    );
    // Filed once by the collector on the way past, before any notice.
    assert_eq!(
        harness.next_call().await,
        Call::HarnessSessionRecorded("harness-native-thread".to_owned())
    );

    harness.evict(30);

    assert_eq!(harness.next_call().await, Call::Interrupt);
    assert_eq!(harness.next_call().await, Call::Flush);
    assert_eq!(
        harness.next_call().await,
        Call::HarnessSessionRecorded("harness-native-thread".to_owned()),
        "the id a resume needs is re-filed and awaited, so the daemon knows it landed"
    );
    assert_eq!(harness.next_call().await, Call::Synced);
    assert_eq!(harness.next_call().await, Call::SpotNoticeReported(30));
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::SpotNotice {
            seconds_remaining: 30
        },
        "the countdown reaches the browsers last, once the work is safe"
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn nothing_opens_a_turn_between_the_notice_and_the_machine_going() {
    // A user message that arrives in the window is not lost — the room
    // keeps it in its mailbox and hands it to the daemon on the
    // replacement machine — but starting a turn here would be work the
    // flushed transcript has no record of.
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;
    harness.evict(30);

    assert_eq!(harness.next_call().await, Call::Interrupt);
    assert_eq!(harness.next_call().await, Call::Flush);
    assert_eq!(harness.next_call().await, Call::Synced);
    assert_eq!(harness.next_call().await, Call::SpotNoticeReported(30));
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::SpotNotice {
            seconds_remaining: 30
        }
    );

    harness.command(ControlToDaemon::UserMessage {
        text: "carry on".to_owned(),
    });
    harness.command(ControlToDaemon::Compact);
    // The socket stays open, so the daemon is still there to answer — and
    // what it does with both is nothing. A terminal keystroke proves the
    // relay is still pumping rather than merely silent.
    harness.command(ControlToDaemon::TerminalInput {
        data: "ls\n".to_owned(),
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), harness.terminal_writes.recv())
            .await
            .expect("the relay is still pumping")
            .expect("the terminal is live"),
        "ls\n"
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_reclamation_pushes_what_the_room_has_not_seen_yet() {
    // The room's stored tail is what a browser replays, and the frames
    // still in the relay's queue when the notice arrives are the last
    // minute of the session. They go out before the notice does.
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;
    harness
        .emit(SessionOutput::Event { event: delta("a") })
        .await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness { event: delta("a") }
    );

    harness.evict(30);
    assert_eq!(harness.next_call().await, Call::Interrupt);

    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::SpotNotice {
            seconds_remaining: 30
        }
    );
    harness.archive().await.expect("the run ended cleanly");
}

// ── Usage observations ──

#[tokio::test]
async fn terminal_turn_events_notify_the_control_plane_before_relaying() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::TurnFailed {
                turn_id: "turn-1".to_owned(),
                error: "tool failed".to_owned(),
            },
        })
        .await;
    assert_eq!(harness.next_notification().await, TurnNotice::Failed);
    assert!(matches!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: HarnessEvent::TurnFailed { .. }
        }
    ));

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_turn_starting_is_reported_before_it_is_relayed() {
    // The start is what the control plane records the session as `working`
    // from, and it is a fact a Durable Object cannot write to D1 — so it
    // takes the same REST route the turn's end does, before the frame that
    // announces it (docs/ux.md §6).
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::TurnStarted {
                turn_id: "turn-1".to_owned(),
            },
        })
        .await;
    assert_eq!(harness.next_notification().await, TurnNotice::Started);
    assert!(matches!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: HarnessEvent::TurnStarted { .. }
        }
    ));

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_turn_that_reported_a_cost_files_an_observation() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::TurnCompleted {
                turn_id: "turn-1".to_owned(),
                usage: UsageReport {
                    input_tokens: 12,
                    output_tokens: 34,
                    context: None,
                    estimated_cost: Some(Usd::from_micros(4_200)),
                },
            },
        })
        .await;

    let observation = harness.next_observation().await;
    assert_eq!(observation.observed_cost, Some(Usd::from_micros(4_200)));
    assert!(observation.rate_limit.is_none());
    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_turn_whose_harness_priced_nothing_files_nothing() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    // Codex reports tokens and no cost. An observation of "no cost" would
    // be a claim the harness never made.
    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::TurnCompleted {
                turn_id: "turn-1".to_owned(),
                usage: UsageReport {
                    input_tokens: 12,
                    output_tokens: 34,
                    context: None,
                    estimated_cost: None,
                },
            },
        })
        .await;
    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::UsageLimited {
                resets_at_unix: Some(1_800_007_200),
            },
        })
        .await;

    // The limit is the first thing filed, so the priced-nothing turn filed
    // nothing before it.
    let observation = harness.next_observation().await;
    assert_eq!(observation.observed_cost, None);
    assert_eq!(
        observation.rate_limit.map(|limit| limit.resets_at_unix),
        Some(Some(1_800_007_200))
    );
    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_refused_observation_does_not_stop_the_session() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;
    // A session on inherited developer credentials has no linked account,
    // so the control plane refuses every observation it posts. The turn
    // still has to reach the room.
    harness.refuse_observations();

    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::UsageLimited {
                resets_at_unix: None,
            },
        })
        .await;

    assert!(matches!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: HarnessEvent::UsageLimited { .. }
        }
    ));
    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_dirty_tree_is_reported_and_keeps_the_agent_awake() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .repo_inject
        .send(" M src/lib.rs\n".to_owned())
        .expect("the watcher is live");
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::RepoDirty {
            summary: " M src/lib.rs\n".to_owned(),
        }
    );

    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::TurnCompleted {
                turn_id: "turn-1".to_owned(),
                usage: UsageReport {
                    input_tokens: 1,
                    output_tokens: 1,
                    context: None,
                    estimated_cost: None,
                },
            },
        })
        .await;
    assert!(matches!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: HarnessEvent::TurnCompleted { .. }
        }
    ));
    assert!(matches!(
        harness.next_call().await,
        Call::UserMessage(text) if text.starts_with("[flyco repo notice]")
    ));

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_usage_limit_with_a_reset_time_auto_continues() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::UsageLimited {
                resets_at_unix: Some(0),
            },
        })
        .await;
    assert!(matches!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: HarnessEvent::UsageLimited { .. }
        }
    ));
    assert!(matches!(
        harness.next_call().await,
        Call::UserMessage(text) if text.starts_with("[flyco usage notice]")
    ));

    harness.archive().await.expect("the run ended cleanly");
}

// ── The REST client, against a real HTTP server ──

mod rest_client {
    use super::{ApprovalRaiser as _, ControlApi, ControlPlane, HttpControlApi, Reply, TOKEN};
    use crate::control::rest::{ControlApiError, TranscriptRead};
    use flyco_core::wire::ApprovalPayload;
    use flyco_core::{ApprovalId, SessionId};

    fn api(base: &url::Url, session: SessionId) -> HttpControlApi {
        HttpControlApi::new(base.clone(), session, TOKEN.to_owned())
    }

    #[tokio::test]
    async fn raising_an_approval_posts_it_and_returns_the_assigned_id() {
        let session = SessionId::generate();
        let id = ApprovalId::generate();
        let mut plane = ControlPlane::start(vec![Reply::approval(id, session)]).await;
        let api = api(&plane.base, session);

        let payload = ApprovalPayload::AgentsMdChange {
            find: "old".to_owned(),
            replace: "new".to_owned(),
        };
        assert_eq!(
            api.raise_approval(payload.clone()).await.expect("raise"),
            id
        );

        let request = plane.next().await.expect("the control plane was called");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, format!("/v1/sessions/{session}/approvals"));
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer fd_a-daemon-token")
        );
        assert_eq!(
            serde_json::from_slice::<ApprovalPayload>(&request.body).expect("the payload"),
            payload
        );
    }

    #[tokio::test]
    async fn a_spot_notice_is_posted_to_the_session_that_is_losing_its_machine() {
        let session = SessionId::generate();
        let mut plane = ControlPlane::start(vec![Reply::no_content()]).await;
        let api = api(&plane.base, session);

        api.report_spot_notice(30).await.expect("report");

        let request = plane.next().await.expect("the control plane was called");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            format!("/v1/sessions/{session}/spot-notice")
        );
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer fd_a-daemon-token")
        );
        assert_eq!(
            serde_json::from_slice::<flyco_core::ReportSpotNotice>(&request.body)
                .expect("the report"),
            flyco_core::ReportSpotNotice {
                seconds_remaining: 30
            }
        );
    }

    #[tokio::test]
    async fn a_transcript_batch_is_put_at_its_sequence_number() {
        let session = SessionId::generate();
        let mut plane = ControlPlane::start(vec![Reply::no_content()]).await;
        let api = api(&plane.base, session);

        api.put_transcript_batch("main", 7, b"{\"n\":7}\n".to_vec())
            .await
            .expect("put");

        let request = plane.next().await.expect("the control plane was called");
        assert_eq!(request.method, "PUT");
        assert_eq!(
            request.target,
            format!("/v1/sessions/{session}/transcript/main/batches/7")
        );
        assert_eq!(request.body, b"{\"n\":7}\n");
    }

    #[tokio::test]
    async fn a_transcript_read_reports_its_batch_count() {
        let session = SessionId::generate();
        let mut plane =
            ControlPlane::start(vec![Reply::transcript(b"{\"n\":0}\n{\"n\":1}\n", 2)]).await;
        let api = api(&plane.base, session);

        assert_eq!(
            api.get_transcript("main").await.expect("read"),
            TranscriptRead {
                body: b"{\"n\":0}\n{\"n\":1}\n".to_vec(),
                batches: 2,
            }
        );

        let request = plane.next().await.expect("the control plane was called");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            format!("/v1/sessions/{session}/transcript/main")
        );
    }

    #[tokio::test]
    async fn a_refusal_carries_the_control_planes_own_explanation() {
        let session = SessionId::generate();
        let mut plane = ControlPlane::start(vec![Reply::problem(
            409,
            "batch-already-stored",
            "transcript batch 0 is already stored and batches are immutable",
        )])
        .await;
        let api = api(&plane.base, session);

        let error = api
            .put_transcript_batch("main", 0, b"x".to_vec())
            .await
            .expect_err("a conflict must not be swallowed");
        match error {
            ControlApiError::Refused { status, detail, .. } => {
                assert_eq!(status, 409);
                assert!(detail.contains("immutable"), "{detail}");
            }
            other => panic!("expected a refusal with its problem document: {other}"),
        }
        plane.next().await.expect("the control plane was called");
    }
}

// ── The remote transcript store ──

mod remote_store {
    use super::{RemoteTranscriptStore, SessionKey, StoreError, TranscriptStore, stream_key};
    use crate::control::rest::{ApprovalRaiser, ControlApi, ControlApiError, TranscriptRead};
    use flyco_core::wire::ApprovalPayload;
    use flyco_core::{ApprovalId, HarnessObservation};
    use serde_json::{Value, json};
    use std::sync::mpsc::{Receiver, Sender, channel};

    /// One call the store made.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Wrote {
        Put {
            stream: String,
            seq: u64,
            body: Vec<u8>,
        },
        Read(String),
    }

    /// A control plane that already holds `batches` batches of `body`.
    #[derive(Debug)]
    struct Held {
        body: Vec<u8>,
        batches: u64,
        wrote: Sender<Wrote>,
    }

    impl ApprovalRaiser for Held {
        fn raise_approval(
            &self,
            _payload: ApprovalPayload,
        ) -> impl core::future::Future<Output = Result<ApprovalId, ControlApiError>> + Send
        {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store never raises approvals".to_owned(),
            )))
        }
    }

    impl ControlApi for Held {
        fn record_observation(
            &self,
            _observation: HarnessObservation,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store records no observations".to_owned(),
            )))
        }

        fn notify_turn_started(
            &self,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Ok(()))
        }

        fn notify_turn_completed(
            &self,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Ok(()))
        }

        fn notify_turn_failed(
            &self,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Ok(()))
        }

        fn record_harness_session(
            &self,
            _harness_session_id: &str,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Ok(()))
        }

        fn report_spot_notice(
            &self,
            _seconds_remaining: u32,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store reports no spot notices".to_owned(),
            )))
        }

        fn harness_session_id(
            &self,
        ) -> impl core::future::Future<Output = Result<Option<String>, ControlApiError>> + Send
        {
            core::future::ready(Ok(None))
        }

        fn put_transcript_batch(
            &self,
            stream: &str,
            seq: u64,
            body: Vec<u8>,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(
                self.wrote
                    .send(Wrote::Put {
                        stream: stream.to_owned(),
                        seq,
                        body,
                    })
                    .map_err(|error| ControlApiError::Transport(error.to_string())),
            )
        }

        fn put_workdir_patch(
            &self,
            _patch: Vec<u8>,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store stores no workdir patches".to_owned(),
            )))
        }

        fn get_workdir_patch(
            &self,
        ) -> impl core::future::Future<Output = Result<Option<Vec<u8>>, ControlApiError>> + Send
        {
            core::future::ready(Ok(None))
        }

        fn report_stage(
            &self,
            _stage: flyco_core::ProvisioningStage,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store announces no provisioning stages".to_owned(),
            )))
        }

        fn get_transcript(
            &self,
            stream: &str,
        ) -> impl core::future::Future<Output = Result<TranscriptRead, ControlApiError>> + Send
        {
            let recorded = self
                .wrote
                .send(Wrote::Read(stream.to_owned()))
                .map_err(|error| ControlApiError::Transport(error.to_string()));
            core::future::ready(recorded.map(|()| TranscriptRead {
                body: self.body.clone(),
                batches: self.batches,
            }))
        }
    }

    fn store(body: &[u8], batches: u64) -> (RemoteTranscriptStore<Held>, Receiver<Wrote>) {
        let (wrote, received) = channel();
        (
            RemoteTranscriptStore::new(Held {
                body: body.to_vec(),
                batches,
                wrote,
            }),
            received,
        )
    }

    fn key(subpath: Option<&str>) -> SessionKey {
        SessionKey {
            project_key: "flyco-work".to_owned(),
            session_id: "9d0f4b1a".to_owned(),
            subpath: subpath.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn a_fresh_stream_numbers_its_batches_from_zero() {
        let (mut store, wrote) = store(b"", 0);
        store
            .append(&key(None), vec![json!({ "uuid": "a" })])
            .await
            .expect("append");
        store
            .append(&key(None), vec![json!({ "uuid": "b" })])
            .await
            .expect("append");

        assert_eq!(
            wrote.try_iter().collect::<Vec<_>>(),
            vec![
                // The sequence is read once, then advanced locally: this
                // daemon is the only writer of its own transcript.
                Wrote::Read("flyco-work.9d0f4b1a".to_owned()),
                Wrote::Put {
                    stream: "flyco-work.9d0f4b1a".to_owned(),
                    seq: 0,
                    body: b"{\"uuid\":\"a\"}\n".to_vec(),
                },
                Wrote::Put {
                    stream: "flyco-work.9d0f4b1a".to_owned(),
                    seq: 1,
                    body: b"{\"uuid\":\"b\"}\n".to_vec(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_resumed_stream_continues_where_the_previous_host_stopped() {
        // Three batches already in the control plane: a session that moved
        // machine must not write batch 0 over its predecessor's.
        let (mut store, wrote) = store(b"{\"uuid\":\"a\"}\n", 3);
        store
            .append(&key(None), vec![json!({ "uuid": "d" })])
            .await
            .expect("append");

        let calls: Vec<Wrote> = wrote.try_iter().collect();
        assert!(matches!(
            calls.as_slice(),
            [Wrote::Read(_), Wrote::Put { seq: 3, .. }]
        ));
    }

    #[tokio::test]
    async fn loading_parses_the_concatenated_stream_and_seeds_the_sequence() {
        let (mut store, wrote) = store(b"{\"uuid\":\"a\"}\n\n{\"uuid\":\"b\"}\n", 2);
        assert_eq!(
            store.load(&key(None)).await.expect("load"),
            vec![json!({ "uuid": "a" }), json!({ "uuid": "b" })]
        );

        // The load already told the store where the stream ends, so the next
        // append needs no second read.
        store
            .append(&key(None), vec![json!({ "uuid": "c" })])
            .await
            .expect("append");
        assert!(matches!(
            wrote.try_iter().collect::<Vec<_>>().as_slice(),
            [Wrote::Read(_), Wrote::Put { seq: 2, .. }]
        ));
    }

    #[tokio::test]
    async fn an_empty_batch_is_not_a_round_trip() {
        let (mut store, wrote) = store(b"", 0);
        store.append(&key(None), Vec::new()).await.expect("append");
        assert_eq!(wrote.try_iter().count(), 0);
    }

    #[tokio::test]
    async fn a_corrupt_stored_line_is_reported_with_its_position() {
        let (mut store, _wrote) = store(b"{\"uuid\":\"a\"}\nnot json\n", 1);
        assert!(matches!(
            store.load(&key(None)).await,
            Err(StoreError::CorruptLine { line: 2, .. })
        ));
    }

    #[test]
    fn a_stream_key_is_one_path_segment_whatever_the_harness_named_things() {
        assert_eq!(stream_key(&key(None)).expect("key"), "flyco-work.9d0f4b1a");
        assert_eq!(
            stream_key(&key(Some("subagents/agent-7"))).expect("key"),
            "flyco-work.9d0f4b1a.subagents_agent-7",
            "a sub-stream's slash must not become a second path segment"
        );

        // Replacing rather than dropping keeps two different keys apart.
        let dotted = SessionKey {
            project_key: "a/b".to_owned(),
            session_id: "s".to_owned(),
            subpath: None,
        };
        let underscored = SessionKey {
            project_key: "a_b".to_owned(),
            session_id: "s".to_owned(),
            subpath: None,
        };
        assert_eq!(
            stream_key(&dotted).expect("key"),
            stream_key(&underscored).expect("key"),
            "this collision is accepted: both are one segment and the SDK's \
             project keys are directory names, not free text"
        );
    }

    #[test]
    fn an_empty_key_part_is_refused_before_any_call() {
        let empty = SessionKey {
            project_key: String::new(),
            session_id: "s".to_owned(),
            subpath: None,
        };
        assert!(matches!(
            stream_key(&empty),
            Err(StoreError::UnusableKeySegment {
                field: "project_key",
                ..
            })
        ));
    }

    #[test]
    fn a_key_longer_than_the_control_plane_accepts_is_refused() {
        let long = SessionKey {
            project_key: "x".repeat(200),
            session_id: "s".to_owned(),
            subpath: None,
        };
        assert!(matches!(
            stream_key(&long),
            Err(StoreError::UnusableKeySegment {
                field: "stream",
                ..
            })
        ));
    }

    /// Keeps the unused-import warning honest: `Value` names the entry type
    /// the store round-trips.
    const _: Option<Vec<Value>> = None;
}
