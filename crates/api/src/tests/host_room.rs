//! The host room, driven the way Cloudflare drives it.
//!
//! A host room is exercised through the same surface the real one serves:
//! the REST calls the Worker makes, and the attach / command-stream /
//! frames trio `flycod host` runs on. What a test observes is what the
//! room *did* — the status it answers, the commands it hands down the
//! stream, and the mailbox rows it keeps.

use std::time::Duration;

use flyco_core::MachineId;
use flyco_core::host::JobOutcome;
use flyco_provider::host::{
    ContainerJob, ControlToHost, HostAttach, HostCommand, HostFrames, HostToControl,
    container_name,
};
use futures_util::StreamExt as _;
use skyzen::durable::DurableObject as _;
use skyzen::http_kit::sse::SseStream;
use skyzen::{Body, Method, Request};
use skyzen_services::durable::{DurableDb, DurableKv};
use skyzen_test::mock::{InMemoryDurableDb, InMemoryDurableKv};

use crate::host_room::{HEADER_HOST, HostAttachResponse, HostRoom, HostStatus};
use crate::room::{HEADER_INTERNAL, INTERNAL};
use crate::testing::host_facts as facts;

/// How long a test waits for the room to hand a command down the stream.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a quiet stream is watched before a test accepts that nothing
/// is coming.
const QUIET: Duration = Duration::from_millis(400);

// ── The harness ──

struct Room {
    host: flyco_core::HostId,
    object: HostRoom,
    db: InMemoryDurableDb,
    kv: InMemoryDurableKv,
    epoch: Option<u64>,
    out_seq: u64,
    applied: u64,
    commands: Option<SseStream>,
}

impl Room {
    async fn open() -> Self {
        Self {
            host: flyco_core::HostId::generate(),
            object: HostRoom,
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

    fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Request {
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
        request
            .extensions_mut()
            .insert(DurableDb::new(self.db.clone()));
        request
            .extensions_mut()
            .insert(DurableKv::new(self.kv.clone()));
        request
    }

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

    async fn call_streaming(&mut self, path: &str) -> skyzen::Response {
        self.object
            .fetch()
            .go(self.request(Method::GET, path, None))
            .await
            .expect("the room answered")
    }

    /// Attaches the test's machine and answers the epoch it was given.
    async fn attach(&mut self) -> u64 {
        let (status, body) = self
            .call(
                Method::POST,
                "/internal/attach",
                Some(
                    serde_json::to_vec(&HostAttach {
                        facts: Box::new(facts()),
                    })
                    .expect("serialize"),
                ),
            )
            .await;
        assert_eq!(status, 200, "an attach: {}", String::from_utf8_lossy(&body));
        let attached: HostAttachResponse =
            serde_json::from_slice(&body).expect("an attach response");
        self.epoch = Some(attached.epoch);
        self.out_seq = 1;
        attached.epoch
    }

    /// Attaches and opens the command stream: a machine coming up.
    async fn greet(&mut self) {
        self.attach().await;
        self.open_commands().await;
    }

    async fn open_commands(&mut self) {
        let epoch = self.epoch.expect("attach first");
        let response = self
            .call_streaming(&format!("/internal/commands?epoch={epoch}"))
            .await;
        assert_eq!(response.status().as_u16(), 200, "the stream opened");
        self.commands = Some(response.into_body().into_sse());
    }

    /// Reads the next command the room hands the machine.
    async fn next_command(&mut self) -> HostCommand {
        let stream = self.commands.as_mut().expect("a command stream is open");
        let item = tokio_select_quiet(stream, PATIENCE)
            .await
            .expect("a command arrived in time")
            .expect("the stream is still open")
            .expect("a decodable SSE frame");
        assert_eq!(item.event(), Some("command"));
        let command: HostCommand = item.data().expect("a command envelope");
        self.applied = self.applied.max(command.seq);
        command
    }

    async fn expect_quiet(&mut self) {
        let stream = self.commands.as_mut().expect("a command stream is open");
        if let Some(item) = tokio_select_quiet(stream, QUIET).await {
            panic!("the stream stayed quiet, then produced {item:?}");
        }
    }

    async fn expect_end(&mut self) {
        let stream = self.commands.as_mut().expect("a command stream is open");
        match tokio_select_quiet(stream, PATIENCE).await {
            None => panic!("the stream outlived its attach"),
            Some(None) => {}
            Some(Some(item)) => panic!("the stream ended, then produced {item:?}"),
        }
    }

    /// Posts one frame, the way a host's outbound flush does.
    async fn deliver(&mut self, frame: &HostToControl) {
        self.deliver_batch(std::slice::from_ref(frame)).await;
    }

    async fn deliver_batch(&mut self, frames: &[HostToControl]) {
        let epoch = self.epoch.expect("attach first");
        let (status, body) = self
            .deliver_raw(&HostFrames {
                epoch,
                from_seq: self.out_seq,
                ack_through: self.applied,
                frames: frames.to_vec(),
            })
            .await;
        assert_eq!(
            status,
            204,
            "a frames batch: {}",
            String::from_utf8_lossy(&body)
        );
        self.out_seq += frames.len() as u64;
    }

    async fn deliver_raw(&mut self, batch: &HostFrames) -> (u16, Vec<u8>) {
        self.call(
            Method::POST,
            "/internal/frames",
            Some(serde_json::to_vec(batch).expect("serialize")),
        )
        .await
    }

    /// Posts a command, the way the Worker does.
    async fn command(&mut self, command: &ControlToHost) -> u16 {
        let (status, _) = self
            .call(
                Method::POST,
                "/internal/command",
                Some(serde_json::to_vec(command).expect("serialize")),
            )
            .await;
        status
    }

    async fn status(&mut self) -> HostStatus {
        let (status, body) = self.call(Method::GET, "/internal/status", None).await;
        assert_eq!(status, 200, "a status read");
        serde_json::from_slice(&body).expect("a host status")
    }

    /// Marks the host's presence expired.
    async fn expire_presence(&self) {
        let db = DurableDb::new(self.db.clone());
        skyzen::sql!(db, "UPDATE host_presence SET live_until = 0 WHERE id = 0")
            .execute()
            .await
            .expect("presence was expired");
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

/// The simplest container job naming `machine`: a stop.
fn job_for(machine: MachineId) -> ControlToHost {
    ControlToHost::Run {
        job: ContainerJob::Stop {
            container: container_name(machine),
        },
    }
}

// ── The attach ──

#[skyzen::test]
async fn an_attach_mints_an_epoch_and_reports_the_machine() {
    let mut room = Room::open().await;
    let epoch = room.attach().await;

    assert_eq!(epoch, 1, "the first attach is epoch one");
    assert_eq!(
        room.status().await,
        HostStatus {
            connected: true,
            facts: Some(facts()),
            pending_jobs: 0,
        },
        "the room reports the host attached and what it said about itself"
    );
}

#[skyzen::test]
async fn a_superseded_attach_ends_its_command_stream() {
    let mut room = Room::open().await;
    room.greet().await;

    room.attach().await;

    room.expect_end().await;
}

#[skyzen::test]
async fn a_stream_or_batch_naming_a_superseded_epoch_is_refused() {
    let mut room = Room::open().await;
    room.greet().await;
    let stale = room.epoch.expect("attached");

    room.attach().await;

    let response = room
        .call_streaming(&format!("/internal/commands?epoch={stale}"))
        .await;
    assert_eq!(response.status().as_u16(), 409, "the dead epoch's stream");

    let (status, _) = room
        .deliver_raw(&HostFrames {
            epoch: stale,
            from_seq: 1,
            ack_through: 0,
            frames: vec![],
        })
        .await;
    assert_eq!(status, 409, "the dead epoch's frames");
}

#[skyzen::test]
async fn frames_from_a_machine_that_never_attached_are_refused() {
    let mut room = Room::open().await;
    let (status, _) = room
        .deliver_raw(&HostFrames {
            epoch: 1,
            from_seq: 1,
            ack_through: 0,
            frames: vec![HostToControl::JobResult {
                job_id: MachineId::generate(),
                outcome: JobOutcome::Done,
            }],
        })
        .await;
    assert_eq!(status, 502);
}

#[skyzen::test]
async fn a_batch_that_skips_sequence_numbers_is_refused() {
    let mut room = Room::open().await;
    room.greet().await;

    let (status, body) = room
        .deliver_raw(&HostFrames {
            epoch: room.epoch.expect("attached"),
            from_seq: 4,
            ack_through: 0,
            frames: vec![],
        })
        .await;
    assert_eq!(status, 409);
    let problem: flyco_core::Problem = serde_json::from_slice(&body).expect("a problem");
    assert_eq!(problem.kind, "https://flyco.dev/problems/relay-frames-gap");
}

// ── The mailbox ──

#[skyzen::test]
async fn a_job_planned_while_the_machine_is_away_waits_for_it() {
    let mut room = Room::open().await;
    let machine = MachineId::generate();

    // No attach: the machine is rebooting. The job is held, and the room
    // says one is pending.
    let status = room.command(&job_for(machine)).await;
    assert_eq!(status, 204);
    assert_eq!(room.status().await.pending_jobs, 1);

    room.greet().await;
    let delivered = room.next_command().await;
    assert_eq!(delivered.command, job_for(machine));
}

#[skyzen::test]
async fn a_job_is_retired_by_its_answer_not_by_being_delivered() {
    let mut room = Room::open().await;
    room.greet().await;
    let machine = MachineId::generate();

    room.command(&job_for(machine)).await;
    room.next_command().await;

    // The host took the job and died before answering. Acknowledging the
    // read retires nothing: a job is done when its answer arrives.
    room.deliver_batch(&[]).await; // carries ack_through
    assert_eq!(
        room.status().await.pending_jobs,
        1,
        "an acknowledged-but-unanswered job is still owed"
    );

    // So the next attach is asked again.
    room.attach().await;
    room.open_commands().await;
    assert_eq!(room.next_command().await.command, job_for(machine));

    // The answer retires it.
    room.deliver(&HostToControl::JobResult {
        job_id: machine,
        outcome: JobOutcome::Done,
    })
    .await;
    assert_eq!(room.status().await.pending_jobs, 0);
}

#[skyzen::test]
async fn the_oldest_answer_for_a_container_retires_the_oldest_job() {
    let mut room = Room::open().await;
    room.greet().await;
    let machine = MachineId::generate();

    // Create then stop: two jobs on one container. The answer to the
    // first says nothing about the second.
    room.command(&job_for(machine)).await;
    room.command(&job_for(machine)).await;
    room.next_command().await;
    room.next_command().await;
    room.deliver(&HostToControl::JobResult {
        job_id: machine,
        outcome: JobOutcome::Done,
    })
    .await;

    assert_eq!(
        room.status().await.pending_jobs,
        1,
        "one answer retires one job"
    );
}

#[skyzen::test]
async fn a_revocation_forgets_every_outstanding_job() {
    let mut room = Room::open().await;
    let machine = MachineId::generate();

    // Planned while away — the mailbox is holding them.
    room.command(&job_for(machine)).await;
    room.command(&job_for(MachineId::generate())).await;
    assert_eq!(room.status().await.pending_jobs, 2);

    let status = room.command(&ControlToHost::Revoked).await;
    assert_eq!(status, 204);
    assert_eq!(
        room.status().await.pending_jobs,
        0,
        "a revoked host will never attach again, so its mailbox is emptied"
    );
}

#[skyzen::test]
async fn a_command_that_is_not_container_work_is_dropped_for_an_offline_host() {
    let mut room = Room::open().await;

    // `Revoked` is the one non-job command; a host that is away is a host
    // whose token will not work again — nothing is held for it. The jobs
    // are still forgotten, which is what the revocation is *for*.
    let machine = MachineId::generate();
    room.command(&job_for(machine)).await;
    assert_eq!(room.status().await.pending_jobs, 1);

    room.command(&ControlToHost::Revoked).await;
    assert_eq!(room.status().await.pending_jobs, 0);
}

// ── Liveness ──

#[skyzen::test]
async fn an_expired_attachment_reports_the_machine_gone() {
    let mut room = Room::open().await;
    room.attach().await;
    assert!(room.status().await.connected);

    room.expire_presence().await;

    let status = room.status().await;
    assert!(!status.connected, "past the deadline the machine is gone");
    assert_eq!(status.facts, Some(facts()), "its last report is kept");
}

#[skyzen::test]
async fn an_empty_mailbox_replays_nothing_to_a_fresh_attach() {
    let mut room = Room::open().await;
    room.greet().await;
    let machine = MachineId::generate();

    room.command(&job_for(machine)).await;
    room.next_command().await;
    room.deliver(&HostToControl::JobResult {
        job_id: machine,
        outcome: JobOutcome::Done,
    })
    .await;

    room.attach().await;
    room.open_commands().await;
    room.expect_quiet().await;
}
