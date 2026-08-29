//! The daemon's end of the relay, driven against a real loopback room.

use core::time::Duration;

use flyco_core::wire::ApprovalPayload;
use flyco_core::{
    ApprovalDecision, ApprovalId, BudgetSignal, ControlToDaemon, DaemonToControl, HarnessEvent,
    HarnessObservation, SessionId, UsageReport, Usd, WIRE_PROTOCOL_VERSION,
};
use tokio::sync::mpsc;

use crate::control::rest::{ControlApi, ControlApiError, HttpControlApi, TranscriptRead};
use crate::control::store::{RemoteTranscriptStore, stream_key};
use crate::control::wire::{self, Endpoint, QUEUE_DEPTH, WireError};
use crate::harness::SessionOutput;
use crate::harness::claude::protocol::SessionKey;
use crate::harness::claude::store::{StoreError, TranscriptStore};
use crate::testing::{Call, ControlPlane, Directive, FakeSession, Greeting, Reply, Room, Seen};

/// A daemon token shaped the way the control plane mints them.
const TOKEN: &str = "fd_a-daemon-token";

/// A [`ControlApi`] that answers without a network.
///
/// The relay tests care about *ordering* — that an approval is durable
/// before it is announced — not about HTTP, which
/// [`the REST tests`](rest_client) cover against a real server.
#[derive(Debug, Clone)]
struct RecordingApi {
    approvals: mpsc::UnboundedSender<ApprovalPayload>,
    observations: mpsc::UnboundedSender<HarnessObservation>,
    id: ApprovalId,
}

impl ControlApi for RecordingApi {
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
}

/// Everything a relay test drives.
struct Harness {
    room: Room,
    session: SessionId,
    outputs: mpsc::Sender<SessionOutput>,
    calls: mpsc::UnboundedReceiver<Call>,
    approvals: mpsc::UnboundedReceiver<ApprovalPayload>,
    observations: mpsc::UnboundedReceiver<HarnessObservation>,
    approval_id: ApprovalId,
    run: tokio::task::JoinHandle<Result<(), WireError>>,
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
        let (sender, receiver) = mpsc::channel(outputs);
        let (approval_sender, approvals) = mpsc::unbounded_channel();
        let (observation_sender, observations) = mpsc::unbounded_channel();
        let approval_id = ApprovalId::generate();
        let api = RecordingApi {
            approvals: approval_sender,
            observations: observation_sender,
            id: approval_id,
        };

        let run = tokio::spawn(wire::run(endpoint, fake, receiver, api));

        Self {
            room,
            session,
            outputs: sender,
            calls,
            approvals,
            observations,
            approval_id,
            run,
        }
    }

    /// Waits for the room to see this daemon's `Hello`.
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

    /// Makes every further observation fail, the way a session with no
    /// linked harness account meets a `404` on each one.
    fn refuse_observations(&mut self) {
        self.observations.close();
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
        self.command(ControlToDaemon::Archive);
        assert_eq!(self.next_call().await, Call::Shutdown);
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

    let id = ApprovalId::generate();
    harness.command(ControlToDaemon::ApprovalDecision {
        id,
        decision: ApprovalDecision::Approved,
    });
    assert_eq!(
        harness.next_call().await,
        Call::Approval { id, allowed: true }
    );

    let denied = ApprovalId::generate();
    harness.command(ControlToDaemon::ApprovalDecision {
        id: denied,
        decision: ApprovalDecision::Denied,
    });
    assert_eq!(
        harness.next_call().await,
        Call::Approval {
            id: denied,
            allowed: false,
        }
    );

    harness.archive().await.expect("the run ended cleanly");
}

#[tokio::test]
async fn a_user_message_and_an_interrupt_reach_the_harness() {
    let mut harness = Harness::start(Greeting::Welcome).await;
    harness.handshake().await;

    harness.command(ControlToDaemon::UserMessage {
        text: "what does this crate do?".to_owned(),
    });
    assert_eq!(
        harness.next_call().await,
        Call::UserMessage("what does this crate do?".to_owned())
    );

    harness.command(ControlToDaemon::Interrupt);
    assert_eq!(harness.next_call().await, Call::Interrupt);

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
    let (outputs, receiver) = mpsc::channel(1);
    let (approvals, _) = mpsc::unbounded_channel();
    let (observations, _) = mpsc::unbounded_channel();
    let api = RecordingApi {
        approvals,
        observations,
        id: ApprovalId::generate(),
    };
    let run = tokio::spawn(wire::run(endpoint, fake, receiver, api));

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

// ── Usage observations ──

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

// ── The REST client, against a real HTTP server ──

mod rest_client {
    use super::{ControlApi, ControlPlane, HttpControlApi, Reply, TOKEN};
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
    use crate::control::rest::{ControlApi, ControlApiError, TranscriptRead};
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

    impl ControlApi for Held {
        fn raise_approval(
            &self,
            _payload: ApprovalPayload,
        ) -> impl core::future::Future<Output = Result<ApprovalId, ControlApiError>> + Send
        {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store never raises approvals".to_owned(),
            )))
        }

        fn record_observation(
            &self,
            _observation: HarnessObservation,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            core::future::ready(Err(ControlApiError::Transport(
                "the transcript store records no observations".to_owned(),
            )))
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
