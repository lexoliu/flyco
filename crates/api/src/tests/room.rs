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

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, RwLock};

use flyco_core::{
    ApprovalDecision, ApprovalId, ClientEvent, ControlToDaemon, DaemonToControl, HarnessEvent,
    SessionId, WIRE_PROTOCOL_VERSION, wire::ApprovalPayload,
};
use skyzen::durable::{
    DurableConnections, DurableConnectionsInner, DurableContext, DurableObject as _,
    DurableObjectError, DurableObjectId, WebSocketConnection, WebSocketConnectionInner,
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

// ── A connection registry that records what the room sent ──

/// One thing the room did to a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sent {
    /// A text frame, with the tag of the socket it went to.
    Text {
        /// Role tag of the receiving socket.
        to: String,
        /// The frame.
        text: String,
    },
    /// A close, with the tag of the socket it went to.
    Closed {
        /// Role tag of the receiving socket.
        to: String,
        /// Close code.
        code: u16,
    },
}

/// A fake hibernating socket.
///
/// Sends go out on a channel — append-only, drained at the end of a test,
/// no shared mutable state. The attachment is the one thing that must be
/// *read back* (it is how the room remembers a daemon was greeted), and the
/// `WebSocketConnectionInner` trait takes `&self` and is `Send + Sync`, so
/// interior mutability behind a lock is structurally required rather than
/// chosen.
#[derive(Debug, Clone)]
struct FakeSocket {
    tags: Vec<String>,
    sent: Sender<Sent>,
    attachment: Arc<RwLock<Option<Vec<u8>>>>,
}

impl FakeSocket {
    fn tag(&self) -> String {
        self.tags
            .first()
            .cloned()
            .unwrap_or_else(|| "untagged".to_owned())
    }
}

impl WebSocketConnectionInner for FakeSocket {
    fn send_text(&self, text: &str) -> Result<(), DurableObjectError> {
        self.sent
            .send(Sent::Text {
                to: self.tag(),
                text: text.to_owned(),
            })
            .map_err(|error| DurableObjectError::WebSocket(error.to_string()))
    }

    fn send_binary(&self, _data: &[u8]) -> Result<(), DurableObjectError> {
        Err(DurableObjectError::WebSocket(
            "the flyco relay never sends binary frames".to_owned(),
        ))
    }

    fn close(&self, code: u16, _reason: &str) -> Result<(), DurableObjectError> {
        self.sent
            .send(Sent::Closed {
                to: self.tag(),
                code,
            })
            .map_err(|error| DurableObjectError::WebSocket(error.to_string()))
    }

    fn tags(&self) -> Result<Vec<String>, DurableObjectError> {
        Ok(self.tags.clone())
    }

    fn get_attachment_raw(&self) -> Result<Option<Vec<u8>>, DurableObjectError> {
        Ok(self.attachment.read().expect("attachment lock").clone())
    }

    fn set_attachment_raw(&self, data: &[u8]) -> Result<(), DurableObjectError> {
        *self.attachment.write().expect("attachment lock") = Some(data.to_vec());
        Ok(())
    }
}

/// The sockets attached to one room.
#[derive(Debug, Clone, Default)]
struct FakeConnections {
    sockets: Vec<FakeSocket>,
}

impl DurableConnectionsInner for FakeConnections {
    fn all(&self) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        Ok(self
            .sockets
            .iter()
            .cloned()
            .map(|socket| WebSocketConnection::new(Box::new(socket)))
            .collect())
    }

    fn by_tag(&self, tag: &str) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        Ok(self
            .sockets
            .iter()
            .filter(|socket| socket.tags.iter().any(|value| value == tag))
            .cloned()
            .map(|socket| WebSocketConnection::new(Box::new(socket)))
            .collect())
    }

    fn set_auto_response(&self, _request: &str, _response: &str) -> Result<(), DurableObjectError> {
        Ok(())
    }

    fn clear_auto_response(&self) -> Result<(), DurableObjectError> {
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn DurableConnectionsInner> {
        Box::new(self.clone())
    }
}

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

    /// Completes the daemon handshake.
    async fn greet(&mut self) {
        self.deliver_json(
            Which::Daemon,
            &DaemonToControl::Hello {
                protocol_version: WIRE_PROTOCOL_VERSION,
                session: self.session,
            },
        )
        .await;
        assert_eq!(
            self.drain(),
            vec![Sent::Text {
                to: ROLE_DAEMON.to_owned(),
                text: serde_json::to_string(&ControlToDaemon::Welcome).expect("serialize"),
            }],
            "a good hello is answered with exactly one welcome"
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
async fn every_command_a_client_may_send_is_forwarded() {
    let mut room = Room::open().await;
    room.greet().await;

    for command in [
        ControlToDaemon::UserMessage {
            text: "go".to_owned(),
        },
        ControlToDaemon::Interrupt,
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
