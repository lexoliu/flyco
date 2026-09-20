//! The session room, driven the way Cloudflare drives it.
//!
//! The room is exercised through the real `fetch` surface — the REST calls
//! the Worker makes and the three routes a daemon's relay is built on —
//! against skyzen's SQLite-backed [`InMemoryDurableDb`]. What these tests
//! observe is what the room *did*: the events a call answered with, which
//! are what the Worker publishes to the session owner's stream, and the
//! commands it handed down the daemon's SSE stream.

use std::time::Duration;

use flyco_core::{
    ApprovalId, ClientEvent, ControlToDaemon, DaemonToControl, HarnessEvent, SessionId,
    ShellOutcome, ShellRunId, ShellStream, WIRE_PROTOCOL_VERSION,
    wire::{ApprovalPayload, DaemonAttach, DaemonCommand, DaemonFrames},
};
use futures_util::StreamExt as _;
use skyzen::durable::DurableObject as _;
use skyzen::http_kit::sse::SseStream;
use skyzen::{Body, Method, Request};
use skyzen_services::durable::{DurableDb, DurableKv};
use skyzen_test::mock::{InMemoryDurableDb, InMemoryDurableKv};

use crate::error::ApiError;
use crate::room::{
    AttachResponse, Emitted, EmittedEvent, SessionRoom, WORKDIR_REPLY_EVENT, WORKDIR_TIMEOUT_EVENT,
};
use crate::rooms::{NativeRooms, NativeUserStreams, Rooms};
use crate::testing::room_request;
use flyco_core::wire::EventPage;

/// How long a test waits for the room to hand a command down the stream.
///
/// The stream's feed polls storage every 150ms; an event a call queued is
/// a couple of ticks away at most, so a wait this long expiring is a
/// failure, not a race.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a quiet stream is watched before a test accepts that nothing
/// is coming. Two full feed ticks plus margin.
const QUIET: Duration = Duration::from_millis(400);

// ── The harness ──

/// A room, its storage, and the state of the daemon the test is playing.
struct Room {
    session: SessionId,
    object: SessionRoom,
    db: InMemoryDurableDb,
    kv: InMemoryDurableKv,
    /// The attach the test's daemon is working under, when it has one.
    epoch: Option<u64>,
    /// The daemon's outbound frame number the next batch starts at.
    out_seq: u64,
    /// The highest command sequence the daemon has applied, which every
    /// batch acknowledges back.
    applied: u64,
    /// The open command stream, when one is.
    commands: Option<SseStream>,
}

impl Room {
    async fn open() -> Self {
        Self {
            session: SessionId::generate(),
            object: SessionRoom::default(),
            db: InMemoryDurableDb::in_memory()
                .await
                .expect("an in-memory database"),
            kv: InMemoryDurableKv::new(),
            epoch: None,
            out_seq: 1,
            applied: 0,
            commands: None,
        }
    }

    /// Builds one request the way the Worker would send it.
    fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Request {
        let mut request = room_request(self.session, method, path, body);

        // The simulator injects exactly these before dispatching; doing the
        // same here keeps this an integration test of `fetch`, not of a
        // hand-rolled router.
        request
            .extensions_mut()
            .insert(DurableDb::new(self.db.clone()));
        request
            .extensions_mut()
            .insert(DurableKv::new(self.kv.clone()));
        request
    }

    /// Calls one of the room's HTTP routes, the way the Worker does.
    async fn call(&mut self, method: Method, path: &str, body: Option<Vec<u8>>) -> (u16, Vec<u8>) {
        let response = self
            .object
            .fetch()
            .go(self.request(method, path, body))
            .await
            .expect("the room answered");
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .into_bytes()
            .await
            .expect("a readable body");
        (status, bytes.to_vec())
    }

    /// Calls a route and keeps the response, for a body that is a stream.
    async fn call_streaming(&mut self, path: &str) -> skyzen::Response {
        self.object
            .fetch()
            .go(self.request(Method::GET, path, None))
            .await
            .expect("the room answered")
    }

    /// Posts to a route and keeps the response, for a body that is a
    /// stream — the held-open answer of a workdir question.
    async fn post_streaming(&mut self, path: &str, body: Option<Vec<u8>>) -> skyzen::Response {
        self.object
            .fetch()
            .go(self.request(Method::POST, path, body))
            .await
            .expect("the room answered")
    }

    /// Attaches the test's daemon and answers the events it produced.
    async fn attach(&mut self) -> AttachResponse {
        let (status, body) = self
            .call(
                Method::POST,
                "/internal/daemon-attach",
                Some(
                    serde_json::to_vec(&DaemonAttach {
                        protocol_version: WIRE_PROTOCOL_VERSION,
                    })
                    .expect("serialize"),
                ),
            )
            .await;
        assert_eq!(status, 200, "an attach: {}", String::from_utf8_lossy(&body));
        let attached: AttachResponse = serde_json::from_slice(&body).expect("an attach response");
        self.epoch = Some(attached.epoch);
        self.out_seq = 1;
        attached
    }

    /// Opens the command stream for the current attach.
    async fn open_commands(&mut self) {
        let epoch = self.epoch.expect("attach first");
        let response = self
            .call_streaming(&format!("/internal/commands?epoch={epoch}"))
            .await;
        assert_eq!(response.status().as_u16(), 200, "the stream opened");
        self.commands = Some(response.into_body().into_sse());
    }

    /// Attaches and opens the stream: a daemon arriving on an empty room.
    async fn greet(&mut self) {
        let attached = self.attach().await;
        assert_eq!(
            attached.events,
            vec![connected()],
            "an attach announces the machine to the session's owner"
        );
        self.open_commands().await;
    }

    /// Reads the next command the room hands the daemon.
    ///
    /// Reading a command is applying it: the harness acknowledges it on
    /// the daemon's next frames POST, which is what retires the row.
    async fn next_command(&mut self) -> DaemonCommand {
        let stream = self.commands.as_mut().expect("a command stream is open");
        let item = tokio_select_quiet(stream, PATIENCE)
            .await
            .expect("a command arrived in time")
            .expect("the stream is still open")
            .expect("a decodable SSE frame");
        assert_eq!(
            item.event(),
            Some("command"),
            "the stream carries only commands"
        );
        let command: DaemonCommand = item.data().expect("a command envelope");
        if let Some(seq) = command.seq {
            self.applied = self.applied.max(seq);
        }
        command
    }

    /// Asserts the stream hands nothing over for a whole quiet window.
    async fn expect_quiet(&mut self) {
        let stream = self.commands.as_mut().expect("a command stream is open");
        if let Some(item) = tokio_select_quiet(stream, QUIET).await {
            panic!("the stream stayed quiet, then produced {item:?}");
        }
    }

    /// Asserts the stream ended — the attach it served was superseded.
    async fn expect_end(&mut self) {
        let stream = self.commands.as_mut().expect("a command stream is open");
        match tokio_select_quiet(stream, PATIENCE).await {
            None => panic!("the stream outlived its attach"),
            Some(None) => {}
            Some(Some(item)) => panic!("the stream ended, then produced {item:?}"),
        }
    }

    /// Posts one daemon frame, the way a daemon's outbound flush does.
    ///
    /// The batch acknowledges every command the harness has read so far —
    /// `applied` — which is what retires them from the room's log.
    async fn deliver(&mut self, frame: &DaemonToControl) -> Emitted {
        self.deliver_batch(std::slice::from_ref(frame)).await
    }

    /// Posts one batch of daemon frames.
    async fn deliver_batch(&mut self, frames: &[DaemonToControl]) -> Emitted {
        let epoch = self.epoch.expect("attach first");
        let batch = DaemonFrames {
            epoch,
            from_seq: self.out_seq,
            ack_through: self.applied,
            frames: frames.to_vec(),
        };
        let (status, body) = self
            .call(
                Method::POST,
                "/internal/frames",
                Some(serde_json::to_vec(&batch).expect("serialize")),
            )
            .await;
        assert_eq!(
            status,
            200,
            "a frames batch: {}",
            String::from_utf8_lossy(&body)
        );
        self.out_seq += frames.len() as u64;
        serde_json::from_slice(&body).expect("an emitted list")
    }

    /// Posts one batch with an explicit position, for the ordering tests
    /// that must lie about where they resume.
    async fn deliver_raw(&mut self, batch: &DaemonFrames) -> (u16, Vec<u8>) {
        self.call(
            Method::POST,
            "/internal/frames",
            Some(serde_json::to_vec(batch).expect("serialize")),
        )
        .await
    }

    /// Posts a command, the way the Worker does after authorizing it.
    ///
    /// The answer names the events the command produced — the echoes a
    /// browser watching the session is owed.
    async fn command(&mut self, command: &ControlToDaemon) -> Emitted {
        let (status, body) = self
            .call(
                Method::POST,
                "/internal/command",
                Some(serde_json::to_vec(command).expect("serialize")),
            )
            .await;
        assert_eq!(status, 200, "a command: {}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).expect("an emitted list")
    }

    /// Marks the daemon's presence expired, the state a daemon that died
    /// mid-stream leaves behind.
    async fn expire_presence(&self) {
        let db = DurableDb::new(self.db.clone());
        skyzen::sql!(db, "UPDATE daemon_presence SET live_until = 0 WHERE id = 0")
            .execute()
            .await
            .expect("presence was expired");
    }

    /// The daemon's undelivered command log, oldest first.
    ///
    /// A row the stream has handed over stays until an `ack_through`
    /// retires it, so this is "owed to the daemon", not "not yet sent".
    async fn command_log(&self) -> Vec<ControlToDaemon> {
        #[derive(skyzen::FromRow)]
        struct Row {
            #[row(json)]
            json: ControlToDaemon,
        }
        let db = DurableDb::new(self.db.clone());
        skyzen::sql!(db, "SELECT json FROM daemon_commands ORDER BY seq")
            .fetch_all::<Row>()
            .await
            .expect("the command log reads")
            .into_iter()
            .map(|row| row.json)
            .collect()
    }

    async fn events(&mut self, after: u64) -> EventPage {
        let (status, body) = self
            .call(
                Method::GET,
                &format!("/internal/events?after={after}"),
                None,
            )
            .await;
        assert_eq!(
            status,
            200,
            "reading events: {}",
            String::from_utf8_lossy(&body)
        );
        serde_json::from_slice(&body).expect("an event page")
    }
}

/// The next item off an SSE stream, or `None` when `within` passes first.
async fn tokio_select_quiet(
    stream: &mut SseStream,
    within: Duration,
) -> Option<Option<Result<skyzen::http_kit::sse::Event, skyzen::http_kit::sse::ParseError>>> {
    let next = stream.next();
    let quiet = futures_timer::Delay::new(within);
    futures_util::pin_mut!(next, quiet);
    match futures_util::future::select(next, quiet).await {
        futures_util::future::Either::Left((item, _)) => Some(item),
        futures_util::future::Either::Right(_) => None,
    }
}

/// What the attach answer carries when the machine lands on the room.
fn connected() -> EmittedEvent {
    EmittedEvent {
        seq: None,
        event: ClientEvent::MachineConnection { connected: true },
    }
}

/// The event a call emitted, unwrapped of its stream position.
fn events_of(emitted: Emitted) -> Vec<ClientEvent> {
    emitted
        .events
        .into_iter()
        .map(|emitted| emitted.event)
        .collect()
}

fn assistant_delta(text: &str) -> HarnessEvent {
    HarnessEvent::AssistantDelta {
        turn_id: "turn-1".to_owned(),
        text: text.to_owned(),
    }
}

/// An event tail with nothing in it.
const NO_EVENTS: [flyco_core::wire::StoredEvent; 0] = [];

// ── The attach ──

#[skyzen::test]
async fn an_attach_mints_an_epoch_and_announces_the_machine() {
    let mut room = Room::open().await;
    let attached = room.attach().await;

    assert_eq!(attached.epoch, 1, "the first attach is epoch one");
    assert_eq!(attached.events, vec![connected()]);

    // The stream the epoch names opens behind it.
    room.open_commands().await;
}

#[skyzen::test]
async fn a_daemon_speaking_another_protocol_version_is_refused() {
    let mut room = Room::open().await;
    let (status, _) = room
        .call(
            Method::POST,
            "/internal/daemon-attach",
            Some(
                serde_json::to_vec(&DaemonAttach {
                    protocol_version: WIRE_PROTOCOL_VERSION + 1,
                })
                .expect("serialize"),
            ),
        )
        .await;
    assert_eq!(
        status, 409,
        "a version the room does not speak is a conflict"
    );
}

#[skyzen::test]
async fn frames_from_a_daemon_that_never_attached_are_refused() {
    let mut room = Room::open().await;
    let (status, _) = room
        .deliver_raw(&DaemonFrames {
            epoch: 1,
            from_seq: 1,
            ack_through: 0,
            frames: vec![DaemonToControl::Harness {
                event: assistant_delta("hello"),
            }],
        })
        .await;
    assert_eq!(status, 502, "a batch with no attach behind it is refused");
    assert_eq!(
        room.events(0).await.events,
        NO_EVENTS,
        "nothing from an unattached daemon may reach the transcript"
    );
}

#[skyzen::test]
async fn an_undecodable_body_fails_the_call() {
    let mut room = Room::open().await;
    room.greet().await;

    // A body that does not decode is a broken caller, and the fetch fails
    // outright rather than producing a problem: these routes are internal,
    // so there is nobody to explain a 400 to.
    for (path, body) in [
        (
            "/internal/frames",
            b"{\"type\":\"from_the_future\"}".as_slice(),
        ),
        ("/internal/command", b"not json at all".as_slice()),
    ] {
        let failed = room
            .object
            .fetch()
            .go(room.request(Method::POST, path, Some(body.to_vec())))
            .await;
        assert!(failed.is_err(), "{path} accepted an undecodable body");
    }
}

// ── The ordering contract ──

#[skyzen::test]
async fn a_batch_that_skips_sequence_numbers_is_refused() {
    let mut room = Room::open().await;
    room.greet().await;

    let (status, body) = room
        .deliver_raw(&DaemonFrames {
            epoch: room.epoch.expect("attached"),
            from_seq: 5,
            ack_through: 0,
            frames: vec![DaemonToControl::Harness {
                event: assistant_delta("skipped ahead"),
            }],
        })
        .await;
    assert_eq!(status, 409, "a gap in the daemon's stream is a conflict");
    let problem: flyco_core::Problem = serde_json::from_slice(&body).expect("a problem");
    assert_eq!(problem.kind, "https://flyco.dev/problems/relay-frames-gap");
    assert_eq!(
        room.events(0).await.events,
        NO_EVENTS,
        "a refused batch applies nothing"
    );
}

#[skyzen::test]
async fn a_retransmitted_batch_is_answered_without_reapplying() {
    let mut room = Room::open().await;
    room.greet().await;

    let batch = DaemonFrames {
        epoch: room.epoch.expect("attached"),
        from_seq: 1,
        ack_through: 0,
        frames: vec![DaemonToControl::Harness {
            event: assistant_delta("once"),
        }],
    };
    let (status, _) = room.deliver_raw(&batch).await;
    assert_eq!(status, 200);
    // The POST's answer is lost on the wire; the daemon re-sends the
    // same batch.
    let (status, body) = room.deliver_raw(&batch).await;
    assert_eq!(status, 200, "a retransmission is answered, not refused");
    let emitted: Emitted = serde_json::from_slice(&body).expect("an emitted list");
    assert_eq!(
        emitted.events,
        vec![],
        "the already-stored head applies nothing a second time"
    );
    assert_eq!(
        room.events(0).await.events.len(),
        1,
        "the transcript has the event once"
    );
}

#[skyzen::test]
async fn a_batch_from_a_superseded_attach_is_refused() {
    let mut room = Room::open().await;
    room.greet().await;
    let stale = room.epoch.expect("attached");

    // A retry raced the first attach's response; the daemon attached
    // again and the first epoch is dead.
    room.attach().await;

    let (status, body) = room
        .deliver_raw(&DaemonFrames {
            epoch: stale,
            from_seq: 1,
            ack_through: 0,
            frames: vec![DaemonToControl::Harness {
                event: assistant_delta("from the dead epoch"),
            }],
        })
        .await;
    assert_eq!(status, 409, "a stale epoch's frames are refused");
    let problem: flyco_core::Problem = serde_json::from_slice(&body).expect("a problem");
    assert_eq!(problem.kind, "https://flyco.dev/problems/relay-epoch-stale");
}

#[skyzen::test]
async fn a_superseded_attach_ends_its_command_stream() {
    let mut room = Room::open().await;
    room.greet().await;

    room.attach().await;

    // The superseded daemon is told why its stream is over before it
    // ends — a bare EOF reads as a dropped connection, and an uninformed
    // loser re-attaches into the epoch that replaced it (issue #336).
    let told = room.next_command().await;
    assert_eq!(
        told,
        DaemonCommand {
            seq: None,
            command: ControlToDaemon::Superseded,
        },
        "the last word on a superseded stream is why it is over"
    );
    room.expect_end().await;
}

#[skyzen::test]
async fn a_superseded_command_is_not_one_the_room_accepts() {
    let mut room = Room::open().await;
    room.greet().await;

    // It is the room's own last word on a superseded stream — composed,
    // never a log row. Sent to the room it is refused rather than queued:
    // a daemon must only ever be told it lost by the room itself.
    let (status, body) = room
        .call(
            Method::POST,
            "/internal/command",
            Some(serde_json::to_vec(&ControlToDaemon::Superseded).expect("serialize")),
        )
        .await;
    assert_eq!(status, 400);
    let problem: flyco_core::Problem = serde_json::from_slice(&body).expect("a problem");
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/superseded-is-room-composed"
    );
    assert!(
        room.command_log()
            .await
            .iter()
            .all(|command| *command != ControlToDaemon::Superseded),
        "nothing was queued"
    );
}

#[skyzen::test]
async fn a_command_stream_for_a_superseded_epoch_is_refused() {
    let mut room = Room::open().await;
    room.greet().await;
    let stale = room.epoch.expect("attached");

    room.attach().await;

    let response = room
        .call_streaming(&format!("/internal/commands?epoch={stale}"))
        .await;
    assert_eq!(
        response.status().as_u16(),
        409,
        "the room serves only the live attach's stream"
    );
}

#[skyzen::test]
async fn a_command_stream_opened_before_any_attach_is_refused() {
    let mut room = Room::open().await;
    let response = room.call_streaming("/internal/commands?epoch=1").await;
    assert_eq!(response.status().as_u16(), 502);
}

// ── Daemon → browsers ──

#[skyzen::test]
async fn a_harness_event_is_stored_and_broadcast() {
    let mut room = Room::open().await;
    room.greet().await;

    let event = assistant_delta("hello");
    let emitted = room
        .deliver(&DaemonToControl::Harness {
            event: event.clone(),
        })
        .await;

    let [emitted] = emitted.events.as_slice() else {
        panic!("one frame emits one event, not {emitted:?}");
    };
    assert_eq!(emitted.event, ClientEvent::Harness { event });
    assert_eq!(
        emitted.seq,
        Some(1),
        "the emitted event names the position it was recorded at"
    );

    let page = room.events(0).await;
    assert_eq!(page.events.len(), 1);
    assert!(!page.more);
    assert_eq!(page.events[0].seq, 1, "the first event is position 1");
}

#[skyzen::test]
async fn the_event_tail_is_replayed_in_order_and_resumes_from_a_cursor() {
    let mut room = Room::open().await;
    room.greet().await;

    for text in ["a", "b", "c"] {
        room.deliver(&DaemonToControl::Harness {
            event: assistant_delta(text),
        })
        .await;
    }

    let page = room.events(0).await;
    let seqs: Vec<u64> = page.events.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3], "positions are monotonic and gapless");

    let resumed = room.events(2).await;
    assert_eq!(resumed.events.len(), 1);
    assert_eq!(resumed.events[0].seq, 3);
    assert_eq!(room.events(3).await.events, NO_EVENTS);
}

#[skyzen::test]
async fn a_spot_notice_is_stored_as_well_as_broadcast() {
    // A reclamation is something that happened to the session, and the
    // browser most likely to want it is one opened *after* the machine was
    // taken away — which reads the stored tail rather than a live event.
    let mut room = Room::open().await;
    room.greet().await;

    let emitted = room
        .deliver(&DaemonToControl::SpotNotice {
            seconds_remaining: 30,
        })
        .await;

    assert_eq!(
        events_of(emitted),
        vec![ClientEvent::SpotNotice {
            seconds_remaining: 30
        }]
    );
    let page = room.events(0).await;
    assert_eq!(page.events.len(), 1);
    assert_eq!(
        serde_json::from_value::<ClientEvent>(page.events[0].event.clone())
            .expect("a client event"),
        ClientEvent::SpotNotice {
            seconds_remaining: 30
        }
    );
}

#[skyzen::test]
async fn a_usage_report_is_broadcast_but_not_stored() {
    let mut room = Room::open().await;
    room.greet().await;

    let usage = flyco_core::UsageReport {
        input_tokens: 10,
        output_tokens: 20,
        context: None,
        estimated_cost: None,
    };
    let emitted = room.deliver(&DaemonToControl::Usage { usage }).await;

    assert_eq!(events_of(emitted), vec![ClientEvent::Usage { usage }]);
    assert_eq!(
        room.events(0).await.events,
        NO_EVENTS,
        "a meter reading is a current value, not a transcript entry"
    );
}

#[skyzen::test]
async fn capabilities_and_the_harness_session_id_are_kept_for_a_late_joiner() {
    let mut room = Room::open().await;
    room.greet().await;

    let started = room
        .deliver(&DaemonToControl::Started {
            harness_session_id: "9d0f4b1a".to_owned(),
        })
        .await;
    let capabilities = room
        .deliver(&DaemonToControl::Capabilities {
            capabilities: vec!["can_use_tool".to_owned()],
        })
        .await;

    assert_eq!(
        events_of(started),
        vec![ClientEvent::Started {
            harness_session_id: "9d0f4b1a".to_owned()
        }]
    );
    assert_eq!(
        events_of(capabilities),
        vec![ClientEvent::Capabilities {
            capabilities: vec!["can_use_tool".to_owned()]
        }]
    );

    let kv = DurableKv::new(room.kv.clone());
    assert_eq!(
        kv.get_json::<String>("room:harness_session_id")
            .await
            .expect("read")
            .as_deref(),
        Some("9d0f4b1a")
    );
    assert_eq!(
        kv.get_json::<Vec<String>>("room:capabilities")
            .await
            .expect("read"),
        Some(vec!["can_use_tool".to_owned()])
    );
}

#[skyzen::test]
async fn an_approval_request_reaches_browsers_as_pending() {
    let mut room = Room::open().await;
    room.greet().await;

    let id = ApprovalId::generate();
    let payload = ApprovalPayload::ToolUse {
        tool: "Bash".to_owned(),
        input: serde_json::json!({ "command": "ls" }),
    };
    let emitted = room
        .deliver(&DaemonToControl::ApprovalRequest {
            id,
            payload: payload.clone(),
        })
        .await;

    assert_eq!(
        events_of(emitted),
        vec![ClientEvent::ApprovalPending { id, payload }]
    );
}

// ── The Worker → the daemon ──

#[skyzen::test]
async fn a_user_message_is_forwarded_to_the_daemon_and_echoed_to_browsers() {
    let mut room = Room::open().await;
    room.greet().await;

    let text = "what does this crate do?";
    let emitted = room
        .command(&ControlToDaemon::UserMessage {
            text: text.to_owned(),
            origin: flyco_core::MessageOrigin::User,
        })
        .await;

    // The browser that typed it already has it; every *other* browser
    // watching the session would otherwise see the agent answer a question
    // it could not see.
    assert_eq!(
        events_of(emitted),
        vec![ClientEvent::UserMessage {
            text: text.to_owned(),
            origin: flyco_core::MessageOrigin::User,
        }]
    );
    assert_eq!(
        room.next_command().await.command,
        ControlToDaemon::UserMessage {
            text: text.to_owned(),
            // The origin travels no further: what reaches the harness is
            // the text alone.
            origin: flyco_core::MessageOrigin::User,
        },
        "the daemon is handed the message"
    );

    let page = room.events(0).await;
    assert_eq!(
        page.events
            .iter()
            .map(|stored| stored.event.clone())
            .collect::<Vec<_>>(),
        vec![
            serde_json::to_value(ClientEvent::UserMessage {
                text: text.to_owned(),
                origin: flyco_core::MessageOrigin::User,
            })
            .expect("serialize")
        ],
        "a replay must carry the user's half of the conversation too"
    );
}

#[skyzen::test]
async fn a_shell_command_is_recorded_named_and_handed_to_the_daemon() {
    let mut room = Room::open().await;
    room.greet().await;

    let emitted = room
        .command(&ControlToDaemon::ShellCommand {
            command: "git status --short".to_owned(),
        })
        .await;

    // The room names the run: a caller sends a request, and what reaches
    // the daemon is an instruction with the identity every frame about it
    // will carry.
    let [asked] = emitted.events.as_slice() else {
        panic!("a shell command is echoed once, not {emitted:?}");
    };
    let ClientEvent::ShellCommand { run, command } = &asked.event else {
        panic!("the browsers see the command that was asked for");
    };
    assert_eq!(command, "git status --short");
    let run = *run;
    assert_eq!(
        room.next_command().await.command,
        ControlToDaemon::RunShell {
            run,
            command: "git status --short".to_owned(),
        },
        "the daemon is told which run it is running"
    );

    assert_eq!(
        room.events(0).await.events.len(),
        1,
        "a `!` command is part of the record of the session, like a message"
    );
}

#[skyzen::test]
async fn a_shell_command_with_no_daemon_to_run_it_is_answered_rather_than_dropped() {
    let mut room = Room::open().await;

    // No attach: the machine is still being provisioned, or its daemon is
    // mid-reconnect. Nothing runs the command, and the user is told so
    // instead of watching a row that never finishes.
    let emitted = room
        .command(&ControlToDaemon::ShellCommand {
            command: "ls".to_owned(),
        })
        .await;

    let [asked, detached, answered] = emitted.events.as_slice() else {
        panic!("an unrunnable command is echoed and then closed off, not {emitted:?}");
    };
    assert_eq!(
        detached.event,
        ClientEvent::MachineConnection { connected: false },
        "the run failed because the machine is not on the room, and that is \
         the part the user has to be told"
    );
    let ClientEvent::ShellCommand { run, .. } = asked.event else {
        panic!("the command is still recorded");
    };
    assert_eq!(
        answered.event,
        ClientEvent::ShellExited {
            run,
            outcome: ShellOutcome::Offline,
            truncated: false,
        }
    );

    // And a browser that opens the session afterwards reads the same two.
    assert_eq!(
        room.events(0)
            .await
            .events
            .into_iter()
            .map(|stored| stored.event)
            .collect::<Vec<_>>(),
        vec![
            serde_json::to_value(ClientEvent::ShellCommand {
                run,
                command: "ls".to_owned(),
            })
            .expect("serialize"),
            serde_json::to_value(ClientEvent::ShellExited {
                run,
                outcome: ShellOutcome::Offline,
                truncated: false,
            })
            .expect("serialize"),
        ]
    );
}

#[skyzen::test]
async fn a_shell_run_replays_with_its_output_and_its_exit_status() {
    let mut room = Room::open().await;
    room.greet().await;

    let run = ShellRunId::generate();
    for frame in [
        DaemonToControl::ShellOutput {
            run,
            stream: ShellStream::Stdout,
            data: " M src/lib.rs\n".to_owned(),
        },
        DaemonToControl::ShellExited {
            run,
            outcome: ShellOutcome::Exited { code: 0 },
            truncated: false,
        },
    ] {
        room.deliver(&frame).await;
    }

    // Stored as well as broadcast, unlike the web terminal's bytes: this
    // output belongs to a row in the transcript, and a replay showing the
    // command with nothing under it would be worse than showing neither.
    assert_eq!(
        room.events(0)
            .await
            .events
            .into_iter()
            .map(|stored| stored.event)
            .collect::<Vec<_>>(),
        vec![
            serde_json::to_value(ClientEvent::ShellOutput {
                run,
                stream: ShellStream::Stdout,
                data: " M src/lib.rs\n".to_owned(),
            })
            .expect("serialize"),
            serde_json::to_value(ClientEvent::ShellExited {
                run,
                outcome: ShellOutcome::Exited { code: 0 },
                truncated: false,
            })
            .expect("serialize"),
        ]
    );
}

#[skyzen::test]
async fn every_command_the_worker_may_send_is_forwarded() {
    let mut room = Room::open().await;
    room.greet().await;

    for command in [
        ControlToDaemon::UserMessage {
            text: "go".to_owned(),
            origin: flyco_core::MessageOrigin::User,
        },
        ControlToDaemon::Interrupt,
        ControlToDaemon::Compact,
        ControlToDaemon::TerminalInput {
            data: "ls\n".to_owned(),
        },
        ControlToDaemon::TerminalResize {
            cols: 132,
            rows: 40,
        },
    ] {
        room.command(&command).await;
        assert_eq!(
            room.next_command().await.command,
            command,
            "{command:?} must reach the daemon"
        );
    }
}

// ── Reconnect and the command log ──

#[skyzen::test]
async fn the_remembered_pane_size_is_replayed_ahead_of_queued_commands() {
    let mut room = Room::open().await;

    // The pane was fitted while no daemon was attached — the ordinary
    // case for a session whose machine is still being built. The size is
    // remembered rather than queued, so it survives the whole gap.
    room.command(&ControlToDaemon::TerminalResize {
        cols: 132,
        rows: 40,
    })
    .await;
    room.command(&ControlToDaemon::SetModel {
        model: flyco_core::harness::ModelChoice {
            model: "claude-opus-4-6".to_owned(),
            effort: None,
        },
    })
    .await;

    room.greet().await;

    // The resize is the stream's first word — before the queued command —
    // and it carries no sequence: it is a state replay, acknowledged for
    // nothing.
    let resized = room.next_command().await;
    assert_eq!(resized.seq, None, "a state replay is outside the log");
    assert_eq!(
        resized.command,
        ControlToDaemon::TerminalResize {
            cols: 132,
            rows: 40
        }
    );
    let queued = room.next_command().await;
    assert!(queued.seq.is_some());
    assert!(matches!(queued.command, ControlToDaemon::SetModel { .. }));
}

#[skyzen::test]
async fn an_unacknowledged_command_is_replayed_to_the_next_attach_and_an_ack_retires_it() {
    let mut room = Room::open().await;
    room.greet().await;

    room.command(&ControlToDaemon::Compact).await;
    let delivered = room.next_command().await;
    let seq = delivered.seq.expect("a logged command carries a sequence");

    // The daemon died holding the command — it read the row and its
    // acknowledgement never landed. Re-attaching must hand the row over
    // again rather than trust a stream that is gone.
    room.applied = 0;
    room.attach().await;
    room.open_commands().await;
    let replayed = room.next_command().await;
    assert_eq!(replayed.seq, Some(seq));
    assert_eq!(replayed.command, ControlToDaemon::Compact);

    // And now the acknowledgement lands: the next batch's `ack_through`
    // retires the row, and a third attach finds nothing left to replay.
    room.deliver(&DaemonToControl::Harness {
        event: assistant_delta("still here"),
    })
    .await;
    room.attach().await;
    room.open_commands().await;
    room.expect_quiet().await;
}

#[skyzen::test]
async fn an_acknowledgement_keeps_acked_rows_from_replaying() {
    let mut room = Room::open().await;
    room.greet().await;

    room.command(&ControlToDaemon::Compact).await;
    room.next_command().await;
    // The daemon's next POST acknowledges the read.
    room.deliver(&DaemonToControl::Harness {
        event: assistant_delta("ok"),
    })
    .await;

    assert_eq!(
        room.command_log().await,
        vec![],
        "an acknowledged row is retired"
    );
}

// ── The room while the daemon is away ──

#[skyzen::test]
async fn a_state_change_held_for_an_offline_daemon_is_delivered_on_attach() {
    let mut room = Room::open().await;

    let model = flyco_core::harness::ModelChoice {
        model: "claude-sonnet-4-6".to_owned(),
        effort: Some("high".to_owned()),
    };
    let emitted = room
        .command(&ControlToDaemon::SetModel {
            model: model.clone(),
        })
        .await;

    // The change is echoed either way — the control plane already
    // recorded it, so a watcher sees the line regardless of the machine.
    assert_eq!(
        events_of(emitted),
        vec![ClientEvent::ModelChanged {
            model: model.clone()
        }]
    );

    room.greet().await;
    assert_eq!(
        room.next_command().await.command,
        ControlToDaemon::SetModel { model },
        "a state is still owed to a daemon that was away"
    );
}

#[skyzen::test]
async fn an_instant_command_for_an_offline_daemon_is_dropped_and_announced() {
    let mut room = Room::open().await;

    let emitted = room.command(&ControlToDaemon::Interrupt).await;

    assert_eq!(
        events_of(emitted),
        vec![ClientEvent::MachineConnection { connected: false }],
        "whoever pressed Stop, and everyone else watching, is told the \
         machine is off the room"
    );
    assert_eq!(
        room.command_log().await,
        vec![],
        "an interrupt is not held for a daemon that is away"
    );
}

#[skyzen::test]
async fn an_expired_attach_is_announced_gone_once_and_then_not_again() {
    let mut room = Room::open().await;
    room.attach().await;
    // No stream is held open: the daemon's last contact has expired.
    room.expire_presence().await;

    let first = room.command(&ControlToDaemon::Interrupt).await;
    assert_eq!(
        events_of(first),
        vec![ClientEvent::MachineConnection { connected: false }],
        "the first caller after the deadline is told the daemon is gone"
    );

    // The marker's flag means "already said"; each later dropped command
    // still has to say it again, because whoever sent *that* one was never
    // in the first call's audience.
    let second = room.command(&ControlToDaemon::Interrupt).await;
    assert_eq!(
        events_of(second),
        vec![ClientEvent::MachineConnection { connected: false }]
    );
}

#[skyzen::test]
async fn a_reattach_resets_the_gone_marker() {
    let mut room = Room::open().await;
    room.attach().await;
    room.expire_presence().await;
    room.command(&ControlToDaemon::Interrupt).await;

    // The daemon came back; the attach says so, and the next expiry is a
    // fresh story rather than a continuation.
    let attached = room.attach().await;
    assert_eq!(attached.events, vec![connected()]);
}

// ── The workdir ──

#[skyzen::test]
async fn a_workdir_question_is_answered_to_the_one_who_asked() {
    let mut room = Room::open().await;
    room.greet().await;

    // The ask stays open: the response's head is the room taking the
    // question, and its body ends with the answer.
    let id = flyco_core::WorkdirRequestId::generate();
    let response = room
        .post_streaming(
            "/internal/workdir",
            Some(
                serde_json::to_vec(&ControlToDaemon::InspectWorkdir {
                    id,
                    request: flyco_core::workdir::WorkdirRequest::Entries {
                        path: "src".to_owned(),
                    },
                })
                .expect("serialize"),
            ),
        )
        .await;
    assert_eq!(
        response.status().as_u16(),
        200,
        "a question for a live daemon is held open"
    );
    let mut waiting = response.into_body().into_sse();

    let asked = room.next_command().await;
    assert_eq!(
        asked.command,
        ControlToDaemon::InspectWorkdir {
            id,
            request: flyco_core::workdir::WorkdirRequest::Entries {
                path: "src".to_owned()
            }
        }
    );

    let reply = flyco_core::workdir::WorkdirReply::Entries {
        listing: flyco_core::workdir::DirectoryListing {
            path: "src".to_owned(),
            entries: vec![flyco_core::workdir::DirectoryEntry {
                name: "lib.rs".to_owned(),
                path: "src/lib.rs".to_owned(),
                kind: flyco_core::workdir::EntryKind::File,
                size_bytes: Some(42),
                ignored: false,
            }],
            truncated: false,
        },
    };
    let emitted = room
        .deliver(&DaemonToControl::WorkdirReply {
            id,
            reply: reply.clone(),
        })
        .await;
    assert_eq!(
        emitted.events,
        vec![],
        "a workdir reply is addressed, not fanned out"
    );

    // The reply arrives on the request that asked — a `reply` event —
    // and the stream ends behind it.
    let event = tokio_select_quiet(&mut waiting, PATIENCE)
        .await
        .expect("the held request was answered in time")
        .expect("the stream is still open")
        .expect("a decodable SSE frame");
    assert_eq!(event.event(), Some(WORKDIR_REPLY_EVENT));
    let answered: flyco_core::workdir::WorkdirReply = event.data().expect("a workdir reply");
    assert_eq!(answered, reply);
    match tokio_select_quiet(&mut waiting, PATIENCE).await {
        Some(None) => {}
        other => panic!("the stream ended with its answer, then produced {other:?}"),
    }

    // Single use: the row went with the answer.
    let key = id.to_string();
    let db = DurableDb::new(room.db.clone());
    let left: Option<String> =
        skyzen::sql!(db, "SELECT json FROM workdir_replies WHERE id = {key}")
            .fetch_scalar_optional()
            .await
            .expect("the reply table reads");
    assert!(left.is_none(), "a served reply is gone");
}

#[skyzen::test]
async fn a_workdir_question_that_outlives_its_deadline_times_out() {
    let mut room = Room::open().await;
    room.attach().await;

    // The daemon is live but never answers, so the held request ends with
    // the room's own deadline — twelve seconds — rather than a reply.
    let response = room
        .post_streaming(
            "/internal/workdir",
            Some(
                serde_json::to_vec(&ControlToDaemon::InspectWorkdir {
                    id: flyco_core::WorkdirRequestId::generate(),
                    request: flyco_core::workdir::WorkdirRequest::Diff { repo: None },
                })
                .expect("serialize"),
            ),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);
    let mut waiting = response.into_body().into_sse();

    // The deadline fires at twelve seconds; watch a little longer. The
    // stream's only named events are terminal — heartbeat comments are
    // skipped.
    let event = loop {
        match tokio_select_quiet(&mut waiting, Duration::from_secs(15)).await {
            Some(Some(item)) => {
                let item = item.expect("a decodable SSE frame");
                if item.event().is_some() {
                    break item;
                }
            }
            Some(None) => panic!("the stream ended without answering"),
            None => panic!("the deadline never fired"),
        }
    };
    assert_eq!(event.event(), Some(WORKDIR_TIMEOUT_EVENT));
}

#[skyzen::test]
async fn a_workdir_reply_lands_while_the_question_is_held_open() {
    // The same flow the way `skyzen dev` reaches it — through the
    // namespace, which holds an object's dispatch lock only until a
    // response's head is produced. The ask's `fetch` returns with its
    // body still open, so the frames POST carrying the reply is a second
    // dispatch rather than a deadlock behind the first.
    let native = NativeRooms::new();
    let session = SessionId::generate();
    let daemon = NamespaceDaemon::attach(&native, session).await;

    let id = flyco_core::WorkdirRequestId::generate();
    let asking = daemon
        .stub
        .fetch(room_request(
            session,
            Method::POST,
            "/internal/workdir",
            Some(
                serde_json::to_vec(&ControlToDaemon::InspectWorkdir {
                    id,
                    request: flyco_core::workdir::WorkdirRequest::Entries {
                        path: "src".to_owned(),
                    },
                })
                .expect("serialize"),
            ),
        ))
        .await
        .expect("the room answered");
    assert_eq!(
        asking.status().as_u16(),
        200,
        "the question was taken and held open"
    );

    let reply = flyco_core::workdir::WorkdirReply::Entries {
        listing: flyco_core::workdir::DirectoryListing {
            path: "src".to_owned(),
            entries: vec![],
            truncated: false,
        },
    };
    daemon.reply(id, reply.clone()).await;

    let mut waiting = asking.into_body().into_sse();
    let event = tokio_select_quiet(&mut waiting, PATIENCE)
        .await
        .expect("the held request was answered in time")
        .expect("the stream is still open")
        .expect("a decodable SSE frame");
    assert_eq!(event.event(), Some(WORKDIR_REPLY_EVENT));
    let got: flyco_core::workdir::WorkdirReply = event.data().expect("a workdir reply");
    assert_eq!(got, reply);
}

/// A daemon the test plays against a room reached through the namespace,
/// the way the Worker's own routes reach it.
struct NamespaceDaemon {
    session: SessionId,
    stub: skyzen::durable::NativeDurableObjectStub<SessionRoom>,
    epoch: u64,
    commands: SseStream,
}

impl NamespaceDaemon {
    /// Attaches to the session's room and opens its command stream.
    async fn attach(rooms: &NativeRooms, session: SessionId) -> Self {
        let stub = rooms
            .get_by_name(&session.to_string())
            .expect("a room stub");
        let attached = stub
            .fetch(room_request(
                session,
                Method::POST,
                "/internal/daemon-attach",
                Some(
                    serde_json::to_vec(&DaemonAttach {
                        protocol_version: WIRE_PROTOCOL_VERSION,
                    })
                    .expect("serialize"),
                ),
            ))
            .await
            .expect("the room answered");
        assert_eq!(attached.status().as_u16(), 200, "the daemon attached");
        let attached: AttachResponse = serde_json::from_slice(
            &attached
                .into_body()
                .into_bytes()
                .await
                .expect("a readable body"),
        )
        .expect("an attach response");
        let commands = stub
            .fetch(room_request(
                session,
                Method::GET,
                &format!("/internal/commands?epoch={}", attached.epoch),
                None,
            ))
            .await
            .expect("the room answered");
        assert_eq!(commands.status().as_u16(), 200, "the command stream opened");
        Self {
            session,
            stub,
            epoch: attached.epoch,
            commands: commands.into_body().into_sse(),
        }
    }

    /// Reads the next question about the checkout the room hands the daemon.
    async fn next_workdir_question(&mut self) -> flyco_core::WorkdirRequestId {
        loop {
            let item = tokio_select_quiet(&mut self.commands, PATIENCE)
                .await
                .expect("a command arrived in time")
                .expect("the stream is still open")
                .expect("a decodable SSE frame");
            let command: DaemonCommand = item.data().expect("a command envelope");
            if let ControlToDaemon::InspectWorkdir { id, .. } = command.command {
                return id;
            }
        }
    }

    /// Answers one question, the way the daemon's outbound flush does.
    async fn reply(
        &self,
        id: flyco_core::WorkdirRequestId,
        reply: flyco_core::workdir::WorkdirReply,
    ) {
        let answered = self
            .stub
            .fetch(room_request(
                self.session,
                Method::POST,
                "/internal/frames",
                Some(
                    serde_json::to_vec(&DaemonFrames {
                        epoch: self.epoch,
                        from_seq: 1,
                        ack_through: 0,
                        frames: vec![DaemonToControl::WorkdirReply { id, reply }],
                    })
                    .expect("serialize"),
                ),
            ))
            .await
            .expect("the room answered");
        assert_eq!(answered.status().as_u16(), 200, "the frames landed");
    }
}

#[skyzen::test]
async fn the_worker_side_returns_the_daemon_answer_from_the_held_stream() {
    // The whole path a browser's request takes: `inspect_workdir` mints the
    // question, the room hands it to the daemon, the daemon's reply lands
    // as a frame, and the held stream's `reply` event comes back decoded.
    let native = NativeRooms::new();
    let rooms = Rooms::from_native(native.clone(), NativeUserStreams::new());
    let session = SessionId::generate();
    let mut daemon = NamespaceDaemon::attach(&native, session).await;

    let reply = flyco_core::workdir::WorkdirReply::Entries {
        listing: flyco_core::workdir::DirectoryListing {
            path: "src".to_owned(),
            entries: vec![],
            truncated: false,
        },
    };
    let (answer, ()) = futures_util::future::join(
        rooms.inspect_workdir(
            session,
            flyco_core::workdir::WorkdirRequest::Entries {
                path: "src".to_owned(),
            },
        ),
        async {
            let id = daemon.next_workdir_question().await;
            daemon.reply(id, reply.clone()).await;
        },
    )
    .await;
    assert_eq!(answer.expect("the daemon's answer"), reply);
}

#[skyzen::test]
async fn the_worker_side_refuses_a_question_no_daemon_can_read() {
    let rooms = Rooms::from_native(NativeRooms::new(), NativeUserStreams::new());
    let refused = rooms
        .inspect_workdir(
            SessionId::generate(),
            flyco_core::workdir::WorkdirRequest::Diff { repo: None },
        )
        .await
        .expect_err("nothing is attached to read the checkout");
    assert!(
        matches!(refused, ApiError::SessionDaemonOffline),
        "the room's 503 is the offline error, got {refused:?}"
    );
}

#[skyzen::test]
async fn the_worker_side_times_out_with_the_held_stream() {
    // A live daemon that never answers: the room's `timeout` event is what
    // ends the browser's request, twelve seconds in.
    let native = NativeRooms::new();
    let rooms = Rooms::from_native(native.clone(), NativeUserStreams::new());
    let session = SessionId::generate();
    let _daemon = NamespaceDaemon::attach(&native, session).await;

    let timed_out = rooms
        .inspect_workdir(
            session,
            flyco_core::workdir::WorkdirRequest::Diff { repo: None },
        )
        .await
        .expect_err("the daemon never answered");
    assert!(
        matches!(timed_out, ApiError::WorkdirTimeout),
        "the stream's timeout is the timeout error, got {timed_out:?}"
    );
}

#[skyzen::test]
async fn a_workdir_question_for_an_offline_daemon_is_refused_rather_than_held() {
    let mut room = Room::open().await;

    let (status, body) = room
        .call(
            Method::POST,
            "/internal/workdir",
            Some(
                serde_json::to_vec(&ControlToDaemon::InspectWorkdir {
                    id: flyco_core::WorkdirRequestId::generate(),
                    request: flyco_core::workdir::WorkdirRequest::Diff { repo: None },
                })
                .expect("serialize"),
            ),
        )
        .await;
    assert_eq!(
        status, 503,
        "a browser is told at once there is nothing to read it"
    );
    let problem: flyco_core::Problem = serde_json::from_slice(&body).expect("a problem");
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/session-daemon-offline"
    );
}

// ── The desktop ──

/// Opens the watcher stream and reads its hello, returning both.
async fn watch(room: &mut Room) -> (SseStream, serde_json::Value) {
    let response = room.call_streaming("/internal/desktop/watch").await;
    assert_eq!(response.status().as_u16(), 200, "the watch stream opened");
    let mut stream = response.into_body().into_sse();
    let item = tokio_select_quiet(&mut stream, PATIENCE)
        .await
        .expect("a hello in time")
        .expect("the stream is still open")
        .expect("a decodable SSE frame");
    assert_eq!(item.event(), Some("hello"), "a watch stream greets first");
    (stream, item.data().expect("a hello"))
}

/// The next event off the watcher stream, or `None` when quiet.
async fn next_watch_event(stream: &mut SseStream) -> Option<skyzen::http_kit::sse::Event> {
    tokio_select_quiet(stream, PATIENCE)
        .await
        .and_then(|item| item.and_then(Result::ok))
}

/// Expires every watcher row, the state a closed browser tab leaves.
async fn expire_watchers(room: &Room) {
    let db = DurableDb::new(room.db.clone());
    skyzen::sql!(db, "UPDATE desktop_watchers SET live_until = 0")
        .execute()
        .await
        .expect("watchers were expired");
}

#[skyzen::test]
async fn a_watcher_joining_turns_the_encoder_on() {
    let mut room = Room::open().await;
    room.greet().await;
    // An idle room owes a fresh attach nothing: a daemon already assumes
    // nobody watches.
    room.expect_quiet().await;

    let (_stream, hello) = watch(&mut room).await;
    assert_eq!(hello["takeover"], false, "a watcher joins not driving");
    let watcher = hello["watcher"].as_u64().expect("a watcher id");
    assert!(watcher > 0, "a watcher id is a real row id");

    let command = room.next_command().await;
    assert_eq!(
        command.command,
        ControlToDaemon::DesktopAudience { watching: true },
        "the daemon is told it has an audience"
    );
}

#[skyzen::test]
async fn a_chunk_never_becomes_a_transcript_event() {
    let mut room = Room::open().await;
    room.greet().await;
    let (_stream, _hello) = watch(&mut room).await;
    room.next_command().await; // the audience command

    let emitted = room
        .deliver(&DaemonToControl::DesktopChunk {
            keyframe: true,
            data: vec![0x82, 0x0b, 0x47],
        })
        .await;
    assert!(
        emitted.events.is_empty(),
        "a chunk is storage, not a transcript entry"
    );
}

#[skyzen::test]
async fn a_watch_stream_replays_the_latest_gop() {
    let mut room = Room::open().await;
    room.greet().await;

    // A dead keyframe and its inter-frames, then a fresh keyframe: only the
    // tail from the newest keyframe down is owed a joining watcher.
    room.deliver(&DaemonToControl::DesktopChunk {
        keyframe: true,
        data: b"old-key".to_vec(),
    })
    .await;
    room.deliver(&DaemonToControl::DesktopChunk {
        keyframe: false,
        data: b"old-inter".to_vec(),
    })
    .await;
    room.deliver(&DaemonToControl::DesktopChunk {
        keyframe: true,
        data: b"new-key".to_vec(),
    })
    .await;
    room.deliver(&DaemonToControl::DesktopChunk {
        keyframe: false,
        data: b"new-inter".to_vec(),
    })
    .await;

    let (mut stream, _hello) = watch(&mut room).await;
    let mut seen = Vec::new();
    while let Some(item) = next_watch_event(&mut stream).await {
        if item.event() != Some("chunk") {
            continue;
        }
        let body: serde_json::Value = item.data().expect("a chunk body");
        seen.push((
            body["keyframe"].as_bool().expect("a flag"),
            base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                body["data"].as_str().expect("a payload"),
            )
            .expect("base64"),
        ));
        if seen.len() == 2 {
            break;
        }
    }
    assert_eq!(
        seen,
        vec![(true, b"new-key".to_vec()), (false, b"new-inter".to_vec())],
        "a joining watcher starts at the newest keyframe"
    );
}

/// Reads commands until `want` arrives, or the stream runs out of
/// reasonable slack — a desktop state reaches the wire by the attach's
/// aggregate replay or by the row a route queued, whichever got there.
async fn next_command_matching(room: &mut Room, want: &ControlToDaemon) -> DaemonCommand {
    for _ in 0..8 {
        let command = room.next_command().await;
        if &command.command == want {
            return command;
        }
    }
    panic!("{want:?} never arrived on the command stream")
}

#[skyzen::test]
async fn a_takeover_is_recorded_and_handed_down() {
    let mut room = Room::open().await;
    room.greet().await;
    let (_stream, hello) = watch(&mut room).await;

    let watcher = hello["watcher"].as_u64().expect("a watcher id");
    let (status, body) = room
        .call(
            Method::POST,
            "/internal/desktop/takeover",
            Some(
                serde_json::to_vec(&flyco_core::DesktopTakeoverRequest {
                    watcher,
                    active: true,
                })
                .expect("serialize"),
            ),
        )
        .await;
    assert_eq!(
        status,
        200,
        "a takeover: {}",
        String::from_utf8_lossy(&body)
    );

    let page = room.events(0).await;
    let taken = serde_json::to_value(ClientEvent::DesktopTakeover { active: true })
        .expect("an event serializes");
    assert!(
        page.events.iter().any(|entry| entry.event == taken),
        "a takeover is recorded for late joiners"
    );

    next_command_matching(
        &mut room,
        &ControlToDaemon::DesktopTakeover { active: true },
    )
    .await;
}

#[skyzen::test]
async fn input_needs_the_takeover_the_watcher_owns() {
    let mut room = Room::open().await;
    room.greet().await;
    let (_stream, hello) = watch(&mut room).await;
    let watcher = hello["watcher"].as_u64().expect("a watcher id");

    let input = || {
        serde_json::to_vec(&flyco_core::DesktopInputRequest {
            watcher,
            events: vec![flyco_core::DesktopInputEvent::Move { x: 10, y: 20 }],
        })
        .expect("serialize")
    };

    let (status, body) = room
        .call(Method::POST, "/internal/desktop/input", Some(input()))
        .await;
    assert_eq!(status, 409, "input without the screen is refused");
    let problem: flyco_core::Problem = serde_json::from_slice(&body).expect("a problem");
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/desktop-takeover-required"
    );

    room.call(
        Method::POST,
        "/internal/desktop/takeover",
        Some(
            serde_json::to_vec(&flyco_core::DesktopTakeoverRequest {
                watcher,
                active: true,
            })
            .expect("serialize"),
        ),
    )
    .await;
    next_command_matching(
        &mut room,
        &ControlToDaemon::DesktopTakeover { active: true },
    )
    .await;

    let (status, body) = room
        .call(Method::POST, "/internal/desktop/input", Some(input()))
        .await;
    assert_eq!(
        status,
        204,
        "the owner drives: {}",
        String::from_utf8_lossy(&body)
    );

    next_command_matching(
        &mut room,
        &ControlToDaemon::DesktopInput {
            events: vec![flyco_core::DesktopInputEvent::Move { x: 10, y: 20 }],
        },
    )
    .await;
}

#[skyzen::test]
async fn a_lapsed_watcher_releases_the_screen() {
    let mut room = Room::open().await;
    room.greet().await;
    let (_stream, hello) = watch(&mut room).await;
    let watcher = hello["watcher"].as_u64().expect("a watcher id");
    room.call(
        Method::POST,
        "/internal/desktop/takeover",
        Some(
            serde_json::to_vec(&flyco_core::DesktopTakeoverRequest {
                watcher,
                active: true,
            })
            .expect("serialize"),
        ),
    )
    .await;
    next_command_matching(
        &mut room,
        &ControlToDaemon::DesktopTakeover { active: true },
    )
    .await;

    expire_watchers(&room).await;

    assert_eq!(
        room.next_command().await.command,
        ControlToDaemon::DesktopAudience { watching: false },
        "the reconciler tells the daemon the room is empty"
    );
    assert_eq!(
        room.next_command().await.command,
        ControlToDaemon::DesktopTakeover { active: false },
        "the reconciler releases the takeover too"
    );
    let page = room.events(0).await;
    let released = serde_json::to_value(ClientEvent::DesktopTakeover { active: false })
        .expect("an event serializes");
    assert!(
        page.events.iter().any(|entry| entry.event == released),
        "the release is recorded for late joiners"
    );
}

#[skyzen::test]
async fn a_dead_watcher_is_a_gone() {
    let mut room = Room::open().await;
    room.greet().await;
    let (_stream, hello) = watch(&mut room).await;
    let watcher = hello["watcher"].as_u64().expect("a watcher id");
    expire_watchers(&room).await;

    let (status, body) = room
        .call(
            Method::POST,
            "/internal/desktop/takeover",
            Some(
                serde_json::to_vec(&flyco_core::DesktopTakeoverRequest {
                    watcher,
                    active: true,
                })
                .expect("serialize"),
            ),
        )
        .await;
    assert_eq!(status, 410, "an expired watcher is gone");
    let problem: flyco_core::Problem = serde_json::from_slice(&body).expect("a problem");
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/desktop-watcher-gone"
    );
}

#[skyzen::test]
async fn a_fresh_attach_hears_the_room_is_driven() {
    let mut room = Room::open().await;
    room.greet().await;
    let (_stream, hello) = watch(&mut room).await;
    let watcher = hello["watcher"].as_u64().expect("a watcher id");
    room.call(
        Method::POST,
        "/internal/desktop/takeover",
        Some(
            serde_json::to_vec(&flyco_core::DesktopTakeoverRequest {
                watcher,
                active: true,
            })
            .expect("serialize"),
        ),
    )
    .await;

    // The daemon restarts: the still-live watcher must be replayed onto the
    // new attach's stream, or its takeover would hang on a daemon that
    // believes nobody is watching.
    room.attach().await;
    room.open_commands().await;
    assert_eq!(
        room.next_command().await.command,
        ControlToDaemon::DesktopAudience { watching: true }
    );
    assert_eq!(
        room.next_command().await.command,
        ControlToDaemon::DesktopTakeover { active: true }
    );
}

// ── The shared SSE machinery ──

#[skyzen::test]
async fn a_quiet_stream_still_says_it_is_alive() {
    use skyzen::Responder as _;

    // `serve` with a heartbeat a test can wait for. A daemon's liveness
    // read is byte-level — the parser throws comments away, so what the
    // room writes to the wire is what has to be checked.
    struct Idle;
    let sse = crate::sse::serve(
        Idle,
        |_feed: &mut Idle| -> crate::sse::PollFn<'_> { Box::pin(async { crate::sse::Poll::Idle }) },
        Duration::from_millis(80),
    );

    let mut response = skyzen::Response::new(Body::empty());
    sse.respond_to(&skyzen::Request::new(Body::empty()), &mut response)
        .expect("the stream responds");

    let mut body = response.into_body();
    let mut seen = Vec::new();
    let deadline = futures_timer::Delay::new(PATIENCE);
    futures_util::pin_mut!(deadline);
    loop {
        let next = body.next();
        futures_util::pin_mut!(next);
        match futures_util::future::select(next, deadline.as_mut()).await {
            futures_util::future::Either::Left((Some(Ok(chunk)), _)) => {
                seen.extend_from_slice(&chunk);
                if seen.windows(2).any(|pair| pair == b"\n:") || seen.starts_with(b":") {
                    return;
                }
            }
            futures_util::future::Either::Left((other, _)) => {
                panic!("the stream ended or errored before its heartbeat: {other:?}")
            }
            futures_util::future::Either::Right(_) => {
                panic!("no heartbeat bytes within {PATIENCE:?}: {seen:?}")
            }
        }
    }
}
