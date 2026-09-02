//! The host room, driven the way Cloudflare drives it.
//!
//! The room is exercised through the real trait surface —
//! `WebSocketConnection::new`, `DurableConnections::new`,
//! `DurableContext::new` — against skyzen's SQLite-backed
//! [`InMemoryDurableDb`] and the recording sockets of
//! [`crate::tests::sockets`]. What these tests observe is what the room *did
//! to its socket*, which is the one thing the runtime cannot supply.

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, RwLock};

use flyco_core::host::{HostFacts, JobOutcome};
use flyco_core::{HostId, MachineId};
use flyco_provider::host::{
    ContainerJob, ControlToHost, HostToControl, container_name, volume_name,
};
use skyzen::durable::{
    DurableConnections, DurableContext, DurableObject as _, DurableObjectId, WebSocketConnection,
    WebSocketEvent,
};
use skyzen::http_kit::ws::WebSocketMessage;
use skyzen::{Body, Method, Request};
use skyzen_services::durable::{Alarm, DurableDb, DurableKv};
use skyzen_test::mock::{InMemoryAlarm, InMemoryDurableDb, InMemoryDurableKv};

use crate::host_room::{HEADER_HOST, HostRoom, HostStatus, ROLE_HOST};
use crate::room::{HEADER_INTERNAL, INTERNAL};
use crate::testing::host_facts;
use crate::tests::sockets::{FakeConnections, FakeSocket, Sent};

/// A room with one host socket and a real database.
struct Room {
    host: HostId,
    object: HostRoom,
    machine: WebSocketConnection,
    connections: FakeConnections,
    db: InMemoryDurableDb,
    kv: InMemoryDurableKv,
    sent: Receiver<Sent>,
}

impl Room {
    async fn open() -> Self {
        let host = HostId::generate();
        let (sender, sent) = channel();
        let machine = FakeSocket {
            tags: vec![ROLE_HOST.to_owned(), format!("host:{host}")],
            sent: sender,
            attachment: Arc::new(RwLock::new(None)),
        };
        let connections = FakeConnections {
            sockets: vec![machine.clone()],
        };

        Self {
            host,
            object: HostRoom,
            machine: WebSocketConnection::new(Box::new(machine)),
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
            DurableObjectId::new(self.host.to_string(), Some(self.host.to_string())),
        )
    }

    /// Delivers one frame from the machine, exactly as the runtime would.
    async fn deliver(&mut self, frame: &HostToControl) {
        let text = serde_json::to_string(frame).expect("serialize");
        let context = self.context();
        self.object
            .websocket(
                &self.machine,
                WebSocketEvent::Message(WebSocketMessage::Text(text.into())),
                &context,
            )
            .await
            .expect("the room handled the frame");
    }

    /// The machine's handshake, which is also what replays its mailbox.
    async fn hello(&mut self) {
        self.deliver(&HostToControl::Hello {
            facts: Box::new(host_facts()),
        })
        .await;
    }

    /// Everything the room has sent since the last drain.
    fn drain(&self) -> Vec<Sent> {
        self.sent.try_iter().collect()
    }

    /// Every container job the room wrote to the machine, in order.
    fn jobs_sent(&self) -> Vec<ContainerJob> {
        self.drain()
            .into_iter()
            .filter_map(|sent| match sent {
                Sent::Text { text, .. } => match serde_json::from_str(&text) {
                    Ok(ControlToHost::Run { job }) => Some(job),
                    _ => None,
                },
                Sent::Closed { .. } => None,
            })
            .collect()
    }

    /// Calls one of the room's HTTP routes, the way the Worker does.
    async fn call(&mut self, method: Method, path: &str, body: Option<Vec<u8>>) -> (u16, Vec<u8>) {
        let mut request = Request::new(body.map_or_else(Body::empty, Body::from));
        *request.method_mut() = method;
        *request.uri_mut() = format!("https://host-room.flyco.invalid{path}")
            .parse()
            .expect("a valid room URL");
        for (name, value) in [
            (HEADER_INTERNAL, INTERNAL.to_owned()),
            (HEADER_HOST, self.host.to_string()),
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
        // same here keeps this an integration test of `fetch`.
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

    /// Posts one command, the way the Worker does.
    async fn command(&mut self, command: &ControlToHost) {
        let body = serde_json::to_vec(command).expect("serialize");
        let (status, answer) = self
            .call(Method::POST, "/internal/command", Some(body))
            .await;
        assert_eq!(
            status,
            204,
            "the room refused a command: {}",
            String::from_utf8_lossy(&answer)
        );
    }

    async fn status(&mut self) -> HostStatus {
        let (status, body) = self.call(Method::GET, "/internal/status", None).await;
        assert_eq!(status, 200, "reading the status");
        serde_json::from_slice(&body).expect("a host status")
    }
}

/// A create job for one machine, as the planner produces it.
fn create(machine: MachineId) -> ControlToHost {
    ControlToHost::Run {
        job: ContainerJob::Create {
            container: container_name(machine),
            volume: volume_name(machine),
            image: flyco_provider::host::DEFAULT_IMAGE.to_owned(),
            machine,
            bootstrap: Box::new(crate::tests::hosts::bootstrap()),
        },
    }
}

fn stop(machine: MachineId) -> ControlToHost {
    ControlToHost::Run {
        job: ContainerJob::Stop {
            container: container_name(machine),
        },
    }
}

#[skyzen::test]
async fn a_greeting_records_what_the_machine_says_it_is() {
    let mut room = Room::open().await;
    assert!(!room.status().await.connected, "nothing has greeted yet");

    room.hello().await;

    let status = room.status().await;
    assert!(
        status.connected,
        "a greeted socket is a machine flyco can reach"
    );
    assert_eq!(status.facts, Some(host_facts()));
    assert_eq!(status.pending_jobs, 0);
}

#[skyzen::test]
async fn a_machine_that_has_not_greeted_is_written_to_by_nothing() {
    let mut room = Room::open().await;
    let machine = MachineId::generate();

    room.command(&create(machine)).await;

    assert!(
        room.jobs_sent().is_empty(),
        "a socket that has not identified its machine gets no session credentials"
    );
    // Held, not lost: the job is what a session is waiting for.
    assert_eq!(room.status().await.pending_jobs, 1);
}

#[skyzen::test]
async fn a_job_planned_while_the_machine_is_away_is_waiting_when_it_returns() {
    let mut room = Room::open().await;
    let first = MachineId::generate();
    let second = MachineId::generate();

    room.command(&create(first)).await;
    room.command(&stop(second)).await;
    assert_eq!(room.status().await.pending_jobs, 2);

    room.hello().await;

    let replayed = room.jobs_sent();
    assert_eq!(replayed.len(), 2, "both jobs are handed over");
    assert_eq!(
        replayed[0].container(),
        container_name(first),
        "oldest first: a container has to exist before anything stops it"
    );
    assert_eq!(replayed[1].container(), container_name(second));
}

#[skyzen::test]
async fn a_job_is_kept_until_the_machine_answers_it() {
    let mut room = Room::open().await;
    let machine = MachineId::generate();
    room.hello().await;
    let _ = room.drain();

    room.command(&create(machine)).await;
    assert_eq!(
        room.jobs_sent().len(),
        1,
        "a connected machine is handed the job at once"
    );
    assert_eq!(
        room.status().await.pending_jobs,
        1,
        "and it stays outstanding: a machine that died mid-podman gets asked again"
    );

    room.deliver(&HostToControl::JobResult {
        job_id: machine,
        outcome: JobOutcome::Running {
            container: container_name(machine),
            volume: volume_name(machine),
        },
    })
    .await;

    assert_eq!(room.status().await.pending_jobs, 0);
    room.hello().await;
    assert!(
        room.jobs_sent().is_empty(),
        "an answered job is not handed over again"
    );
}

#[skyzen::test]
async fn answering_one_job_leaves_the_next_one_for_the_same_machine_outstanding() {
    let mut room = Room::open().await;
    let machine = MachineId::generate();
    room.hello().await;

    room.command(&create(machine)).await;
    room.command(&stop(machine)).await;
    assert_eq!(room.status().await.pending_jobs, 2);

    // The answer to the create says nothing about the stop behind it.
    room.deliver(&HostToControl::JobResult {
        job_id: machine,
        outcome: JobOutcome::Running {
            container: container_name(machine),
            volume: volume_name(machine),
        },
    })
    .await;
    assert_eq!(room.status().await.pending_jobs, 1);
}

#[skyzen::test]
async fn a_revoked_machine_is_told_once_and_left_with_no_work() {
    let mut room = Room::open().await;
    let machine = MachineId::generate();
    room.hello().await;
    room.command(&create(machine)).await;
    let _ = room.drain();

    room.command(&ControlToHost::Revoked).await;

    let sent = room.drain();
    assert_eq!(sent.len(), 1, "the machine is told its token is gone");
    assert!(
        matches!(&sent[0], Sent::Text { text, .. } if text.contains("revoked")),
        "{sent:?}"
    );
    assert_eq!(
        room.status().await.pending_jobs,
        0,
        "a machine that will never open another socket has no work outstanding"
    );
}

#[skyzen::test]
async fn a_frame_before_the_greeting_closes_the_socket() {
    let mut room = Room::open().await;

    room.deliver(&HostToControl::Heartbeat).await;

    assert!(
        matches!(room.drain().as_slice(), [Sent::Closed { .. }]),
        "the first frame must be `hello`"
    );
}

#[skyzen::test]
async fn a_frame_this_protocol_does_not_define_closes_the_socket() {
    let mut room = Room::open().await;
    let context = room.context();
    room.object
        .websocket(
            &room.machine,
            WebSocketEvent::Message(WebSocketMessage::Text(r#"{"type":"nonsense"}"#.into())),
            &context,
        )
        .await
        .expect("the room handled the frame");

    assert!(matches!(room.drain().as_slice(), [Sent::Closed { .. }]));
}

#[skyzen::test]
async fn a_room_route_without_the_internal_marker_is_refused() {
    let mut room = Room::open().await;
    let mut request = Request::new(Body::empty());
    *request.uri_mut() = "https://host-room.flyco.invalid/internal/status"
        .parse()
        .expect("a valid room URL");
    request
        .extensions_mut()
        .insert(DurableKv::new(room.kv.clone()));
    request
        .extensions_mut()
        .insert(DurableDb::new(room.db.clone()));
    request
        .extensions_mut()
        .insert(DurableConnections::new(Box::new(room.connections.clone())));

    let response = room
        .object
        .fetch()
        .go(request)
        .await
        .expect("the room answered");
    assert_eq!(
        response.status().as_u16(),
        502,
        "a room route is only ever reached from this Worker, and says so"
    );
}

/// The facts a machine reports are the ones the room hands back, whatever it
/// enrolled with.
#[skyzen::test]
async fn the_newest_facts_win() {
    let mut room = Room::open().await;
    room.hello().await;

    let grown = HostFacts {
        memory_mib: 64 * 1024,
        ..host_facts()
    };
    room.deliver(&HostToControl::Hello {
        facts: Box::new(grown.clone()),
    })
    .await;

    assert_eq!(room.status().await.facts, Some(grown));
}
