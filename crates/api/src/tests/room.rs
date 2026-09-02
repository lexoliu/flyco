//! The session room, driven the way Cloudflare drives it.
//!
//! The room is exercised through the real trait surface —
//! `WebSocketConnection::new`, `DurableConnections::new`,
//! `DurableContext::new` — against skyzen's SQLite-backed
//! [`InMemoryDurableDb`] and a connection registry that records what was
//! sent. What these tests observe is what the room *did to its sockets*, and
//! that is the one thing the runtime cannot supply: a socket here reports
//! every frame the room wrote to it, which is how a broadcast, a targeted
//! forward and a refusal are told apart. So the sockets are flyco's and
//! everything behind them is skyzen's.

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, RwLock};

use flyco_core::{
    ApprovalDecision, ApprovalId, ClientEvent, ControlToDaemon, DaemonToControl, HarnessEvent,
    SessionId, ShellOutcome, ShellRunId, ShellStream, WIRE_PROTOCOL_VERSION, wire::ApprovalPayload,
};
use skyzen::durable::{
    DurableConnections, DurableContext, DurableObject as _, DurableObjectId, WebSocketConnection,
    WebSocketEvent,
};
use skyzen::http_kit::ws::WebSocketMessage;
use skyzen::{Body, Method, Request};
use skyzen_services::durable::{Alarm, DurableDb, DurableKv};
use skyzen_test::mock::{InMemoryAlarm, InMemoryDurableDb, InMemoryDurableKv};

use crate::room::{
    EventPage, HEADER_INTERNAL, HEADER_ROLE, HEADER_SESSION, INTERNAL, ROLE_CLIENT, ROLE_DAEMON,
    SessionRoom, StoredEvent,
};
use crate::tests::sockets::{FakeConnections, FakeSocket, Sent};

// ── The harness ──

/// A room with a daemon socket, a client socket, and a real database.
struct Room {
    session: SessionId,
    object: SessionRoom,
    daemon: WebSocketConnection,
    client: WebSocketConnection,
    connections: FakeConnections,
    db: InMemoryDurableDb,
    kv: InMemoryDurableKv,
    sent: Receiver<Sent>,
}

impl Room {
    async fn open() -> Self {
        let session = SessionId::generate();
        let (sender, sent) = channel();

        let socket = |role: &str| FakeSocket {
            tags: vec![role.to_owned(), format!("session:{session}")],
            sent: sender.clone(),
            attachment: Arc::new(RwLock::new(None)),
        };
        let daemon = socket(ROLE_DAEMON);
        let client = socket(ROLE_CLIENT);
        let connections = FakeConnections {
            sockets: vec![daemon.clone(), client.clone()],
        };

        Self {
            session,
            object: SessionRoom,
            daemon: WebSocketConnection::new(Box::new(daemon)),
            client: WebSocketConnection::new(Box::new(client)),
            connections,
            db: InMemoryDurableDb::in_memory()
                .await
                .expect("an in-memory database"),
            kv: InMemoryDurableKv::new(),
            sent,
        }
    }

    fn context(&self) -> DurableContext {
        DurableContext::new(
            DurableKv::new(self.kv.clone()),
            DurableDb::new(self.db.clone()),
            Alarm::new(InMemoryAlarm::new()),
            DurableConnections::new(Box::new(self.connections.clone())),
            DurableObjectId::new(self.session.to_string(), Some(self.session.to_string())),
        )
    }

    /// Delivers one frame from a socket, exactly as the runtime would.
    async fn deliver(&mut self, from: Which, frame: &str) {
        let context = self.context();
        let socket = match from {
            Which::Daemon => &self.daemon,
            Which::Client => &self.client,
        };
        // The room reports a peer's misbehaviour by closing it, not by
        // erroring: an `Err` here is a fault in the room itself.
        self.object
            .websocket(
                socket,
                WebSocketEvent::Message(WebSocketMessage::Text(frame.into())),
                &context,
            )
            .await
            .expect("the room handled the frame");
    }

    async fn deliver_json<T: serde::Serialize + Sync>(&mut self, from: Which, frame: &T) {
        let text = serde_json::to_string(frame).expect("serialize");
        self.deliver(from, &text).await;
    }

    /// Sends the daemon's handshake without asserting what came back.
    ///
    /// What a `Hello` is answered with is the welcome *plus the mailbox*,
    /// and the mailbox tests are the ones that care what is in it.
    async fn hello(&mut self) {
        self.deliver_json(
            Which::Daemon,
            &DaemonToControl::Hello {
                protocol_version: WIRE_PROTOCOL_VERSION,
                session: self.session,
            },
        )
        .await;
    }

    /// Completes the daemon handshake on a room with nothing held for it.
    async fn greet(&mut self) {
        self.hello().await;
        assert_eq!(
            self.drain(),
            vec![welcome()],
            "a good hello with an empty mailbox is answered with exactly one welcome"
        );
    }

    /// Everything the room has sent since the last drain.
    fn drain(&self) -> Vec<Sent> {
        self.sent.try_iter().collect()
    }

    /// Calls one of the room's HTTP routes, the way the Worker does.
    async fn call(&mut self, method: Method, path: &str, body: Option<Vec<u8>>) -> (u16, Vec<u8>) {
        let mut request = Request::new(body.map_or_else(Body::empty, Body::from));
        *request.method_mut() = method;
        *request.uri_mut() = format!("https://session-room.flyco.invalid{path}")
            .parse()
            .expect("a valid room URL");
        for (name, value) in [
            (HEADER_INTERNAL, INTERNAL.to_owned()),
            (HEADER_SESSION, self.session.to_string()),
        ] {
            request
                .headers_mut()
                .insert(name, value.parse().expect("a valid header"));
        }
        request.headers_mut().insert(
            skyzen::header::CONTENT_TYPE,
            skyzen::header::HeaderValue::from_static("application/json"),
        );

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
            .extensions_mut()
            .insert(DurableConnections::new(Box::new(self.connections.clone())));

        let response = self
            .object
            .fetch()
            .go(request)
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

/// Which socket a frame came from.
#[derive(Debug, Clone, Copy)]
enum Which {
    Daemon,
    Client,
}

fn to_client(event: &ClientEvent) -> Sent {
    Sent::Text {
        to: ROLE_CLIENT.to_owned(),
        text: serde_json::to_string(event).expect("serialize"),
    }
}

/// The event a frame the room broadcast carries.
///
/// Read back rather than predicted, because the room assigns a shell run's
/// identity: what a test can assert is what the room *said*, and that the
/// frames it sent afterwards name the same run.
fn event_in(sent: &Sent) -> ClientEvent {
    let Sent::Text { to, text } = sent else {
        panic!("{sent:?} is not a frame");
    };
    assert_eq!(to, ROLE_CLIENT, "this frame did not go to a browser");
    serde_json::from_str(text).expect("a client event")
}

/// The welcome every accepted handshake is answered with.
fn welcome() -> Sent {
    to_daemon(&ControlToDaemon::Welcome)
}

fn to_daemon(command: &ControlToDaemon) -> Sent {
    Sent::Text {
        to: ROLE_DAEMON.to_owned(),
        text: serde_json::to_string(command).expect("serialize"),
    }
}

fn assistant_delta(text: &str) -> HarnessEvent {
    HarnessEvent::AssistantDelta {
        turn_id: "turn-1".to_owned(),
        text: text.to_owned(),
    }
}

/// Close code the room uses for a policy violation (RFC 6455 §7.4.1).
const CLOSE_POLICY: u16 = 1008;

/// An event tail with nothing in it.
const NO_EVENTS: [StoredEvent; 0] = [];

// ── The handshake ──

#[skyzen::test]
async fn a_matching_hello_is_welcomed() {
    let mut room = Room::open().await;
    room.greet().await;
}

#[skyzen::test]
async fn a_daemon_speaking_another_protocol_version_is_closed() {
    let mut room = Room::open().await;
    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::Hello {
            protocol_version: WIRE_PROTOCOL_VERSION + 1,
            session: room.session,
        },
    )
    .await;

    assert_eq!(
        room.drain(),
        vec![Sent::Closed {
            to: ROLE_DAEMON.to_owned(),
            code: CLOSE_POLICY,
        }]
    );
}

#[skyzen::test]
async fn a_daemon_greeting_the_wrong_room_is_closed() {
    let mut room = Room::open().await;
    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::Hello {
            protocol_version: WIRE_PROTOCOL_VERSION,
            session: SessionId::generate(),
        },
    )
    .await;

    assert_eq!(
        room.drain(),
        vec![Sent::Closed {
            to: ROLE_DAEMON.to_owned(),
            code: CLOSE_POLICY,
        }]
    );
}

#[skyzen::test]
async fn a_daemon_that_skips_the_handshake_is_closed() {
    let mut room = Room::open().await;
    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::Harness {
            event: assistant_delta("hello"),
        },
    )
    .await;

    assert_eq!(
        room.drain(),
        vec![Sent::Closed {
            to: ROLE_DAEMON.to_owned(),
            code: CLOSE_POLICY,
        }]
    );
    assert_eq!(
        room.events(0).await.events,
        NO_EVENTS,
        "nothing from an ungreeted daemon may reach the transcript"
    );
}

#[skyzen::test]
async fn an_undecodable_frame_closes_the_peer_that_sent_it() {
    let mut room = Room::open().await;
    room.greet().await;

    room.deliver(Which::Daemon, "{\"type\":\"from_the_future\"}")
        .await;
    assert_eq!(
        room.drain(),
        vec![Sent::Closed {
            to: ROLE_DAEMON.to_owned(),
            code: CLOSE_POLICY,
        }]
    );

    room.deliver(Which::Client, "not json at all").await;
    assert_eq!(
        room.drain(),
        vec![Sent::Closed {
            to: ROLE_CLIENT.to_owned(),
            code: CLOSE_POLICY,
        }]
    );
}

// ── Daemon → browsers ──

#[skyzen::test]
async fn a_harness_event_is_stored_and_broadcast() {
    let mut room = Room::open().await;
    room.greet().await;

    let event = assistant_delta("hello");
    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::Harness {
            event: event.clone(),
        },
    )
    .await;

    assert_eq!(
        room.drain(),
        vec![to_client(&ClientEvent::Harness {
            event: event.clone()
        })],
        "the event reaches browsers and nothing is echoed to the daemon"
    );

    let page = room.events(0).await;
    assert_eq!(page.events.len(), 1);
    assert!(!page.more);
    assert_eq!(
        serde_json::from_value::<ClientEvent>(page.events[0].event.clone())
            .expect("a client event"),
        ClientEvent::Harness { event }
    );
    assert_eq!(page.events[0].seq, 1, "the first event is position 1");
}

#[skyzen::test]
async fn the_event_tail_is_replayed_in_order_and_resumes_from_a_cursor() {
    let mut room = Room::open().await;
    room.greet().await;

    for text in ["a", "b", "c"] {
        room.deliver_json(
            Which::Daemon,
            &DaemonToControl::Harness {
                event: assistant_delta(text),
            },
        )
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
    // taken away — which reads the stored tail rather than a live frame.
    let mut room = Room::open().await;
    room.greet().await;

    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::SpotNotice {
            seconds_remaining: 30,
        },
    )
    .await;

    assert_eq!(
        room.drain(),
        vec![to_client(&ClientEvent::SpotNotice {
            seconds_remaining: 30
        })]
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
    room.deliver_json(Which::Daemon, &DaemonToControl::Usage { usage })
        .await;

    assert_eq!(room.drain(), vec![to_client(&ClientEvent::Usage { usage })]);
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

    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::Started {
            harness_session_id: "9d0f4b1a".to_owned(),
        },
    )
    .await;
    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::Capabilities {
            capabilities: vec!["can_use_tool".to_owned()],
        },
    )
    .await;

    assert_eq!(
        room.drain(),
        vec![
            to_client(&ClientEvent::Started {
                harness_session_id: "9d0f4b1a".to_owned()
            }),
            to_client(&ClientEvent::Capabilities {
                capabilities: vec!["can_use_tool".to_owned()]
            }),
        ]
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
    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::ApprovalRequest {
            id,
            payload: payload.clone(),
        },
    )
    .await;

    assert_eq!(
        room.drain(),
        vec![to_client(&ClientEvent::ApprovalPending { id, payload })]
    );
}

// ── Browsers → daemon ──

#[skyzen::test]
async fn a_user_message_is_forwarded_to_the_daemon_and_echoed_to_browsers() {
    let mut room = Room::open().await;
    room.greet().await;

    let text = "what does this crate do?";
    let command = ControlToDaemon::UserMessage {
        text: text.to_owned(),
    };
    room.deliver_json(Which::Client, &command).await;

    // The browser that typed it already has it; every *other* browser
    // watching the session would otherwise see the agent answer a question
    // it could not see.
    assert_eq!(
        room.drain(),
        vec![
            to_client(&ClientEvent::UserMessage {
                text: text.to_owned()
            }),
            to_daemon(&command),
        ]
    );

    let page = room.events(0).await;
    assert_eq!(
        page.events
            .iter()
            .map(|stored| stored.event.clone())
            .collect::<Vec<_>>(),
        vec![
            serde_json::to_value(ClientEvent::UserMessage {
                text: text.to_owned()
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

    room.deliver_json(
        Which::Client,
        &ControlToDaemon::ShellCommand {
            command: "git status --short".to_owned(),
        },
    )
    .await;

    // The room names the run: a browser sends a request, and what reaches
    // the daemon is an instruction with the identity every frame about it
    // will carry.
    let sent = room.drain();
    let [echoed, forwarded] = sent.as_slice() else {
        panic!("a shell command is echoed once and forwarded once, not {sent:?}");
    };
    let ClientEvent::ShellCommand { run, command } = event_in(echoed) else {
        panic!("the browsers see the command that was asked for");
    };
    assert_eq!(command, "git status --short");
    assert_eq!(
        forwarded,
        &to_daemon(&ControlToDaemon::RunShell {
            run,
            command: "git status --short".to_owned(),
        }),
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

    // No handshake: the machine is still being provisioned, or its daemon
    // is mid-reconnect. Nothing runs the command, and the user is told so
    // instead of watching a row that never finishes.
    room.deliver_json(
        Which::Client,
        &ControlToDaemon::ShellCommand {
            command: "ls".to_owned(),
        },
    )
    .await;

    let sent = room.drain();
    let [asked, answered] = sent.as_slice() else {
        panic!("an unrunnable command is echoed and then closed off, not {sent:?}");
    };
    let ClientEvent::ShellCommand { run, .. } = event_in(asked) else {
        panic!("the command is still recorded");
    };
    assert_eq!(
        answered,
        &to_client(&ClientEvent::ShellExited {
            run,
            outcome: ShellOutcome::Offline,
            truncated: false,
        })
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
        room.deliver_json(Which::Daemon, &frame).await;
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
async fn every_command_a_client_may_send_is_forwarded() {
    let mut room = Room::open().await;
    room.greet().await;

    for command in [
        ControlToDaemon::UserMessage {
            text: "go".to_owned(),
        },
        ControlToDaemon::Interrupt,
        ControlToDaemon::Compact,
        ControlToDaemon::TerminalInput {
            data: "ls\n".to_owned(),
        },
    ] {
        room.deliver_json(Which::Client, &command).await;
        let sent = room.drain();
        assert_eq!(
            sent.last(),
            Some(&to_daemon(&command)),
            "{command:?} must reach the daemon"
        );
    }
}

#[skyzen::test]
async fn a_client_reaching_for_control_plane_authority_is_closed() {
    for command in [
        ControlToDaemon::Archive {
            preserve_workdir: false,
        },
        ControlToDaemon::Budget {
            signal: flyco_core::BudgetSignal::Pause,
        },
        ControlToDaemon::ApprovalDecision {
            id: ApprovalId::generate(),
            decision: ApprovalDecision::Approved,
        },
    ] {
        let mut room = Room::open().await;
        room.greet().await;
        room.deliver_json(Which::Client, &command).await;

        assert_eq!(
            room.drain(),
            vec![Sent::Closed {
                to: ROLE_CLIENT.to_owned(),
                code: CLOSE_POLICY,
            }],
            "a browser must not be able to send {command:?}"
        );
    }
}

// ── The control plane → the room ──

#[skyzen::test]
async fn a_decided_approval_reaches_the_daemon_and_every_browser() {
    let mut room = Room::open().await;
    room.greet().await;

    let id = ApprovalId::generate();
    let command = ControlToDaemon::ApprovalDecision {
        id,
        decision: ApprovalDecision::Approved,
    };
    let (status, _) = room
        .call(
            Method::POST,
            "/internal/command",
            Some(serde_json::to_vec(&command).expect("serialize")),
        )
        .await;
    assert_eq!(status, 204);

    assert_eq!(
        room.drain(),
        vec![
            to_daemon(&command),
            to_client(&ClientEvent::ApprovalDecided {
                id,
                decision: ApprovalDecision::Approved,
            }),
        ]
    );
}

#[skyzen::test]
async fn a_message_forwarded_from_the_worker_is_recorded_like_any_other() {
    let mut room = Room::open().await;
    room.greet().await;

    let text = "keep going";
    let command = ControlToDaemon::UserMessage {
        text: text.to_owned(),
    };
    let (status, _) = room
        .call(
            Method::POST,
            "/internal/command",
            Some(serde_json::to_vec(&command).expect("serialize")),
        )
        .await;
    assert_eq!(status, 204);

    let echo = ClientEvent::UserMessage {
        text: text.to_owned(),
    };
    assert_eq!(room.drain(), vec![to_client(&echo), to_daemon(&command)]);
    assert_eq!(
        room.events(0)
            .await
            .events
            .into_iter()
            .map(|stored| stored.event)
            .collect::<Vec<_>>(),
        vec![serde_json::to_value(&echo).expect("serialize")],
        "the route a message came in by is not something a replay can tell"
    );
}

// ── The working tree ──

#[skyzen::test]
async fn a_working_tree_nobody_has_looked_at_is_not_found() {
    let mut room = Room::open().await;
    let (status, _) = room.call(Method::GET, "/internal/repo-status", None).await;
    assert_eq!(
        status, 404,
        "an unreported tree is not a clean one, and must not be answered as one"
    );
}

#[skyzen::test]
async fn the_working_tree_the_daemon_reported_is_served_back() {
    let mut room = Room::open().await;
    room.greet().await;

    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::RepoDirty {
            summary: " M src/lib.rs".to_owned(),
        },
    )
    .await;

    let (status, body) = room.call(Method::GET, "/internal/repo-status", None).await;
    assert_eq!(status, 200);
    let reported: flyco_core::RepoStatus = serde_json::from_slice(&body).expect("a working tree");
    assert!(reported.dirty);
    assert_eq!(reported.summary, " M src/lib.rs");

    // A later report replaces it: the tree is a current value, not a log.
    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::RepoDirty {
            summary: String::new(),
        },
    )
    .await;
    let (_, body) = room.call(Method::GET, "/internal/repo-status", None).await;
    let reported: flyco_core::RepoStatus = serde_json::from_slice(&body).expect("a working tree");
    assert!(
        !reported.dirty,
        "an empty `git status --short` is the clean tree"
    );
}

// ── Questions about the checkout ──

/// The question the `Files` tab asks, addressed to `id`.
fn ask(id: flyco_core::WorkdirRequestId) -> ControlToDaemon {
    ControlToDaemon::InspectWorkdir {
        id,
        request: flyco_core::workdir::WorkdirRequest::Entries {
            path: "src".to_owned(),
        },
    }
}

/// One listing, as a daemon would answer it.
fn listing() -> flyco_core::workdir::WorkdirReply {
    flyco_core::workdir::WorkdirReply::Entries {
        listing: flyco_core::workdir::DirectoryListing {
            path: "src".to_owned(),
            entries: vec![flyco_core::workdir::DirectoryEntry {
                name: "lib.rs".to_owned(),
                path: "src/lib.rs".to_owned(),
                kind: flyco_core::workdir::EntryKind::File,
                size_bytes: Some(12),
                ignored: false,
            }],
            truncated: false,
        },
    }
}

#[skyzen::test]
async fn a_question_about_the_checkout_with_no_daemon_to_answer_it_is_refused_at_once() {
    let mut room = Room::open().await;
    let id = flyco_core::WorkdirRequestId::generate();

    let (status, _) = room
        .call(
            Method::POST,
            "/internal/workdir",
            Some(serde_json::to_vec(&ask(id)).expect("serialize")),
        )
        .await;
    assert_eq!(
        status, 503,
        "a browser waiting for a listing is told at once that nothing can read it"
    );
    assert_eq!(room.drain(), vec![], "there was nobody to forward it to");
}

#[skyzen::test]
async fn a_question_reaches_the_daemon_and_its_answer_is_collected_once() {
    let mut room = Room::open().await;
    room.greet().await;
    let id = flyco_core::WorkdirRequestId::generate();

    let (status, _) = room
        .call(
            Method::POST,
            "/internal/workdir",
            Some(serde_json::to_vec(&ask(id)).expect("serialize")),
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(room.drain(), vec![to_daemon(&ask(id))]);

    // Nothing to collect until the daemon has answered.
    let (status, _) = room
        .call(Method::GET, &format!("/internal/workdir?id={id}"), None)
        .await;
    assert_eq!(status, 404);

    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::WorkdirReply {
            id,
            reply: listing(),
        },
    )
    .await;
    assert_eq!(
        room.drain(),
        vec![],
        "an answer addressed to one request is not shown to every browser watching"
    );
    assert_eq!(
        room.events(0).await.events,
        Vec::<StoredEvent>::new(),
        "nor is it part of what a replay carries"
    );

    let (status, body) = room
        .call(Method::GET, &format!("/internal/workdir?id={id}"), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<flyco_core::workdir::WorkdirReply>(&body).expect("a reply"),
        listing()
    );

    let (status, _) = room
        .call(Method::GET, &format!("/internal/workdir?id={id}"), None)
        .await;
    assert_eq!(
        status, 404,
        "the Worker that asked is the only caller there will ever be"
    );
}

#[skyzen::test]
async fn an_answer_to_a_question_nobody_asked_is_not_served_to_another_one() {
    let mut room = Room::open().await;
    room.greet().await;

    let answered = flyco_core::WorkdirRequestId::generate();
    room.deliver_json(
        Which::Daemon,
        &DaemonToControl::WorkdirReply {
            id: answered,
            reply: listing(),
        },
    )
    .await;

    let other = flyco_core::WorkdirRequestId::generate();
    let (status, _) = room
        .call(Method::GET, &format!("/internal/workdir?id={other}"), None)
        .await;
    assert_eq!(status, 404);
}

#[skyzen::test]
async fn an_archive_command_announces_the_new_lifecycle_state() {
    let mut room = Room::open().await;
    room.greet().await;

    let (status, _) = room
        .call(
            Method::POST,
            "/internal/command",
            Some(
                serde_json::to_vec(&ControlToDaemon::Archive {
                    preserve_workdir: false,
                })
                .expect("serialize"),
            ),
        )
        .await;
    assert_eq!(status, 204);

    assert_eq!(
        room.drain(),
        vec![
            to_daemon(&ControlToDaemon::Archive {
                preserve_workdir: false,
            }),
            to_client(&ClientEvent::SessionStateChanged {
                state: flyco_core::SessionState::Archived,
            }),
        ]
    );
}

#[skyzen::test]
async fn a_budget_signal_reaches_the_daemon_without_disturbing_browsers() {
    let mut room = Room::open().await;
    room.greet().await;

    let command = ControlToDaemon::Budget {
        signal: flyco_core::BudgetSignal::Pause,
    };
    let (status, _) = room
        .call(
            Method::POST,
            "/internal/command",
            Some(serde_json::to_vec(&command).expect("serialize")),
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(room.drain(), vec![to_daemon(&command)]);
}

// ── The internal boundary ──

#[skyzen::test]
async fn a_room_route_without_the_internal_marker_is_refused() {
    let mut room = Room::open().await;

    let mut request = Request::new(Body::empty());
    *request.method_mut() = Method::GET;
    *request.uri_mut() = "https://session-room.flyco.invalid/internal/events"
        .parse()
        .expect("a valid room URL");
    request
        .extensions_mut()
        .insert(DurableDb::new(room.db.clone()));

    let response = room
        .object
        .fetch()
        .go(request)
        .await
        .expect("the room answered");
    assert_eq!(response.status().as_u16(), 502);
}

#[skyzen::test]
async fn a_relay_upgrade_names_its_role_and_session() {
    let mut room = Room::open().await;

    // No role header at all.
    let (status, _) = room.call(Method::GET, "/relay/daemon", None).await;
    assert_eq!(status, 502, "an upgrade with no role is refused");

    // The right role reaches the accept path, which native builds refuse
    // with 501: nothing native carries a browser's upgrade this far.
    let mut request = Request::new(Body::empty());
    *request.method_mut() = Method::GET;
    *request.uri_mut() = "https://session-room.flyco.invalid/relay/daemon"
        .parse()
        .expect("a valid room URL");
    for (name, value) in [
        (HEADER_INTERNAL, INTERNAL.to_owned()),
        (HEADER_SESSION, room.session.to_string()),
        (HEADER_ROLE, ROLE_DAEMON.to_owned()),
    ] {
        request
            .headers_mut()
            .insert(name, value.parse().expect("a valid header"));
    }
    let response = room
        .object
        .fetch()
        .go(request)
        .await
        .expect("the room answered");
    assert_eq!(response.status().as_u16(), 501);

    // A daemon's credentials must not open a browser's socket.
    let mut request = Request::new(Body::empty());
    *request.method_mut() = Method::GET;
    *request.uri_mut() = "https://session-room.flyco.invalid/relay/client"
        .parse()
        .expect("a valid room URL");
    for (name, value) in [
        (HEADER_INTERNAL, INTERNAL.to_owned()),
        (HEADER_SESSION, room.session.to_string()),
        (HEADER_ROLE, ROLE_DAEMON.to_owned()),
    ] {
        request
            .headers_mut()
            .insert(name, value.parse().expect("a valid header"));
    }
    let response = room
        .object
        .fetch()
        .go(request)
        .await
        .expect("the room answered");
    assert_eq!(
        response.status().as_u16(),
        502,
        "a daemon upgrade must not be accepted on the client route"
    );
}

// ── The daemon's mailbox ──
//
// A user message is conversation: it must reach the agent whether or not a
// daemon happened to be connected when it was written. Both orders are
// covered, because they are the two real ones — the prompt `POST
// /v1/sessions` writes minutes before a machine exists, and every message
// after that.

#[skyzen::test]
async fn a_message_sent_before_the_daemon_arrives_is_delivered_on_its_hello() {
    let mut room = Room::open().await;

    // No handshake yet: this is the session's opening prompt, written by the
    // Worker while the machine is still being provisioned.
    let command = ControlToDaemon::UserMessage {
        text: "add a test for the mailbox".to_owned(),
    };
    let (status, _) = room
        .call(
            Method::POST,
            "/internal/command",
            Some(serde_json::to_vec(&command).expect("serialize")),
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(
        room.drain(),
        vec![to_client(&ClientEvent::UserMessage {
            text: "add a test for the mailbox".to_owned(),
        })],
        "browsers see it immediately; the daemon is not there to see anything"
    );

    room.hello().await;
    assert_eq!(
        room.drain(),
        vec![welcome(), to_daemon(&command)],
        "the greeting is answered with everything the daemon missed"
    );

    // A reconnect does not replay it a second time: the cursor moved.
    room.greet().await;
}

#[skyzen::test]
async fn messages_held_for_a_daemon_are_replayed_in_the_order_they_were_written() {
    let mut room = Room::open().await;

    for text in ["first", "second", "third"] {
        let (status, _) = room
            .call(
                Method::POST,
                "/internal/command",
                Some(
                    serde_json::to_vec(&ControlToDaemon::UserMessage {
                        text: text.to_owned(),
                    })
                    .expect("serialize"),
                ),
            )
            .await;
        assert_eq!(status, 204);
    }
    room.drain();

    room.hello().await;
    let expected: Vec<Sent> = core::iter::once(welcome())
        .chain(["first", "second", "third"].into_iter().map(|text| {
            to_daemon(&ControlToDaemon::UserMessage {
                text: text.to_owned(),
            })
        }))
        .collect();
    assert_eq!(
        room.drain(),
        expected,
        "a conversation replayed out of order is a different conversation"
    );
}

#[skyzen::test]
async fn a_message_sent_while_the_daemon_is_connected_is_not_replayed_later() {
    let mut room = Room::open().await;
    room.greet().await;

    let command = ControlToDaemon::UserMessage {
        text: "keep going".to_owned(),
    };
    room.deliver_json(Which::Client, &command).await;
    room.drain();

    // The daemon dropped its socket and came back. It already has that
    // message, and hearing it again would run the turn twice.
    room.greet().await;
}

#[skyzen::test]
async fn only_user_messages_wait_for_a_daemon() {
    let mut room = Room::open().await;

    // An interrupt for a daemon that is not there is about a turn that is
    // not running; replaying it into a later one would be an instruction
    // nobody gave.
    let (status, _) = room
        .call(
            Method::POST,
            "/internal/command",
            Some(
                serde_json::to_vec(&ControlToDaemon::Budget {
                    signal: flyco_core::BudgetSignal::Pause,
                })
                .expect("serialize"),
            ),
        )
        .await;
    assert_eq!(status, 204);
    room.drain();

    room.greet().await;
}
