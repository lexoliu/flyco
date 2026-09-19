//! The daemon's end of the relay, driven against a real loopback room.

use core::time::Duration;

use flyco_core::wire::ApprovalPayload;
use flyco_core::{
    ApprovalDecision, ApprovalId, BudgetSignal, ControlToDaemon, DaemonToControl, HarnessEvent,
    HarnessObservation, HarnessSessionView, MachineOrigin, ModelChoice, ModelOption,
    ProvisioningStage, SessionId, ShellOutcome, ShellRunId, ShellStream, StopReason, UsageReport,
    UsageWindow, Usd, WIRE_PROTOCOL_VERSION,
};
use tokio::sync::mpsc;

use crate::control::rest::{
    ApprovalRaiser, CommandStream, ControlApi, ControlApiError, HttpControlApi, RelayTransport,
    TranscriptRead,
};
use crate::control::store::{RemoteTranscriptStore, stream_key};
use crate::control::wire::{self, QUEUE_DEPTH, SessionRelay, WireError};
use crate::desktop::FakeDesktop;
use crate::git::FakeRepos;
use crate::harness::SessionOutput;
use crate::harness::claude::protocol::SessionKey;
use crate::harness::claude::store::{StoreError, TranscriptStore};
use crate::shell::{FakeShell, ShellEvent, ShellUpdate, StartedRun};
use crate::spot::{FakeEviction, SpotNotice};
use crate::terminal::{FakeTerminal, TerminalCall};
use crate::testing::{
    AttachAnswer, Call, ControlPlane, Directive, FakeDisk, FakeSession, Reply, Room, Seen,
};
use crate::workdir::Workspace;
use flyco_core::workdir::{WorkdirRefusal, WorkdirReply, WorkdirRequest};

/// What `git diff --cached --binary` produces over the one uncommitted edit
/// in a session's checkout.
///
/// The bytes the daemon stores, rather than a placeholder: what the stop
/// sequence has to get off a machine with no disk is a patch a later `git
/// apply` accepts, and a test asserting on its size should be asserting on
/// the size of one.
const WORKDIR_PATCH: &[u8] = b"diff --git a/NOTES.md b/NOTES.md\n\
index 3b18e512..8c7e5a61 100644\n\
--- a/NOTES.md\n\
+++ b/NOTES.md\n\
@@ -1 +1,2 @@\n\
 the agent was here\n\
+and this line was never committed\n";

/// A daemon token shaped the way the control plane mints them.
const TOKEN: &str = "fd_a-daemon-token";

/// GitHub credentials for the relay to clone an `AddRepo` with.
///
/// The token is a shape, not a secret: `FakeRepos` never runs git, so it
/// is never spent — but the field cannot be `None`, because a relay with
/// none refuses the command and the tests that send one would be refused
/// for the wrong reason.
fn github_access() -> crate::config::GithubAccess {
    crate::config::GithubAccess {
        token: "gho_a-test-token".to_owned(),
        identity: crate::config::GitIdentity {
            name: "flyco".to_owned(),
            email: "flyco@flyco.dev".to_owned(),
        },
    }
}

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

/// A [`ControlApi`] that answers without a network — except for the relay
/// itself, which is real HTTP+SSE against the loopback [`Room`].
///
/// The relay tests care about *ordering* — that an approval is durable
/// before it is announced — so the durable-write half is recorded here,
/// while attach, the command stream, and the frames batches are delegated
/// to a real [`HttpControlApi`] so the transport under test is the one
/// production runs.
#[derive(Debug, Clone)]
struct RecordingApi {
    /// The relay transport, pointed at the test's loopback room.
    transport: HttpControlApi,
    approvals: mpsc::UnboundedSender<ApprovalPayload>,
    observations: mpsc::UnboundedSender<HarnessObservation>,
    notifications: mpsc::UnboundedSender<TurnNotice>,
    /// The one channel every double in a test records into, so an ordering
    /// across the harness, the control plane and the disk is assertable.
    calls: mpsc::UnboundedSender<Call>,
    id: ApprovalId,
}

impl RelayTransport for RecordingApi {
    fn attach(
        &self,
    ) -> impl core::future::Future<
        Output = Result<flyco_core::wire::DaemonAttached, ControlApiError>,
    > + Send {
        self.transport.attach()
    }

    fn commands(
        &self,
        epoch: u64,
        idle: Duration,
    ) -> impl core::future::Future<
        Output = Result<CommandStream<flyco_core::wire::DaemonCommand>, ControlApiError>,
    > + Send {
        self.transport.commands(epoch, idle)
    }

    fn frames(
        &self,
        batch: &flyco_core::wire::DaemonFrames,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        self.transport.frames(batch)
    }
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

    fn report_startup_failure(
        &self,
        _message: String,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(Ok(()))
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

    fn harness_session(
        &self,
    ) -> impl core::future::Future<Output = Result<HarnessSessionView, ControlApiError>> + Send
    {
        // The relay never asks: the conversation and the model are resolved
        // once, before the harness is started, by `flycod run` itself.
        core::future::ready(Ok(HarnessSessionView {
            harness_session_id: None,
            model: ModelChoice {
                model: "sonnet".to_owned(),
                effort: None,
            },
            permission_mode: flyco_core::PermissionMode::Auto,
        }))
    }

    fn report_models(
        &self,
        models: &[ModelOption],
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        let recorded = self
            .calls
            .send(Call::ModelsReported(models.to_vec()))
            .map_err(|error| ControlApiError::Transport(error.to_string()));
        core::future::ready(recorded)
    }

    fn report_usage(
        &self,
        windows: &[UsageWindow],
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        let recorded = self
            .calls
            .send(Call::UsageReported(windows.to_vec()))
            .map_err(|error| ControlApiError::Transport(error.to_string()));
        core::future::ready(recorded)
    }

    fn report_usage_limit(
        &self,
        window: &UsageWindow,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        let recorded = self
            .calls
            .send(Call::UsageLimitReported(window.clone()))
            .map_err(|error| ControlApiError::Transport(error.to_string()));
        core::future::ready(recorded)
    }

    fn put_workdir_patch(
        &self,
        dir: Option<&str>,
        patch: Vec<u8>,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(
            self.calls
                .send(Call::WorkdirPatchStored(
                    dir.map(str::to_owned),
                    patch.len(),
                ))
                .map_err(|error| ControlApiError::Transport(error.to_string())),
        )
    }

    fn get_workdir_patch(
        &self,
        _dir: Option<&str>,
    ) -> impl core::future::Future<Output = Result<Option<Vec<u8>>, ControlApiError>> + Send {
        core::future::ready(Ok(None))
    }

    fn get_handoff(
        &self,
    ) -> impl core::future::Future<Output = Result<Option<flyco_core::HandoffView>, ControlApiError>>
    + Send {
        core::future::ready(Ok(None))
    }

    fn get_handoff_transcript(
        &self,
    ) -> impl core::future::Future<Output = Result<Option<Vec<u8>>, ControlApiError>> + Send {
        core::future::ready(Ok(None))
    }

    fn report_stopping(
        &self,
        reason: StopReason,
    ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
        core::future::ready(
            self.calls
                .send(Call::StoppingReported(reason))
                .map_err(|error| ControlApiError::Transport(error.to_string())),
        )
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
    /// The session the daemon under test is running — a second attach for
    /// the same one is how a test supersedes it.
    session: SessionId,
    outputs: mpsc::Sender<SessionOutput>,
    calls: mpsc::UnboundedReceiver<Call>,
    approvals: mpsc::UnboundedReceiver<ApprovalPayload>,
    observations: mpsc::UnboundedReceiver<HarnessObservation>,
    notifications: mpsc::UnboundedReceiver<TurnNotice>,
    approval_id: ApprovalId,
    terminal_writes: mpsc::UnboundedReceiver<TerminalCall>,
    terminal_inject: mpsc::Sender<crate::terminal::TerminalEvent>,
    /// What the daemon asked its desktop to do.
    desktop_calls: mpsc::UnboundedReceiver<crate::desktop::DesktopCall>,
    /// Reports what the test's desktop did, through the channel the real
    /// supervisor fills.
    desktop_reports: mpsc::Sender<crate::desktop::DesktopEvent>,
    /// The `!` commands the daemon asked its shell to run.
    shell_runs: mpsc::UnboundedReceiver<StartedRun>,
    /// Injects a checkout's `(dir, summary)` dirty report into the relay.
    repo_inject: mpsc::UnboundedSender<(Option<String>, String)>,
    /// Makes the fake metadata endpoint announce a reclamation. Taken once:
    /// a provider announces one machine's reclamation exactly once.
    evict: Option<tokio::sync::oneshot::Sender<SpotNotice>>,
    /// Makes the platform ask this container to stop, on the channel
    /// `crate::stop::watch` fills on a machine with no disk.
    stopper: mpsc::Sender<StopReason>,
    run: tokio::task::JoinHandle<Result<(), WireError>>,
    /// Whether the agent-ready stage is still to come.
    ///
    /// It is announced once a room has *accepted* the daemon's attach, and
    /// never again after that, so [`Harness::handshake`] expects it on the
    /// first attach a welcoming room answers and never on a re-attach or
    /// on a room that refuses attaches outright.
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
    async fn start(answer: AttachAnswer) -> Self {
        Self::with_capacity(answer, 32).await
    }

    async fn with_capacity(answer: AttachAnswer, outputs: usize) -> Self {
        Self::build(answer, outputs, wire::Deadlines::default()).await
    }

    /// A harness whose relay notices a dead stream fast enough to watch.
    ///
    /// The production silence limit is ninety seconds, which is a liveness
    /// budget and not a test; the loop under test is the same one either
    /// way.
    async fn with_deadlines(answer: AttachAnswer, deadlines: wire::Deadlines) -> Self {
        Self::build(answer, 32, deadlines).await
    }

    /// A harness whose agent has stopped reading its commands.
    async fn wedged(deadlines: wire::Deadlines) -> Self {
        let (session, calls) = FakeSession::wedged();
        Self::assemble(
            AttachAnswer::Accept,
            32,
            deadlines,
            (session, calls),
            FakeRepos::with_snapshots([(None, Some(WORKDIR_PATCH.to_vec()))]),
        )
        .await
    }

    async fn build(answer: AttachAnswer, outputs: usize, deadlines: wire::Deadlines) -> Self {
        Self::assemble(
            answer,
            outputs,
            deadlines,
            FakeSession::new(),
            FakeRepos::with_snapshots([(None, Some(WORKDIR_PATCH.to_vec()))]),
        )
        .await
    }

    /// A harness with the checkout set the test describes — several named
    /// checkouts, or one a `clone_repo` must refuse.
    async fn with_repos(
        answer: AttachAnswer,
        repos: (FakeRepos, mpsc::UnboundedSender<(Option<String>, String)>),
    ) -> Self {
        Self::assemble(
            answer,
            32,
            wire::Deadlines::default(),
            FakeSession::new(),
            repos,
        )
        .await
    }

    async fn assemble(
        answer: AttachAnswer,
        outputs: usize,
        deadlines: wire::Deadlines,
        harness: (FakeSession, mpsc::UnboundedReceiver<Call>),
        (repos, repo_inject): (FakeRepos, mpsc::UnboundedSender<(Option<String>, String)>),
    ) -> Self {
        let room = Room::start(answer).await;
        let session = SessionId::generate();

        let (fake, calls) = harness;
        let recorder = fake.recorder();
        let (sender, receiver) = mpsc::channel(outputs);
        let (approval_sender, approvals) = mpsc::unbounded_channel();
        let (observation_sender, observations) = mpsc::unbounded_channel();
        let (notification_sender, notifications) = mpsc::unbounded_channel();
        let approval_id = ApprovalId::generate();
        let api = RecordingApi {
            transport: HttpControlApi::new(room.base.clone(), session, TOKEN.to_owned()),
            approvals: approval_sender,
            observations: observation_sender,
            notifications: notification_sender,
            calls: recorder.clone(),
            id: approval_id,
        };
        let (watcher, evict) = FakeEviction::pair();
        let (notices, spot) = mpsc::channel(1);
        crate::spot::spawn(watcher, notices);
        // The stop signal is injected on the channel `crate::stop::watch`
        // would have handed the relay, because a test cannot raise a real
        // `SIGTERM` at this process without ending the test binary.
        let (stopper, stops) = mpsc::channel(1);

        let (terminal, terminal_writes, terminal_inject, terminal_out) = FakeTerminal::pair();
        let (desktop, desktop_calls, desktop_reports, desktop_out) = FakeDesktop::pair();
        let (shell, shell_runs) = FakeShell::pair();
        // The developer-machine shape by default: one checkout at the
        // workspace root, which is the `None` dir both the wire and the
        // patch store use.
        let checkout_dir = scratch_checkout();
        let run = tokio::spawn(wire::run(SessionRelay {
            session_id: session,
            deadlines,
            session: fake,
            outputs: receiver,
            api,
            terminal,
            terminal_out,
            desktop,
            desktop_out,
            tui: crate::tui::HarnessTui::fixture(),
            shell,
            repos,
            workspace: Workspace::new(checkout_dir.0.clone(), None),
            github: Some(github_access()),
            disk: FakeDisk::new(recorder),
            spot,
            stops,
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
            desktop_calls,
            desktop_reports,
            shell_runs,
            repo_inject,
            evict: Some(evict),
            stopper,
            run,
            expect_ready: answer == AttachAnswer::Accept,
            checkout_dir,
        }
    }

    /// Makes the provider announce this machine's reclamation.
    ///
    /// The notice travels the whole way a real one does — through the
    /// watcher's own task and the channel the relay selects on — so what
    /// the test drives is the daemon's reaction rather than a function call
    /// into the middle of it.
    /// Makes the platform ask this container to stop.
    ///
    /// The signal travels the channel a real `SIGTERM` would arrive on, so
    /// what the test drives is the relay's reaction rather than a call into
    /// the middle of it.
    async fn stop(&self) {
        self.stopper
            .send(StopReason::Sigterm)
            .await
            .expect("the relay is listening for a stop");
    }

    fn evict(&mut self, seconds_remaining: u32) {
        self.evict
            .take()
            .expect("a machine is reclaimed once")
            .send(SpotNotice { seconds_remaining })
            .expect("the watcher is live");
    }

    /// Waits for the room to see this daemon's attach and its stream.
    ///
    /// On the first attachment the attach is followed by the last stage
    /// of the provisioning timeline (docs/ux.md §9.2): the harness is up
    /// and the room has the daemon's stream, which is the whole meaning of
    /// "the agent is ready". A reconnect does not repeat it.
    async fn handshake(&mut self) -> Option<String> {
        let authorization = match self.room.next().await {
            Some(Seen::Attached { authorization, .. }) => authorization,
            other => panic!("the daemon did not attach: {other:?}"),
        };
        assert!(
            matches!(self.room.next().await, Some(Seen::StreamOpened(_))),
            "an accepted attach opens the command stream"
        );
        if self.expect_ready {
            self.expect_ready = false;
            let DaemonToControl::ProvisioningStage {
                stage: ProvisioningStage::Ready,
                ..
            } = self.room.next_frame().await
            else {
                panic!("the first attach did not announce that the agent is ready");
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
    /// Waits for the relay to end by itself, which a stop makes it do.
    ///
    /// Unlike [`Self::archive`] nothing is sent: the platform's signal is
    /// what ends this run, and a relay still waiting for a command would be
    /// the bug the test is looking for.
    async fn ended(self) -> Result<(), WireError> {
        tokio::time::timeout(Duration::from_secs(5), self.run)
            .await
            .expect("the run ended by itself")
            .expect("the run did not panic")
    }

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

// ── The attach ──

#[tokio::test]
async fn a_daemon_attaches_with_its_token_and_its_protocol_version() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;

    let Some(Seen::Attached {
        authorization,
        body,
    }) = harness.room.next().await
    else {
        panic!("the daemon did not attach");
    };
    assert_eq!(
        authorization.as_deref(),
        Some("Bearer fd_a-daemon-token"),
        "the attach carries the session's daemon token"
    );
    assert_eq!(
        body["protocol_version"], WIRE_PROTOCOL_VERSION,
        "and the wire version it speaks"
    );

    // The room's answer opens the stream, and the first batch announces
    // the agent is ready — `handshake`'s everyday assertions, run once by
    // hand so this test can read the attach itself.
    assert!(matches!(
        harness.room.next().await,
        Some(Seen::StreamOpened(1))
    ));
    harness.expect_ready = false;
    let DaemonToControl::ProvisioningStage {
        stage: ProvisioningStage::Ready,
        ..
    } = harness.room.next_frame().await
    else {
        panic!("the first attach did not announce that the agent is ready");
    };

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_refusal_naming_a_wait_is_honoured_before_the_next_attach() {
    // The control plane refusing with `429` and `Retry-After` is saying
    // when: attaching sooner is one more request charged to a budget it
    // just said is spent (issue #342). The wait beats the ladder, whose
    // first rung is at most one second.
    let wait = Duration::from_secs(3);
    let mut harness = Harness::start(AttachAnswer::Spent(wait)).await;

    let Some(Seen::Attached { .. }) = harness.room.next().await else {
        panic!("the daemon did not attempt an attach");
    };
    let refused_at = tokio::time::Instant::now();
    let Some(Seen::Attached { .. }) = harness.room.next().await else {
        panic!("the daemon did not attach again within the room's patience");
    };
    assert!(
        refused_at.elapsed() >= wait,
        "the second attach came {:?} after the refusal, before the {wait:?} it named",
        refused_at.elapsed()
    );
    // A daemon that never attached has no stream to take an archive on.
    harness.run.abort();
}

#[tokio::test]
async fn frames_produced_within_the_window_ride_one_batch() {
    // Two events spaced well inside `COALESCE` are one POST: the frames
    // route is charged per request, and a harness streaming a terminal
    // would otherwise be one request per chunk (issue #342).
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness
        .emit(SessionOutput::Event {
            event: delta("one"),
        })
        .await;
    tokio::time::sleep(wire::COALESCE / 5).await;
    harness
        .emit(SessionOutput::Event {
            event: delta("two"),
        })
        .await;

    let batch = loop {
        match harness.room.next().await {
            Some(Seen::Batch { frames, .. }) => break frames,
            Some(_) => {}
            None => panic!("no batch reached the room"),
        }
    };
    assert_eq!(
        batch.len(),
        2,
        "two events inside one window were posted as one batch: {batch:?}"
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn nothing_is_pumped_before_an_attach_is_accepted() {
    // A room that refuses the attach gets the attempt and nothing else,
    // however much the harness produces: a frame posted to a room that has
    // not accepted the attach is a frame the session lost.
    let mut harness = Harness::start(AttachAnswer::Refuse).await;

    // The refusal is a retryable one — a room mid-deploy — so the daemon
    // backs off and attaches again rather than ending.
    let Some(Seen::Attached { .. }) = harness.room.next().await else {
        panic!("the daemon did not attempt an attach");
    };
    harness
        .emit(SessionOutput::Event { event: delta("hi") })
        .await;

    assert!(
        matches!(harness.room.next().await, Some(Seen::Attached { .. })),
        "a refused daemon backs off and attaches again"
    );

    // No stream was ever opened and no batch ever posted: without an epoch
    // the daemon has nothing to pump into.
    assert!(
        !matches!(
            harness.room.next().await,
            Some(Seen::StreamOpened(_) | Seen::Batch { .. })
        ),
        "a refused attach opens no stream and carries no frames"
    );
    harness.run.abort();
}

// ── The event pump ──

#[tokio::test]
async fn session_output_reaches_the_room_as_wire_frames() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
        payload: ApprovalPayload::ToolUse {
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "ls" }),
        },
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::TerminalInput {
        data: "ls\n".to_owned(),
    });
    assert_eq!(
        harness.terminal_writes.recv().await,
        Some(TerminalCall::Write("ls\n".to_owned()))
    );

    harness
        .terminal_inject
        .send(crate::terminal::TerminalEvent::Output(
            "file.txt\n".to_owned(),
        ))
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

#[tokio::test]
async fn the_pane_size_reaches_the_pty() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::TerminalResize {
        cols: 132,
        rows: 40,
    });
    assert_eq!(
        harness.terminal_writes.recv().await,
        Some(TerminalCall::Resize(132, 40))
    );

    harness.archive().await.expect("the run ended cleanly");
}

// ── The composer's `!` commands ──

#[tokio::test]
async fn a_shell_command_runs_on_the_machine_and_its_output_reaches_the_room() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
async fn a_question_about_the_checkout_is_answered_over_the_relay() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::UserMessage {
        text: "what does this crate do?".to_owned(),
        origin: flyco_core::MessageOrigin::User,
    });
    // The first message carries the machine notice in front of it; every
    // message after it is the user's words alone.
    let Call::UserMessage(opening) = harness.next_call().await else {
        panic!("a user message must reach the harness as one");
    };
    assert!(opening.ends_with("what does this crate do?"));

    harness.command(ControlToDaemon::UserMessage {
        text: "and what does it depend on?".to_owned(),
        origin: flyco_core::MessageOrigin::User,
    });
    assert_eq!(
        harness.next_call().await,
        Call::UserMessage("and what does it depend on?".to_owned())
    );

    harness.command(ControlToDaemon::Interrupt);
    assert_eq!(harness.next_call().await, Call::Interrupt);

    harness.command(ControlToDaemon::Compact);
    assert_eq!(harness.next_call().await, Call::Compact);

    let model = ModelChoice {
        model: "claude-fable-5-1[1m]".to_owned(),
        effort: Some("max".to_owned()),
    };
    harness.command(ControlToDaemon::SetModel {
        model: model.clone(),
    });
    assert_eq!(harness.next_call().await, Call::ModelSet(model));

    harness.command(ControlToDaemon::SetPermissionMode {
        mode: flyco_core::PermissionMode::Plan,
    });
    assert_eq!(
        harness.next_call().await,
        Call::PermissionModeSet(flyco_core::PermissionMode::Plan)
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn what_is_left_of_the_plan_is_filed_over_rest_rather_than_sent_as_a_frame() {
    // The snapshot is recorded against the *account*, which lives in D1,
    // so it leaves the relay by the same door the model list does and the
    // control plane announces it to the browsers itself.
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    let windows = vec![
        UsageWindow::new(Some(300), None, 12, Some(1_789_002_000)),
        UsageWindow::new(Some(10_080), None, 40, Some(1_789_570_800)),
    ];
    harness
        .emit(SessionOutput::PlanUsage {
            windows: windows.clone(),
        })
        .await;
    assert_eq!(harness.next_call().await, Call::UsageReported(windows));

    // And nothing was queued for the room: the next frame is the one the
    // output after it produces.
    harness
        .emit(SessionOutput::Event { event: delta("hi") })
        .await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness { event: delta("hi") }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn the_commands_a_harness_offers_travel_as_a_frame_rather_than_over_rest() {
    // The other way round from the model list beside it, and for a reason:
    // the command set carries the checkout's own skills, so it is a fact
    // about this session and there is no account row to file it against.
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    let commands = vec![flyco_core::HarnessCommand {
        name: "goal".to_owned(),
        description: "Set a goal — keep working until the condition is met".to_owned(),
        argument_hint: None,
    }];
    harness
        .emit(SessionOutput::Commands {
            commands: commands.clone(),
        })
        .await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Commands { commands }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn the_models_a_harness_offers_are_filed_over_rest_rather_than_sent_as_a_frame() {
    // The list is recorded against the *account*, which lives in D1, and a
    // session room is a Durable Object that cannot reach it. So this output
    // is the one that leaves the relay by the other door — and the control
    // plane announces it to the browsers itself.
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    let models = flyco_core::builtin_models(flyco_core::HarnessKind::Codex);
    harness
        .emit(SessionOutput::Models {
            models: models.clone(),
        })
        .await;
    assert_eq!(harness.next_call().await, Call::ModelsReported(models));

    // And nothing was queued for the room: the next frame is the one the
    // output after it produces.
    harness
        .emit(SessionOutput::Event { event: delta("hi") })
        .await;
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::Harness { event: delta("hi") }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn the_agent_is_told_what_machine_it_is_on_before_it_is_given_any_work() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::UserMessage {
        text: "port the build to arm64".to_owned(),
        origin: flyco_core::MessageOrigin::User,
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::ApprovalDecision {
        id: ApprovalId::generate(),
        decision: ApprovalDecision::Approved,
        payload: ApprovalPayload::MachineResizeLicenseBound {
            machine_type: "Standard_NC4as_T4_v3".to_owned(),
            minimum: flyco_core::machine::BillingMinimum::new(1, flyco_core::Usd::from_cents(60)),
            reason: "the build needs a GPU".to_owned(),
        },
    });

    // The session keeps working, which is what "not fatal" means here.
    harness.command(ControlToDaemon::Interrupt);
    assert_eq!(harness.next_call().await, Call::Interrupt);

    harness.archive().await.expect("the run ended cleanly");
}

/// The call the user approved died with the suspended machine (issue #355):
/// the agent is told, and its re-run is answered without a second ask.
#[tokio::test]
async fn an_approval_decided_after_a_suspension_reaches_the_agent_and_answers_the_rerun() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    let payload = ApprovalPayload::ToolUse {
        tool: "Bash".to_owned(),
        input: serde_json::json!({ "command": "cargo test -p flyco-core" }),
    };
    harness.command(ControlToDaemon::ApprovalDecision {
        id: ApprovalId::generate(),
        decision: ApprovalDecision::Approved,
        payload: payload.clone(),
    });
    let Call::UserMessage(notice) = harness.next_call().await else {
        panic!("the agent is told what the user decided");
    };
    assert!(
        notice.starts_with("[flyco approval notice]")
            && notice.contains("approved")
            && notice.contains("cargo test -p flyco-core"),
        "the notice names the decision and the call: {notice}"
    );

    // The agent runs it again: answered on the machine, raised to nobody.
    let native = ApprovalId::generate();
    harness
        .emit(SessionOutput::ApprovalRequest {
            id: native,
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "cargo test -p flyco-core" }),
            suggestions: None,
        })
        .await;
    assert_eq!(
        harness.next_call().await,
        Call::Approval {
            id: native,
            allowed: true
        }
    );
    assert!(
        harness.approvals.try_recv().is_err(),
        "an approved call is not raised over REST again"
    );

    // Once. The same call a second time is a new question for the user.
    harness
        .emit(SessionOutput::ApprovalRequest {
            id: ApprovalId::generate(),
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "cargo test -p flyco-core" }),
            suggestions: None,
        })
        .await;
    assert_eq!(harness.approvals.recv().await, Some(payload));

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_denial_decided_after_a_suspension_reaches_the_agent_and_preapproves_nothing() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    let payload = ApprovalPayload::ToolUse {
        tool: "Bash".to_owned(),
        input: serde_json::json!({ "command": "rm -rf target" }),
    };
    harness.command(ControlToDaemon::ApprovalDecision {
        id: ApprovalId::generate(),
        decision: ApprovalDecision::Denied,
        payload: payload.clone(),
    });
    let Call::UserMessage(notice) = harness.next_call().await else {
        panic!("the agent is told what the user decided");
    };
    assert!(
        notice.starts_with("[flyco approval notice]") && notice.contains("denied"),
        "{notice}"
    );

    harness
        .emit(SessionOutput::ApprovalRequest {
            id: ApprovalId::generate(),
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "rm -rf target" }),
            suggestions: None,
        })
        .await;
    assert_eq!(
        harness.approvals.recv().await,
        Some(payload),
        "a denied call that is asked for again is the user's to decide again"
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn the_agent_ready_stage_is_announced_once_and_not_on_every_reconnect() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;

    // The first attachment carries it; `handshake` asserts the frame and
    // its stage.
    harness.handshake().await;

    harness
        .room
        .directives
        .send(Directive::Close)
        .expect("the room is live");
    assert_eq!(harness.room.next().await, Some(Seen::StreamClosed));
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

#[tokio::test]
async fn a_superseded_attach_stops_the_daemon_rather_than_reconnecting() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    // A second flycod for this session attached — `postStart` firing
    // twice on one codespace, a `flycod run` started beside the unit's —
    // and the room bumped its epoch. The superseded stream is told why
    // before it ends, and the answer to losing is to stop: re-attaching
    // would end the winner's stream in turn, which is the ping-pong the
    // room ended this one over (issue #336).
    let spare = HttpControlApi::new(harness.room.base.clone(), harness.session, TOKEN.to_owned());
    spare.attach().await.expect("the spare's attach");
    assert!(
        matches!(harness.room.next().await, Some(Seen::Attached { .. })),
        "the room saw the spare attach"
    );
    assert_eq!(
        harness.room.next().await,
        Some(Seen::StreamClosed),
        "and the superseded stream ending"
    );

    // The run ends by itself: a superseded daemon stands down rather than
    // re-attaching into the epoch that replaced it.
    harness
        .ended()
        .await
        .expect("a superseded daemon stands down cleanly");
}

// ── The desktop ──

#[tokio::test]
async fn desktop_levels_reach_the_supervisor() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::SetComputerUse { enabled: true });
    assert_eq!(
        harness.desktop_calls.recv().await,
        Some(crate::desktop::DesktopCall::Enabled(true))
    );

    harness.command(ControlToDaemon::DesktopAudience { watching: true });
    assert_eq!(
        harness.desktop_calls.recv().await,
        Some(crate::desktop::DesktopCall::Watching(true))
    );

    harness.command(ControlToDaemon::DesktopInput {
        events: vec![flyco_core::wire::DesktopInputEvent::Move { x: 12, y: 34 }],
    });
    assert_eq!(
        harness.desktop_calls.recv().await,
        Some(crate::desktop::DesktopCall::Input(vec![
            flyco_core::wire::DesktopInputEvent::Move { x: 12, y: 34 }
        ]))
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_takeover_interrupts_the_turn() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    // The user's hands and the model's cannot both be on the screen:
    // claiming it is Stop by another name.
    harness.command(ControlToDaemon::DesktopTakeover { active: true });
    assert_eq!(harness.next_call().await, Call::Interrupt);
    assert_eq!(
        harness.desktop_calls.recv().await,
        Some(crate::desktop::DesktopCall::Takeover(true))
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn desktop_reports_reach_the_room_as_frames() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness
        .desktop_reports
        .send(crate::desktop::DesktopEvent::State {
            status: flyco_core::wire::DesktopStatus::Ready,
            detail: None,
        })
        .await
        .expect("the desktop channel is open");
    harness
        .desktop_reports
        .send(crate::desktop::DesktopEvent::Chunk {
            keyframe: true,
            bytes: vec![0xDE, 0xAD],
        })
        .await
        .expect("the desktop channel is open");

    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::DesktopState {
            status: flyco_core::wire::DesktopStatus::Ready,
            detail: None,
        }
    );
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::DesktopChunk {
            keyframe: true,
            data: vec![0xDE, 0xAD],
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

// ── Reconnection ──

/// A silence limit short enough to watch in a test: the fake room pings
/// every 30ms, so 150ms of byte-level quiet is several missed heartbeats
/// to a relay built this way, which is what makes a dead path show up
/// inside a test's patience.
fn brisk() -> wire::Deadlines {
    wire::Deadlines::silent(Duration::from_millis(150))
}

#[tokio::test]
async fn a_harness_that_stops_answering_ends_the_session_instead_of_the_relay() {
    // The pump awaits the harness inside its own loop, so a command that
    // never returns takes the stream read and the acks down with it:
    // the daemon stops answering, stops reconnecting, and stops being able
    // to say why (issue #201). It must give up on the harness instead.
    let deadlines = brisk().waiting_on_the_harness(Duration::from_millis(150));
    let mut harness = Harness::wedged(deadlines).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::UserMessage {
        text: "anyone there?".to_owned(),
        origin: flyco_core::MessageOrigin::User,
    });

    let ended = tokio::time::timeout(Duration::from_secs(5), harness.run)
        .await
        .expect("a wedged harness must not wedge the relay")
        .expect("the run did not panic");
    let Err(WireError::Harness(reason)) = ended else {
        panic!("a harness that never answers is a failure, not a clean end: {ended:?}");
    };
    // Named, so the session says which command went unanswered — and the
    // user's own words are not repeated into the reason.
    assert!(
        reason.contains("user_message") && !reason.contains("anyone there?"),
        "the reason names the command without quoting it: {reason}"
    );
}

#[tokio::test]
async fn an_idle_stream_is_kept_alive_by_the_rooms_heartbeat() {
    let mut harness = Harness::with_deadlines(AttachAnswer::Accept, brisk()).await;
    harness.handshake().await;

    // Nothing has happened in the session at all, for many times the
    // silence limit. The stream still has to be the same one: the room's
    // own heartbeat comments are what keep an idle flow alive across the
    // NATs that reclaim one.
    tokio::time::sleep(Duration::from_millis(400)).await;
    harness.command(ControlToDaemon::UserMessage {
        text: "still there?".to_owned(),
        origin: flyco_core::MessageOrigin::User,
    });
    let Call::UserMessage(text) = harness.next_call().await else {
        panic!("a stream kept alive still delivers the next command");
    };
    assert!(text.ends_with("still there?"), "{text}");

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_silenced_stream_is_abandoned_and_the_daemon_returns() {
    let mut harness = Harness::with_deadlines(AttachAnswer::Accept, brisk()).await;
    harness.handshake().await;

    // A flow a NAT dropped without a FIN looks exactly like this: the
    // stream still stands, writes still appear to succeed, and no bytes —
    // not even the room's ping — ever arrive. The daemon must not
    // read it forever.
    harness
        .room
        .directives
        .send(Directive::Silence)
        .expect("the room is live");

    loop {
        match harness.room.next().await.expect("the daemon went quiet") {
            Seen::StreamClosed => break,
            Seen::Batch { .. } => panic!("an idle daemon posted a batch"),
            Seen::Attached { .. } | Seen::StreamOpened(_) => {}
        }
    }

    // And it dials again rather than sitting on a dead session.
    harness.handshake().await;
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
async fn a_dropped_stream_is_reconnected_under_a_fresh_epoch() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness
        .room
        .directives
        .send(Directive::Close)
        .expect("the room is live");
    assert_eq!(harness.room.next().await, Some(Seen::StreamClosed));

    // The daemon comes back: a new attach, a new epoch, a new stream —
    // a room that restarted has forgotten the last one.
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    // Gate the next attach, then close the stream: the daemon's notice of
    // the dead stream is its next attach, and the gate holds it there —
    // parked, provably detached — while the frames below are produced.
    harness
        .room
        .directives
        .send(Directive::GateAttaches)
        .expect("the room is live");
    harness
        .room
        .directives
        .send(Directive::Close)
        .expect("the room is live");
    assert_eq!(harness.room.next().await, Some(Seen::StreamClosed));

    // The parked attach is the proof the daemon has noticed and has
    // nowhere to flush to. Anything produced now can only wait.
    let Some(Seen::Attached { .. }) = harness.room.next().await else {
        panic!("a dead stream did not bring the daemon back to attach");
    };
    harness
        .emit(SessionOutput::Event { event: delta("a") })
        .await;
    harness
        .emit(SessionOutput::Event { event: delta("b") })
        .await;

    harness
        .room
        .directives
        .send(Directive::ReleaseAttaches)
        .expect("the room is live");
    assert!(
        matches!(harness.room.next().await, Some(Seen::StreamOpened(_))),
        "the released attach opens its command stream"
    );
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

    let (fake, _calls) = FakeSession::new();
    let recorder = fake.recorder();
    let (outputs, receiver) = mpsc::channel(1);
    let (approvals, _) = mpsc::unbounded_channel();
    let (observations, _) = mpsc::unbounded_channel();
    let (notifications, _) = mpsc::unbounded_channel();
    let api = RecordingApi {
        transport: HttpControlApi::new(base, session, TOKEN.to_owned()),
        approvals,
        observations,
        notifications,
        calls: recorder.clone(),
        id: ApprovalId::generate(),
    };
    let (terminal, _, _, terminal_out) = FakeTerminal::pair();
    let (desktop, _, _, desktop_out) = FakeDesktop::pair();
    let (shell, _shell_runs) = FakeShell::pair();
    let (repos, _) = FakeRepos::pair(&[None]);
    let stops = crate::stop::nothing_to_watch();
    let run = tokio::spawn(wire::run(SessionRelay {
        session_id: session,
        deadlines: wire::Deadlines::default(),
        session: fake,
        outputs: receiver,
        api,
        terminal,
        terminal_out,
        desktop,
        desktop_out,
        tui: crate::tui::HarnessTui::fixture(),
        shell,
        repos,
        workspace: Workspace::new(std::env::temp_dir(), None),
        github: None,
        disk: FakeDisk::new(recorder),
        spot: crate::spot::nothing_to_watch(),
        stops,
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
        origin: flyco_core::MessageOrigin::User,
    });
    harness.command(ControlToDaemon::TerminalInput {
        data: "ls\n".to_owned(),
    });
    // …but the session is still reachable, so an archive still lands.
    assert_eq!(harness.archive().await.map_err(|e| e.to_string()), Ok(()));
}

#[tokio::test]
async fn a_budget_threshold_below_the_pause_is_told_to_the_agent() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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

#[tokio::test]
async fn a_raised_budget_lifts_the_pause_and_tells_the_agent_to_carry_on() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::Budget {
        signal: BudgetSignal::Pause,
    });
    assert_eq!(harness.next_call().await, Call::Interrupt);

    harness.command(ControlToDaemon::BudgetRaised {
        limit: Usd::from_dollars(25),
    });
    let Call::UserMessage(text) = harness.next_call().await else {
        panic!("lifting a pause must reach the agent as a message");
    };
    assert!(
        text.starts_with("[flyco budget notice]"),
        "the agent must be able to tell this from the user: {text}"
    );
    assert!(text.contains("$25.00"), "the new limit is the news: {text}");

    // And the session accepts work again, which is the whole point. The
    // machine notice still rides in front of the first user message: the
    // pause never let one through, so this is still the first.
    harness.command(ControlToDaemon::UserMessage {
        text: "keep going".to_owned(),
        origin: flyco_core::MessageOrigin::User,
    });
    let Call::UserMessage(resumed) = harness.next_call().await else {
        panic!("a released session must accept a user message");
    };
    assert!(resumed.ends_with("keep going"), "{resumed}");

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_budget_raised_on_a_session_that_never_paused_says_nothing() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    // Topping a budget up early is not news the agent has to read: there
    // was no pause to lift, and `budget_status` answers whenever it wants
    // the number.
    harness.command(ControlToDaemon::BudgetRaised {
        limit: Usd::from_dollars(25),
    });
    harness.command(ControlToDaemon::UserMessage {
        text: "carry on".to_owned(),
        origin: flyco_core::MessageOrigin::User,
    });
    let Call::UserMessage(first) = harness.next_call().await else {
        panic!("the user's message must be the first thing the harness hears");
    };
    assert!(
        first.ends_with("carry on") && !first.contains("[flyco budget notice]"),
        "the raise must not have put a message in front of the user's: {first}"
    );

    harness.archive().await.expect("the run ended cleanly");
}

// ── Archival ──

#[tokio::test]
async fn archiving_shuts_the_harness_down_and_ends_the_run() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
        origin: flyco_core::MessageOrigin::User,
    });
    harness.command(ControlToDaemon::Compact);
    // The stream stays open, so the daemon is still there to answer — and
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
        TerminalCall::Write("ls\n".to_owned())
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_stopping_container_flushes_before_it_writes_the_patch_and_reports_last() {
    // The order is the feature. The transcript is flushed while the harness
    // still answers; the working tree is written out only once nothing is
    // still changing it; and the control plane is told last, because
    // "this machine is stopping" must not be true before the user's
    // uncommitted work has left it.
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    assert_eq!(
        harness.next_call().await,
        Call::HarnessSessionRecorded("harness-native-thread".to_owned())
    );

    harness.stop().await;

    assert_eq!(harness.next_call().await, Call::Interrupt);
    assert_eq!(harness.next_call().await, Call::Flush);
    assert_eq!(
        harness.next_call().await,
        Call::HarnessSessionRecorded("harness-native-thread".to_owned()),
        "the id the next execution resumes on is re-filed and awaited"
    );
    assert_eq!(
        harness.next_call().await,
        // The stored object is the snapshot's envelope: the base-commit
        // line (40 hex + newline), then the patch.
        Call::WorkdirPatchStored(None, WORKDIR_PATCH.len() + 41),
        "the working tree leaves the machine, because nothing here survives the stop"
    );
    assert_eq!(
        harness.next_call().await,
        Call::StoppingReported(StopReason::Sigterm),
        "the control plane is told last, once the work is safe"
    );

    // No `sync`: there is no disk for a page cache to be flushed onto, and
    // spending a second of the grace period on one would be a second not
    // spent getting the patch off the machine.
    assert_eq!(harness.next_call().await, Call::Shutdown);
    harness.ended().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_stop_ends_the_run_rather_than_holding_the_stream() {
    // The container counterpart of a reclamation's held stream. A machine
    // being reclaimed keeps its connection because it is about to be killed
    // anyway and a clean disconnect would read as "coming back"; a stopping
    // container has been *asked* to exit, and exiting 0 is what stops the
    // platform recording the execution as failed.
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.stop().await;
    assert_eq!(harness.next_call().await, Call::Interrupt);
    assert_eq!(harness.next_call().await, Call::Flush);
    assert_eq!(
        harness.next_call().await,
        Call::WorkdirPatchStored(None, WORKDIR_PATCH.len() + 41)
    );
    assert_eq!(
        harness.next_call().await,
        Call::StoppingReported(StopReason::Sigterm)
    );
    assert_eq!(harness.next_call().await, Call::Shutdown);

    harness.ended().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_reclamation_pushes_what_the_room_has_not_seen_yet() {
    // The room's stored tail is what a browser replays, and the frames
    // still in the relay's queue when the notice arrives are the last
    // minute of the session. They go out before the notice does.
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
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
                window: UsageWindow::new(Some(300), None, 100, Some(1_800_007_200)),
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;
    // A session on inherited developer credentials has no linked account,
    // so the control plane refuses every observation it posts. The turn
    // still has to reach the room.
    harness.refuse_observations();

    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::UsageLimited {
                window: UsageWindow::new(None, None, 100, None),
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
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness
        .repo_inject
        .send((None, " M src/lib.rs\n".to_owned()))
        .expect("the watcher is live");
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::RepoDirty {
            dir: None,
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

/// Which checkout went dirty is part of the report: a session working
/// across two repositories owes the room — and the refusal a dirty tree
/// causes — the name of the tree the work is in.
#[tokio::test]
async fn a_dirty_report_names_the_checkout_it_is_about() {
    let mut harness = Harness::with_repos(
        AttachAnswer::Accept,
        FakeRepos::pair(&[Some("flyco".to_owned()), Some("api".to_owned())]),
    )
    .await;
    harness.handshake().await;

    harness
        .repo_inject
        .send((Some("api".to_owned()), " M src/lib.rs\n".to_owned()))
        .expect("the watcher is live");
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::RepoDirty {
            dir: Some("api".to_owned()),
            summary: " M src/lib.rs\n".to_owned(),
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

/// An `AddRepo` the user approved clones the checkout, tells the room it
/// arrived, and tells the agent where it landed — in that order, so the
/// transcript never shows a notice about a repository the control plane
/// was not told about.
#[tokio::test]
async fn an_approved_repository_is_cloned_announced_and_named_to_the_agent() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::AddRepo {
        slug: "lexoliu/api".parse().expect("a valid slug"),
        branch: "main".parse().expect("a valid branch"),
        dir: "api".to_owned(),
    });

    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::RepoAdded {
            slug: "lexoliu/api".parse().expect("a valid slug"),
            branch: "main".parse().expect("a valid branch"),
            dir: "api".to_owned(),
        }
    );
    assert!(matches!(
        harness.next_call().await,
        Call::UserMessage(text)
            if text.starts_with("[flyco repo notice]")
                && text.contains("lexoliu/api")
                && text.contains("`api/`")
    ));

    harness.archive().await.expect("the run ended cleanly");
}

/// The command is redelivered until applied, so a replay must not announce
/// a second time: a directory the set already holds is adopted silently,
/// and the proof is that a second command's announcement is the first the
/// room sees.
#[tokio::test]
async fn a_replayed_add_repo_announces_nothing() {
    let mut harness = Harness::with_repos(
        AttachAnswer::Accept,
        FakeRepos::pair(&[Some("api".to_owned())]),
    )
    .await;
    harness.handshake().await;

    // `api` is already checked out — this is the replayed command — while
    // `web` is genuinely new, so its frame proves where the stream stood.
    harness.command(ControlToDaemon::AddRepo {
        slug: "lexoliu/api".parse().expect("a valid slug"),
        branch: "main".parse().expect("a valid branch"),
        dir: "api".to_owned(),
    });
    harness.command(ControlToDaemon::AddRepo {
        slug: "lexoliu/web".parse().expect("a valid slug"),
        branch: "main".parse().expect("a valid branch"),
        dir: "web".to_owned(),
    });

    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::RepoAdded {
            slug: "lexoliu/web".parse().expect("a valid slug"),
            branch: "main".parse().expect("a valid branch"),
            dir: "web".to_owned(),
        },
        "the replay produced no frame; the first announcement is `web`'s"
    );
    assert!(matches!(
        harness.next_call().await,
        Call::UserMessage(text) if text.contains("lexoliu/web")
    ));

    harness.archive().await.expect("the run ended cleanly");
}

/// A clone that fails tells the agent the repository did not arrive —
/// it asked for one and deserves the answer — without ending a session
/// that is otherwise healthy.
#[tokio::test]
async fn a_clone_that_fails_tells_the_agent_and_leaves_the_session_running() {
    let (repos, inject) = FakeRepos::pair(&[Some("flyco".to_owned())]);
    let mut harness =
        Harness::with_repos(AttachAnswer::Accept, (repos.unclonable(&["gone"]), inject)).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::AddRepo {
        slug: "lexoliu/gone".parse().expect("a valid slug"),
        branch: "main".parse().expect("a valid branch"),
        dir: "gone".to_owned(),
    });

    assert!(matches!(
        harness.next_call().await,
        Call::UserMessage(text)
            if text.starts_with("[flyco repo notice]")
                && text.contains("could not clone")
                && text.contains("lexoliu/gone")
    ));

    // The relay is alive: a command sent next is still answered.
    harness.command(ControlToDaemon::AddRepo {
        slug: "lexoliu/web".parse().expect("a valid slug"),
        branch: "main".parse().expect("a valid branch"),
        dir: "web".to_owned(),
    });
    assert_eq!(
        harness.room.next_frame().await,
        DaemonToControl::RepoAdded {
            slug: "lexoliu/web".parse().expect("a valid slug"),
            branch: "main".parse().expect("a valid branch"),
            dir: "web".to_owned(),
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

/// Every checkout's work leaves a machine that is stopping, under its own
/// name — the patch a resume applies is per checkout, so storing only the
/// primary's would lose the rest.
#[tokio::test]
async fn a_stop_stores_each_checkouts_patch_under_its_own_dir() {
    let mut harness = Harness::with_repos(
        AttachAnswer::Accept,
        FakeRepos::with_snapshots([
            (Some("flyco".to_owned()), Some(WORKDIR_PATCH.to_vec())),
            (Some("api".to_owned()), Some(WORKDIR_PATCH.to_vec())),
        ]),
    )
    .await;
    harness.handshake().await;

    harness.stop().await;
    assert_eq!(harness.next_call().await, Call::Interrupt);
    assert_eq!(harness.next_call().await, Call::Flush);
    for dir in ["api", "flyco"] {
        assert_eq!(
            harness.next_call().await,
            Call::WorkdirPatchStored(Some(dir.to_owned()), WORKDIR_PATCH.len() + 41),
            "the `{dir}` checkout's patch is stored under `{dir}`"
        );
    }
    assert_eq!(
        harness.next_call().await,
        Call::StoppingReported(StopReason::Sigterm)
    );
    assert_eq!(harness.next_call().await, Call::Shutdown);
    harness.ended().await.expect("the run ended cleanly");
}

/// A limit the harness can place in time is filed with the control plane,
/// which is what pauses the session and brings it back (issue #244).
///
/// The daemon does *not* continue the conversation itself any more. It cannot:
/// the control plane releases the machine for a wait that can be days long, so
/// nothing on the machine is running when the window turns over, and the
/// continuation is the control plane's to send — with whatever the user typed
/// into the composer while it waited, which the daemon has never seen.
#[tokio::test]
async fn a_usage_limit_with_a_reset_time_is_filed_with_the_control_plane() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    let window = UsageWindow::new(Some(300), None, 100, Some(1_800_007_200));
    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::UsageLimited {
                window: window.clone(),
            },
        })
        .await;

    assert_eq!(harness.next_call().await, Call::UsageLimitReported(window));
    assert!(matches!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: HarnessEvent::UsageLimited { .. }
        }
    ));

    harness.archive().await.expect("the run ended cleanly");
}

/// A limit with no reset is announced and nothing more.
///
/// Nothing can be scheduled around it — the control plane refuses such a
/// report outright — so the daemon does not make a call it knows will be
/// refused. Claude Code's `api_retry` half of the signal is exactly this
/// shape, and it arrives before the frame that names the window.
#[tokio::test]
async fn a_usage_limit_that_names_no_reset_is_not_filed() {
    let mut harness = Harness::start(AttachAnswer::Accept).await;
    harness.handshake().await;

    harness
        .emit(SessionOutput::Event {
            event: HarnessEvent::UsageLimited {
                window: UsageWindow::new(None, None, 100, None),
            },
        })
        .await;
    assert!(matches!(
        harness.room.next_frame().await,
        DaemonToControl::Harness {
            event: HarnessEvent::UsageLimited { .. }
        }
    ));

    // A message after it proves nothing was queued for the harness in
    // between: the next thing the fake session is told is that message — with
    // the opening machine notice ahead of it, which is the first user message
    // of every session — and not a canned continuation.
    harness
        .room
        .directives
        .send(Directive::Send(ControlToDaemon::UserMessage {
            text: "carry on".to_owned(),
            origin: flyco_core::MessageOrigin::User,
        }))
        .expect("the room is live");
    let Call::UserMessage(said) = harness.next_call().await else {
        panic!("a user message must reach the harness as one");
    };
    assert!(said.starts_with("[flyco machine notice]"));
    assert!(said.ends_with("carry on"));

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

    /// A checkout's patch travels under its directory: `?repo=<dir>` on the
    /// same route the workspace root's patch uses bare, so the store keeps
    /// one checkout's work apart from another's.
    #[tokio::test]
    async fn a_workdir_patch_is_addressed_to_its_checkout() {
        let session = SessionId::generate();
        let mut plane = ControlPlane::start(vec![
            Reply::no_content(),
            Reply::no_content(),
            Reply::text("the patch"),
            Reply::no_content(),
        ])
        .await;
        let api = api(&plane.base, session);

        api.put_workdir_patch(Some("api"), b"patch-bytes".to_vec())
            .await
            .expect("store a checkout's patch");
        api.put_workdir_patch(None, b"patch-bytes".to_vec())
            .await
            .expect("store the root checkout's patch");
        api.get_workdir_patch(Some("api"))
            .await
            .expect("read a checkout's patch back");
        api.get_workdir_patch(None)
            .await
            .expect("read the root checkout's patch back");

        for (method, target) in [
            (
                "PUT",
                format!("/v1/sessions/{session}/workdir-patch?repo=api"),
            ),
            ("PUT", format!("/v1/sessions/{session}/workdir-patch")),
            (
                "GET",
                format!("/v1/sessions/{session}/workdir-patch?repo=api"),
            ),
            ("GET", format!("/v1/sessions/{session}/workdir-patch")),
        ] {
            let request = plane.next().await.expect("the control plane was called");
            assert_eq!(request.method, method);
            assert_eq!(request.target, target);
        }
    }
}

// ── The remote transcript store ──

mod remote_store {
    use super::{RemoteTranscriptStore, SessionKey, StoreError, TranscriptStore, stream_key};
    use crate::control::rest::{ApprovalRaiser, ControlApi, ControlApiError, TranscriptRead};
    use flyco_core::wire::ApprovalPayload;
    use flyco_core::{
        ApprovalId, HarnessObservation, HarnessSessionView, ModelOption, StopReason, UsageWindow,
    };
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
        /// Refuse every batch with this status, the way a control plane
        /// that has run out of room, or of patience, does.
        refuse_puts: Option<u16>,
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

        fn report_startup_failure(
            &self,
            _message: String,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store reports no startup failures".to_owned(),
            )))
        }

        fn report_spot_notice(
            &self,
            _seconds_remaining: u32,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store reports no spot notices".to_owned(),
            )))
        }

        fn report_stopping(
            &self,
            _reason: StopReason,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store reports no stops".to_owned(),
            )))
        }

        fn harness_session(
            &self,
        ) -> impl core::future::Future<Output = Result<HarnessSessionView, ControlApiError>> + Send
        {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store answers no harness-session reads".to_owned(),
            )))
        }

        fn report_models(
            &self,
            _models: &[ModelOption],
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store reports no model lists".to_owned(),
            )))
        }

        fn report_usage(
            &self,
            _windows: &[UsageWindow],
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store reports no usage snapshots".to_owned(),
            )))
        }

        fn report_usage_limit(
            &self,
            _window: &UsageWindow,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store reports no usage limits".to_owned(),
            )))
        }

        fn put_transcript_batch(
            &self,
            stream: &str,
            seq: u64,
            body: Vec<u8>,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            if let Some(status) = self.refuse_puts {
                return core::future::ready(Err(ControlApiError::Refused {
                    method: "PUT",
                    path: format!("/v1/sessions/s/transcript/{stream}/{seq}"),
                    status,
                    title: "Payload Too Large".to_owned(),
                    detail: "the batch is over the 1 MiB a transcript batch may be".to_owned(),
                    kind: "payload-too-large".to_owned(),
                    retry_after_secs: None,
                }));
            }
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
            _dir: Option<&str>,
            _patch: Vec<u8>,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store stores no workdir patches".to_owned(),
            )))
        }

        fn get_workdir_patch(
            &self,
            _dir: Option<&str>,
        ) -> impl core::future::Future<Output = Result<Option<Vec<u8>>, ControlApiError>> + Send
        {
            core::future::ready(Ok(None))
        }

        fn get_handoff(
            &self,
        ) -> impl core::future::Future<
            Output = Result<Option<flyco_core::HandoffView>, ControlApiError>,
        > + Send {
            core::future::ready(Ok(None))
        }

        fn get_handoff_transcript(
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
                refuse_puts: None,
            }),
            received,
        )
    }

    #[tokio::test]
    async fn a_refused_batch_is_reported_with_the_control_planes_own_answer() {
        // Issue #231: the session failed with `transcript store I/O failed
        // at <control plane>`, and nothing about which request or why.
        let (wrote, _received) = channel();
        let mut store = RemoteTranscriptStore::new(Held {
            body: Vec::new(),
            batches: 0,
            wrote,
            refuse_puts: Some(413),
        });

        let error = store
            .append(&key(None), vec![json!({ "uuid": "a" })])
            .await
            .expect_err("a refused batch is an error");

        assert!(matches!(error, StoreError::ControlPlane { .. }));
        let described = crate::harness::describe(&error);
        assert_eq!(
            described,
            "the control plane could not keep the transcript: the control plane refused PUT \
             /v1/sessions/s/transcript/flyco-work.9d0f4b1a/0: 413 Payload Too Large — the \
             batch is over the 1 MiB a transcript batch may be"
        );
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
